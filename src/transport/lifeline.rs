//! A worker's lifeline: its standard error, which its monitor gives it as
//! one end of a socket pair, holding the other. The monitor reads what the
//! worker says there, and passes it on, but writes nothing; so the worker,
//! waiting for it to be readable, wakes only once the monitor's end closes,
//! however the monitor ended, killed or crashed. With no timer and no
//! network, the worker then stops, rather than staying until `sshd` finds
//! its connection gone, should the network have dropped too. See the
//! [draft](../../../docs/drafts/silent-drops.md).
//!
//! Standard error, because `sudo` keeps it, as it does not a descriptor
//! above 2; a socket pair, not a pipe, because a pipe's writing end has no
//! quiet wait for its reader closing on macOS. Where `sudo` logs what the
//! worker says, its own pipe stands between: the worker sees its standard
//! error is no socket, and says the lifeline cannot be relied on.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::Stdio;

use rustix::event::{PollFd, PollFlags};
use rustix::fs::FileType;

/// The monitor's end of a worker's lifeline.
pub struct Lifeline(UnixStream);

/// A new lifeline: the monitor's end, and what the worker's standard error
/// is to be. Neither end goes to any other process the monitor starts.
pub fn pair() -> io::Result<(Lifeline, Stdio)> {
    let (held, given) = UnixStream::pair()?;
    Ok((Lifeline(held), Stdio::from(OwnedFd::from(given))))
}

impl Lifeline {
    /// Reads what the worker says, handing each part to `said`, until the
    /// worker, and whatever it handed its standard error to, closes it; all
    /// of it, so that the worker never waits for room to say more. The
    /// monitor's end stays open meanwhile: closing it is the lifeline
    /// breaking.
    pub fn read(self, mut said: impl FnMut(&[u8])) {
        let mut buffer = [0u8; 8192];
        loop {
            match (&self.0).read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => said(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }
}

/// What a worker's standard error is, as its lifeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Own {
    /// A socket: the monitor's end of it is watched.
    Watched,
    /// Not a socket, as where `sudo` logs what the worker says, between it
    /// and its monitor: the monitor ending cannot be seen.
    Unreliable,
}

/// In a worker given a lifeline: watches it, and calls `gone` once the
/// monitor's end closes, in a thread of its own; or says it cannot be
/// watched.
pub fn watch(gone: fn()) -> io::Result<Own> {
    let stderr = io::stderr();
    let fd = stderr.as_fd();
    if FileType::from_raw_mode(rustix::fs::fstat(fd)?.st_mode) != FileType::Socket {
        return Ok(Own::Unreliable);
    }
    let watched = File::from(fd.try_clone_to_owned()?);
    std::thread::Builder::new()
        .name("lifeline".to_owned())
        .spawn(move || {
            loop {
                let mut polled = [PollFd::new(&watched, PollFlags::IN)];
                match rustix::event::poll(&mut polled, None) {
                    Ok(_) if !polled[0].revents().is_empty() => break,
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => break,
                }
            }
            gone();
        })?;
    Ok(Own::Watched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use crate::transport::child::Child;

    /// A worker whose standard error is its lifeline: `script` run by `sh`.
    fn worker(script: &str, stderr: Stdio) -> Child {
        Child::spawn(
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(stderr),
            None,
        )
        .unwrap()
    }

    #[test]
    fn what_a_worker_says_on_its_lifeline_is_passed_on() {
        let (lifeline, stderr) = pair().unwrap();
        let child = worker("echo said >&2; echo again >&2", stderr);
        let mut said = Vec::new();
        lifeline.read(|part| said.extend_from_slice(part));
        assert_eq!(said, b"said\nagain\n");
        assert!(child.wait().success());
    }

    /// The worker waits on its lifeline, which never becomes readable while
    /// the monitor's end is open, and breaks as soon as it closes.
    #[test]
    fn a_lifeline_breaks_as_the_monitor_s_end_closes() {
        let (lifeline, stderr) = pair().unwrap();
        let mut child = worker("read -r _ <&2; echo broke", stderr);
        assert!(
            child
                .wait_until(Instant::now() + Duration::from_millis(300))
                .is_none(),
            "the worker went on while its lifeline held"
        );
        drop(lifeline);
        assert!(
            child
                .wait_until(Instant::now() + Duration::from_secs(10))
                .is_some(),
            "the worker did not see its lifeline break"
        );
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "broke\n");
    }

    /// Where standard error is no socket, as a test's own, the lifeline
    /// cannot be watched, and says so.
    #[test]
    fn a_standard_error_that_is_no_socket_is_no_lifeline() {
        let socket = FileType::from_raw_mode(rustix::fs::fstat(io::stderr()).unwrap().st_mode)
            == FileType::Socket;
        if socket {
            eprintln!("this test's standard error is a socket: not tested");
            return;
        }
        assert_eq!(watch(|| {}).unwrap(), Own::Unreliable);
    }
}
