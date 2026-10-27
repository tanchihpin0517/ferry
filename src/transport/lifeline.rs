//! A worker's lifeline: its standard error, which its monitor gives it as
//! one end of a socket pair, holding the other. The monitor reads what the
//! worker says there, and passes it on, but writes nothing; so the worker,
//! waiting for it to be readable, wakes only once the monitor's end closes,
//! however the monitor ended, killed or crashed. With no timer and no
//! network, the worker then stops, rather than staying until `sshd` finds
//! its connection gone, should the network have dropped too. See
//! [Monitors](../../docs/design/architecture.md#monitors).
//!
//! Standard error, because `sudo` keeps it, as it does not a descriptor
//! above 2; a socket pair, not a pipe, because a pipe's writing end has no
//! quiet wait for its reader closing on macOS. Where `sudo` logs what the
//! worker says, its own pipe stands between: the worker sees its standard
//! error is no socket, and says the lifeline cannot be relied on.
//!
//! Among what it says for people, a worker tells its monitor there, every
//! second, how long ago its stream last carried something from its peer, in
//! a marked line, as a first session's are; see [`super::start::frame`].
//! That came over the host's SSH connection from the client's side, so the
//! monitor counts it as word from the client, as it does the heartbeat:
//! one waiting behind the data on a slow connection takes nothing for lost.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::time::Duration;

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
    /// Reads what the worker says, handing what it says for people to
    /// `said`, part by part, and how long ago its stream last carried
    /// something from its peer, each time it tells, to `heard`; until the
    /// worker, and whatever it handed its standard error to, closes it; all
    /// of it, so that the worker never waits for room to say more. The
    /// monitor's end stays open meanwhile: closing it is the lifeline
    /// breaking.
    pub fn read(self, mut said: impl FnMut(&[u8]), mut heard: impl FnMut(Duration)) {
        let mut parted = Parted::default();
        let mut buffer = [0u8; 8192];
        loop {
            match (&self.0).read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => parted.take(&buffer[..read], &mut said, &mut heard),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if !parted.pending.is_empty() {
            said(&parted.pending);
        }
    }
}

/// The tag of the lines a worker says to its monitor on its lifeline: no one
/// else writes there, and nothing for people has a NUL in it, so it needs
/// no run's own.
const TAG: &str = "lifeline";

/// The kind of the line that says how long ago the worker's stream last
/// carried something from its peer, in milliseconds.
const HEARD: &str = "heard";

/// The longest a line for the monitor may be, past its mark: what is longer
/// is taken for what is said for people.
const LONGEST: usize = 64;

/// In a worker given a lifeline: tells its monitor that its stream last
/// carried something from its peer `ago`, in one write.
pub fn tell_heard(ago: Duration) {
    let line = super::start::frame(TAG, HEARD, ago.as_millis().to_string().as_bytes());
    let _ = io::stderr().lock().write_all(&line);
}

/// What a worker says on its lifeline, taken apart as it comes, cut
/// anywhere: what is for people, and the lines for its monitor.
#[derive(Default)]
struct Parted {
    /// What came, and is not handed on yet: what may begin a line for the
    /// monitor, or one not ended yet.
    pending: Vec<u8>,
}

impl Parted {
    fn take(
        &mut self,
        part: &[u8],
        said: &mut impl FnMut(&[u8]),
        heard: &mut impl FnMut(Duration),
    ) {
        let mark = format!("\0ferry:{TAG}:").into_bytes();
        self.pending.extend_from_slice(part);
        loop {
            let Some(at) = self
                .pending
                .windows(mark.len())
                .position(|window| window == mark)
            else {
                // All but what may yet begin the mark is for people.
                let keep = (1..mark.len())
                    .rev()
                    .find(|&length| self.pending.ends_with(&mark[..length]))
                    .unwrap_or(0);
                let cut = self.pending.len() - keep;
                if cut > 0 {
                    said(&self.pending[..cut]);
                    self.pending.drain(..cut);
                }
                return;
            };
            if at > 0 {
                said(&self.pending[..at]);
                self.pending.drain(..at);
            }
            let rest = &self.pending[mark.len()..];
            match rest.iter().position(|&byte| byte == b'\n') {
                Some(end) if end <= LONGEST => {
                    let told = heard_in(&rest[..end]);
                    let line = mark.len() + end + 1;
                    match told {
                        Some(ago) => heard(ago),
                        // Not a line it tells: for people, as it is.
                        None => said(&self.pending[..line]),
                    }
                    self.pending.drain(..line);
                }
                // Not ended yet: the rest of it is still to come.
                None if rest.len() <= LONGEST => return,
                // No line of Ferry's: for people, mark and all.
                _ => {
                    said(&self.pending[..mark.len()]);
                    self.pending.drain(..mark.len());
                }
            }
        }
    }
}

/// How long ago, as a line of the kind [`HEARD`] says it past its mark:
/// `heard:<length>:<milliseconds>`.
fn heard_in(line: &[u8]) -> Option<Duration> {
    let line = std::str::from_utf8(line).ok()?;
    let (kind, rest) = line.split_once(':')?;
    let (length, content) = rest.split_once(':')?;
    if kind != HEARD || length.parse::<usize>().ok()? != content.len() {
        return None;
    }
    Some(Duration::from_millis(content.parse().ok()?))
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
        lifeline.read(|part| said.extend_from_slice(part), |_| {});
        assert_eq!(said, b"said\nagain\n");
        assert!(child.wait().success());
    }

    /// The lines a worker tells its monitor, however what it says comes cut,
    /// are taken out of what is passed on, and each says how long ago; what
    /// only looks like one is passed on as it is.
    #[test]
    fn what_a_worker_tells_its_monitor_is_taken_out() {
        let mut said_all = b"before ".to_vec();
        said_all.extend(super::super::start::frame(TAG, HEARD, b"250"));
        said_all.extend_from_slice(b"between\n");
        said_all.extend(super::super::start::frame(TAG, HEARD, b"7"));
        said_all.extend_from_slice(b"\0ferry:lifeline:heard:3:12\n\0ferry: no\nafter\0fe");
        for cut in 1..said_all.len() {
            let mut parted = Parted::default();
            let (mut said, mut heard) = (Vec::new(), Vec::new());
            for part in said_all.chunks(cut) {
                parted.take(
                    part,
                    &mut |part: &[u8]| said.extend_from_slice(part),
                    &mut |ago| heard.push(ago),
                );
            }
            said.extend_from_slice(&parted.pending);
            assert_eq!(
                heard,
                [Duration::from_millis(250), Duration::from_millis(7)],
                "cut every {cut}"
            );
            assert_eq!(
                String::from_utf8_lossy(&said),
                "before between\n\0ferry:lifeline:heard:3:12\n\0ferry: no\nafter\0fe",
                "cut every {cut}"
            );
        }
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
