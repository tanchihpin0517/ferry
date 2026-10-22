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
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Ends the holder should Ferry exit on a signal: nothing else would,
    /// its input being none of Ferry's, and it would keep the master up.
    _ending: interrupt::EndOnExit,
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
        let configured = ssh::configured(host);
        let mut held = Vec::new();
        for index in 0..count {
            let ssh = Ssh::new(host, index, configured)?;
            let (holder, ending) = log_in(&ssh, index, count)?;
            held.push(Held {
                ssh,
                holder,
                _ending: ending,
            });
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
                crate::limits::restore_in_child(&mut command);
                interrupt::unblock_in_child(&mut command)
                    .arg("-c")
                    .arg(script);
                command
            }
            // Control connections go over the first SSH connection.
            Self::Ssh(held) => held[0].ssh.script(script),
        }
    }

    /// Runs `script` at the far end, with nothing on its input, and returns
    /// how it exited, and what it printed: without what the host's shell
    /// printed around it, which is said, or `None` if it did not run to its
    /// end. Its standard error stays this process's.
    pub fn output(&self, script: &str) -> Result<(ExitStatus, Option<Vec<u8>>)> {
        let output = self
            .command(&start::marked(script))
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("run a script on {}", self.place()))?;
        let (before, inside) = start::unmarked(&output.stdout);
        say_skipped(self.place(), &before);
        Ok((output.status, inside.map(<[u8]>::to_vec)))
    }

    /// Opens a control connection to the program `script` runs, writing
    /// `first` to it before anything else, such as a password for `sudo` to
    /// read before it runs the program.
    pub fn control(&self, script: &str, first: &[u8]) -> Result<ControlConnection> {
        let mut link = Link::spawn(self.command(script), Stdio::inherit())?;
        if !first.is_empty() {
            link.writer
                .write_all(first)
                .and_then(|()| link.writer.flush())?;
        }
        let child = link.child.take().expect("a process");
        let writer: Box<dyn Write + Send> = Box::new(link.writer);
        Ok(ControlConnection {
            place: self.place().to_owned(),
            child,
            reader: Some(link.reader),
            writer: Arc::new(Mutex::new(Some(writer))),
            session: None,
            channel: None,
        })
    }

    /// Opens a connection to the server that sent `welcome`: one path over
    /// each SSH connection, or one on this machine, and beside each, a
    /// heartbeat channel, and over the first, a channel for the control
    /// connection to go on; see [`ControlConnection::attach`]. Each reaches
    /// the server's socket straight, as a channel SSH forwards to it, which
    /// nothing else on the host can write to; or, given `join`, each runs it
    /// instead, through the shell on the host, to join the server through
    /// its socket, for a host whose `sshd` will not forward to a socket.
    ///
    /// The heartbeats start as it opens, for the server watches for them
    /// from then on, however long until the transfer starts.
    pub fn connect(&self, welcome: &Welcome, join: Option<&str>) -> Result<Connection> {
        self.connect_with(welcome, join, heartbeat::times())
    }

    /// [`Transport::connect`], with heartbeats at `times`.
    fn connect_with(
        &self,
        welcome: &Welcome,
        join: Option<&str>,
        times: heartbeat::Times,
    ) -> Result<Connection> {
        let count = match self {
            Self::Local => 1,
            Self::Ssh(held) => held.len(),
        };
        let socket = String::from_utf8_lossy(&welcome.socket);
        let mut links = Vec::new();
        if let Err(error) = self.open_links(&socket, join, count, &mut links) {
            // What started is ended, rather than left running.
            for (_, _, link) in links {
                link.end();
            }
            return Err(error);
        }
        let mut paths = Vec::new();
        let mut channels = Vec::new();
        let mut carriers = Carriers::default();
        let mut control = None;
        for (index, kind, link) in links {
            match kind {
                Kind::Path => paths.push(link),
                Kind::Heartbeat => {
                    let name = match self {
                        Self::Local => "the connection".to_owned(),
                        Self::Ssh(_) => format!("SSH connection {} of {count}", index + 1),
                    };
                    let Link {
                        child,
                        socket,
                        reader,
                        writer,
                    } = link;
                    channels.push(heartbeat::Channel {
                        name,
                        reader: Box::new(reader),
                        writer: Box::new(writer),
                    });
                    carriers.children.extend(child);
                    carriers.sockets.extend(socket);
                }
                Kind::Control => control = Some(link),
            }
        }
        // A heartbeat channel that falls silent, or closes before its end,
        // fails the receiving half, once there is one, and kills every path
        // and heartbeat channel, so that nothing waits on them.
        let fault = FaultSlot::default();
        let ids: Vec<u32> = paths
            .iter()
            .filter_map(|link| link.child.as_ref())
            .chain(&carriers.children)
            .map(Child::id)
            .collect();
        let shut: Vec<UnixStream> = paths
            .iter()
            .filter_map(|link| link.socket.as_ref())
            .chain(&carriers.sockets)
            .filter_map(|socket| socket.try_clone().ok())
            .collect();
        let failing = Arc::clone(&fault);
        let heartbeats = heartbeat::start(
            channels,
            times,
            Box::new(move |message| {
                failing.lock().expect("not poisoned").fail(message);
                for id in ids {
                    // SAFETY: `kill` takes any pid and signal; the children
                    // are not yet waited for, so the pids are still theirs:
                    // they are waited for only once the heartbeats end.
                    unsafe {
                        libc::kill(id as libc::pid_t, libc::SIGKILL);
                    }
                }
                for socket in shut {
                    let _ = socket.shutdown(std::net::Shutdown::Both);
                }
            }),
        );
        let monitor = match self {
            Self::Local => None,
            Self::Ssh(held) => Some(Monitor {
                host: held[0].ssh.host().to_owned(),
                bytes: Arc::new((0..count).map(|_| AtomicU64::new(0)).collect()),
                ages: heartbeats.ages(),
            }),
        };
        Ok(Connection {
            paths,
            heartbeats: Some(heartbeats),
            carriers,
            fault,
            control,
            cut: false,
            monitor,
        })
    }

    /// Opens a connection's paths and channels into `links`, and waits for
    /// each to start; on a failure, `links` has those that started.
    fn open_links(
        &self,
        socket: &str,
        join: Option<&str>,
        count: usize,
        links: &mut Vec<(usize, Kind, Link)>,
    ) -> Result<()> {
        for index in 0..count {
            let kinds: &[Kind] = if index == 0 {
                &[Kind::Control, Kind::Path, Kind::Heartbeat]
            } else {
                &[Kind::Path, Kind::Heartbeat]
            };
            for &kind in kinds {
                let link = match join {
                    None => self.straight(index, socket, count, kind)?,
                    Some(join) => self.joining(index, join, count, kind)?,
                };
                links.push((index, kind, link));
            }
        }
        // Each starts with the start mark, so they are all waited for at
        // once: the server answers one that came straight with it, and a
        // joining process writes it as it starts. Only a joining process runs
        // through a shell, which may print before it.
        for (_, _, link) in links.iter_mut() {
            let skipped = match start::skip(&mut link.reader) {
                Ok(skipped) => skipped,
                // Closed by the far end before what was written to it was
                // read: it ended, as much as one that closed at once.
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                    start::Skipped {
                        before: Vec::new(),
                        found: false,
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("{}: wait for a path to start", self.place()));
                }
            };
            if join.is_some() {
                say_skipped(self.place(), &skipped.before);
            }
            if !skipped.found {
                match (self, join) {
                    (Self::Ssh(held), None) => bail!(
                        "{}: SSH did not forward a path to the server's socket, as where \
                         sshd has AllowStreamLocalForwarding no; --remote-join has the \
                         paths join it through the shell on the host instead",
                        held[0].ssh.host()
                    ),
                    _ => bail!("{}: a path ended before it started", self.place()),
                }
            }
        }
        Ok(())
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
            Self::Ssh(held) => Link::spawn(held[index].ssh.forward(socket), Stdio::inherit())?,
        };
        // A link that did not get through may close before these are
        // written; that is found, and said, once its start mark is waited for.
        let _ = link.writer.write_all(&[STRAIGHT]);
        let _ = striped::write_header(&mut link.writer, count, kind);
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
        // As for a path that goes straight: one whose process ended at once
        // is found once its start mark is waited for.
        let _ = striped::write_header(&mut link.writer, count, kind);
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
/// terminal. Without a terminal to ask in, it fails, saying which, and why,
/// as `ssh` said it: a login it needs only where `ssh` was refused one.
fn log_in(ssh: &Ssh, index: usize, count: usize) -> Result<(Child, interrupt::EndOnExit)> {
    let which = format!("SSH connection {} of {count}", index + 1);
    let quietly = {
        let _shown = crate::status::show(if count > 1 {
            format!("connecting to {}, {which}", ssh.host())
        } else {
            format!("connecting to {}", ssh.host())
        });
        hold(ssh, ssh.hold_quietly(), true)
    };
    let error = match quietly {
        Ok(holder) => return Ok(holder),
        Err(error) => error,
    };
    // Asking would run it no better.
    if error.is::<CannotRun>() {
        return Err(error.context(format!("{}: {which}", ssh.host())));
    }
    if std::fs::File::open("/dev/tty").is_err() {
        if refused_a_login(&error) {
            bail!(
                "{}: {which} needs a login, and there is no terminal to type it in: {error:#}",
                ssh.host()
            );
        }
        bail!("{}: {which} failed: {error:#}", ssh.host());
    }
    if count > 1 {
        eprintln!("ferry: logging in {which} to {}", ssh.host());
    }
    hold(ssh, ssh.hold(), false).with_context(|| {
        format!(
            "{}: {which}, which also failed without asking: {error:#}",
            ssh.host()
        )
    })
}

/// The program run instead of `ssh` could not be run at all.
#[derive(Debug)]
struct CannotRun(String);

impl std::fmt::Display for CannotRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CannotRun {}

/// Whether `ssh`, as `hold` says it failed, was refused a login: a key it
/// did not have, or a password or code it could not ask for. Its exit status
/// says nothing of it, 255 for a host it could not reach as well.
fn refused_a_login(error: &anyhow::Error) -> bool {
    let said = format!("{error:#}");
    [
        "Permission denied",
        "Authentication failed",
        "keyboard-interactive",
        "password",
    ]
    .iter()
    .any(|sign| said.contains(sign))
}

/// How long a quiet login may hold a connection with no master to share
/// answering: `ssh` logged in, but something, such as a `FERRY_SSH` that
/// ignores `ControlPath`, keeps it from being shared.
const MASTER_WAIT: Duration = Duration::from_secs(30);

/// Starts `holder`, an `ssh -N`, and waits until its ControlMaster is up;
/// `ssh -N` runs nothing, and holds the connection open. A `quiet` one says
/// nothing in the terminal: what `ssh` says goes to a file, read back for
/// why it failed. It gives up on a master that never comes, after
/// [`MASTER_WAIT`]; one that may ask in the terminal waits as long as the
/// one typing takes.
fn hold(ssh: &Ssh, mut holder: Command, quiet: bool) -> Result<(Child, interrupt::EndOnExit)> {
    let up = |ssh: &Ssh| {
        ssh.check()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    // A file, not a pipe: a master that `ssh` leaves in the background keeps
    // it, and a pipe would not end while it lasts.
    let said = if quiet { Some(said_file()?) } else { None };
    let stderr = match &said {
        Some(file) => Stdio::from(file.try_clone()?),
        None => Stdio::inherit(),
    };
    let mut holder = holder
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|error| CannotRun(format!("cannot run {}: {error}", ssh::program_name())))?;
    let ending = interrupt::end_on_exit(&holder);
    let started = std::time::Instant::now();
    let wait = crate::inject::master_wait().unwrap_or(MASTER_WAIT);
    loop {
        if up(ssh) {
            return Ok((holder, ending));
        }
        if let Some(status) = holder.try_wait()? {
            if up(ssh) {
                return Ok((holder, ending));
            }
            let said = said.map(last_said).unwrap_or_default();
            if said.is_empty() {
                bail!("could not log in ({status})");
            }
            bail!("could not log in: {said} ({status})");
        }
        if quiet && started.elapsed() > wait {
            let _ = holder.kill();
            let _ = holder.wait();
            bail!(
                "ssh logged in, but no ControlMaster answered for {} s to share the connection",
                wait.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A file for what a quiet `ssh` says, open, and already removed, private to
/// this user.
fn said_file() -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = crate::home::runtime_dir()?;
    loop {
        let path = directory.join(format!("ssh-said-{:016x}", rand_u64()));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                let _ = std::fs::remove_file(&path);
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", path.display()));
            }
        }
    }
}

/// A number unlikely to repeat, for a file's name.
fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

/// The last few lines `ssh` said, in `file`, joined: why it failed.
fn last_said(mut file: std::fs::File) -> String {
    use std::io::Seek;
    let mut said = Vec::new();
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = file.read_to_end(&mut said);
    let said = String::from_utf8_lossy(&said);
    let lines: Vec<&str> = said
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(3)..].join("; ")
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

/// What a program reads on its control connection, until the connection
/// closes; shared with its senders.
type ControlWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// What the control connection is read from.
pub type ControlReader = BufReader<Box<dyn Read + Send>>;

/// A control connection: a byte stream to one program at the far end.
pub struct ControlConnection {
    /// Where the program runs, as a message names it.
    place: String,
    child: Child,
    /// What the program writes, until a thread takes it to read.
    reader: Option<ControlReader>,
    writer: ControlWriter,
    /// The program's input, kept open once the connection goes on a channel
    /// of its own, for the server to know the client is there.
    session: Option<Box<dyn Write + Send>>,
    /// That channel, once the connection goes on it.
    channel: Option<Link>,
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

    /// Moves the connection, once its server has started, onto the channel
    /// `connection` opened to the server's socket, which a shell on the host,
    /// or what it left running, cannot write to as it can to the program's
    /// output. What the program writes from now on is read and dropped; its
    /// input stays open, for the server to know the client is there.
    pub fn attach(&mut self, connection: &mut Connection) {
        let Some(mut channel) = connection.control.take() else {
            return;
        };
        let reader = std::mem::replace(
            &mut channel.reader,
            BufReader::new(Box::new(std::io::empty())),
        );
        let writer = std::mem::replace(
            &mut channel.writer,
            BufWriter::new(Box::new(std::io::sink())),
        );
        if let Some(mut output) = self.reader.replace(reader) {
            std::thread::spawn(move || std::io::copy(&mut output, &mut std::io::sink()));
        }
        self.session = self
            .writer
            .lock()
            .expect("not poisoned")
            .replace(Box::new(writer));
        self.channel = Some(channel);
    }

    /// What the program writes from now on, for a thread of its own to read.
    pub fn take_reader(&mut self) -> Option<ControlReader> {
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
        drop(self.session);
        let status = child.wait().context("wait for the server")?;
        if let Some(channel) = self.channel {
            channel.end();
        }
        exited(status)
    }

    /// Closes the connection after a failure: waits for the program to end
    /// for `time` at most, then kills it, as when its SSH connection is gone
    /// and would never say that it ended.
    pub fn close_within(self, time: Duration) -> Result<()> {
        let mut child = self.child;
        self.writer.lock().expect("not poisoned").take();
        drop(self.session);
        if let Some(channel) = self.channel {
            channel.end();
        }
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().context("wait for the server")? {
                return exited(status);
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

/// How a server ended: well, or not, as with an internal error, which it
/// said itself.
fn exited(status: std::process::ExitStatus) -> Result<()> {
    match status.code() {
        Some(0) => Ok(()),
        Some(code) if code == i32::from(crate::stop::INTERNAL) => Err(crate::stop::Internal.into()),
        _ => bail!("the server exited with {status}"),
    }
}

/// A connection: whole messages, in order, to a server, over its paths, and
/// the heartbeat channel beside each.
pub struct Connection {
    paths: Vec<Link>,
    /// Sent and watched from when the connection opens; taken by `split`.
    heartbeats: Option<heartbeat::Heartbeats>,
    carriers: Carriers,
    /// What a heartbeat failure reaches.
    fault: FaultSlot,
    /// The channel for the control connection, until it takes it.
    control: Option<Link>,
    /// Cut the last path short before its first piece: for `ferry test`.
    cut: bool,
    /// For a host, what the dashboard shows of each SSH connection.
    monitor: Option<Monitor>,
}

/// What carries a connection's heartbeat channels: the processes, and the
/// sockets on this machine.
#[derive(Default)]
struct Carriers {
    children: Vec<Child>,
    sockets: Vec<UnixStream>,
}

/// What a heartbeat failure reaches: the receiving half, once the connection
/// is split, and until then, the failure itself, for it to fail at once.
#[derive(Default)]
struct Fault {
    message: Option<String>,
    failer: Option<Failer>,
}

impl Fault {
    fn fail(&mut self, message: String) {
        match &self.failer {
            Some(failer) => failer.fail(message),
            None => {
                self.message.get_or_insert(message);
            }
        }
    }
}

type FaultSlot = Arc<Mutex<Fault>>;

/// What the dashboard shows of a host's SSH connections: the bytes each has
/// carried, both ways, and how long since its last heartbeat. Clones see the
/// same connections.
#[derive(Clone)]
pub struct Monitor {
    host: String,
    bytes: Arc<Vec<AtomicU64>>,
    ages: heartbeat::Ages,
}

impl Monitor {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// For each SSH connection: the bytes it has carried, and how long since
    /// its last heartbeat.
    pub fn connections(&self) -> Vec<(u64, Option<Duration>)> {
        let ages = self.ages.now();
        self.bytes
            .iter()
            .enumerate()
            .map(|(index, bytes)| (bytes.load(Ordering::Relaxed), ages.get(index).copied()))
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
    /// sends. Its queues are gauged under names that start with `label`, such
    /// as which side it reaches. A heartbeat failure, before or after, fails
    /// the receiving half.
    pub fn split(mut self, label: &str) -> (Receiving, Sending, Paths) {
        let mut readers = Vec::new();
        let mut writers = Vec::new();
        let mut children = Vec::new();
        let mut sockets = Vec::new();
        let counts = self
            .monitor
            .as_ref()
            .map(|monitor| Arc::clone(&monitor.bytes));
        // What comes on a path is from its SSH connection, whose heartbeats
        // may wait behind it on a slow link: the paths and the heartbeat
        // channels are in the connections' order.
        let hearing = self.heartbeats.as_ref().expect("split once").hearing();
        for (
            index,
            Link {
                child,
                socket,
                reader,
                writer,
                ..
            },
        ) in std::mem::take(&mut self.paths).into_iter().enumerate()
        {
            let bytes = counts.clone().map(|counts| (counts, index));
            readers.push(Counted {
                inner: heartbeat::Heard::new(reader, hearing.clone(), Some(index)),
                bytes: bytes.clone(),
            });
            writers.push(Counted {
                inner: writer,
                bytes,
            });
            children.extend(child);
            sockets.extend(socket);
        }
        let carriers = std::mem::take(&mut self.carriers);
        children.extend(carriers.children);
        sockets.extend(carriers.sockets);
        // A heartbeat failure kills the paths after it reaches the fault, so
        // under its lock, one that came already is the receiving half's
        // failure before a path can say it ended.
        let receiving = {
            let mut fault = self.fault.lock().expect("not poisoned");
            let receiving = striped::receiving_failed_as(readers, label, fault.message.take());
            fault.failer = Some(receiving.failer());
            receiving
        };
        let heartbeats = self.heartbeats.take().expect("split once");
        (
            receiving,
            striped::sending_as(writers, label, self.cut),
            Paths {
                children,
                sockets,
                heartbeats,
            },
        )
    }
}

impl Drop for Connection {
    /// A connection not split, as when the other side could not start, ends
    /// its heartbeats, and every path and channel.
    fn drop(&mut self) {
        let Some(heartbeats) = self.heartbeats.take() else {
            return;
        };
        heartbeats.end();
        for link in self.paths.drain(..) {
            link.end();
        }
        if let Some(control) = self.control.take() {
            control.end();
        }
        let carriers = std::mem::take(&mut self.carriers);
        for socket in carriers.sockets {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        for mut child in carriers.children {
            let _ = child.kill();
            let _ = child.wait();
        }
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

/// How long a server is given to let go of its paths once the transfer is
/// over: with the heartbeats ended, nothing else tells of a connection gone
/// without a word, which TCP may take hours to.
const LET_GO: Duration = Duration::from_secs(10);

impl Paths {
    /// Ends the heartbeats, and waits for the server to let go of every path
    /// and heartbeat channel: for the programs that carry them to end, and
    /// for the sockets to close at the far end, as the server ends; for
    /// [`LET_GO`] at most, then lets go of them itself.
    pub fn wait(self) -> Result<()> {
        self.wait_within(LET_GO)
    }

    fn wait_within(self, time: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + time;
        let left = || {
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .max(Duration::from_millis(1))
        };
        self.heartbeats.end();
        let mut result = Ok(());
        let (sockets, children) = (self.sockets.len(), self.children.len());
        for (index, mut socket) in self.sockets.into_iter().enumerate() {
            // Everything has been sent; what comes now is only the end.
            let _ = socket.shutdown(std::net::Shutdown::Write);
            let _ = socket.set_read_timeout(Some(left()));
            if let Err(error) = std::io::copy(&mut socket, &mut std::io::sink())
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
                && result.is_ok()
            {
                result = Err(anyhow::anyhow!(
                    "a path did not end: socket {} of {sockets}",
                    index + 1
                ));
            }
        }
        for (index, mut child) in self.children.into_iter().enumerate() {
            let status = loop {
                if let Some(status) = child.try_wait().context("wait for a path")? {
                    break Some(status);
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            match status {
                Some(status) if !status.success() && result.is_ok() => {
                    result = Err(anyhow::anyhow!("a path ended with {status}"));
                }
                None if result.is_ok() => {
                    result = Err(anyhow::anyhow!(
                        "a path did not end: program {} of {children}",
                        index + 1
                    ));
                }
                _ => {}
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
    /// sent, and checks that they all come back, in order. Says how far it
    /// got, for a test that hangs to say where (TRN-11).
    fn echoes(connection: Connection) {
        let (mut receiving, mut sending, paths) = connection.split("");
        let sent: Vec<Vec<u8>> = (0..200u32)
            .map(|index| vec![index as u8; (index as usize * 997) % 70_000])
            .collect();
        let echoed = std::thread::spawn(move || {
            let mut echoed = Vec::new();
            loop {
                match receiving.recv() {
                    Ok(Some(packet)) => echoed.push(packet.into_payload()),
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("echoes: receiving failed after {}: {error:#}", echoed.len());
                        // What else comes is not read, so that nothing waits
                        // to write it.
                        receiving.drain();
                        break;
                    }
                }
            }
            eprintln!("echoes: received {}", echoed.len());
            echoed
        });
        for (index, bytes) in sent.iter().enumerate() {
            if let Err(error) = sending.send(packet(bytes)) {
                panic!("echoes: sending {index} failed: {error:#}");
            }
        }
        eprintln!("echoes: sent {}", sent.len());
        // The end marker comes back too, and ends the connection.
        sending.close().unwrap();
        eprintln!("echoes: closed");
        assert_eq!(echoed.join().unwrap(), sent);
        // A path the far end did not let go of: where every thread is, and
        // on macOS what is still open, before failing (TRN-11).
        if let Err(error) = paths.wait() {
            crate::test_support::say_where_threads_are(&format!("echoes: {error:#}"));
            panic!("echoes: {error:#}");
        }
        eprintln!("echoes: the paths ended");
    }

    #[test]
    fn messages_arrive_whole_and_in_order_until_the_end() {
        crate::test_support::within(
            "messages arrive whole and in order until the end",
            crate::test_support::PATIENCE,
            move || {
                // A server's socket, whose every path, once it is answered with the
                // start mark, sends back what comes after its first bytes.
                let temp = tempfile::tempdir().unwrap();
                let socket = temp.path().join("socket");
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                // Its failures are said, not panicked at in a thread no one
                // joins, for a test that hangs to say why (TRN-11).
                std::thread::spawn(move || {
                    for (index, stream) in listener.incoming().enumerate() {
                        let answered = (|| -> std::io::Result<()> {
                            let mut stream = stream?;
                            let mut first = [0u8; 6];
                            stream.read_exact(&mut first)?;
                            assert_eq!(first[0], STRAIGHT);
                            start::write(&mut stream)?;
                            let mut reader = stream.try_clone()?;
                            std::thread::spawn(move || {
                                if let Err(error) = std::io::copy(&mut reader, &mut stream) {
                                    eprintln!("echo server: path {index} failed: {error}");
                                }
                            });
                            Ok(())
                        })();
                        if let Err(error) = answered {
                            eprintln!("echo server: path {index} not answered: {error}");
                        }
                    }
                });
                let welcome = Welcome {
                    socket: socket.into_os_string().into_encoded_bytes(),
                    root: crate::protocol::RootIdentity::default(),
                };
                echoes(Transport::Local.connect(&welcome, None).unwrap());
            },
        );
    }

    #[test]
    fn a_path_closed_at_once_is_said_to_have_ended_before_it_started() {
        crate::test_support::within(
            "a path closed at once is said to have ended before it started",
            crate::test_support::PATIENCE,
            move || {
                // A socket that closes every path as it comes, before its first bytes
                // can be written.
                let temp = tempfile::tempdir().unwrap();
                let socket = temp.path().join("socket");
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        drop(stream);
                    }
                });
                let welcome = Welcome {
                    socket: socket.into_os_string().into_encoded_bytes(),
                    root: crate::protocol::RootIdentity::default(),
                };
                let error = Transport::Local
                    .connect(&welcome, None)
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("ended before it started"), "{error}");
            },
        );
    }

    /// Heartbeats as often as a test can see them come, and a silence long
    /// enough that no stall of a busy machine reaches it.
    const QUICK: heartbeat::Times = heartbeat::Times {
        interval: Duration::from_millis(20),
        silence: Duration::from_secs(60),
    };

    /// A server's socket that answers every path and channel with the start
    /// mark, then hands it to the test, with its kind; and the welcome that
    /// names it.
    fn serving(
        streams: crossbeam_channel::Sender<(Kind, UnixStream)>,
    ) -> (tempfile::TempDir, Welcome) {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut straight = [0u8; 1];
                stream.read_exact(&mut straight).unwrap();
                assert_eq!(straight[0], STRAIGHT);
                let (_, kind) = striped::read_header(&mut stream).unwrap();
                start::write(&mut stream).unwrap();
                if streams.send((kind, stream)).is_err() {
                    return;
                }
            }
        });
        let welcome = Welcome {
            socket: socket.into_os_string().into_encoded_bytes(),
            root: crate::protocol::RootIdentity::default(),
        };
        (temp, welcome)
    }

    /// Every path and channel of a connection on this machine, as the server
    /// has them, and its heartbeat channel apart.
    fn joined(
        streams: &crossbeam_channel::Receiver<(Kind, UnixStream)>,
    ) -> (UnixStream, Vec<UnixStream>) {
        let mut heartbeat = None;
        let mut others = Vec::new();
        // A control channel, a path, and a heartbeat channel.
        for _ in 0..3 {
            match streams.recv().unwrap() {
                (Kind::Heartbeat, stream) => heartbeat = Some(stream),
                (_, stream) => others.push(stream),
            }
        }
        (heartbeat.expect("a heartbeat channel"), others)
    }

    /// A server that never lets go of its paths, as one whose connection
    /// went without a word, holds the client up for so long only.
    #[test]
    fn a_server_that_never_lets_go_of_its_paths_is_waited_for_so_long_only() {
        crate::test_support::within(
            "a server that never lets go of its paths is waited for so long only",
            crate::test_support::PATIENCE,
            move || {
                let (sender, streams) = crossbeam_channel::unbounded();
                let (_temp, welcome) = serving(sender);
                let connection = Transport::Local
                    .connect_with(&welcome, None, QUICK)
                    .unwrap();
                // Held, and never closed.
                let (_heartbeat, _others) = joined(&streams);
                let (_receiving, sending, paths) = connection.split("");
                drop(sending);
                let started = std::time::Instant::now();
                let error = paths.wait_within(Duration::from_millis(500)).unwrap_err();
                let said = error.to_string();
                assert!(said.starts_with("a path did not end: socket "), "{said}");
                assert!(started.elapsed() < Duration::from_secs(5));
            },
        );
    }

    #[test]
    fn heartbeats_go_from_when_a_connection_opens_until_it_is_dropped() {
        crate::test_support::within(
            "heartbeats go from when a connection opens until it is dropped",
            crate::test_support::PATIENCE,
            move || {
                let (sender, streams) = crossbeam_channel::unbounded();
                let (_temp, welcome) = serving(sender);
                let connection = Transport::Local
                    .connect_with(&welcome, None, QUICK)
                    .unwrap();
                let (mut channel, _others) = joined(&streams);
                // Heartbeats come, though the connection is not split, as
                // until the other side is ready.
                let mut beats = [1u8; 3];
                channel.read_exact(&mut beats).unwrap();
                assert_eq!(beats, [0; 3], "a heartbeat is a 0");
                // Dropped before it was split, it ends its heartbeats: the
                // last byte is the end, a 1, and then the channel closes.
                drop(connection);
                let mut rest = Vec::new();
                channel.read_to_end(&mut rest).unwrap();
                let (end, beats) = rest.split_last().expect("the end");
                assert_eq!(*end, 1);
                assert!(beats.iter().all(|&byte| byte == 0), "{rest:?}");
            },
        );
    }

    #[test]
    fn a_heartbeat_failure_before_the_split_fails_the_receiving_half() {
        crate::test_support::within(
            "a heartbeat failure before the split fails the receiving half",
            crate::test_support::PATIENCE,
            move || {
                let (sender, streams) = crossbeam_channel::unbounded();
                let (_temp, welcome) = serving(sender);
                let connection = Transport::Local
                    .connect_with(&welcome, None, QUICK)
                    .unwrap();
                // The server closes the heartbeat channel, as when its SSH
                // connection drops while the other side is still starting.
                let (channel, _others) = joined(&streams);
                drop(channel);
                while connection
                    .fault
                    .lock()
                    .expect("not poisoned")
                    .message
                    .is_none()
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // It is what the receiving half then says, at once, not that
                // a path, killed by it, ended.
                let (mut receiving, _sending, paths) = connection.split("");
                let error = receiving.recv().err().unwrap().to_string();
                assert_eq!(error, "the connection: dropped");
                let _ = paths.wait();
            },
        );
    }

    #[test]
    fn paths_can_join_through_a_shell() {
        crate::test_support::within(
            "paths can join through a shell",
            crate::test_support::PATIENCE,
            move || {
                // The joining process, after something a shell might print and the
                // start mark, sends back what it is sent, but for its first bytes.
                let welcome = Welcome {
                    socket: b"/nonexistent/socket".to_vec(),
                    root: crate::protocol::RootIdentity::default(),
                };
                let join = "echo noise; printf '\\000ferry: start\\000'; \
                        dd bs=1 count=5 >/dev/null 2>&1; exec cat";
                echoes(Transport::Local.connect(&welcome, Some(join)).unwrap());
            },
        );
    }

    /// A connection that cannot open all its paths ends those that opened.
    #[test]
    fn a_connection_that_fails_to_open_ends_what_it_started() {
        crate::test_support::within(
            "a connection that fails to open ends what it started",
            crate::test_support::PATIENCE,
            move || {
                // One joining process starts, and stays; the others end, as
                // where the host refuses them, once it has said which it is.
                let temp = tempfile::tempdir().unwrap();
                let first = temp.path().join("first");
                let pid = temp.path().join("pid");
                let join = format!(
                    "if mkdir {first} 2>/dev/null; then echo $$ > {pid}; \
                     printf '\\000ferry: start\\000'; exec sleep 600; \
                     else while [ ! -s {pid} ]; do sleep 0.01; done; exit 1; fi",
                    first = first.display(),
                    pid = pid.display(),
                );
                let welcome = Welcome {
                    socket: b"/nonexistent/socket".to_vec(),
                    root: crate::protocol::RootIdentity::default(),
                };
                let error = Transport::Local
                    .connect(&welcome, Some(&join))
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("ended before it started"), "{error}");
                let pid: libc::pid_t = std::fs::read_to_string(&pid)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                // SAFETY: signal 0 only asks whether the process is there.
                let there = unsafe { libc::kill(pid, 0) } == 0;
                assert!(!there, "the joining process {pid} was left running");
            },
        );
    }

    #[test]
    fn a_program_that_fails_says_so_when_closed() {
        crate::test_support::within(
            "a program that fails says so when closed",
            crate::test_support::PATIENCE,
            move || {
                let control = Transport::Local.control("exit 3", &[]).unwrap();
                let error = control.close().unwrap_err().to_string();
                assert!(error.contains('3'), "{error}");
            },
        );
    }
}
