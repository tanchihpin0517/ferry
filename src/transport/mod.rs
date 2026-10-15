//! The transport: how the client reaches the place of each side, this machine
//! or a host. Nothing else in Ferry runs `ssh`, or knows whether a place is
//! this machine or another; see the
//! [architecture](../../docs/design/architecture.md#transport).
//!
//! Opening a transport is the first thing that reaches a place: for a host, it
//! logs in, and runs nothing. Through it, the client then opens connections
//! of two kinds, each by naming a script that `sh` runs at the far end:
//!
//! - a **control connection**: a plain byte stream to one program, such as a
//!   probe, an install, or the server, which lives as long as its control
//!   connection;
//! - a **connection**: whole messages, in order, between the client and a
//!   server, over paths that each run a command that joins the server.

mod join;
pub mod ssh;

use std::io::{BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use join::{Joining, join, listen};

use crate::config::SshConfig;
use crate::interrupt;
use crate::protocol::{self, Hello, Packet};
use crate::remote::Location;
use crate::session;
use ssh::Ssh;

/// How a place is reached.
pub enum Transport {
    /// This machine: programs run as child processes.
    Local,
    /// A host, over SSH: programs run in sessions of a connection already
    /// logged in, which `held` keeps open for the whole run.
    Ssh { ssh: Ssh, held: Option<Child> },
}

impl Transport {
    /// Opens the transport to a location's place. For a host, this logs in
    /// and waits until the connection is up, running nothing there; a
    /// password or an OTP is asked for here.
    pub fn open(location: &Location, config: &SshConfig) -> Result<Self> {
        match location {
            Location::Local(_) => Ok(Self::Local),
            Location::Remote { host, .. } => {
                let ssh = Ssh::new(host, config)?;
                let held = if ssh.shares() {
                    Some(hold(&ssh)?)
                } else {
                    None
                };
                Ok(Self::Ssh { ssh, held })
            }
        }
    }

    /// The place, as messages name it.
    pub fn place(&self) -> &str {
        match self {
            Self::Local => "this machine",
            Self::Ssh { ssh, .. } => ssh.host(),
        }
    }

    /// A command that runs `script` with `sh` at the far end. Its standard
    /// input and output are the caller's to set; its standard error stays
    /// this process's, so that what goes wrong there shows.
    pub fn command(&self, script: &str) -> Command {
        match self {
            Self::Local => {
                let mut command = Command::new("sh");
                interrupt::unblock_in_child(&mut command)
                    .arg("-c")
                    .arg(script);
                command
            }
            Self::Ssh { ssh, .. } => ssh.script(script),
        }
    }

    /// Opens a control connection to the program `script` runs.
    pub fn control(&self, script: &str) -> Result<ControlConnection> {
        Ok(ControlConnection(Stream::spawn(self.command(script))?))
    }

    /// Opens a connection whose paths each run `script`, which joins the
    /// server at the far end.
    pub fn connect(&self, script: &str) -> Result<Connection> {
        Ok(Connection(Stream::spawn(self.command(script))?))
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Self::Ssh {
            held: Some(held), ..
        } = self
        {
            let _ = held.kill();
            let _ = held.wait();
        }
    }
}

/// Logs in to the host and starts its ControlMaster, unless one is up
/// already, and holds the connection open: `ssh -N` runs nothing.
fn hold(ssh: &Ssh) -> Result<Child> {
    let up = |ssh: &Ssh| {
        ssh.check()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let mut held = ssh
        .hold()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .context("start ssh")?;
    loop {
        if up(ssh) {
            return Ok(held);
        }
        if let Some(status) = held.try_wait()? {
            if up(ssh) {
                return Ok(held);
            }
            bail!("{}: could not log in ({status})", ssh.host());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A child's standard input and output, as a stream.
struct Stream {
    child: Child,
    reader: BufReader<ChildStdout>,
    writer: BufWriter<ChildStdin>,
}

impl Stream {
    fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("start {:?}", command.get_program()))?;
        let reader = BufReader::new(child.stdout.take().expect("piped"));
        let writer = BufWriter::new(child.stdin.take().expect("piped"));
        Ok(Self {
            child,
            reader,
            writer,
        })
    }
}

/// A control connection: a byte stream to one program at the far end.
pub struct ControlConnection(Stream);

impl ControlConnection {
    /// What the program writes.
    pub fn reader(&mut self) -> &mut BufReader<ChildStdout> {
        &mut self.0.reader
    }

    /// What the program reads.
    pub fn writer(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.0.writer
    }

    /// Says hello to the server this control connection runs, and waits to
    /// be welcomed; returns the token its paths join by. A server that exits
    /// before it answers, such as one that could not be started, is reported
    /// with how it exited; what it printed is already on standard error.
    pub fn handshake(&mut self, hello: Hello) -> Result<String> {
        match session::client_handshake(&mut self.0.reader, &mut self.0.writer, hello) {
            Err(error) if error.is::<session::Closed>() => match self.0.child.wait() {
                Ok(status) => bail!("the server did not start ({status})"),
                Err(_) => bail!("the server did not start"),
            },
            result => result,
        }
    }

    /// Closes the connection, and waits for the program to end: a server
    /// ends with its control connection.
    pub fn close(self) -> Result<()> {
        let Stream {
            mut child, writer, ..
        } = self.0;
        drop(writer);
        let status = child.wait().context("wait for the server")?;
        if !status.success() {
            bail!("the server exited with {status}");
        }
        Ok(())
    }
}

/// A connection: whole messages, in order, to a server.
pub struct Connection(Stream);

impl Connection {
    /// Splits the connection, so that one thread can receive while another
    /// sends.
    pub fn split(self) -> (Receiving, Sending, Paths) {
        let Stream {
            child,
            reader,
            writer,
        } = self.0;
        (Receiving(reader), Sending(writer), Paths(child))
    }
}

/// The receiving half of a connection.
pub struct Receiving(BufReader<ChildStdout>);

impl Receiving {
    /// The next message, or `None` once the far end has closed.
    pub fn recv(&mut self) -> Result<Option<Packet>> {
        protocol::receive(&mut self.0)
    }

    /// Whether a message is already here, so that one received now would
    /// not wait.
    pub fn ready(&self) -> bool {
        !self.0.buffer().is_empty()
    }

    /// Reads whatever is left, so that the far end never waits to write.
    pub fn drain(mut self) {
        let _ = std::io::copy(&mut self.0, &mut std::io::sink());
    }
}

/// The sending half of a connection. Closing it, by dropping it, ends the
/// far end's input.
pub struct Sending(BufWriter<ChildStdin>);

impl Sending {
    /// Sends a message. It may wait in a buffer until `flush`.
    pub fn send(&mut self, packet: &Packet) -> Result<()> {
        protocol::forward(&mut self.0, packet)
    }

    /// Sends what waits in the buffer.
    pub fn flush(&mut self) -> Result<()> {
        self.0.flush()?;
        Ok(())
    }
}

/// The paths of a connection, to wait for once it is closed.
pub struct Paths(Child);

impl Paths {
    /// Waits for the paths' programs to end.
    pub fn wait(mut self) -> Result<()> {
        let status = self.0.wait().context("wait for a path")?;
        if !status.success() {
            bail!("a path ended with {status}");
        }
        Ok(())
    }
}
