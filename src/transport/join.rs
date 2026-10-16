//! How a path reaches its server: through a Unix socket the server listens
//! on, in one of two ways.
//!
//! - **Straight.** The path is a connection to the socket itself: over SSH,
//!   a channel SSH forwards to it, which no shell or other process on the
//!   host can write to. It starts with [`STRAIGHT`], and the server answers
//!   with the start mark, so that the client knows it got through.
//! - **By joining.** Where SSH may not forward to a socket, `ferry server
//!   --join <token>`, run at the far end of a path, hands its standard input
//!   and output to the server through the socket; the server reads and
//!   writes them from then on. The joining process stays until the server
//!   lets go of the socket, because its session ends with it: over SSH, a
//!   session's input stops once its process exits.
//!
//! The socket is in a directory only this user may open, and is named by a
//! random token; the server tells the client both over its control
//! connection. No port is opened.
//!
//! A server started through `sudo`, for `--remote-sudo`, runs as root, and
//! its paths join as the user who ran `sudo`, who need not be root to hand
//! over a path. Its socket is then in a directory of root's that others may
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
use crate::transport::ssh::private_directory;

/// The length of a token, in hexadecimal digits.
const TOKEN_LENGTH: usize = 32;

/// The first byte of a path that reaches the socket straight. A joining
/// process sends 0, with its input and output.
pub const STRAIGHT: u8 = 1;

/// The writing half of a path that is a socket. Dropped, it shuts the socket
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

/// A path that has reached the server.
pub struct Joined {
    /// What to read from.
    pub input: Box<dyn Read + Send>,
    /// What to write to.
    pub output: Box<dyn Write + Send>,
    /// The socket of a joining process, to keep while its input and output
    /// are in use, since the process waits for it to close; none for a path
    /// that came straight.
    pub held: Option<UnixStream>,
}

/// Where the server waits for its paths.
pub struct Joining {
    listener: UnixListener,
    path: PathBuf,
    token: String,
}

/// The directory of the sockets, private to this user.
fn sockets() -> Result<PathBuf> {
    let runtime = home::runtime_dir();
    private_directory(&runtime)?;
    let sockets = runtime.join("join");
    private_directory(&sockets)?;
    Ok(sockets)
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
fn sudo_sockets(uid: u32) -> Result<PathBuf> {
    let path = home::sudo_runtime_dir(uid);
    match DirBuilder::new().mode(0o711).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    let metadata =
        fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        bail!(
            "{} must be a directory of this user's that others cannot write to",
            path.display()
        );
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o711))?;
    Ok(path)
}

/// Opens a socket for this server's paths to join, under a new token. With
/// `sudo`, the server was started through `sudo`, and its paths join as the
/// user who ran it.
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
    if sudo {
        give(&path, sudo_user()?)?;
    }
    Ok(Joining {
        listener,
        path,
        token,
    })
}

impl Joining {
    /// The token a path joins by.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The socket's path, which a path reaches straight.
    pub fn socket(&self) -> &Path {
        &self.path
    }

    /// Waits for a path to come, straight or by joining.
    pub fn accept(&self) -> Result<Joined> {
        let (stream, _) = self.listener.accept().context("wait for a path")?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut byte = [0u8; 1];
        let read = rustix::net::recvmsg(
            &stream,
            &mut [IoSliceMut::new(&mut byte)],
            &mut control,
            RecvFlags::empty(),
        )
        .context("receive a path")?
        .bytes;
        let mut received: Vec<OwnedFd> = Vec::new();
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = message {
                received.extend(fds);
            }
        }
        if read == 1 && byte[0] == STRAIGHT && received.is_empty() {
            super::start::write(&mut &stream).context("answer a path")?;
            let input = stream.try_clone().context("answer a path")?;
            return Ok(Joined {
                input: Box::new(input),
                output: Box::new(Writing(stream)),
                held: None,
            });
        }
        let [input, output]: [OwnedFd; 2] = received
            .try_into()
            .map_err(|_| anyhow::anyhow!("a path joined without its input and output"))?;
        Ok(Joined {
            input: Box::new(File::from(input)),
            output: Box::new(File::from(output)),
            held: Some(stream),
        })
    }
}

impl Drop for Joining {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Gives the socket at `path` to the user `uid`, and to no one else.
fn give(path: &Path, uid: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), None)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o600)))
        .with_context(|| format!("give {} to user {uid}", path.display()))
}

/// Joins the server waiting under `token`, handing it this process's
/// standard input and output, and waits until the server is done with them.
/// With `sudo`, the server was started through `sudo` by this user.
pub fn join(token: &str, sudo: bool) -> Result<()> {
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("not a token: {token}");
    }
    let path = if sudo {
        home::sudo_runtime_dir(rustix::process::getuid().as_raw())
    } else {
        sockets()?
    }
    .join(token);
    let stream = UnixStream::connect(&path).with_context(|| format!("join {}", path.display()))?;
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
    // Nothing comes back: reading ends once the server closes its end.
    let _ = (&stream).read(&mut [0u8; 1]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_must_be_one() {
        assert!(join("../../etc/passwd", false).is_err());
        assert!(join("abc", true).is_err());
    }
}
