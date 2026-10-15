//! How a path joins its server: `ferry server --join <token>`, run at the far
//! end of a path, hands its standard input and output to the server through a
//! Unix socket; the server reads and writes them from then on. The joining
//! process stays until the server lets go of the socket, because its session
//! ends with it: over SSH, a session's input stops once its process exits.
//!
//! The socket is in a directory only this user may open, and is named by a
//! random token, which the server tells the client over its control
//! connection. No port is opened.

use std::fs::{self, File};
use std::io::{self, IoSlice, IoSliceMut, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

use crate::home;
use crate::transport::ssh::private_directory;

/// The length of a token, in hexadecimal digits.
const TOKEN_LENGTH: usize = 32;

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

/// Opens a socket for this server's paths to join, under a new token.
pub fn listen() -> Result<Joining> {
    let mut random = [0u8; TOKEN_LENGTH / 2];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("make a token")?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = sockets()?.join(&token);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("listen on {}", path.display()))?;
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

    /// Waits for a path to join, and takes its input and output: what to read
    /// from, and what to write to; and the socket, to keep while they are in
    /// use, since the joining process waits for it to close.
    pub fn accept(&self) -> Result<(File, File, UnixStream)> {
        let (stream, _) = self.listener.accept().context("wait for a path")?;
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut byte = [0u8; 1];
        rustix::net::recvmsg(
            &stream,
            &mut [IoSliceMut::new(&mut byte)],
            &mut control,
            RecvFlags::empty(),
        )
        .context("receive a path")?;
        let mut received: Vec<OwnedFd> = Vec::new();
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = message {
                received.extend(fds);
            }
        }
        let [input, output]: [OwnedFd; 2] = received
            .try_into()
            .map_err(|_| anyhow::anyhow!("a path joined without its input and output"))?;
        Ok((File::from(input), File::from(output), stream))
    }
}

impl Drop for Joining {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Joins the server waiting under `token`, handing it this process's
/// standard input and output, and waits until the server is done with them.
pub fn join(token: &str) -> Result<()> {
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("not a token: {token}");
    }
    let path = sockets()?.join(token);
    let stream = UnixStream::connect(&path).with_context(|| format!("join {}", path.display()))?;
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
        assert!(join("../../etc/passwd").is_err());
        assert!(join("abc").is_err());
    }
}
