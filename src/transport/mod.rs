//! The transport: how the client reaches the place of each side, this machine
//! or a host. Nothing else in Ferry runs `ssh`, or knows whether a place is
//! this machine or another; see the
//! [architecture](../../docs/design/architecture.md#transport).
//!
//! Opening a transport is the first thing that reaches a place: for a host, it
//! opens every SSH connection, each only logged in, running nothing. Through
//! it, the client then opens connections of two kinds, each by naming a script
//! that `sh` runs at the far end:
//!
//! - a **control connection**: a plain byte stream to one program, such as a
//!   probe, an install, or the server, which lives as long as its control
//!   connection;
//! - a **connection**: whole messages, in order, between the client and a
//!   server, spread over paths, one on each SSH connection, that each run a
//!   command that joins the server; see [`striped`].

mod join;
pub mod ssh;
pub mod striped;

use std::io::{BufReader, BufWriter};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use join::{Joining, join, listen};
pub use striped::{Receiving, Sending};

use crate::interrupt;
use crate::protocol::Hello;
use crate::remote::Location;
use crate::session;
use ssh::Ssh;

/// How a place is reached.
pub enum Transport {
    /// This machine: programs run as child processes.
    Local,
    /// A host, over SSH: programs run in sessions of SSH connections already
    /// logged in, each held open for the whole run.
    Ssh(Vec<Held>),
}

/// An SSH connection, logged in, and the `ssh -N` that holds it open.
pub struct Held {
    ssh: Ssh,
    holder: Child,
}

impl Transport {
    /// Opens the transport to a location's place. For a host, this opens
    /// `ssh_connections` SSH connections, one at a time, and runs nothing
    /// there: each is only logged in, and held open. A password or an OTP is
    /// asked for here, if a connection needs one; if one cannot be opened,
    /// neither is the transport.
    pub fn open(location: &Location, ssh_connections: usize) -> Result<Self> {
        let Location::Remote { host, .. } = location else {
            return Ok(Self::Local);
        };
        let count = ssh_connections.max(1);
        let yours = ssh::yours(host);
        let mut held = Vec::new();
        for index in 0..count {
            let ssh = Ssh::new(host, index, yours)?;
            let holder = log_in(&ssh, index, count)?;
            held.push(Held { ssh, holder });
        }
        Ok(Self::Ssh(held))
    }

    /// The place, as messages name it.
    pub fn place(&self) -> &str {
        match self {
            Self::Local => "this machine",
            Self::Ssh(held) => held[0].ssh.host(),
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
            // Control connections go over the first SSH connection.
            Self::Ssh(held) => held[0].ssh.script(script),
        }
    }

    /// Opens a control connection to the program `script` runs.
    pub fn control(&self, script: &str) -> Result<ControlConnection> {
        Ok(ControlConnection(Stream::spawn(self.command(script))?))
    }

    /// Opens a connection whose paths each run `script`, which joins the
    /// server at the far end: one path over each SSH connection, or one on
    /// this machine.
    pub fn connect(&self, script: &str) -> Result<Connection> {
        let commands: Vec<Command> = match self {
            Self::Local => vec![self.command(script)],
            Self::Ssh(held) => held.iter().map(|held| held.ssh.script(script)).collect(),
        };
        let count = commands.len();
        let mut paths = Vec::new();
        for command in commands {
            let mut stream = Stream::spawn(command)?;
            striped::write_header(&mut stream.writer, count)?;
            paths.push(stream);
        }
        Ok(Connection(paths))
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Self::Ssh(held) = self {
            for held in held {
                let _ = held.holder.kill();
                let _ = held.holder.wait();
            }
        }
    }
}

/// Opens the `index`th of `count` SSH connections: quietly first, which needs
/// nothing typed with a key or a master still up, then asking in the
/// terminal. Without a terminal to ask in, it fails, saying which.
fn log_in(ssh: &Ssh, index: usize, count: usize) -> Result<Child> {
    let which = format!("SSH connection {} of {count}", index + 1);
    if let Ok(holder) = hold(ssh, ssh.hold_quietly(), Stdio::null()) {
        return Ok(holder);
    }
    if std::fs::File::open("/dev/tty").is_err() {
        bail!(
            "{}: {which} needs a login, and there is no terminal to type it in",
            ssh.host()
        );
    }
    if count > 1 {
        eprintln!("ferry: logging in {which} to {}", ssh.host());
    }
    hold(ssh, ssh.hold(), Stdio::inherit()).with_context(|| format!("{}: {which}", ssh.host()))
}

/// Starts `holder`, an `ssh -N`, and waits until its ControlMaster is up;
/// `ssh -N` runs nothing, and holds the connection open.
fn hold(ssh: &Ssh, mut holder: Command, stderr: Stdio) -> Result<Child> {
    let up = |ssh: &Ssh| {
        ssh.check()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    let mut holder = holder
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .context("start ssh")?;
    loop {
        if up(ssh) {
            return Ok(holder);
        }
        if let Some(status) = holder.try_wait()? {
            if up(ssh) {
                return Ok(holder);
            }
            bail!("could not log in ({status})");
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

/// A connection: whole messages, in order, to a server, over its paths.
pub struct Connection(Vec<Stream>);

impl Connection {
    /// Splits the connection, so that one thread can receive while another
    /// sends.
    pub fn split(self) -> (Receiving, Sending, Paths) {
        let mut readers = Vec::new();
        let mut writers = Vec::new();
        let mut children = Vec::new();
        for Stream {
            child,
            reader,
            writer,
        } in self.0
        {
            readers.push(reader);
            writers.push(writer);
            children.push(child);
        }
        (
            striped::receiving(readers),
            striped::sending(writers),
            Paths(children),
        )
    }
}

/// The paths of a connection, to wait for once it is closed.
pub struct Paths(Vec<Child>);

impl Paths {
    /// Waits for the paths' programs to end.
    pub fn wait(self) -> Result<()> {
        for mut child in self.0 {
            let status = child.wait().context("wait for a path")?;
            if !status.success() {
                bail!("a path ended with {status}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, Packet};

    /// A packet, as it would arrive.
    fn packet(bytes: &[u8]) -> Packet {
        let encoded = protocol::encode_data(bytes).unwrap();
        protocol::receive(&mut encoded.as_slice()).unwrap().unwrap()
    }

    #[test]
    fn messages_arrive_whole_and_in_order_until_the_end() {
        // A path whose far end sends back what it is sent, its header too,
        // which it so skips first.
        let connection = Transport::Local
            .connect("dd bs=1 count=4 >/dev/null 2>&1; exec cat")
            .unwrap();
        let (mut receiving, sending, paths) = connection.split();
        let mut sending = sending;
        let sent: Vec<Vec<u8>> = (0..200u32)
            .map(|index| vec![index as u8; (index as usize * 997) % 70_000])
            .collect();
        let echoed = std::thread::spawn(move || {
            let mut echoed = Vec::new();
            while let Some(packet) = receiving.recv().unwrap() {
                echoed.push(packet.into_payload());
            }
            echoed
        });
        for bytes in &sent {
            sending.send(packet(bytes)).unwrap();
        }
        // Closing the sending half ends the far end's input, and so the
        // connection.
        sending.close().unwrap();
        assert_eq!(echoed.join().unwrap(), sent);
        paths.wait().unwrap();
    }

    #[test]
    fn a_program_that_fails_says_so_when_closed() {
        let control = Transport::Local.control("exit 3").unwrap();
        let error = control.close().unwrap_err().to_string();
        assert!(error.contains('3'), "{error}");
    }
}
