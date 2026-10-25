//! The transport: how the client reaches the place of each side, this machine
//! or a host. Nothing else in Ferry runs `ssh`, or knows whether a place is
//! this machine or another; see the
//! [architecture](../../docs/design/architecture.md#transport).
//!
//! Opening a transport is the first thing that reaches a place: for a host, it
//! opens every SSH connection, each only logged in, running nothing. Through
//! it, the client then opens:
//!
//! - **control connections**: each a plain byte stream to one program, such
//!   as a probe, an install, or the server, which lives as long as its
//!   control connection, and whose control connection goes on a channel of
//!   its own once it has started;
//! - the **streams** between a transfer's two servers, one on each SSH
//!   connection, which the client makes and hands to the servers, never
//!   touching what goes on them; see [`streams`]. On them, the servers'
//!   [`session`] carries what the peers send each other.

pub mod driver;
mod frame;
pub mod join;
pub mod session;
pub mod ssh;
pub mod start;
pub mod streams;

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
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

    /// Opens the channel the control connection goes on once the server
    /// that sent `welcome` has started; see [`ControlConnection::attach`].
    /// It reaches the server's socket straight, as a channel SSH forwards to
    /// it over the first SSH connection, which nothing else on the host can
    /// write to; or, given `join`, it runs that instead, through the shell on
    /// the host, to join the server through its socket, for a host whose
    /// `sshd` will not forward to a socket.
    pub fn channel(&self, welcome: &Welcome, join: Option<&str>) -> Result<Channel> {
        let socket = String::from_utf8_lossy(&welcome.socket);
        let mut link = match join {
            None => self.straight(&socket)?,
            Some(join) => self.joining(join)?,
        };
        // A link that did not get through may close before this is written;
        // that is found, and said, as its start mark is waited for.
        let _ = join::write_header(&mut link.writer, join::Kind::Control);
        // The server answers one that came straight with the start mark,
        // and a joining process writes it as it starts, after what the shell
        // printed.
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
                    "{}: SSH did not forward a channel to the server's socket, as where \
                     sshd has AllowStreamLocalForwarding no; --remote-join has the \
                     streams join it through the shell on the host instead",
                    held[0].ssh.host()
                ),
                _ => bail!("{}: a channel ended before it started", self.place()),
            }
        }
        Ok(Channel(link))
    }

    /// How many streams the session has: one on each SSH connection, one on
    /// this machine.
    pub fn streams(&self) -> usize {
        match self {
            Self::Local => 1,
            Self::Ssh(held) => held.len(),
        }
    }

    /// How the client reaches the server that sent `welcome` for the
    /// stream `number`: on this machine, through its socket; on a host, by
    /// a channel over the SSH connection the stream goes on, which reaches
    /// its socket straight, or, given `join`, runs that; see [`streams`].
    pub fn half(&self, welcome: &Welcome, join: Option<&str>, number: u32) -> Half {
        let socket = String::from_utf8_lossy(&welcome.socket).into_owned();
        match self {
            Self::Local => Half::Here(PathBuf::from(socket)),
            Self::Ssh(held) => {
                let ssh = &held[number as usize % held.len()].ssh;
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

    /// A link over the first SSH connection, or on this machine, that
    /// reaches the server's socket straight, its first byte written.
    fn straight(&self, socket: &str) -> Result<Link> {
        let mut link = match self {
            Self::Local => {
                let stream =
                    UnixStream::connect(socket).with_context(|| format!("connect to {socket}"))?;
                Link::socket(stream)?
            }
            Self::Ssh(held) => Link::spawn(held[0].ssh.forward(socket), Stdio::inherit())?,
        };
        let _ = link.writer.write_all(&[STRAIGHT]);
        Ok(link)
    }

    /// A link over the first SSH connection, or on this machine, that runs
    /// `join`.
    fn joining(&self, join: &str) -> Result<Link> {
        let command = match self {
            Self::Local => self.command(join),
            Self::Ssh(held) => held[0].ssh.script(join),
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

/// The control connection's channel, opened; see [`Transport::channel`].
pub struct Channel(Link);

/// A control connection's channel, as the client has it.
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
    /// be welcomed; returns where its streams reach it. A server that exits
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
            Err(error) if error.is::<handshake::Closed>() => match self.child.wait() {
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
    pub fn attach(&mut self, channel: Channel) {
        let Channel(mut channel) = channel;
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

/// For each SSH connection, the bytes its streams carried, and how long ago
/// anything came on them, if anything has.
type Seen = Vec<(u64, Option<Duration>)>;

/// What the dashboard shows of a host's SSH connections: how many bytes the
/// streams on each carried, and how long since anything came on them.
pub struct Monitor {
    host: String,
    seen: Arc<Mutex<Seen>>,
}

impl Monitor {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// For each SSH connection, the bytes carried so far, and how long ago
    /// anything came, if anything has.
    pub fn connections(&self) -> Seen {
        self.seen.lock().expect("not poisoned").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server's socket that answers every channel that comes straight
    /// with the start mark, then hands the test what came first on it, and
    /// the channel; and the welcome that names it.
    fn serving() -> (
        tempfile::TempDir,
        Welcome,
        crossbeam_channel::Receiver<(Vec<u8>, UnixStream)>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (sender, channels) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut first = vec![0u8; 6];
                stream.read_exact(&mut first).unwrap();
                start::write(&mut stream).unwrap();
                if sender.send((first, stream)).is_err() {
                    return;
                }
            }
        });
        let welcome = Welcome {
            socket: socket.into_os_string().into_encoded_bytes(),
            root: crate::protocol::RootIdentity::default(),
        };
        (temp, welcome, channels)
    }

    /// What goes on a channel after its start goes both ways.
    fn carries(channel: Channel, far: &mut (impl Read + Write)) {
        let Channel(mut link) = channel;
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

    /// The control connection's channel comes straight, says that it does
    /// and what it is, and once answered, carries what it is given.
    #[test]
    fn a_channel_comes_straight_says_what_it_is_and_carries() {
        crate::test_support::within(
            "a channel comes straight says what it is and carries",
            crate::test_support::PATIENCE,
            move || {
                let (_temp, welcome, channels) = serving();
                let channel = Transport::Local.channel(&welcome, None).unwrap();
                let (first, mut far) = channels.recv().unwrap();
                assert_eq!(first, [STRAIGHT, 2, 0, 0, 0, 0]);
                carries(channel, &mut far);
            },
        );
    }

    #[test]
    fn a_channel_closed_at_once_is_said_to_have_ended_before_it_started() {
        crate::test_support::within(
            "a channel closed at once is said to have ended before it started",
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
                let welcome = Welcome {
                    socket: socket.into_os_string().into_encoded_bytes(),
                    root: crate::protocol::RootIdentity::default(),
                };
                let error = Transport::Local
                    .channel(&welcome, None)
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("ended before it started"), "{error}");
            },
        );
    }

    /// The channel joins through a shell: the joining process, after what a
    /// shell might print and the start mark, sends back what it is sent, but
    /// for the header, which a server would read.
    #[test]
    fn a_channel_can_join_through_a_shell() {
        crate::test_support::within(
            "a channel can join through a shell",
            crate::test_support::PATIENCE,
            move || {
                let welcome = Welcome {
                    socket: b"/nonexistent/socket".to_vec(),
                    root: crate::protocol::RootIdentity::default(),
                };
                let join = "echo noise; printf '\\000ferry: start\\000'; \
                            dd bs=1 count=5 >/dev/null 2>&1; exec cat";
                let channel = Transport::Local.channel(&welcome, Some(join)).unwrap();
                let Channel(mut link) = channel;
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
    fn a_channel_whose_process_ends_first_is_said_to_have_ended() {
        crate::test_support::within(
            "a channel whose process ends first is said to have ended",
            crate::test_support::PATIENCE,
            move || {
                let welcome = Welcome {
                    socket: b"/nonexistent/socket".to_vec(),
                    root: crate::protocol::RootIdentity::default(),
                };
                let error = Transport::Local
                    .channel(&welcome, Some("exit 1"))
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("ended before it started"), "{error}");
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
