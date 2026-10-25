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
//! the stream. The launcher's socket is reached the same three ways, but for
//! a header: what comes first on it says what the connection is for.
//!
//! The socket is in a directory only this user may open, and is named by a
//! random token; the server tells the client both over its control
//! connection. No port is opened.
//!
//! A server started through `sudo`, for `--remote-sudo`, runs as root, and
//! what joins it does so as the user who ran `sudo`, who need not be root to
//! hand over a stream. Its socket is then in a directory of root's that others may
//! pass through but not list or write to, and is that user's, and theirs
//! alone; nothing is made in a directory the user could change under it.

use std::fs::{self, DirBuilder, File};
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

use crate::home;
use crate::interrupt;
use crate::transport::ssh::private_directory;

/// The length of a token, in hexadecimal digits.
const TOKEN_LENGTH: usize = 32;

/// The first byte of a stream, or a channel, that reaches the socket
/// straight. A joining
/// process sends 0, with its input and output.
pub const STRAIGHT: u8 = 1;

/// How long the client waits for a server on this machine to take the end
/// of the stream it hands over: as long as the stream may be silent.
const TAKING: std::time::Duration = super::stream::SILENCE;

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
    /// The stream between the two servers.
    Stream,
}

/// Writes the header that starts a stream.
pub fn write_header(writer: &mut impl Write, kind: Kind) -> io::Result<()> {
    let byte = match kind {
        Kind::Stream => 0,
    };
    writer.write_all(&[byte])?;
    writer.flush()
}

/// Reads the header that starts a stream.
pub fn read_header(reader: &mut impl Read) -> Result<Kind> {
    let mut header = [0u8; 1];
    reader
        .read_exact(&mut header)
        .context("read what a stream is")?;
    Ok(match header[0] {
        0 => Kind::Stream,
        other => bail!("a stream says it is of kind {other}"),
    })
}

/// A stream or a channel that has reached the server.
pub struct Joined {
    /// What to read from.
    pub input: OwnedFd,
    /// What to write to: the same socket, or another.
    pub output: OwnedFd,
    /// The socket it was handed over through, to keep while its input and
    /// output are in use, since a joining process waits for it to close;
    /// none for one that came straight.
    pub held: Option<UnixStream>,
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
    let mut random = [0u8; TOKEN_LENGTH / 2];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("make a token")?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
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
    /// handed over.
    pub fn accept(&self) -> Result<Joined> {
        let (stream, _) = self.listener.accept().context("wait for a stream")?;
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

/// Gives the socket at `path` to the user `uid`, and to no one else.
fn give(path: &Path, uid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), None)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o600)))
        .with_context(|| format!("give {} to user {uid}", path.display()))
}

/// Hands the server waiting at the socket `socket`, on this machine, `end`,
/// one end of a socket pair, to read and write, as a joining process hands
/// over its input and output; and waits, for [`TAKING`] at most, until the
/// server says it took it, the caller holding `end` until then.
pub fn hand(socket: &Path, end: &UnixStream) -> Result<()> {
    let stream =
        UnixStream::connect(socket).with_context(|| format!("reach {}", socket.display()))?;
    // Before anything is sent: once the server has its end, it may be done
    // with the connection, as by failing at once, and macOS refuses a
    // timeout on a connection that ended.
    stream
        .set_read_timeout(Some(TAKING))
        .context("wait for the stream to be taken")?;
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
/// the socket at `socket`, and what comes from it to its standard output,
/// as `ssh -W` does, until the far end is done. A debug build's only. It
/// reads and writes, as `ssh` does: `io::copy` would splice from the socket
/// into the pipe, holding the pipe while it waits for more, so that a reader
/// told the pipe has what came first could not read it.
#[cfg(debug_assertions)]
pub fn relay(socket: &str) -> Result<()> {
    fn carry(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
        let mut buffer = vec![0u8; 64 << 10];
        loop {
            let read = match from.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            to.write_all(&buffer[..read])?;
        }
    }
    let stream = UnixStream::connect(socket).with_context(|| format!("reach {socket}"))?;
    let sending = stream.try_clone()?;
    std::thread::spawn(move || {
        let input = File::from(
            io::stdin()
                .as_fd()
                .try_clone_to_owned()
                .expect("standard input"),
        );
        let _ = carry(input, &sending);
        let _ = sending.shutdown(std::net::Shutdown::Write);
    });
    // Unbuffered, as `ssh` writes: Rust's own standard output holds what
    // has no newline.
    let output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    carry(&stream, output)?;
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
            let joined = joining.accept().unwrap();
            (joining, joined)
        });
        hand(&socket, &end).unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(300));
        drop(end);
        let (_joining, joined) = taking.join().unwrap();
        File::from(joined.output).write_all(b"taken").unwrap();
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
            drop(joining.accept().unwrap());
            joining
        });
        hand(&socket, &end).unwrap();
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
        let error = format!("{:#}", hand(&socket, &end).unwrap_err());
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
}
