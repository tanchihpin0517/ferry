//! The transport: how the client reaches the place of each side, this machine
//! or a host. Nothing else in Ferry runs `ssh`, or knows whether a place is
//! this machine or another; see the
//! [architecture](../../docs/design/architecture.md#transport).
//!
//! Opening a transport is the first thing that reaches a place: for a host, it
//! opens the SSH connection, only logged in, running nothing. A host has one
//! SSH connection, and only one: a host name may lead to several machines,
//! as a cluster's login nodes, and a second connection could reach another
//! one than the server's. Through it, the client then opens:
//!
//! - the host's **first session**, its only one, as each session runs the
//!   user's login shell and its startup files: it probes the host, installs
//!   Ferry there if it must, and becomes Ferry's [`launcher`];
//! - **control connections**: each a plain byte stream to one program, a
//!   server, which lives as long as its control connection; on a host, the
//!   launcher starts it on a channel to its socket, which no shell can write
//!   to;
//! - the **stream** between a transfer's two servers, over the SSH
//!   connection, which the client makes and hands to the servers, never
//!   touching what goes on it; see [`streams`]. On it, each server's
//!   [`stream`] carries what the peers send each other.

pub mod join;
pub mod launcher;
pub mod ssh;
pub mod start;
pub mod stream;
pub mod streams;

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use join::{Joined, Joining, STRAIGHT, join, listen};

use crate::handshake;
use crate::interrupt;
use crate::protocol::{self, Hello, Message, Welcome};
use crate::remote::Location;
use ssh::Ssh;

/// How a place is reached.
pub enum Transport {
    /// This machine: programs run as child processes.
    Local,
    /// A host, over SSH: programs run in sessions of its SSH connection,
    /// already logged in, and held open for the whole run.
    Ssh(Held),
}

/// The SSH connection, logged in, and the `ssh -N` that holds it open, if
/// one does.
pub struct Held {
    ssh: Ssh,
    holder: Option<Holder>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(holder) = self.holder.take() {
            holder.end();
        }
    }
}

/// An `ssh -N` that logged in, and still ran once the master was up: the
/// master itself, where no `ControlPersist` moved that to the background.
struct Holder {
    child: Child,
    /// Ends it should Ferry exit on a signal: nothing else would, its input
    /// being none of Ferry's, and it would keep the master up.
    _ending: interrupt::EndOnExit,
}

impl Holder {
    /// Ends it, and waits for it.
    fn end(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Transport {
    /// Opens the transport to a location's place. For a host, this opens
    /// its SSH connection, and runs nothing there: it is only logged in,
    /// and held open. A password or an OTP is asked for here, if it needs
    /// one; if it cannot be opened, neither is the transport.
    pub fn open(location: &Location) -> Result<Self> {
        let Location::Remote { host, .. } = location else {
            return Ok(Self::Local);
        };
        let ssh = Ssh::new(host, ssh::configured(host))?;
        let holder = log_in(&ssh)?;
        Ok(Self::Ssh(Held { ssh, holder }))
    }

    /// The place, as messages name it.
    pub fn place(&self) -> &str {
        match self {
            Self::Local => "this machine",
            Self::Ssh(held) => held.ssh.host(),
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
            Self::Ssh(held) => held.ssh.script(script),
        }
    }

    /// What a script that [`start::marked`] ran here printed itself, given
    /// all it printed: without what the host's shell printed around it,
    /// which is said, or `None` if it did not run to its end.
    pub fn unmarked<'a>(&self, output: &'a [u8]) -> Option<&'a [u8]> {
        let (before, inside) = start::unmarked(output);
        say_skipped(self.place(), &before);
        inside
    }

    /// Opens a control connection to the program `script` runs here, on
    /// this machine, as a server.
    pub fn control(&self, script: &str) -> Result<ControlConnection> {
        let mut link = Link::spawn(self.command(script), Stdio::inherit())?;
        let child = link.child.take().expect("a process");
        Ok(ControlConnection::new(
            self.place(),
            Program::Child(child),
            link,
        ))
    }

    /// Opens a host's first session, which runs `script`, whose lines are
    /// marked with `tag`; see [`FirstSession`].
    pub fn first_session(&self, script: &str, tag: &str) -> Result<FirstSession> {
        let link = Link::spawn(self.command(script), Stdio::inherit())
            .with_context(|| format!("reach {}", self.place()))?;
        Ok(FirstSession {
            place: self.place().to_owned(),
            tag: tag.to_owned(),
            link,
        })
    }

    /// Reaches the socket at `socket`, at the far end: straight, as a
    /// channel SSH forwards to it over the SSH connection, which nothing
    /// else on the host can write to, or on this machine, a connection to
    /// it; or, given `join`, by running that instead, through the shell on
    /// the host, for a host whose `sshd` will not forward to a socket. Done
    /// once the far end has written the start mark: what listens on the
    /// socket answers one that came straight with it, and a joining process
    /// writes it as it starts, after what the shell printed.
    fn connect(&self, socket: &str, join: Option<&str>) -> Result<Link> {
        let mut link = match join {
            None => self.straight(socket)?,
            Some(join) => self.joining(join)?,
        };
        let skipped = match start::skip(&mut link.reader) {
            Ok(skipped) => skipped,
            // Closed by the far end before what was written to it was read:
            // it ended, as much as one that closed at once.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => start::Skipped {
                before: Vec::new(),
                found: false,
            },
            Err(error) => {
                link.end();
                return Err(error)
                    .with_context(|| format!("{}: wait for a channel to start", self.place()));
            }
        };
        if join.is_some() {
            say_skipped(self.place(), &skipped.before);
        }
        if !skipped.found {
            link.end();
            match (self, join) {
                (Self::Ssh(held), None) => bail!(
                    "{}: SSH did not forward a channel to Ferry's socket there, as where \
                     sshd has AllowStreamLocalForwarding no; --remote-join has what \
                     reaches it join it through the shell on the host instead",
                    held.ssh.host()
                ),
                _ => bail!("{}: a channel ended before it started", self.place()),
            }
        }
        Ok(link)
    }

    /// What the dashboard shows of a host's SSH connection; none on this
    /// machine.
    pub fn monitor(&self) -> Option<Monitor> {
        match self {
            Self::Local => None,
            Self::Ssh(held) => Some(Monitor {
                host: held.ssh.host().to_owned(),
                seen: Arc::new(Mutex::new((0, None))),
            }),
        }
    }

    /// How the client reaches the server that sent `welcome` for a stream:
    /// on this machine, through its socket; on a host, by a channel over the
    /// SSH connection, which reaches its socket straight, or, given `join`,
    /// runs that; see [`streams`].
    pub fn half(&self, welcome: &Welcome, join: Option<&str>) -> Half {
        let socket = String::from_utf8_lossy(&welcome.socket).into_owned();
        match self {
            Self::Local => Half::Here(PathBuf::from(socket)),
            Self::Ssh(held) => {
                let ssh = &held.ssh;
                match join {
                    None => Half::There {
                        command: ssh.forward(&socket),
                        straight: true,
                    },
                    Some(join) => Half::There {
                        command: ssh.script(join),
                        straight: false,
                    },
                }
            }
        }
    }

    /// A link over the SSH connection, or on this machine, that reaches the
    /// server's socket straight, its first byte written.
    fn straight(&self, socket: &str) -> Result<Link> {
        let mut link = match self {
            Self::Local => {
                let stream =
                    UnixStream::connect(socket).with_context(|| format!("connect to {socket}"))?;
                Link::socket(stream)?
            }
            Self::Ssh(held) => Link::spawn(held.ssh.forward(socket), Stdio::inherit())?,
        };
        // A link that did not get through may close before this is written;
        // that is found, and said, as its start mark is waited for.
        let _ = link
            .writer
            .write_all(&[STRAIGHT])
            .and_then(|()| link.writer.flush());
        Ok(link)
    }

    /// A link over the SSH connection, or on this machine, that runs `join`.
    fn joining(&self, join: &str) -> Result<Link> {
        let command = match self {
            Self::Local => self.command(join),
            Self::Ssh(held) => held.ssh.script(join),
        };
        Link::spawn(command, Stdio::inherit())
    }
}

/// How the client reaches a server for one stream; see [`streams`].
pub enum Half {
    /// On this machine: the client hands it its end through its socket.
    Here(PathBuf),
    /// On a host: its end is the input and output of `command`, a channel
    /// SSH forwards to its socket, `straight`, or one that runs a joining
    /// process.
    There { command: Command, straight: bool },
}

/// A byte stream to a program at the far end, as the client has it: the
/// input and output of the process that carries it, or a socket.
struct Link {
    /// The process that carries it, if any: `ssh`, or a joining process.
    child: Option<Child>,
    /// The socket it is, on this machine, to shut down to stop it.
    socket: Option<UnixStream>,
    reader: BufReader<Box<dyn Read + Send>>,
    writer: BufWriter<Box<dyn Write + Send>>,
    /// Set once the link is let go: whoever reads it stops.
    let_go: Arc<AtomicBool>,
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
        let let_go = Arc::new(AtomicBool::new(false));
        let reader: Box<dyn Read + Send> = Box::new(LetGo {
            inner: child.stdout.take().expect("piped"),
            let_go: Arc::clone(&let_go),
        });
        let writer: Box<dyn Write + Send> = Box::new(child.stdin.take().expect("piped"));
        Ok(Self {
            child: Some(child),
            socket: None,
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            let_go,
        })
    }

    /// A link that is `stream`.
    fn socket(stream: UnixStream) -> Result<Self> {
        let let_go = Arc::new(AtomicBool::new(false));
        let reader: Box<dyn Read + Send> = Box::new(LetGo {
            inner: stream.try_clone()?,
            let_go: Arc::clone(&let_go),
        });
        let shut = stream.try_clone()?;
        let writer: Box<dyn Write + Send> = Box::new(join::Writing(stream));
        Ok(Self {
            child: None,
            socket: Some(shut),
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            let_go,
        })
    }

    /// Ends a link that is not to be used.
    fn end(self) {
        self.let_go.store(true, Ordering::SeqCst);
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

/// What a link is read from, which stops being read once the link is let
/// go: a read waits a moment at a time for what comes. So a server whose
/// SSH connection is gone is let go of without waiting for its pipe to end:
/// a ControlMaster that stopped answering still holds the pipe's other end,
/// which `ssh` handed it.
struct LetGo<R> {
    inner: R,
    let_go: Arc<AtomicBool>,
}

/// How long a read of a link waits at a time before it looks whether the
/// link was let go.
const LOOK_EVERY: Duration = Duration::from_millis(250);

impl<R: Read + AsFd> Read for LetGo<R> {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: LOOK_EVERY.as_nanos() as _,
        };
        loop {
            if self.let_go.load(Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "the connection was let go",
                ));
            }
            let mut polled = [rustix::event::PollFd::new(
                &self.inner,
                rustix::event::PollFlags::IN,
            )];
            match rustix::event::poll(&mut polled, Some(&timeout)) {
                Ok(0) | Err(rustix::io::Errno::INTR) => {}
                Ok(_) => return self.inner.read(into),
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// Opens the SSH connection: quietly first, which needs nothing typed with a
/// key or a master still up, then asking in the terminal. Without a terminal
/// to ask in, it fails, saying why, as `ssh` said it: a login it needs only
/// where `ssh` was refused one. Returns the `ssh -N` that holds it open, if
/// one does.
fn log_in(ssh: &Ssh) -> Result<Option<Holder>> {
    let which = "the SSH connection";
    let up = || master_up(ssh);
    let quietly = {
        let _shown = crate::status::show(format!("connecting to {}", ssh.host()));
        hold(up, ssh.hold_quietly(), true)
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
    hold(up, ssh.hold(), false).with_context(|| {
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

/// Whether the ControlMaster that `ssh` would share answers.
fn master_up(ssh: &Ssh) -> bool {
    ssh.check()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Starts `holder`, an `ssh -N`, and waits until the ControlMaster is up, as
/// `up` says. `ssh -N` runs nothing, and logs in: with `ControlPersist`, it
/// then leaves the master running in the background and ends; without, it
/// is the master, and holds the connection open while it runs, so it is
/// returned. A master already up, the user's own or one Ferry left running,
/// needs none: given one, `ssh -N` asks it for a session, which starts a
/// login shell on the host. A `quiet` one says nothing in the terminal:
/// what `ssh` says goes to a file, read back for why it failed. It gives up
/// on a master that never comes, after [`MASTER_WAIT`]; one that may ask in
/// the terminal waits as long as the one typing takes.
fn hold(up: impl Fn() -> bool, mut holder: Command, quiet: bool) -> Result<Option<Holder>> {
    if up() {
        return Ok(None);
    }
    // A file, not a pipe: a master that `ssh` leaves in the background keeps
    // it, and a pipe would not end while it lasts.
    let said = if quiet { Some(said_file()?) } else { None };
    let stderr = match &said {
        Some(file) => Stdio::from(file.try_clone()?),
        None => Stdio::inherit(),
    };
    let child = holder
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|error| CannotRun(format!("cannot run {}: {error}", ssh::program_name())))?;
    let mut holder = Holder {
        _ending: interrupt::end_on_exit(&child),
        child,
    };
    let started = std::time::Instant::now();
    let wait = crate::inject::master_wait().unwrap_or(MASTER_WAIT);
    loop {
        let master = up();
        if let Some(status) = holder.child.try_wait()? {
            // Waited for, its ID may be another process's from now on, so
            // it is no longer one to end on exit.
            drop(holder);
            if master || up() {
                // It left the master running in the background.
                return Ok(None);
            }
            let said = said.map(last_said).unwrap_or_default();
            if said.is_empty() {
                bail!("could not log in ({status})");
            }
            bail!("could not log in: {said} ({status})");
        }
        if master {
            return Ok(Some(holder));
        }
        if quiet && started.elapsed() > wait {
            holder.end();
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

/// A host's first session, as the client has it: it probes the host,
/// installs Ferry there if it must, and becomes Ferry's [`launcher`]. What it
/// tells the client comes in lines marked with the run's tag, among what the
/// host's shell, and what it left running, print; see [`start::frame`]. It
/// reads one line at a time from the client, each a command, as it gets to
/// it.
pub struct FirstSession {
    place: String,
    tag: String,
    link: Link,
}

impl FirstSession {
    /// The tag its lines are marked with.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Reads the next part of what it tells the client, up to the line that
    /// ends it: what came before that is no line of Ferry's is said, once.
    pub fn read_part(&mut self) -> Result<Vec<start::Line>> {
        let mut skipped = Vec::new();
        let read = start::read_part(&mut self.link.reader, &self.tag, &mut skipped);
        say_skipped(&self.place, &skipped);
        match read {
            Ok(Some(lines)) => Ok(lines),
            Ok(None) => {
                let status = self.link.child.as_mut().map(Child::wait);
                match status {
                    Some(Ok(status)) if status.code() == Some(255) => {
                        bail!("{}: its SSH connection dropped", self.place)
                    }
                    Some(Ok(status)) => bail!("{}: it ended first ({status})", self.place),
                    _ => bail!("{}: it ended first", self.place),
                }
            }
            Err(error) => Err(error).with_context(|| format!("read what {} said", self.place)),
        }
    }

    /// Sends it `line`, a command for its shell to run as it gets to it.
    pub fn send(&mut self, line: &str) -> Result<()> {
        self.link
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| self.link.writer.write_all(b"\n"))
            .and_then(|()| self.link.writer.flush())
            .with_context(|| format!("tell {} what to do", self.place))
    }

    /// Sends it what `upload` writes, after which nothing more is sent: the
    /// command it runs reads it to its end.
    pub fn upload(&mut self, upload: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        upload(&mut self.link.writer)?;
        self.link.writer.flush()?;
        let writer = std::mem::replace(
            &mut self.link.writer,
            BufWriter::new(Box::new(std::io::sink())),
        );
        drop(writer);
        Ok(())
    }

    /// Ends it, being done with it: its input closed, it is given a moment to
    /// end, and then ended.
    pub fn end(self) {
        end_within(self.link, ENDING);
    }
}

/// How long a process that carries something the client is done with is
/// given to end on its own, as its input closes.
const ENDING: Duration = Duration::from_secs(5);

/// Ends `link`, its input closed first: its process is given `time` to end on
/// its own, then ended.
fn end_within(mut link: Link, time: Duration) {
    let writer = std::mem::replace(&mut link.writer, BufWriter::new(Box::new(std::io::sink())));
    drop(writer);
    if let Some(child) = link.child.as_mut() {
        let started = std::time::Instant::now();
        while started.elapsed() < time {
            if child.try_wait().is_ok_and(|status| status.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    link.end();
}

/// What a program reads on its control connection, until the connection
/// closes; shared with its senders.
type ControlWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// What the control connection is read from.
pub type ControlReader = BufReader<Box<dyn Read + Send>>;

/// The program at the far end of a control connection.
enum Program {
    /// A child process of the client's, on this machine.
    Child(Child),
    /// One the launcher on a host started, which says how it ended.
    /// `carrier` is what carries the connection: `ssh`, or a joining
    /// process.
    Launched {
        carrier: Option<Child>,
        launched: launcher::Launched,
    },
}

impl Program {
    /// Waits for it to end, and says how it did.
    fn wait(&mut self) -> Result<ExitStatus> {
        match self {
            Self::Child(child) => child.wait().context("wait for the server"),
            Self::Launched { carrier, launched } => launched.wait(carrier),
        }
    }

    /// How it ended, if it has.
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        match self {
            Self::Child(child) => child.try_wait().context("wait for the server"),
            Self::Launched { carrier, launched } => launched.try_wait(carrier),
        }
    }

    /// Ends it, as one that does not end: on a host, what carries its
    /// connection, which then closes.
    fn kill(&mut self) {
        let child = match self {
            Self::Child(child) => Some(child),
            Self::Launched { carrier, .. } => carrier.as_mut(),
        };
        if let Some(child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Once it has ended: what carries its connection is given a moment to
    /// end on its own too, as the connection closes, then ended.
    fn finish(&mut self) {
        if let Self::Launched {
            carrier: Some(carrier),
            ..
        } = self
        {
            let started = std::time::Instant::now();
            while started.elapsed() < ENDING
                && !carrier.try_wait().is_ok_and(|status| status.is_some())
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = carrier.kill();
            let _ = carrier.wait();
        }
    }
}

/// A control connection: a byte stream to one program at the far end.
pub struct ControlConnection {
    /// Where the program runs, as a message names it.
    place: String,
    program: Program,
    /// What the program writes, until a thread takes it to read.
    reader: Option<ControlReader>,
    writer: ControlWriter,
    /// Set once the connection is closed: what reads it stops.
    let_go: Arc<AtomicBool>,
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
    /// The control connection that `link` is, to `program`, at `place`.
    fn new(place: &str, program: Program, link: Link) -> Self {
        let Link {
            reader,
            writer,
            let_go,
            ..
        } = link;
        let writer: Box<dyn Write + Send> = Box::new(writer);
        Self {
            place: place.to_owned(),
            program,
            reader: Some(reader),
            writer: Arc::new(Mutex::new(Some(writer))),
            let_go,
        }
    }

    /// Says hello to the server this control connection runs, and waits to
    /// be welcomed; returns where the stream reaches it. A server that exits
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
        match handshake::client_handshake(reader, writer, hello, skipped) {
            Err(error) if error.is::<handshake::Closed>() => match self.program.wait() {
                Ok(status) => bail!("the server did not start ({status})"),
                Err(error) => Err(error.context("the server did not start")),
            },
            result => result,
        }
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
    pub fn close(mut self) -> Result<()> {
        self.writer.lock().expect("not poisoned").take();
        let status = self.program.wait();
        // What it said last is read by now; what still reads it, as on a
        // pipe whose other end a ControlMaster holds, stops.
        self.let_go.store(true, Ordering::SeqCst);
        self.program.finish();
        exited(status?)
    }

    /// Closes the connection after a failure: waits for the program to end
    /// for `time` at most, then ends it, as when its SSH connection is gone
    /// and would never say that it ended; what reads the connection stops at
    /// once.
    pub fn close_within(mut self, time: Duration) -> Result<()> {
        self.writer.lock().expect("not poisoned").take();
        self.let_go.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = self.program.try_wait()? {
                self.program.finish();
                return exited(status);
            }
            if started.elapsed() >= time {
                self.program.kill();
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

/// The bytes the stream over an SSH connection carried, and how long ago
/// anything came on it, if anything has.
type Seen = (u64, Option<Duration>);

/// What the dashboard shows of a host's SSH connection: how many bytes the
/// stream over it carried, and how long since anything came on it, as the
/// server there last said. Clones see the same.
#[derive(Clone)]
pub struct Monitor {
    host: String,
    seen: Arc<Mutex<Seen>>,
}

impl Monitor {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Takes what the server's stream carried, over the SSH connection.
    pub fn carried(&self, carried: &crate::report::Carried) {
        *self.seen.lock().expect("not poisoned") =
            (carried.bytes, carried.heard.map(Duration::from_millis));
    }

    /// The bytes carried so far, and how long ago anything came, if
    /// anything has.
    pub fn connection(&self) -> Seen {
        *self.seen.lock().expect("not poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket that answers every connection that comes straight with the
    /// start mark, then hands the test what came first on it, and the
    /// connection; and its path.
    fn serving() -> (
        tempfile::TempDir,
        String,
        crossbeam_channel::Receiver<(Vec<u8>, UnixStream)>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (sender, connections) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut first = vec![0u8; 1];
                stream.read_exact(&mut first).unwrap();
                start::write(&mut stream).unwrap();
                if sender.send((first, stream)).is_err() {
                    return;
                }
            }
        });
        (temp, socket.display().to_string(), connections)
    }

    /// What goes on a connection after its start goes both ways.
    fn carries(mut link: Link, far: &mut (impl Read + Write)) {
        link.writer.write_all(b"ping").unwrap();
        link.writer.flush().unwrap();
        let mut ping = [0u8; 4];
        far.read_exact(&mut ping).unwrap();
        assert_eq!(&ping, b"ping");
        far.write_all(b"pong").unwrap();
        let mut pong = [0u8; 4];
        link.reader.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
    }

    /// A connection to a socket comes straight, says that it does, and once
    /// answered, carries what it is given.
    #[test]
    fn a_connection_comes_straight_says_so_and_carries() {
        crate::test_support::within(
            "a connection comes straight says so and carries",
            crate::test_support::PATIENCE,
            move || {
                let (_temp, socket, connections) = serving();
                let link = Transport::Local.connect(&socket, None).unwrap();
                let (first, mut far) = connections.recv().unwrap();
                assert_eq!(first, [STRAIGHT]);
                carries(link, &mut far);
            },
        );
    }

    #[test]
    fn a_connection_closed_at_once_is_said_to_have_ended_before_it_started() {
        crate::test_support::within(
            "a connection closed at once is said to have ended before it started",
            crate::test_support::PATIENCE,
            move || {
                // A socket that closes every channel as it comes, before its
                // first bytes can be written.
                let temp = tempfile::tempdir().unwrap();
                let socket = temp.path().join("socket");
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        drop(stream);
                    }
                });
                let error = Transport::Local
                    .connect(&socket.display().to_string(), None)
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("ended before it started"), "{error}");
            },
        );
    }

    /// A connection joins through a shell: the joining process, after what
    /// a shell might print and the start mark, sends back what it is sent.
    #[test]
    fn a_connection_can_join_through_a_shell() {
        crate::test_support::within(
            "a connection can join through a shell",
            crate::test_support::PATIENCE,
            move || {
                let join = "echo noise; printf '\\000ferry: start\\000'; exec cat";
                let mut link = Transport::Local
                    .connect("/nonexistent/socket", Some(join))
                    .unwrap();
                link.writer.write_all(b"ping").unwrap();
                link.writer.flush().unwrap();
                let mut ping = [0u8; 4];
                link.reader.read_exact(&mut ping).unwrap();
                assert_eq!(&ping, b"ping");
                link.end();
            },
        );
    }

    /// A joining process that ends before its start mark is said to have,
    /// and is not left running.
    #[test]
    fn a_connection_whose_process_ends_first_is_said_to_have_ended() {
        crate::test_support::within(
            "a connection whose process ends first is said to have ended",
            crate::test_support::PATIENCE,
            move || {
                let error = Transport::Local
                    .connect("/nonexistent/socket", Some("exit 1"))
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("ended before it started"), "{error}");
            },
        );
    }

    /// What a host's server says its stream carried goes on its SSH
    /// connection.
    #[test]
    fn what_the_stream_carried_goes_on_the_connection() {
        let monitor = Monitor {
            host: "host".to_owned(),
            seen: Arc::new(Mutex::new((0, None))),
        };
        assert_eq!(monitor.connection(), (0, None));
        monitor.carried(&crate::report::Carried {
            bytes: 116,
            heard: Some(200),
        });
        assert_eq!(
            monitor.connection(),
            (116, Some(Duration::from_millis(200)))
        );
    }

    /// A read of a link stops once the link is let go, though its pipe's
    /// other end stays open, as a ControlMaster that stopped answering
    /// holds it.
    #[test]
    fn a_link_let_go_stops_being_read() {
        crate::test_support::within(
            "a link let go stops being read",
            crate::test_support::PATIENCE,
            move || {
                let (reader, writer) = std::io::pipe().unwrap();
                let let_go = Arc::new(AtomicBool::new(false));
                let mut reading = LetGo {
                    inner: reader,
                    let_go: Arc::clone(&let_go),
                };
                let reads = std::thread::spawn(move || {
                    let mut byte = [0u8; 1];
                    reading.read(&mut byte)
                });
                std::thread::sleep(Duration::from_millis(300));
                let started = std::time::Instant::now();
                let_go.store(true, Ordering::SeqCst);
                let read = reads.join().unwrap();
                assert_eq!(
                    read.unwrap_err().kind(),
                    std::io::ErrorKind::ConnectionAborted
                );
                assert!(started.elapsed() < Duration::from_secs(2));
                drop(writer);
            },
        );
    }

    #[test]
    fn a_program_that_fails_says_so_when_closed() {
        crate::test_support::within(
            "a program that fails says so when closed",
            crate::test_support::PATIENCE,
            move || {
                let control = Transport::Local.control("exit 3").unwrap();
                let error = control.close().unwrap_err().to_string();
                assert!(error.contains('3'), "{error}");
            },
        );
    }

    /// A master already up, the user's own or one left running, needs no
    /// `ssh -N`: given one, it would ask it for a session, a login shell on
    /// the host, for nothing.
    #[test]
    fn a_master_already_up_starts_no_holder() {
        let temp = tempfile::tempdir().unwrap();
        let ran = temp.path().join("ran");
        let mut holder = Command::new("sh");
        holder.arg("-c").arg(format!("touch '{}'", ran.display()));
        assert!(hold(|| true, holder, true).unwrap().is_none());
        assert!(!ran.exists());
    }

    /// An `ssh -N` that is the master, still running once it is up, is held,
    /// and ended on exit until it is ended.
    #[test]
    fn a_holder_that_is_the_master_is_held_until_ended() {
        crate::test_support::within(
            "a holder that is the master is held until ended",
            crate::test_support::PATIENCE,
            move || {
                let mut holder = Command::new("sleep");
                holder.arg("30");
                // Not up before it starts; up once it has.
                let asked = std::sync::atomic::AtomicU32::new(0);
                let up = || asked.fetch_add(1, Ordering::SeqCst) > 0;
                let holder = hold(up, holder, true).unwrap().expect("held");
                let id = holder.child.id();
                assert!(interrupt::ends_on_exit(id));
                holder.end();
                assert!(!interrupt::ends_on_exit(id));
            },
        );
    }

    /// An `ssh -N` gone once the master is up, as one that left the master
    /// in the background with `ControlPersist`, is not held: waited for, its
    /// ID may be another process's, so it is no longer one to end on exit.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_holder_gone_once_the_master_is_up_is_not_held() {
        crate::test_support::within(
            "a holder gone once the master is up is not held",
            crate::test_support::PATIENCE,
            move || {
                let temp = tempfile::tempdir().unwrap();
                let pid = temp.path().join("pid");
                let mut holder = Command::new("sh");
                holder
                    .arg("-c")
                    .arg(format!("echo $$ > '{}'", pid.display()));
                let id = || loop {
                    let read = std::fs::read_to_string(&pid).unwrap_or_default();
                    if let Ok(id) = read.trim().parse::<u32>() {
                        return id;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                // Not up before it starts, nor as it is first looked at; then
                // up once it has ended, not yet waited for. So it is found
                // gone as the master answers, whichever look finds it so.
                let asked = std::sync::atomic::AtomicU32::new(0);
                let up = || match asked.fetch_add(1, Ordering::SeqCst) {
                    0 | 1 => false,
                    _ => {
                        ended(id());
                        true
                    }
                };
                assert!(hold(up, holder, true).unwrap().is_none());
                assert!(!interrupt::ends_on_exit(id()));
            },
        );
    }

    /// Waits until the child `id` has ended, without waiting for it as
    /// `wait` does, which would take its status: it stays a zombie, and its
    /// ID its own. One already waited for has ended.
    #[cfg(target_os = "linux")]
    fn ended(id: u32) {
        // SAFETY: a zeroed `siginfo_t` is one to fill, and `waitid` fills
        // only it.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT);
        }
    }
}
