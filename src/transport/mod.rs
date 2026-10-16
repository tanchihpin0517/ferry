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
pub mod start;
pub mod striped;

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use join::{Joined, Joining, STRAIGHT, join, listen};
pub use striped::{Failer, Receiving, Sending};

use crate::interrupt;
use crate::protocol::{self, Hello, Message, Welcome};
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

    /// Opens a control connection to the program `script` runs, writing
    /// `first` to it before anything else, such as a password for `sudo` to
    /// read before it runs the program.
    pub fn control(&self, script: &str, first: &[u8]) -> Result<ControlConnection> {
        let Stream {
            child,
            reader,
            writer,
        } = Stream::spawn(self.command(script), first)?;
        Ok(ControlConnection {
            place: self.place().to_owned(),
            child,
            reader: Some(reader),
            writer: Arc::new(Mutex::new(Some(writer))),
        })
    }

    /// Opens a connection to the server that sent `welcome`: one path over
    /// each SSH connection, or one on this machine, and beside each, a
    /// heartbeat channel. Each reaches the server's socket straight, as a
    /// channel SSH forwards to it, which nothing else on the host can write
    /// to. Where SSH will not forward to a socket, each runs `join` instead,
    /// which joins the server through it.
    pub fn connect(&self, welcome: &Welcome, join: &str) -> Result<Connection> {
        let count = match self {
            Self::Local => 1,
            Self::Ssh(held) => held.len(),
        };
        let socket = String::from_utf8_lossy(&welcome.socket);
        // Whether the paths go straight: the first one tells, waited for
        // before any other starts.
        let mut straight = true;
        let mut links: Vec<(Kind, Link)> = Vec::new();
        for index in 0..count {
            for kind in [Kind::Path, Kind::Heartbeat] {
                let mut link = None;
                if straight {
                    match self.straight(index, &socket, count, kind) {
                        Ok(mut tried) if !links.is_empty() => {
                            tried.started = false;
                            link = Some(tried);
                        }
                        Ok(mut tried) => match start::skip(&mut tried.reader) {
                            Ok(skipped) if skipped.found => {
                                tried.started = true;
                                link = Some(tried);
                            }
                            _ => {
                                tried.end();
                                straight = false;
                            }
                        },
                        Err(_) if links.is_empty() => straight = false,
                        Err(error) => return Err(error),
                    }
                }
                let link = match link {
                    Some(link) => link,
                    None => self.joining(index, join, count, kind)?,
                };
                links.push((kind, link));
            }
        }
        // Each writes the start mark as it starts, so they are all waited
        // for at once. Only a joining process runs through a shell, which may
        // print before it.
        for (_, link) in &mut links {
            if link.started {
                continue;
            }
            let skipped = start::skip(&mut link.reader)
                .with_context(|| format!("{}: wait for a path to start", self.place()))?;
            if !straight {
                say_skipped(self.place(), &skipped.before);
            }
            if !skipped.found {
                bail!("{}: a path ended before it started", self.place());
            }
        }
        let mut paths = Vec::new();
        let mut heartbeats = Vec::new();
        for (number, (kind, link)) in links.into_iter().enumerate() {
            match kind {
                Kind::Path => paths.push(link),
                Kind::Heartbeat => {
                    let name = match self {
                        Self::Local => "the connection".to_owned(),
                        Self::Ssh(_) => format!("SSH connection {} of {count}", number / 2 + 1),
                    };
                    heartbeats.push((name, link));
                }
            }
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

    /// A path or heartbeat channel over the `index`th SSH connection, or on
    /// this machine, that reaches the server's socket straight, its first
    /// bytes written.
    fn straight(&self, index: usize, socket: &str, count: usize, kind: Kind) -> Result<Link> {
        let mut link = match self {
            Self::Local => {
                let stream =
                    UnixStream::connect(socket).with_context(|| format!("connect to {socket}"))?;
                Link::socket(stream)?
            }
            // What `ssh` says when it cannot forward is no matter: the path
            // then joins instead.
            Self::Ssh(held) => Link::spawn(held[index].ssh.forward(socket), Stdio::null())?,
        };
        link.writer.write_all(&[STRAIGHT])?;
        striped::write_header(&mut link.writer, count, kind)?;
        Ok(link)
    }

    /// A path or heartbeat channel over the `index`th SSH connection, or on
    /// this machine, that runs `join`, its first bytes written.
    fn joining(&self, index: usize, join: &str, count: usize, kind: Kind) -> Result<Link> {
        let command = match self {
            Self::Local => self.command(join),
            Self::Ssh(held) => held[index].ssh.script(join),
        };
        let mut link = Link::spawn(command, Stdio::inherit())?;
        striped::write_header(&mut link.writer, count, kind)?;
        Ok(link)
    }
}

/// A path or a heartbeat channel, as the client has it.
struct Link {
    /// The process that carries it, if any: `ssh`, or a joining process.
    child: Option<Child>,
    /// The socket it is, on this machine, to shut down to stop it.
    socket: Option<UnixStream>,
    reader: BufReader<Box<dyn Read + Send>>,
    writer: BufWriter<Box<dyn Write + Send>>,
    /// Its start mark has been read.
    started: bool,
}

impl Link {
    /// Starts `command`, whose input and output the link is.
    fn spawn(mut command: Command, stderr: Stdio) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("start {:?}", command.get_program()))?;
        let reader: Box<dyn Read + Send> = Box::new(child.stdout.take().expect("piped"));
        let writer: Box<dyn Write + Send> = Box::new(child.stdin.take().expect("piped"));
        Ok(Self {
            child: Some(child),
            socket: None,
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            started: false,
        })
    }

    /// A link that is `stream`.
    fn socket(stream: UnixStream) -> Result<Self> {
        let reader: Box<dyn Read + Send> = Box::new(stream.try_clone()?);
        let shut = stream.try_clone()?;
        let writer: Box<dyn Write + Send> = Box::new(join::Writing(stream));
        Ok(Self {
            child: None,
            socket: Some(shut),
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            started: false,
        })
    }

    /// Ends a link that is not to be used.
    fn end(self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        drop(self.writer);
        drop(self.reader);
        if let Some(mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
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
    let quietly = {
        let _shown = crate::status::show(if count > 1 {
            format!("connecting to {}, {which}", ssh.host())
        } else {
            format!("connecting to {}", ssh.host())
        });
        hold(ssh, ssh.hold_quietly(), Stdio::null())
    };
    if let Ok(holder) = quietly {
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

/// Says what a host's shell printed before Ferry started, which was
/// skipped: once for each thing it printed, however many processes it
/// printed it for.
fn say_skipped(place: &str, before: &[u8]) {
    static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let shown = start::shown(before);
    if shown.is_empty() {
        return;
    }
    let mut said = SAID.lock().expect("not poisoned");
    if said.contains(&shown) {
        return;
    }
    said.push(shown.clone());
    crate::status::say(format_args!(
        "{place}: the shell printed this before Ferry started, which Ferry \
         skipped; its startup files should print nothing to standard output \
         when it is not interactive: {shown}"
    ));
}

/// A child's standard input and output, as a stream.
struct Stream {
    child: Child,
    reader: BufReader<ChildStdout>,
    writer: BufWriter<ChildStdin>,
}

impl Stream {
    /// Starts `command`, and writes it `first`.
    fn spawn(mut command: Command, first: &[u8]) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("start {:?}", command.get_program()))?;
        let reader = BufReader::new(child.stdout.take().expect("piped"));
        let mut writer = BufWriter::new(child.stdin.take().expect("piped"));
        if !first.is_empty() {
            writer.write_all(first).and_then(|()| writer.flush())?;
        }
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
    /// Where the program runs, as a message names it.
    place: String,
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
    /// be welcomed; returns where its paths reach it. A server that exits
    /// before it answers, such as one that could not be started, is reported
    /// with how it exited; what it printed is already on standard error.
    pub fn handshake(&mut self, hello: Hello) -> Result<Welcome> {
        let reader = self
            .reader
            .as_mut()
            .context("the connection is being read")?;
        let mut writer = self.writer.lock().expect("not poisoned");
        let writer = writer.as_mut().context("the connection is closed")?;
        let place = &self.place;
        let skipped = |before: &[u8]| say_skipped(place, before);
        match session::client_handshake(reader, writer, hello, skipped) {
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
    paths: Vec<Link>,
    /// Each named as a failure names it.
    heartbeats: Vec<(String, Link)>,
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
        let mut sockets = Vec::new();
        let counts = self
            .monitor
            .as_ref()
            .map(|monitor| Arc::clone(&monitor.bytes));
        for (
            index,
            Link {
                child,
                socket,
                reader,
                writer,
                ..
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
            children.extend(child);
            sockets.extend(socket);
        }
        let mut channels = Vec::new();
        for (name, link) in self.heartbeats {
            channels.push(heartbeat::Channel {
                name,
                reader: Box::new(link.reader),
                writer: Box::new(link.writer),
            });
            children.extend(link.child);
            sockets.extend(link.socket);
        }
        let receiving = striped::receiving(readers);
        let failer = receiving.failer();
        let ids: Vec<u32> = children.iter().map(Child::id).collect();
        let shut: Vec<UnixStream> = sockets
            .iter()
            .filter_map(|socket| socket.try_clone().ok())
            .collect();
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
                for socket in shut {
                    let _ = socket.shutdown(std::net::Shutdown::Both);
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
                sockets,
                heartbeats,
            },
        )
    }
}

/// The paths and heartbeat channels of a connection, to end once it is
/// closed.
pub struct Paths {
    children: Vec<Child>,
    /// The paths and heartbeat channels that are sockets on this machine.
    sockets: Vec<UnixStream>,
    heartbeats: heartbeat::Heartbeats,
}

impl Paths {
    /// Ends the heartbeats, and waits for the server to let go of every path
    /// and heartbeat channel: for the programs that carry them to end, and
    /// for the sockets to close at the far end, as the server ends.
    pub fn wait(self) -> Result<()> {
        self.heartbeats.end();
        for mut socket in self.sockets {
            // Everything has been sent; what comes now is only the end.
            let _ = socket.shutdown(std::net::Shutdown::Write);
            let _ = std::io::copy(&mut socket, &mut std::io::sink());
        }
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

    /// Sends packets over `connection`, whose far end sends back what it is
    /// sent, and checks that they all come back, in order.
    fn echoes(connection: Connection) {
        let (mut receiving, mut sending, paths) = connection.split();
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
        // The end marker comes back too, and ends the connection.
        sending.close().unwrap();
        assert_eq!(echoed.join().unwrap(), sent);
        paths.wait().unwrap();
    }

    #[test]
    fn messages_arrive_whole_and_in_order_until_the_end() {
        // A server's socket, whose every path, once it is answered with the
        // start mark, sends back what comes after its first bytes.
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut first = [0u8; 6];
                stream.read_exact(&mut first).unwrap();
                assert_eq!(first[0], STRAIGHT);
                start::write(&mut stream).unwrap();
                let mut reader = stream.try_clone().unwrap();
                std::thread::spawn(move || std::io::copy(&mut reader, &mut stream));
            }
        });
        let welcome = Welcome {
            token: String::new(),
            socket: socket.into_os_string().into_encoded_bytes(),
        };
        echoes(Transport::Local.connect(&welcome, "exit 1").unwrap());
    }

    #[test]
    fn a_path_joins_where_it_cannot_go_straight() {
        // No socket to reach; the joining process, after something a shell
        // might print and the start mark, sends back what it is sent, but for
        // its first bytes.
        let welcome = Welcome {
            token: String::new(),
            socket: b"/nonexistent/socket".to_vec(),
        };
        let join = "echo noise; printf '\\000ferry: start\\000'; \
                    dd bs=1 count=5 >/dev/null 2>&1; exec cat";
        echoes(Transport::Local.connect(&welcome, join).unwrap());
    }

    #[test]
    fn a_program_that_fails_says_so_when_closed() {
        let control = Transport::Local.control("exit 3", &[]).unwrap();
        let error = control.close().unwrap_err().to_string();
        assert!(error.contains('3'), "{error}");
    }
}
