//! How a stream, or a control connection's channel, reaches its server:
//! through a Unix socket the server listens on, in one of three ways.
//!
//! - **Straight.** It is a connection to the socket itself: over SSH, a
//!   channel SSH forwards to it, which no shell or other process on the
//!   host can write to. It starts with [`STRAIGHT`], and the server answers
//!   with the start mark.
//! - **By joining.** Where SSH may not forward to a socket, `ferry server
//!   --join <token>`, run at its far end, hands its standard input and
//!   output to the server through the socket; the server reads and writes
//!   them from then on. The joining process stays until the server lets go
//!   of the socket, because its session ends with it: over SSH, a session's
//!   input stops once its process exits.
//! - **Handed over.** On the server's own machine, the client hands it one
//!   end of a socket pair through the socket, as a joining process hands
//!   over its input and output.
//!
//! Whichever way, what the server reads first is a header that says it is
//! the stream. The monitor's socket is reached the same three ways, but for
//! a header: what comes first on it says what the connection is for.
//!
//! The socket is in a directory only this user may open, and is named by a
//! random token; the server tells the client both over its control
//! connection. Where SSH forwards to no socket, but to a port, a server
//! listens on a port on loopback instead, and the monitor on both, until
//! the first connection says which one the client reaches; see
//! [`Listening`]. A port is anyone's on the host to connect to, so each
//! connection to one gives a secret first, which the client got as it got
//! the socket's path, and is let go of without it.
//!
//! A server started through `sudo`, for `--remote-sudo`, runs as root, and
//! what joins it does so as the user who ran `sudo`, who need not be root to
//! hand over a stream. Its socket is then in a directory of root's that others may
//! pass through but not list or write to, and is that user's, and theirs
//! alone; nothing is made in a directory the user could change under it.

use std::fs::{self, DirBuilder, File};
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

use super::scope::{Scope, Waited};
use super::wire::Wire;
use crate::home;
use crate::interrupt;
use crate::profile::State;
use crate::transport::ssh::private_directory;
use rustix::event::PollFlags;

/// The length of a token, in hexadecimal digits.
const TOKEN_LENGTH: usize = 32;

/// The first byte of a stream, or a channel, that reaches the socket
/// straight. A joining
/// process sends 0, with its input and output.
pub const STRAIGHT: u8 = 1;

/// The writing half of a stream that is a socket. Dropped, it shuts the socket
/// for writing, so that the far end reads to its end, as it would a pipe's
/// whose writer closed; the reading half alone keeps the socket open.
pub struct Writing(pub UnixStream);

impl Write for Writing {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Write);
    }
}

/// What a connection to a server's socket is, as its header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One of the streams between the two servers, by its number, from 0,
    /// the first, which carries all but the data's chunks.
    Stream(u8),
}

/// The most streams a transfer has.
pub const STREAMS: u8 = 8;

/// Writes the header that starts a stream: which it is, and the level its
/// pipes are compressed at, 0 for none.
pub fn write_header(writer: &mut impl Write, kind: Kind, compress: Option<i32>) -> io::Result<()> {
    let Kind::Stream(byte) = kind;
    let level = compress.map_or(0, |level| level.clamp(1, 22) as u8);
    writer.write_all(&[byte, level])?;
    writer.flush()
}

/// Reads the header that starts a stream: which it is, and the level its
/// pipes are compressed at, if they are.
pub fn read_header(reader: &mut impl Read) -> Result<(Kind, Option<i32>)> {
    let mut header = [0u8; 2];
    reader
        .read_exact(&mut header)
        .context("read what a stream is")?;
    let kind = match header[0] {
        index if index < STREAMS => Kind::Stream(index),
        other => bail!("a stream says it is of kind {other}"),
    };
    let compress = match header[1] {
        0 => None,
        level @ 1..=22 => Some(i32::from(level)),
        other => bail!("a stream says it is compressed at level {other}"),
    };
    Ok((kind, compress))
}

/// A stream or a channel that has reached the server, before it is a wire.
struct Joined {
    /// What to read from.
    input: OwnedFd,
    /// What to write to: the same socket, or another.
    output: OwnedFd,
    /// The socket it was handed over through, to keep while its input and
    /// output are in use, since a joining process waits for it to close;
    /// none for one that came straight.
    held: Option<UnixStream>,
}

impl Joined {
    /// The wire it is, within `scope`.
    fn wire(self, scope: &Scope) -> Result<Wire> {
        Wire::new(
            File::from(self.input),
            File::from(self.output),
            self.held,
            scope,
        )
        .context("take a stream")
    }
}

/// Where the server waits for what joins it.
pub struct Joining {
    listener: UnixListener,
    path: PathBuf,
    /// Removes the socket when dropped, and should the server be stopped or
    /// signalled before.
    _removing: interrupt::RemoveOnExit,
}

/// The directory of the sockets, private to this user: on Linux, in
/// `$XDG_RUNTIME_DIR`, where it is set, as this user's alone, and emptied
/// when they log out; otherwise in [`home::runtime_dir`].
fn sockets() -> Result<PathBuf> {
    let runtime = match xdg_runtime() {
        Some(runtime) => {
            let runtime = runtime.join(home::runtime_prefix());
            private_directory(&runtime)?;
            runtime
        }
        None => home::runtime_dir()?,
    };
    let sockets = runtime.join("join");
    private_directory(&sockets)?;
    Ok(sockets)
}

/// `$XDG_RUNTIME_DIR`, on Linux, where it is a directory of this user's
/// alone.
fn xdg_runtime() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let metadata = fs::symlink_metadata(&runtime).ok()?;
    (runtime.is_absolute()
        && metadata.is_dir()
        && metadata.uid() == rustix::process::getuid().as_raw()
        && metadata.mode() & 0o077 == 0)
        .then_some(runtime)
}

/// The user who ran `sudo` to start this process, as `sudo` says.
fn sudo_user() -> Result<u32> {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|uid| uid.parse().ok())
        .context("a server for --remote-sudo must be started through sudo, which sets SUDO_UID")
}

/// The directory of a server's sockets started through `sudo` by `uid`:
/// this process's, which others may pass through, but not list or write to.
/// Where another user made one of its name first, one of a few spare names,
/// as [`home::first_private`] takes.
fn sudo_sockets(uid: u32) -> Result<PathBuf> {
    let name = home::sudo_runtime_name(uid);
    let mut refused = None;
    for spare in 0..10 {
        let path = match spare {
            0 => PathBuf::from("/tmp").join(&name),
            _ => PathBuf::from("/tmp").join(format!("{name}-{spare}")),
        };
        match sudo_directory(&path) {
            Ok(()) => return Ok(path),
            Err(error) => {
                refused.get_or_insert(error);
            }
        }
    }
    Err(refused.expect("tried"))
}

/// Makes `path` a directory of this process's that others may pass through
/// but not list or write to, or refuses one that is not.
fn sudo_directory(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o711).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        bail!(
            "{} must be a directory of this user's that others cannot write to",
            path.display()
        );
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o711))?;
    Ok(())
}

/// Opens a socket for what joins this server, under a new token. With
/// `sudo`, the server was started through `sudo`, and what joins it does so
/// as the user who ran it.
pub fn listen(sudo: bool) -> Result<Joining> {
    let token = token(TOKEN_LENGTH)?;
    let path = if sudo {
        sudo_sockets(sudo_user()?)?
    } else {
        sockets()?
    }
    .join(&token);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("listen on {}", path.display()))?;
    let removing = interrupt::remove_on_exit(&path);
    if sudo {
        give(&path, sudo_user()?)?;
    }
    Ok(Joining {
        listener,
        path,
        _removing: removing,
    })
}

impl Joining {
    /// The socket's path, which a path reaches straight.
    pub fn socket(&self) -> &Path {
        &self.path
    }

    /// Waits for a stream or a channel to come, straight, by joining, or
    /// handed over, within `scope`: the wire it is, within it too. One on
    /// which nothing comes for as long as a stream may be silent fails with
    /// [`Silent`], so that it holds up no other.
    pub fn accept(&self, scope: &Scope) -> Result<Wire> {
        self.joined(scope)?.wire(scope)
    }

    /// As [`Joining::accept`], before it is a wire.
    fn joined(&self, scope: &Scope) -> Result<Joined> {
        let (stream, _) = self.listener.accept().context("wait for a stream")?;
        let silence = crate::monitor::Timing::wanted().silence;
        let deadline = std::time::Instant::now() + silence;
        match scope
            .wait(
                &stream,
                PollFlags::IN,
                Some(deadline),
                State::ReadConnection,
            )
            .context("wait for a stream")?
        {
            Waited::Ready => {}
            Waited::TimedOut => return Err(Silent(silence).into()),
        }
        let (read, byte, received) = receive(&stream)?;
        if read == 1 && byte[0] == STRAIGHT && received.is_empty() {
            super::start::write(&mut &stream).context("answer a stream")?;
            let input = stream.try_clone().context("answer a stream")?;
            return Ok(Joined {
                input: OwnedFd::from(input),
                output: OwnedFd::from(stream),
                held: None,
            });
        }
        let [input, output]: [OwnedFd; 2] = received
            .try_into()
            .map_err(|_| anyhow::anyhow!("a stream joined without its input and output"))?;
        // Says it took them, as whoever handed them over holds them until
        // then: see `hand`. One gone already only misses it.
        let _ = (&stream).write_all(&[0]);
        Ok(Joined {
            input,
            output,
            held: Some(stream),
        })
    }
}

/// Nothing came on a connection for this long: it is let go.
#[derive(Debug)]
pub struct Silent(pub std::time::Duration);

impl std::fmt::Display for Silent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            formatter,
            "nothing came on a connection for {} s",
            self.0.as_secs()
        )
    }
}

impl std::error::Error for Silent {}

/// Receives a path's first byte from `stream`, and the input and output a
/// joining process hands over with it, if any: closed in any child the
/// server runs, such as `getent`, which would otherwise hold the path open.
fn receive(stream: &UnixStream) -> Result<(usize, [u8; 1], Vec<OwnedFd>)> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut byte = [0u8; 1];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = RecvFlags::empty();
    let read = rustix::net::recvmsg(
        stream,
        &mut [IoSliceMut::new(&mut byte)],
        &mut control,
        flags,
    )
    .context("receive a path")?
    .bytes;
    let mut received: Vec<OwnedFd> = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = message {
            received.extend(fds);
        }
    }
    // Where the receiving cannot say so, as on macOS: before any child
    // runs, since the server starts none until a path has come.
    for fd in &received {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).context("receive a path")?;
    }
    Ok((read, byte, received))
}

/// How a connection came to what listens: by its socket, or its port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Socket,
    Port,
}

/// The length of a port's secret, in hexadecimal digits.
pub const SECRET_LENGTH: usize = 64;

/// How many connections to a port may be waited on for their secret at
/// once: more are let go of, as no client of this run's makes them.
const UNPROVEN: usize = 16;

/// Where a server, or the monitor, waits for what reaches it: a socket, a
/// port on loopback, or both, until [`Listening::keep`] keeps one. A
/// connection to the port must first give the secret: each is waited on for
/// it in a thread of its own, so that one that never gives it holds up
/// nothing, and for as long as a stream may be silent, as one to the socket
/// is for its first byte, so that it holds no place among the few waited on
/// for long; one that gives another is let go of.
pub struct Listening {
    socket: Mutex<Option<Joining>>,
    port: Mutex<Option<TcpListener>>,
    /// The port's number and secret, should it listen on one.
    open_port: Option<(u16, String)>,
    /// The socket's path, should it listen on one.
    path: Option<PathBuf>,
    /// What came by the port and gave the secret, as it is taken.
    proven: (
        crossbeam_channel::Sender<Joined>,
        crossbeam_channel::Receiver<Joined>,
    ),
    /// Written to as one is, to wake the wait.
    woken: (io::PipeReader, io::PipeWriter),
    unproven: Arc<AtomicUsize>,
    /// How long a connection to the port is waited on for its secret.
    proving: Duration,
}

impl Listening {
    /// Listens on a socket, as [`listen`] does with `sudo`, given `socket`,
    /// and on a port on loopback, given `port`.
    pub fn open(sudo: bool, socket: bool, port: bool) -> Result<Self> {
        let joining = socket.then(|| listen(sudo)).transpose()?;
        let path = joining.as_ref().map(|joining| joining.socket().to_owned());
        let (listener, open_port) = if port {
            let listener = TcpListener::bind(("127.0.0.1", 0)).context("listen on a port")?;
            let number = listener.local_addr().context("listen on a port")?.port();
            (Some(listener), Some((number, token(SECRET_LENGTH)?)))
        } else {
            (None, None)
        };
        Ok(Self {
            socket: Mutex::new(joining),
            port: Mutex::new(listener),
            open_port,
            path,
            proven: crossbeam_channel::unbounded(),
            woken: io::pipe().context("listen")?,
            unproven: Arc::new(AtomicUsize::new(0)),
            proving: crate::monitor::Timing::wanted().silence,
        })
    }

    /// The socket's path, should it listen on one.
    pub fn socket(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The port's number and its secret, should it listen on one.
    pub fn port(&self) -> Option<(u16, &str)> {
        self.open_port
            .as_ref()
            .map(|(number, secret)| (*number, secret.as_str()))
    }

    /// Keeps listening only as `via` says: the other closes, a socket
    /// removed with it.
    pub fn keep(&self, via: Via) {
        match via {
            Via::Socket => drop(self.port.lock().expect("not poisoned").take()),
            Via::Port => drop(self.socket.lock().expect("not poisoned").take()),
        }
    }

    /// Waits for a stream or a channel to come: by the socket, straight, by
    /// joining, or handed over; or by the port, its secret given; or until
    /// `scope` ends. What came is a wire within it.
    pub fn accept(&self, scope: &Scope) -> Result<(Via, Wire)> {
        use rustix::event::{PollFd, poll};
        loop {
            scope.check().context("wait for a stream")?;
            if let Ok(joined) = self.proven.1.try_recv() {
                return Ok((Via::Port, joined.wire(scope)?));
            }
            let socket = self.socket.lock().expect("not poisoned");
            let port = self.port.lock().expect("not poisoned");
            if socket.is_none() && port.is_none() {
                bail!("listening nowhere");
            }
            let watched = scope.fd();
            let mut polled = vec![
                PollFd::new(&self.woken.0, PollFlags::IN),
                PollFd::new(&watched, PollFlags::IN),
            ];
            if let Some(joining) = socket.as_ref() {
                polled.push(PollFd::new(&joining.listener, PollFlags::IN));
            }
            if let Some(listener) = port.as_ref() {
                polled.push(PollFd::new(listener, PollFlags::IN));
            }
            match crate::profile::wait(State::Work, || poll(&mut polled, None)) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error).context("wait for a stream"),
            }
            let ready: Vec<bool> = polled.iter().map(|fd| !fd.revents().is_empty()).collect();
            drop(polled);
            if ready[0] {
                let _ = (&self.woken.0).read(&mut [0u8; 64]);
            }
            if ready[1] {
                continue;
            }
            let mut at = 2;
            if let Some(joining) = socket.as_ref() {
                if ready[at] {
                    return Ok((Via::Socket, joining.accept(scope)?));
                }
                at += 1;
            }
            if let (Some(listener), Some((_, secret))) = (port.as_ref(), &self.open_port)
                && ready[at]
                && let Ok((stream, _)) = listener.accept()
            {
                self.prove(stream, secret.clone());
            }
        }
    }

    /// Waits, in a thread of its own, for what came by the port to give the
    /// secret: then answers it with the start mark, and hands it on.
    fn prove(&self, stream: TcpStream, secret: String) {
        if self.unproven.fetch_add(1, Ordering::SeqCst) >= UNPROVEN {
            self.unproven.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let unproven = Arc::clone(&self.unproven);
        let proven = self.proven.0.clone();
        let Ok(woken) = self.woken.1.try_clone() else {
            unproven.fetch_sub(1, Ordering::SeqCst);
            return;
        };
        let proving = self.proving;
        std::thread::spawn(move || {
            let mut given = vec![0u8; secret.len()];
            let right = stream.set_read_timeout(Some(proving)).is_ok()
                && (&stream).read_exact(&mut given).is_ok()
                && given
                    .iter()
                    .zip(secret.as_bytes())
                    .fold(0, |differs, (a, b)| differs | (a ^ b))
                    == 0;
            if right
                && super::start::write(&mut &stream).is_ok()
                && let Ok(input) = stream.try_clone()
            {
                let _ = proven.send(Joined {
                    input: OwnedFd::from(input),
                    output: OwnedFd::from(stream),
                    held: None,
                });
                let _ = (&woken).write_all(&[1]);
            }
            unproven.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// A new token of `length` hexadecimal digits, from the system's randomness.
fn token(length: usize) -> Result<String> {
    let mut random = vec![0u8; length / 2];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("make a token")?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Gives the socket at `path` to the user `uid`, and to no one else.
fn give(path: &Path, uid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), None)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o600)))
        .with_context(|| format!("give {} to user {uid}", path.display()))
}

/// Hands the server waiting at the socket `socket`, on this machine, `end`,
/// one end of a socket pair, to read and write, as a joining process hands
/// over its input and output; and waits, within `scope`, until the server
/// says it took it, the caller holding `end` until then: the server is on
/// this machine, its end seen, as its socket closes once it ends.
pub fn hand(socket: &Path, end: &UnixStream, scope: &Scope) -> Result<()> {
    let stream =
        UnixStream::connect(socket).with_context(|| format!("reach {}", socket.display()))?;
    let fds = [end.as_fd(), end.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(&fds));
    rustix::net::sendmsg(
        &stream,
        &[IoSlice::new(&[0])],
        &mut control,
        SendFlags::empty(),
    )
    .with_context(|| format!("hand a stream to {}", socket.display()))?;
    // `end` is held until the server says it took it: on macOS, a socket in
    // flight to a connection not accepted yet, which no process holds, is
    // emptied as garbage, and the server would read nothing from it.
    scope
        .wait(&stream, PollFlags::IN, None, State::WriteConnection)
        .with_context(|| format!("wait for {} to take the stream", socket.display()))?;
    match (&stream).read(&mut [0u8; 1]) {
        Ok(1) => Ok(()),
        Ok(_) => bail!("{} ended before it took the stream", socket.display()),
        Err(error) => {
            Err(error).with_context(|| format!("wait for {} to take the stream", socket.display()))
        }
    }
}

/// Joins the server waiting at the socket `socket`, handing it this
/// process's standard input and output, and waits until the server is done
/// with them. The socket must be named by a token, in a directory no one
/// else may write to: this user's alone, or, with `sudo`, for a server
/// started through `sudo` by this user, root's, with the socket this user's.
pub fn join(socket: &str, sudo: bool) -> Result<()> {
    let path = Path::new(socket);
    let token = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("not a socket named by a token: {socket}");
    }
    let directory = path.parent().filter(|parent| parent.is_absolute());
    let directory = directory.with_context(|| format!("not an absolute path: {socket}"))?;
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("inspect {}", directory.display()))?;
    let me = rustix::process::getuid().as_raw();
    let private = if sudo {
        let socket = fs::symlink_metadata(path).with_context(|| format!("inspect {socket}"))?;
        (metadata.uid() == 0 || metadata.uid() == me)
            && metadata.mode() & 0o022 == 0
            && socket.uid() == me
    } else {
        metadata.uid() == me && metadata.mode() & 0o077 == 0
    };
    if !metadata.is_dir() || !private {
        bail!(
            "{} is not a directory that only its owner may write to",
            directory.display()
        );
    }
    let stream = UnixStream::connect(path).with_context(|| format!("join {}", path.display()))?;
    // Before the server writes anything on the path, so that the client
    // knows where it starts.
    super::start::write(&mut io::stdout().lock()).context("write the start mark")?;
    let input = io::stdin();
    let output = io::stdout();
    let fds = [input.as_fd(), output.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(&fds));
    rustix::net::sendmsg(
        &stream,
        &[IoSlice::new(&[0])],
        &mut control,
        SendFlags::empty(),
    )
    .context("hand over the path")?;
    // The server says it took them, then nothing more: reading ends once it
    // closes its end.
    while let Ok(1..) = (&stream).read(&mut [0u8; 1]) {}
    Ok(())
}

/// For tests, which have no `ssh`: carries this process's standard input to
/// the socket at `socket`, or the port `host:port` names, and what comes
/// from it to its standard output,
/// as `ssh -W` does, until the far end is done. A debug build's only. It
/// reads and writes, as `ssh` does: `io::copy` would splice from the socket
/// into the pipe, holding the pipe while it waits for more, so that a reader
/// told the pipe has what came first could not read it. While the file that
/// `FERRY_TEST_RELAY_STALL` names is there, it carries nothing more either
/// way, and closes nothing, as a connection that dropped without a word, or
/// one whose heartbeat waits behind the data; or, with
/// `FERRY_TEST_RELAY_STALL_WAY=down`, nothing more from the socket, while
/// what goes to it still goes; with `up`, the other way round. Given
/// `FERRY_TEST_RELAY_PACE`, it carries at most that many bytes a second each
/// way, as a slow connection does.
#[cfg(debug_assertions)]
pub fn relay(socket: &str) -> Result<()> {
    fn carry(mut from: impl Read, mut to: impl Write, down: bool) -> io::Result<()> {
        let way = std::env::var("FERRY_TEST_RELAY_STALL_WAY");
        let stall = std::env::var_os("FERRY_TEST_RELAY_STALL").filter(|_| match way.as_deref() {
            Ok("down") => down,
            Ok("up") => !down,
            _ => true,
        });
        let pace: Option<u64> = std::env::var("FERRY_TEST_RELAY_PACE")
            .ok()
            .and_then(|pace| pace.parse().ok());
        // While stalled, the end is held too, as what came: a connection
        // that dropped without a word closes nothing.
        let hold = || {
            while stall
                .as_ref()
                .is_some_and(|stall| Path::new(stall).exists())
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let started = std::time::Instant::now();
        let mut carried = 0u64;
        let mut buffer = vec![0u8; if pace.is_some() { 4 << 10 } else { 64 << 10 }];
        loop {
            let read = match from.read(&mut buffer) {
                Ok(0) => {
                    hold();
                    return Ok(());
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    hold();
                    return Err(error);
                }
            };
            hold();
            to.write_all(&buffer[..read])?;
            carried += read as u64;
            if let Some(pace) = pace {
                let due = std::time::Duration::from_secs_f64(carried as f64 / pace as f64);
                if let Some(early) = due.checked_sub(started.elapsed()) {
                    std::thread::sleep(early);
                }
            }
        }
    }
    // Unbuffered, as `ssh` writes: Rust's own standard output holds what
    // has no newline.
    let output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let input = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    // A host and a port, as `ssh -W` takes them, or a socket's path.
    if !socket.contains('/')
        && let Some((host, port)) = socket.rsplit_once(':')
    {
        let port: u16 = port
            .parse()
            .with_context(|| format!("not a port: {socket}"))?;
        let stream = TcpStream::connect((host, port)).with_context(|| format!("reach {socket}"))?;
        let sending = stream.try_clone()?;
        std::thread::spawn(move || {
            let _ = carry(input, &sending, false);
            let _ = sending.shutdown(std::net::Shutdown::Write);
        });
        carry(&stream, output, true)?;
        return Ok(());
    }
    let stream = UnixStream::connect(socket).with_context(|| format!("reach {socket}"))?;
    let sending = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = carry(input, &sending, false);
        let _ = sending.shutdown(std::net::Shutdown::Write);
    });
    carry(&stream, output, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_must_be_one() {
        assert!(join("../../etc/passwd", false).is_err());
        assert!(join("abc", true).is_err());
        assert!(join("/tmp/x/../../etc/passwd", false).is_err());
    }

    /// A socket in a directory others may write to, or named by no token,
    /// is not joined.
    #[test]
    fn a_socket_in_a_shared_directory_is_not_joined() {
        let temp = tempfile::tempdir().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let socket = temp.path().join(token);
        let error = join(socket.to_str().unwrap(), false).unwrap_err();
        assert!(error.to_string().contains("only its owner"), "{error}");
        let error = join(temp.path().join("abc").to_str().unwrap(), false).unwrap_err();
        assert!(error.to_string().contains("token"), "{error}");
    }

    /// An end handed over is held until the server says it took it: on
    /// macOS, one left in flight to a connection not accepted yet would be
    /// emptied.
    #[test]
    fn a_hand_waits_until_the_server_takes_the_end() {
        let joining = listen(false).unwrap();
        let socket = joining.socket().to_owned();
        let (end, mut other) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        let taking = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let joined = joining
                .accept(&crate::transport::scope::Scope::new().unwrap())
                .unwrap();
            (joining, joined)
        });
        hand(
            &socket,
            &end,
            &crate::transport::scope::Scope::new().unwrap(),
        )
        .unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(300));
        drop(end);
        let (_joining, mut joined) = taking.join().unwrap();
        joined.write_all(b"taken").unwrap();
        let mut taken = [0u8; 5];
        other.read_exact(&mut taken).unwrap();
        assert_eq!(&taken, b"taken");
    }

    /// A server that takes its end, and is done with the connection at
    /// once, as one that fails at once is, took it all the same.
    #[test]
    fn a_hand_to_a_server_done_at_once_was_taken() {
        let joining = listen(false).unwrap();
        let socket = joining.socket().to_owned();
        let (end, _other) = UnixStream::pair().unwrap();
        let taking = std::thread::spawn(move || {
            drop(
                joining
                    .accept(&crate::transport::scope::Scope::new().unwrap())
                    .unwrap(),
            );
            joining
        });
        hand(
            &socket,
            &end,
            &crate::transport::scope::Scope::new().unwrap(),
        )
        .unwrap();
        drop(taking.join().unwrap());
    }

    /// A server gone before it takes an end handed over is said so.
    #[test]
    fn a_hand_to_a_server_gone_fails() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let (end, _other) = UnixStream::pair().unwrap();
        let going = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(listener);
        });
        let error = format!(
            "{:#}",
            hand(
                &socket,
                &end,
                &crate::transport::scope::Scope::new().unwrap()
            )
            .unwrap_err()
        );
        going.join().unwrap();
        // As the connection ends, on macOS, or is reset, on Linux.
        assert!(
            error.contains(&*socket.to_string_lossy()) && error.contains("the stream"),
            "{error}"
        );
    }

    /// What a joining process hands over is closed in any child the server
    /// runs.
    #[test]
    fn a_path_handed_over_is_closed_in_children() {
        let (server, joining) = UnixStream::pair().unwrap();
        let (input, output) = UnixStream::pair().unwrap();
        let fds = [input.as_fd(), output.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        control.push(SendAncillaryMessage::ScmRights(&fds));
        rustix::net::sendmsg(
            &joining,
            &[IoSlice::new(&[0])],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let (read, byte, received) = receive(&server).unwrap();
        assert_eq!((read, byte), (1, [0]));
        assert_eq!(received.len(), 2);
        for fd in &received {
            let flags = rustix::io::fcntl_getfd(fd).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        }
    }

    /// The socket listened on is removed once the server lets go of it.
    #[test]
    fn the_socket_is_removed_once_let_go() {
        let joining = listen(false).unwrap();
        let path = joining.socket().to_owned();
        assert!(path.exists());
        drop(joining);
        assert!(!path.exists());
    }

    /// A connection to a port that does not give the secret is let go of,
    /// and taken for nothing; one that gives it is answered with the start
    /// mark, and taken.
    #[test]
    fn a_connection_to_a_port_without_its_secret_is_let_go() {
        crate::test_support::within(
            "a connection to a port without its secret is let go",
            crate::test_support::PATIENCE,
            move || {
                let listening = Arc::new(Listening::open(false, false, true).unwrap());
                let (number, secret) = listening.port().unwrap();
                let secret = secret.to_owned();
                assert_eq!(secret.len(), SECRET_LENGTH);
                let accepting = Arc::clone(&listening);
                let accepted = std::thread::spawn(move || {
                    accepting
                        .accept(&crate::transport::scope::Scope::new().unwrap())
                        .unwrap()
                });
                let mut wrong = TcpStream::connect(("127.0.0.1", number)).unwrap();
                wrong.write_all(&[b'0'; SECRET_LENGTH]).unwrap();
                let mut rest = Vec::new();
                assert_eq!(wrong.read_to_end(&mut rest).unwrap(), 0, "{rest:?}");
                let mut right = TcpStream::connect(("127.0.0.1", number)).unwrap();
                right.write_all(secret.as_bytes()).unwrap();
                let mut mark = [0u8; super::super::start::MARK.len()];
                right.read_exact(&mut mark).unwrap();
                assert_eq!(&mark, super::super::start::MARK);
                let (via, mut joined) = accepted.join().unwrap();
                assert_eq!(via, Via::Port);
                joined.write_all(b"pong").unwrap();
                let mut pong = [0u8; 4];
                right.read_exact(&mut pong).unwrap();
                assert_eq!(&pong, b"pong");
            },
        );
    }

    /// A connection to a port that gives nothing is let go of once it has
    /// been waited on for as long as a stream may be silent: here, shorter.
    #[test]
    fn a_connection_to_a_port_that_gives_nothing_is_let_go_in_time() {
        crate::test_support::within(
            "a connection to a port that gives nothing is let go in time",
            crate::test_support::PATIENCE,
            move || {
                let mut listening = Listening::open(false, false, true).unwrap();
                listening.proving = Duration::from_millis(200);
                let listening = Arc::new(listening);
                let (number, _) = listening.port().unwrap();
                let accepting = Arc::clone(&listening);
                // Taken by nothing: it ends with the test.
                std::thread::spawn(move || {
                    let _ = accepting.accept(&crate::transport::scope::Scope::new().unwrap());
                });
                let mut silent = TcpStream::connect(("127.0.0.1", number)).unwrap();
                let started = std::time::Instant::now();
                let mut rest = Vec::new();
                assert_eq!(silent.read_to_end(&mut rest).unwrap(), 0, "{rest:?}");
                assert!(started.elapsed() >= Duration::from_millis(150));
            },
        );
    }

    /// A wait for what reaches the listener stops once its scope ends,
    /// saying why.
    #[test]
    fn a_wait_for_a_connection_stops_as_its_scope_ends() {
        crate::test_support::within(
            "a wait for a connection stops as its scope ends",
            crate::test_support::PATIENCE,
            move || {
                let listening = Listening::open(false, true, true).unwrap();
                let scope = crate::transport::scope::Scope::new().unwrap();
                let ender = scope.clone();
                let ending = std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    ender.end("the host was lost");
                });
                let error = listening.accept(&scope).err().expect("ended");
                ending.join().unwrap();
                assert!(
                    format!("{error:#}").contains("the host was lost"),
                    "{error:#}"
                );
            },
        );
    }

    /// Once a way is kept, the other closes: the port refuses connections,
    /// or the socket is gone.
    #[test]
    fn the_way_not_kept_closes() {
        crate::test_support::within(
            "the way not kept closes",
            crate::test_support::PATIENCE,
            move || {
                let listening = Listening::open(false, true, true).unwrap();
                let (number, _) = listening.port().unwrap();
                let path = listening.socket().unwrap().to_owned();
                listening.keep(Via::Socket);
                // A process another test starts holds every descriptor of
                // this one's for a moment, until it runs its program: the
                // port is refused once that has passed.
                let refused = (0..100).any(|_| {
                    TcpStream::connect(("127.0.0.1", number)).is_err() || {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        false
                    }
                });
                assert!(refused);
                assert!(path.exists());

                let listening = Listening::open(false, true, true).unwrap();
                let (number, _) = listening.port().unwrap();
                let path = listening.socket().unwrap().to_owned();
                listening.keep(Via::Port);
                assert!(!path.exists());
                assert!(TcpStream::connect(("127.0.0.1", number)).is_ok());
            },
        );
    }
}
