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
//!   Ferry there if it must, and becomes Ferry's [`monitor`];
//! - **control connections**: each a plain byte stream to one program, a
//!   server, which lives as long as its control connection; on a host, the
//!   monitor starts it on a channel to its socket, which no shell can write
//!   to;
//! - the **stream** between a transfer's two servers, over the SSH
//!   connection, which the client makes and hands to the servers, never
//!   touching what goes on it; see [`streams`]. Each server has it as a
//!   [`wire`], which carries what the peers send each other.
//!
//! Every connection, at either end, is read and written as a [`Wire`], and
//! nowhere else: whatever waits on one waits within its scope, which ends
//! as the place it reaches is lost.

pub mod child;
pub mod join;
pub mod lifeline;
pub mod monitor;
pub mod pipes;
pub mod scope;
pub mod ssh;
pub mod start;
pub mod streams;
pub mod wire;

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub use join::{Joining, STRAIGHT, join, listen};

use crate::handshake;
use crate::interrupt;
use crate::monitor::client::Node;
use crate::monitor::{New, Phase, Role};
use crate::protocol::{self, Hello, Message, Welcome};
use crate::remote::Location;
use child::Child;
use scope::Scope;
use ssh::Ssh;
use wire::Wire;

/// How a place is reached.
pub enum Transport {
    /// This machine: programs run as child processes, each a scope of the
    /// client's monitor under the given one.
    Local(Node),
    /// A host, over SSH: programs run in sessions of its SSH connection,
    /// already logged in, and held open for the whole run.
    Ssh(Held),
}

/// The SSH connection, logged in, and the `ssh -N` that holds it open, if
/// one does; and the host as the client's monitor has it, with the link to
/// its monitor under it, the connections that link carries.
pub struct Held {
    ssh: Ssh,
    holder: Option<Holder>,
    host: Node,
    link: Node,
    /// When anything last came from the host, on any of its connections.
    heard: Heard,
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
    fn end(self) {
        self.child.kill();
        self.child.wait();
    }
}

impl Transport {
    /// Opens the transport to a location's place. For a host, this opens
    /// its SSH connection, and runs nothing there: it is only logged in,
    /// and held open. A password or an OTP is asked for here, if it needs
    /// one; if it cannot be opened, neither is the transport. A host is a
    /// scope of the client's monitor under `under`, logging in.
    pub fn open(location: &Location, under: &Node) -> Result<Self> {
        let Location::Remote { host, .. } = location else {
            return Ok(Self::Local(under.clone()));
        };
        let node = under.open(New::Host, host)?;
        let link = node.open(New::Plain, &format!("the link to {host}"))?;
        let ssh = Ssh::new(host, ssh::configured(host))?;
        let holder = log_in(&ssh)?;
        Ok(Self::Ssh(Held {
            ssh,
            holder,
            host: node,
            link,
            heard: Arc::new(Mutex::new(None)),
        }))
    }

    /// The place, as messages name it.
    pub fn place(&self) -> &str {
        match self {
            Self::Local(_) => "this machine",
            Self::Ssh(held) => held.ssh.host(),
        }
    }

    /// The place as the client's monitor has it: a host, or, on this
    /// machine, what the transport was opened under.
    pub fn node(&self) -> &Node {
        match self {
            Self::Local(node) => node,
            Self::Ssh(held) => &held.host,
        }
    }

    /// Word came from the host at `at`: anything that came on one of its
    /// connections, the link to its monitor, a server's control connection,
    /// or the stream, as the server reading it said; nothing on this
    /// machine.
    pub fn heard_from(&self, at: std::time::Instant) {
        if let Self::Ssh(held) = self {
            let mut heard = held.heard.lock().expect("not poisoned");
            if heard.is_none_or(|heard| heard < at) {
                *heard = Some(at);
            }
            held.host.heard_at(at);
        }
    }

    /// When anything last came from a host; none on this machine.
    pub fn heard(&self) -> Option<Heard> {
        match self {
            Self::Local(_) => None,
            Self::Ssh(held) => Some(Arc::clone(&held.heard)),
        }
    }

    /// What carries the link to a host's monitor, and its first session, as
    /// the client's monitor has it: closed last, once what works on the host
    /// is done. On this machine, as [`Transport::node`].
    pub fn link(&self) -> &Node {
        match self {
            Self::Local(node) => node,
            Self::Ssh(held) => &held.link,
        }
    }

    /// A command that runs `script` with `sh` at the far end. Its standard
    /// input and output are the caller's to set; its standard error stays
    /// this process's, so that what goes wrong there shows.
    pub fn command(&self, script: &str) -> Command {
        match self {
            Self::Local(_) => {
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

    /// Opens a control connection to the program `script` runs here, on
    /// this machine, as a server, its process a scope of `worker`'s, in a
    /// process group of its own, so that a Ctrl-C reaches the client alone,
    /// which stops it. Its standard error is its lifeline, which the client
    /// reads, and passes on.
    pub fn control(&self, script: &str, worker: &Node) -> Result<ControlConnection> {
        use std::os::unix::process::CommandExt;
        let (lifeline, stderr) = lifeline::pair().context("start the server")?;
        let mut command = self.command(script);
        command.process_group(0);
        let mut wire = carried(command, stderr, worker, Role::Worker, "its process")?;
        let child = wire.take_carrier().expect("a process");
        // What its stream carried, it tells the client on its control
        // connection too, which counts that.
        std::thread::spawn(move || {
            lifeline.read(
                |part| {
                    let _ = std::io::stderr().write_all(part);
                },
                |_| {},
            );
        });
        Ok(ControlConnection::new(
            self.place(),
            Program::Child(child),
            wire,
            worker,
        ))
    }

    /// Opens a host's first session, which runs `script`, whose lines are
    /// marked with `tag`; see [`FirstSession`]. What the host says is its
    /// Liveness from now on.
    pub fn first_session(&self, script: &str, tag: &str) -> Result<FirstSession> {
        let name = format!("the session on {}", self.place());
        let wire = carried(
            self.command(script),
            Stdio::inherit(),
            self.link(),
            Role::Carrier,
            &name,
        )
        .with_context(|| format!("reach {}", self.place()))?;
        self.node().phase(Phase::Hearing);
        Ok(FirstSession::start(self.place(), tag, wire, self.node()))
    }

    /// Reaches what listens at the far end, as `reach` says: its socket
    /// straight, as a channel SSH forwards to it over the SSH connection,
    /// which nothing else on the host can write to, or on this machine, a
    /// connection to it; its port on loopback, likewise, giving the secret
    /// first; or by running a joining process, through the shell on the host.
    /// Done once the far end has written the start mark: what listens
    /// answers one that came straight, or gave the secret, with it, and a
    /// joining process writes it as it starts, after what the shell printed.
    /// One SSH would not forward fails with [`NotForwarded`]. What carries
    /// it is a scope of `under`'s, `name`d.
    fn connect(&self, reach: &Reach, under: &Node, name: &str) -> Result<Wire> {
        let mut wire = match reach {
            Reach::Socket(socket) => self.straight(socket, under, name)?,
            Reach::Port { number, secret } => self.to_port(*number, secret, under, name)?,
            Reach::Join(join) => self.joining(join, under, name)?,
        };
        let join = matches!(reach, Reach::Join(_));
        let skipped = match start::skip(&mut wire) {
            Ok(skipped) => skipped,
            // Closed by the far end before what was written to it was read:
            // it ended, as much as one that closed at once.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => start::Skipped {
                before: Vec::new(),
                found: false,
            },
            Err(error) => {
                wire.end();
                return Err(error)
                    .with_context(|| format!("{}: wait for a channel to start", self.place()));
            }
        };
        if join {
            say_skipped(self.place(), &skipped.before);
        }
        if !skipped.found {
            wire.end();
            match (self, join) {
                (Self::Ssh(_), false) => return Err(NotForwarded.into()),
                _ => bail!("{}: a channel ended before it started", self.place()),
            }
        }
        Ok(wire)
    }

    /// What the dashboard shows of a host's SSH connection; none on this
    /// machine.
    pub fn traffic(&self) -> Option<Traffic> {
        match self {
            Self::Local(_) => None,
            Self::Ssh(held) => Some(Traffic {
                host: held.ssh.host().to_owned(),
                bytes: Arc::new(Mutex::new(0)),
                heard: Arc::clone(&held.heard),
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
            Self::Local(_) => Half::Here(PathBuf::from(socket)),
            Self::Ssh(held) => {
                let ssh = &held.ssh;
                match (join, &welcome.port) {
                    (Some(join), _) => Half::There {
                        command: ssh.script(join),
                        opens: Opens::Joining,
                    },
                    (None, Some(port)) => Half::There {
                        command: ssh.forward(&format!("127.0.0.1:{}", port.number)),
                        opens: Opens::Secret(port.secret.clone()),
                    },
                    (None, None) => Half::There {
                        command: ssh.forward(&socket),
                        opens: Opens::Straight,
                    },
                }
            }
        }
    }

    /// A wire over the SSH connection, or on this machine, that reaches the
    /// server's socket straight, its first byte written.
    fn straight(&self, socket: &str, under: &Node, name: &str) -> Result<Wire> {
        let mut wire = match self {
            Self::Local(_) => {
                let stream =
                    UnixStream::connect(socket).with_context(|| format!("connect to {socket}"))?;
                Wire::socket(stream, &under.scope().child()?)?
            }
            Self::Ssh(held) => carried(
                held.ssh.forward(socket),
                Stdio::inherit(),
                under,
                Role::Carrier,
                name,
            )?,
        };
        // A wire that did not get through may close before this is written;
        // that is found, and said, as its start mark is waited for.
        let _ = wire.write_all(&[STRAIGHT]);
        Ok(wire)
    }

    /// A wire over the SSH connection, or on this machine, that reaches a
    /// port on loopback, the secret it wants written.
    fn to_port(&self, number: u16, secret: &str, under: &Node, name: &str) -> Result<Wire> {
        let mut wire = match self {
            Self::Local(_) => {
                let stream = TcpStream::connect(("127.0.0.1", number))
                    .with_context(|| format!("connect to port {number}"))?;
                Wire::tcp(stream, &under.scope().child()?)?
            }
            Self::Ssh(held) => carried(
                held.ssh.forward(&format!("127.0.0.1:{number}")),
                Stdio::inherit(),
                under,
                Role::Carrier,
                name,
            )?,
        };
        // As with one straight to a socket: one that did not get through is
        // found as its start mark is waited for.
        let _ = wire.write_all(secret.as_bytes());
        Ok(wire)
    }

    /// A wire over the SSH connection, or on this machine, that runs `join`.
    fn joining(&self, join: &str, under: &Node, name: &str) -> Result<Wire> {
        let command = match self {
            Self::Local(_) => self.command(join),
            Self::Ssh(held) => held.ssh.script(join),
        };
        carried(command, Stdio::inherit(), under, Role::Carrier, name)
    }
}

/// How the client reaches what listens at the far end: a server, or the
/// monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Straight to its socket.
    Socket(String),
    /// Straight to its port on loopback, giving the secret first.
    Port { number: u16, secret: String },
    /// By running this, a joining process, through the shell on the host.
    Join(String),
}

/// SSH did not forward a channel to what listens on a host, as where `sshd`
/// forwards to no socket, or to no port: it ended before its start mark.
#[derive(Debug)]
pub struct NotForwarded;

impl std::fmt::Display for NotForwarded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("SSH did not forward a channel to it")
    }
}

impl std::error::Error for NotForwarded {}

/// How the client reaches a server for one stream; see [`streams`].
pub enum Half {
    /// On this machine: the client hands it its end through its socket.
    Here(PathBuf),
    /// On a host: its end is the input and output of `command`, a channel
    /// SSH forwards, or one that runs a joining process, which first carries
    /// what `opens` says.
    There { command: Command, opens: Opens },
}

/// What a stream's channel to a host's server carries first, to reach it.
pub enum Opens {
    /// The byte that says it came straight to its socket.
    Straight,
    /// The secret its port wants.
    Secret(String),
    /// Nothing: a joining process reaches it.
    Joining,
}

/// Starts `command`, whose input and output are the wire to a program at
/// the far end, the process carrying it: what it reads and writes is a
/// scope of the client's monitor under `under`, in the role it has,
/// `name`d, so that its waits end as that does: a server whose SSH
/// connection is gone is let go of without waiting for its pipe to end, as
/// a ControlMaster that stopped answering still holds the pipe's other end,
/// which `ssh` handed it.
fn carried(
    mut command: Command,
    stderr: Stdio,
    under: &Node,
    role: Role,
    name: &str,
) -> Result<Wire> {
    let child = Child::spawn(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr),
        Some(under.told()),
    )
    .with_context(|| format!("start {:?}", command.get_program()))?;
    let scope = under.process(&child, role, name)?.scope().clone();
    Ok(Wire::carried(child, &scope)?)
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
    let child = Child::spawn(
        holder
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr),
        None,
    )
    .map_err(|error| CannotRun(format!("cannot run {}: {error}", ssh::program_name())))?;
    let holder = Holder {
        _ending: interrupt::end_on_exit(&child),
        child,
    };
    let started = std::time::Instant::now();
    let wait = crate::inject::master_wait().unwrap_or(MASTER_WAIT);
    loop {
        let master = up();
        if let Some(status) = holder.child.try_wait() {
            // Ended, it is no longer one to end on exit.
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
/// installs Ferry there if it must, and becomes Ferry's [`monitor`]. What it
/// tells the client comes in lines marked with the run's tag, among what the
/// host's shell, and what it left running, print; see [`start::frame`]. It
/// reads one line at a time from the client, each a command, as it gets to
/// it.
///
/// Until it becomes the monitor, it has a heartbeat of its own, both ways:
/// it prints an alive line every beat, which a thread here reads, as it
/// reads all it says, whether anything waits for it or not, each line word
/// from the host for the client's monitor; and the client sends it a line
/// every beat, `:` when it has nothing else to send, which its watchdog
/// counts; see [`start::heartbeat`]. Once nothing came from it for the
/// silence, the client's monitor takes the host for lost, which ends what
/// reads it.
pub struct FirstSession {
    place: String,
    tag: String,
    /// What carries it: `ssh`, or `sh` on this machine.
    child: Option<Child>,
    /// Ended once it is taken for gone, or let go: what reads it, and
    /// writes it, stops.
    scope: Scope,
    /// What the client writes to it, shared with what sends the heartbeat:
    /// none once an upload went to its end.
    writer: SessionWriter,
    /// What it says, as the thread that reads it found it.
    told: crossbeam_channel::Receiver<Told>,
    /// The heartbeat goes on, both ways: until it becomes the monitor, or
    /// ends.
    beating: Arc<AtomicBool>,
    /// It was given a command that makes it the monitor: one that read the
    /// rest of its input, and so could read no command more.
    becoming: bool,
}

/// What the client writes to a first session, shared.
type SessionWriter = Arc<Mutex<Option<BufWriter<wire::Output>>>>;

/// What the thread that reads a first session found, and what came before
/// it that was no line of Ferry's.
enum Told {
    Line(start::Line, Vec<u8>),
    /// It ended.
    Ended(Vec<u8>),
    /// Reading it failed, as when it was taken for gone.
    Failed(std::io::Error, Vec<u8>),
}

impl FirstSession {
    /// The first session that `wire` is, at `place`, the client's monitor's
    /// `host`, its lines marked with `tag`, its heartbeat started.
    fn start(place: &str, tag: &str, mut wire: Wire, host: &Node) -> Self {
        let child = wire.take_carrier();
        let scope = wire.scope().clone();
        let (reader, writer) = wire.split();
        let timing = crate::monitor::Timing::wanted();
        let writer = Arc::new(Mutex::new(Some(BufWriter::new(writer))));
        let beating = Arc::new(AtomicBool::new(true));
        let (told_in, told) = crossbeam_channel::unbounded();
        {
            let (tag, host, beating) = (tag.to_owned(), host.clone(), Arc::clone(&beating));
            std::thread::spawn(move || read_session(reader, &tag, &host, &beating, &told_in));
        }
        {
            let (writer, beating) = (Arc::clone(&writer), Arc::clone(&beating));
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(timing.beat);
                    if !beating.load(Ordering::SeqCst) {
                        return;
                    }
                    let mut writer = writer.lock().expect("not poisoned");
                    let Some(writer) = writer.as_mut() else {
                        return;
                    };
                    if writer
                        .write_all(b":\n")
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        Self {
            place: place.to_owned(),
            tag: tag.to_owned(),
            child,
            scope,
            writer,
            told,
            beating,
            becoming: false,
        }
    }

    /// The tag its lines are marked with.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The place it runs at, as messages name it.
    pub fn place(&self) -> &str {
        &self.place
    }

    /// Notes that the command it was given makes it the monitor once done.
    pub fn becomes_monitor(&mut self) {
        self.becoming = true;
    }

    /// Whether it was given a command that makes it the monitor.
    pub fn becoming_monitor(&self) -> bool {
        self.becoming
    }

    /// It is the monitor now, and said where it is: its own heartbeat ends,
    /// as the monitor's link has one.
    pub fn became_monitor(&self) {
        self.beating.store(false, Ordering::SeqCst);
    }

    /// Reads the next part of what it tells the client, up to the line that
    /// ends it, which must count them all: what came before that is no line
    /// of Ferry's is said, once.
    pub fn read_part(&mut self) -> Result<Vec<start::Line>> {
        let mut lines = Vec::new();
        let mut skipped = Vec::new();
        let told = loop {
            match self.told.recv() {
                Ok(Told::Line(line, before)) => {
                    skipped.extend_from_slice(&before);
                    if line.kind != start::DONE {
                        lines.push(line);
                        continue;
                    }
                    let counted = std::str::from_utf8(&line.content)
                        .ok()
                        .and_then(|count| count.parse::<usize>().ok());
                    break if counted == Some(lines.len()) {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!(
                            "a line of Ferry's went missing among what the host printed"
                        ))
                    };
                }
                Ok(Told::Ended(before)) => {
                    skipped.extend_from_slice(&before);
                    let status = self.child.as_ref().map(Child::wait);
                    break match status {
                        Some(status) if status.code() == Some(255) => {
                            Err(anyhow::anyhow!("its SSH connection dropped"))
                        }
                        Some(status) => Err(anyhow::anyhow!("its session ended ({status})")),
                        None => Err(anyhow::anyhow!("its session ended")),
                    };
                }
                Ok(Told::Failed(error, before)) => {
                    skipped.extend_from_slice(&before);
                    break Err(anyhow::Error::from(error)
                        .context(format!("read what {} said", self.place)));
                }
                Err(_) => break Err(anyhow::anyhow!("its session ended")),
            }
        };
        say_skipped(&self.place, &skipped);
        told.map(|()| lines)
    }

    /// Sends it `line`, a command for its shell to run as it gets to it.
    pub fn send(&mut self, line: &str) -> Result<()> {
        let mut writer = self.writer.lock().expect("not poisoned");
        let writer = writer
            .as_mut()
            .with_context(|| format!("{}: the session reads no more commands", self.place))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush())
            .with_context(|| format!("tell {} what to do", self.place))
    }

    /// Sends it what `upload` writes, after which nothing more is sent: the
    /// command it runs reads it to its end. Nothing of the heartbeat goes in
    /// between.
    pub fn upload(&mut self, upload: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
        let mut writer = self.writer.lock().expect("not poisoned");
        let mut taken = writer
            .take()
            .with_context(|| format!("{}: the session reads no more commands", self.place))?;
        upload(&mut taken)?;
        taken.flush()?;
        Ok(())
    }
}

impl Drop for FirstSession {
    /// Cuts it, as on a failure, unless it ended: what reads it, and writes
    /// it, stops, and what carries it is ended, so that nothing of it
    /// outlives the client.
    fn drop(&mut self) {
        self.beating.store(false, Ordering::SeqCst);
        self.scope.end(wire::LET_GO);
        drop(self.writer.lock().expect("not poisoned").take());
        if let Some(child) = self.child.as_ref() {
            child.kill();
            child.wait();
        }
    }
}

/// Reads what a first session says, in a thread of its own, until it ends:
/// each line of Ferry's, and what came before it, handed on to `told`, but
/// alive lines, which, as anything that comes, are only heard from the
/// host.
fn read_session(
    mut reader: wire::Input,
    tag: &str,
    host: &Node,
    beating: &AtomicBool,
    told: &crossbeam_channel::Sender<Told>,
) {
    let mut skipped = Vec::new();
    loop {
        let line = start::next_line(&mut reader, tag, &mut skipped);
        if beating.load(Ordering::SeqCst) {
            host.heard();
        }
        let said = match line {
            Ok(Some(line)) if line.kind == start::ALIVE => continue,
            Ok(Some(line)) => Told::Line(line, std::mem::take(&mut skipped)),
            Ok(None) => Told::Ended(std::mem::take(&mut skipped)),
            Err(error) => Told::Failed(error, std::mem::take(&mut skipped)),
        };
        let last = !matches!(said, Told::Line(..));
        if last {
            beating.store(false, Ordering::SeqCst);
        }
        if told.send(said).is_err() || last {
            return;
        }
    }
}

/// What a program reads on its control connection, once the handshake is
/// done, until the connection closes; shared with its senders.
type ControlWriter = Arc<Mutex<Option<wire::Writing>>>;

/// What the control connection is read from.
pub type ControlReader = BufReader<Box<dyn Read + Send>>;

/// The program at the far end of a control connection.
enum Program {
    /// A child process of the client's, on this machine.
    Child(Child),
    /// One the monitor on a host started, which says how it ended.
    /// `carrier` is what carries the connection: `ssh`, or a joining
    /// process.
    Launched {
        carrier: Option<Child>,
        launched: monitor::Launched,
    },
}

impl Program {
    /// Waits for it to end, and says how it did.
    fn wait(&mut self) -> Result<ExitStatus> {
        match self {
            Self::Child(child) => Ok(child.wait()),
            Self::Launched { carrier, launched } => launched.wait(carrier),
        }
    }
}

/// A control connection: a byte stream to one program at the far end. The
/// handshake goes on it as it is; after, it goes over the stream layer, its
/// packets and its end. Whether the far end is there is the monitors' to
/// judge, by their heartbeat, not the connection's.
pub struct ControlConnection {
    /// Where the program runs, as a message names it.
    place: String,
    program: Program,
    /// The server, as the client's monitor has it.
    worker: Node,
    /// The connection as it is, until the handshake is done.
    wire: Option<Wire>,
    /// After: what the program writes, until a thread takes it to read.
    reader: Option<ControlReader>,
    writer: ControlWriter,
}

/// What a server says on its control connection, as read: once no more of
/// it is, the client's monitor is told, as all of it is read.
struct Talk {
    reading: wire::Reading,
    worker: Node,
}

impl Read for Talk {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        self.reading.read(into)
    }
}

impl Drop for Talk {
    fn drop(&mut self) {
        self.worker.heard_all();
    }
}

/// A way to send a server messages on its control connection while the
/// transfer goes on, such as a request for a snapshot.
#[derive(Clone)]
pub struct ControlSender(ControlWriter);

impl ControlSender {
    /// Sends `message`, unless the connection is closed, or not yet past its
    /// handshake.
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
    /// The control connection that `wire` is, to `program`, at `place`.
    fn new(place: &str, program: Program, wire: Wire, worker: &Node) -> Self {
        Self {
            place: place.to_owned(),
            program,
            worker: worker.clone(),
            wire: Some(wire),
            reader: None,
            writer: Arc::new(Mutex::new(None)),
        }
    }

    /// Says hello to the server this control connection runs, and waits to
    /// be welcomed; returns where the stream reaches it. A server that exits
    /// before it answers, such as one that could not be started, is reported
    /// with how it exited; what it printed is already on standard error.
    /// Welcomed, the connection goes over the stream layer, as the server's
    /// does once it has answered.
    pub fn handshake(&mut self, hello: Hello) -> Result<Welcome> {
        let wire = self
            .wire
            .as_mut()
            .context("the connection is past its handshake")?;
        let place = &self.place;
        let skipped = |before: &[u8]| say_skipped(place, before);
        let welcome = match handshake::client_handshake(wire, hello, skipped) {
            Err(error) if error.is::<handshake::Closed>() => {
                return match self.program.wait() {
                    Ok(status) => bail!("the server did not start ({status})"),
                    Err(error) => Err(error.context("the server did not start")),
                };
            }
            result => result?,
        };
        // What was read past the answer, if anything, goes first.
        let wire = self.wire.take().expect("the wire is there");
        let (reading, writing, carried) = wire
            .stream()
            .context("go on the server's control connection")?;
        // What it says from now on is read, to its end, before its end is
        // taken for known, as why it failed, said as it ended.
        self.worker.listening();
        self.reader = Some(BufReader::new(Box::new(Talk {
            reading,
            worker: self.worker.clone(),
        })));
        *self.writer.lock().expect("not poisoned") = Some(writing);
        drop(carried);
        Ok(welcome)
    }

    /// What the program writes from now on, for a thread of its own to read.
    pub fn take_reader(&mut self) -> Option<ControlReader> {
        self.reader.take()
    }

    /// A way to send the program messages from another thread.
    pub fn sender(&self) -> ControlSender {
        ControlSender(Arc::clone(&self.writer))
    }

    /// Closes the connection, and waits for the program to end, saying how
    /// it did: a server ends with its control connection.
    pub fn close(mut self) -> Result<()> {
        (self.closer())();
        if let Some(wire) = self.wire.as_mut() {
            wire.close_output();
        }
        exited(self.program.wait()?)
    }

    /// What closes what the client writes on the connection, with its end,
    /// from any thread, once: told it is over, or to stop, the server ends.
    /// It holds nothing open: the connection dropped, as on a failure
    /// preparing, is gone without its end, and the server stops.
    pub fn closer(&self) -> impl Fn() + Send + Sync + 'static {
        let writer = Arc::downgrade(&self.writer);
        move || {
            if let Some(writer) = writer.upgrade()
                && let Some(writing) = writer.lock().expect("not poisoned").take()
            {
                let _ = writing.finish();
            }
        }
    }
}

/// How a server ended: well, or not, as with an internal error, which it
/// said itself.
fn exited(status: ExitStatus) -> Result<()> {
    match status.code() {
        Some(0) => Ok(()),
        Some(code) if code == i32::from(crate::stop::INTERNAL) => Err(crate::stop::Internal.into()),
        _ => bail!("the server exited with {status}"),
    }
}

/// The bytes the stream over an SSH connection carried, and how long ago
/// anything came from the host, if anything has.
type Seen = (u64, Option<Duration>);

/// When anything last came from a host, on any of its connections, its
/// heartbeat among it: shared by what shows the host's SSH connection.
pub type Heard = Arc<Mutex<Option<std::time::Instant>>>;

/// What the dashboard shows of a host's SSH connection: how many bytes the
/// stream over it carried, as the server there last said, and how long
/// since anything came from the host, its heartbeat at least every beat.
/// Clones see the same.
#[derive(Clone)]
pub struct Traffic {
    host: String,
    bytes: Arc<Mutex<u64>>,
    heard: Heard,
}

impl Traffic {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Takes what the server's stream carried, over the SSH connection.
    pub fn carried(&self, carried: &crate::report::Carried) {
        *self.bytes.lock().expect("not poisoned") = carried.bytes;
    }

    /// The bytes carried so far, and how long ago anything came from the
    /// host, if anything has.
    pub fn connection(&self) -> Seen {
        let heard = *self.heard.lock().expect("not poisoned");
        (
            *self.bytes.lock().expect("not poisoned"),
            heard.map(|heard| heard.elapsed()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node of no monitor.
    fn detached() -> Node {
        Node::detached().unwrap()
    }

    /// This machine, its transport under a node of no monitor.
    fn local() -> Transport {
        Transport::Local(detached())
    }

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
    fn carries(mut wire: Wire, far: &mut (impl Read + Write)) {
        wire.write_all(b"ping").unwrap();
        let mut ping = [0u8; 4];
        far.read_exact(&mut ping).unwrap();
        assert_eq!(&ping, b"ping");
        far.write_all(b"pong").unwrap();
        let mut pong = [0u8; 4];
        wire.read_exact(&mut pong).unwrap();
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
                let wire = local()
                    .connect(&Reach::Socket(socket.clone()), &detached(), "a channel")
                    .unwrap();
                let (first, mut far) = connections.recv().unwrap();
                assert_eq!(first, [STRAIGHT]);
                carries(wire, &mut far);
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
                let error = local()
                    .connect(
                        &Reach::Socket(socket.display().to_string()),
                        &detached(),
                        "a channel",
                    )
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
                let mut wire = local()
                    .connect(&Reach::Join(join.to_owned()), &detached(), "a channel")
                    .unwrap();
                wire.write_all(b"ping").unwrap();
                let mut ping = [0u8; 4];
                wire.read_exact(&mut ping).unwrap();
                assert_eq!(&ping, b"ping");
                wire.end();
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
                let error = local()
                    .connect(&Reach::Join("exit 1".to_owned()), &detached(), "a channel")
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
        let traffic = Traffic {
            host: "host".to_owned(),
            bytes: Arc::new(Mutex::new(0)),
            heard: Arc::new(Mutex::new(None)),
        };
        assert_eq!(traffic.connection(), (0, None));
        traffic.carried(&crate::report::Carried {
            bytes: 116,
            heard: Some(200),
        });
        *traffic.heard.lock().unwrap() = Some(std::time::Instant::now());
        let (bytes, heard) = traffic.connection();
        assert_eq!(bytes, 116);
        assert!(heard.unwrap() < Duration::from_secs(5));
    }

    /// A read of a wire stops once the wire is let go, though its pipe's
    /// other end stays open, as a ControlMaster that stopped answering
    /// holds it; so does a write that waits for room.
    #[test]
    fn a_wire_let_go_stops_being_read_and_written() {
        crate::test_support::within(
            "a wire let go stops being read and written",
            crate::test_support::PATIENCE,
            move || {
                use std::os::fd::OwnedFd;
                let (reader, writer) = std::io::pipe().unwrap();
                let (other_reader, other_writer) = std::io::pipe().unwrap();
                let scope = Scope::new().unwrap();
                let wire = Wire::new(
                    std::fs::File::from(OwnedFd::from(reader)),
                    std::fs::File::from(OwnedFd::from(other_writer)),
                    None,
                    &scope,
                )
                .unwrap();
                let (mut input, mut output) = wire.split();
                let reads = std::thread::spawn(move || {
                    let mut byte = [0u8; 1];
                    input.read(&mut byte)
                });
                // Nothing reads the pipe: it fills, and the writer waits.
                let writes = std::thread::spawn(move || {
                    let chunk = vec![0u8; 1 << 16];
                    loop {
                        if let Err(error) = output.write_all(&chunk) {
                            return error;
                        }
                    }
                });
                std::thread::sleep(Duration::from_millis(300));
                let started = std::time::Instant::now();
                scope.end(wire::LET_GO);
                let read = reads.join().unwrap();
                assert_eq!(
                    read.unwrap_err().kind(),
                    std::io::ErrorKind::ConnectionAborted
                );
                let written = writes.join().unwrap();
                assert_eq!(written.kind(), std::io::ErrorKind::ConnectionAborted);
                assert!(started.elapsed() < Duration::from_secs(2));
                drop((writer, other_reader));
            },
        );
    }

    #[test]
    fn a_program_that_fails_says_so_when_closed() {
        crate::test_support::within(
            "a program that fails says so when closed",
            crate::test_support::PATIENCE,
            move || {
                let control = local().control("exit 3", &detached()).unwrap();
                let error = control.close().unwrap_err().to_string();
                assert!(error.contains('3'), "{error}");
            },
        );
    }

    /// What the server sends right after its answer to the hello, which the
    /// client's reader may read with the answer, comes first once the
    /// connection goes over the stream layer: no byte of it is lost.
    #[test]
    fn what_comes_right_after_the_answer_is_not_lost() {
        crate::test_support::within(
            "what comes right after the answer is not lost",
            crate::test_support::PATIENCE,
            move || {
                let temp = tempfile::tempdir().unwrap();
                let welcome = Welcome {
                    socket: b"/nowhere".to_vec(),
                    port: None,
                    root: crate::fsys::root_identity(temp.path().as_os_str()),
                };
                let mut sent =
                    start::framed(&protocol::encode(&protocol::Control::Welcome(welcome)).unwrap());
                sent.extend(protocol::encode(&crate::report::Report::Done).unwrap());
                let file = temp.path().join("sent");
                std::fs::write(&file, &sent).unwrap();
                // All of it at once, before the hello is even read.
                let script = format!("cat '{}'; exec cat > /dev/null", file.display());
                let mut control = local().control(&script, &detached()).unwrap();
                let hello = Hello {
                    build: crate::build_info::identity(),
                    role: protocol::Role::Destination,
                    root: b"dest".to_vec(),
                    dry_run: true,
                    modify_window: 0,
                    numeric_ids: false,
                    delete: false,
                    one_file_system: false,
                    compress: true,
                    compress_level: 1,
                    profile: false,
                    part_entries: 0,
                    backlog_files: 0,
                    paced_bytes: 0,
                    in_flight_entries: 0,
                    data_credit: 0,
                    cpus: 0,
                    place: 0,
                };
                control.handshake(hello).unwrap();
                let mut reader = control.take_reader().unwrap();
                assert!(matches!(
                    protocol::expect::<crate::report::Report>(&mut reader).unwrap(),
                    crate::report::Report::Done
                ));
                drop(control);
            },
        );
    }

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
                let up = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
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
                let up = || match asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
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
