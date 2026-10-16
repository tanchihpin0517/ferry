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
//!   command that joins the server; see [`striped`]. Beside its path, each
//!   SSH connection carries a heartbeat channel, which joins the server too;
//!   see [`heartbeat`].

pub mod heartbeat;
mod join;
pub mod ssh;
pub mod striped;

use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use join::{Joining, join, listen};
pub use striped::{Failer, Receiving, Sending};

use crate::interrupt;
use crate::protocol::{self, Hello, Message};
use crate::remote::Location;
use crate::session;
use ssh::Ssh;
use striped::Kind;

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
        let Stream {
            child,
            reader,
            writer,
        } = Stream::spawn(self.command(script))?;
        Ok(ControlConnection {
            child,
            reader: Some(reader),
            writer: Arc::new(Mutex::new(Some(writer))),
        })
    }

    /// Opens a connection whose paths each run `script`, which joins the
    /// server at the far end: one path over each SSH connection, or one on
    /// this machine, and beside each, a heartbeat channel, running it too.
    pub fn connect(&self, script: &str) -> Result<Connection> {
        let count = match self {
            Self::Local => 1,
            Self::Ssh(held) => held.len(),
        };
        let mut paths = Vec::new();
        let mut heartbeats = Vec::new();
        for index in 0..count {
            let command = || match self {
                Self::Local => self.command(script),
                Self::Ssh(held) => held[index].ssh.script(script),
            };
            let mut path = Stream::spawn(command())?;
            striped::write_header(&mut path.writer, count, Kind::Path)?;
            paths.push(path);
            let mut heartbeat = Stream::spawn(command())?;
            striped::write_header(&mut heartbeat.writer, count, Kind::Heartbeat)?;
            let name = match self {
                Self::Local => "the connection".to_owned(),
                Self::Ssh(_) => format!("SSH connection {} of {count}", index + 1),
            };
            heartbeats.push((name, heartbeat));
        }
        let monitor = match self {
            Self::Local => None,
            Self::Ssh(held) => Some(Monitor {
                host: held[0].ssh.host().to_owned(),
                bytes: Arc::new((0..count).map(|_| AtomicU64::new(0)).collect()),
                ages: Arc::new(OnceLock::new()),
            }),
        };
        Ok(Connection {
            paths,
            heartbeats,
            cut: false,
            monitor,
        })
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

/// What a program reads on its control connection, until the connection
/// closes; shared with its senders.
type ControlWriter = Arc<Mutex<Option<BufWriter<ChildStdin>>>>;

/// A control connection: a byte stream to one program at the far end.
pub struct ControlConnection {
    child: Child,
    /// What the program writes, until a thread takes it to read.
    reader: Option<BufReader<ChildStdout>>,
    writer: ControlWriter,
}

/// A way to send a server messages on its control connection while the
/// transfer goes on, such as a request for a snapshot.
#[derive(Clone)]
pub struct ControlSender(ControlWriter);

impl ControlSender {
    /// Sends `message`, unless the connection is closed.
    pub fn send<M: Message>(&self, message: &M) -> Result<()> {
        let mut writer = self.0.lock().expect("not poisoned");
        let Some(writer) = writer.as_mut() else {
            return Ok(());
        };
        protocol::send(writer, message)?;
        writer.flush()?;
        Ok(())
    }
}

impl ControlConnection {
    /// Says hello to the server this control connection runs, and waits to
    /// be welcomed; returns the token its paths join by. A server that exits
    /// before it answers, such as one that could not be started, is reported
    /// with how it exited; what it printed is already on standard error.
    pub fn handshake(&mut self, hello: Hello) -> Result<String> {
        let reader = self
            .reader
            .as_mut()
            .context("the connection is being read")?;
        let mut writer = self.writer.lock().expect("not poisoned");
        let writer = writer.as_mut().context("the connection is closed")?;
        match session::client_handshake(reader, writer, hello) {
            Err(error) if error.is::<session::Closed>() => match self.child.wait() {
                Ok(status) => bail!("the server did not start ({status})"),
                Err(_) => bail!("the server did not start"),
            },
            result => result,
        }
    }

    /// What the program writes from now on, for a thread of its own to read.
    pub fn take_reader(&mut self) -> Option<BufReader<ChildStdout>> {
        self.reader.take()
    }

    /// A way to send the program messages from another thread.
    pub fn sender(&self) -> ControlSender {
        ControlSender(Arc::clone(&self.writer))
    }

    /// Closes the connection, and waits for the program to end: a server
    /// ends with its control connection.
    pub fn close(self) -> Result<()> {
        let mut child = self.child;
        self.writer.lock().expect("not poisoned").take();
        let status = child.wait().context("wait for the server")?;
        if !status.success() {
            bail!("the server exited with {status}");
        }
        Ok(())
    }

    /// Closes the connection after a failure: waits for the program to end
    /// for `time` at most, then kills it, as when its SSH connection is gone
    /// and would never say that it ended.
    pub fn close_within(self, time: Duration) -> Result<()> {
        let mut child = self.child;
        self.writer.lock().expect("not poisoned").take();
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().context("wait for the server")? {
                if !status.success() {
                    bail!("the server exited with {status}");
                }
                return Ok(());
            }
            if started.elapsed() >= time {
                let _ = child.kill();
                let _ = child.wait();
                bail!("the server did not end");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A connection: whole messages, in order, to a server, over its paths, and
/// the heartbeat channel beside each.
pub struct Connection {
    paths: Vec<Stream>,
    /// Each named as a failure names it.
    heartbeats: Vec<(String, Stream)>,
    /// Cut the last path short before its first piece: for `ferry test`.
    cut: bool,
    /// For a host, what the dashboard shows of each SSH connection.
    monitor: Option<Monitor>,
}

/// What the dashboard shows of a host's SSH connections: the bytes each has
/// carried, both ways, and how long since its last heartbeat. Clones see the
/// same connections.
#[derive(Clone)]
pub struct Monitor {
    host: String,
    bytes: Arc<Vec<AtomicU64>>,
    /// Set once the heartbeats start.
    ages: Arc<OnceLock<heartbeat::Ages>>,
}

impl Monitor {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// For each SSH connection: the bytes it has carried, and how long since
    /// its last heartbeat, once the heartbeats have started.
    pub fn connections(&self) -> Vec<(u64, Option<Duration>)> {
        let ages = self.ages.get().map(heartbeat::Ages::now);
        self.bytes
            .iter()
            .enumerate()
            .map(|(index, bytes)| {
                (
                    bytes.load(Ordering::Relaxed),
                    ages.as_ref().and_then(|ages| ages.get(index).copied()),
                )
            })
            .collect()
    }
}

/// Counts the bytes that go through, into one of a monitor's counts.
struct Counted<T> {
    inner: T,
    bytes: Option<(Arc<Vec<AtomicU64>>, usize)>,
}

impl<T> Counted<T> {
    fn count(&self, amount: usize) {
        if let Some((bytes, index)) = &self.bytes {
            bytes[*index].fetch_add(amount as u64, Ordering::Relaxed);
        }
    }
}

impl<T: Read> Read for Counted<T> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.count(read);
        Ok(read)
    }
}

impl<T: Write> Write for Counted<T> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.count(written);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Connection {
    /// Cuts the last path short before its first piece, as a dropped
    /// connection would: for `ferry test`.
    pub fn cut_a_path(&mut self) {
        self.cut = true;
    }

    /// For a host, what the dashboard shows of its SSH connections.
    pub fn monitor(&self) -> Option<Monitor> {
        self.monitor.clone()
    }

    /// Splits the connection, so that one thread can receive while another
    /// sends, and starts the heartbeats. A heartbeat channel that falls
    /// silent, or closes before its end, fails the receiving half and kills
    /// every path, so that nothing waits on them.
    pub fn split(self) -> (Receiving, Sending, Paths) {
        let mut readers = Vec::new();
        let mut writers = Vec::new();
        let mut children = Vec::new();
        let counts = self
            .monitor
            .as_ref()
            .map(|monitor| Arc::clone(&monitor.bytes));
        for (
            index,
            Stream {
                child,
                reader,
                writer,
            },
        ) in self.paths.into_iter().enumerate()
        {
            let bytes = counts.clone().map(|counts| (counts, index));
            readers.push(Counted {
                inner: reader,
                bytes: bytes.clone(),
            });
            writers.push(Counted {
                inner: writer,
                bytes,
            });
            children.push(child);
        }
        let mut channels = Vec::new();
        for (name, stream) in self.heartbeats {
            channels.push(heartbeat::Channel {
                name,
                reader: Box::new(stream.reader),
                writer: Box::new(stream.writer),
            });
            children.push(stream.child);
        }
        let receiving = striped::receiving(readers);
        let failer = receiving.failer();
        let ids: Vec<u32> = children.iter().map(Child::id).collect();
        let heartbeats = heartbeat::start(
            channels,
            heartbeat::TIMES,
            Box::new(move |message| {
                failer.fail(message);
                for id in ids {
                    // SAFETY: `kill` takes any pid and signal; the children
                    // are not yet waited for, so the pids are still theirs.
                    unsafe {
                        libc::kill(id as libc::pid_t, libc::SIGKILL);
                    }
                }
            }),
        );
        if let Some(monitor) = &self.monitor {
            let _ = monitor.ages.set(heartbeats.ages());
        }
        (
            receiving,
            striped::sending_cut(writers, self.cut),
            Paths {
                children,
                heartbeats,
            },
        )
    }
}

/// The paths and heartbeat channels of a connection, to end once it is
/// closed.
pub struct Paths {
    children: Vec<Child>,
    heartbeats: heartbeat::Heartbeats,
}

impl Paths {
    /// Ends the heartbeats, and waits for the programs of the paths and the
    /// heartbeat channels to end.
    pub fn wait(self) -> Result<()> {
        self.heartbeats.end();
        let mut result = Ok(());
        for mut child in self.children {
            let status = child.wait().context("wait for a path")?;
            if !status.success() && result.is_ok() {
                result = Err(anyhow::anyhow!("a path ended with {status}"));
            }
        }
        result
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
            .connect("dd bs=1 count=5 >/dev/null 2>&1; exec cat")
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
