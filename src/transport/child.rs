//! A process Ferry started, as its monitor watches it: a thread of its own
//! waits for it, so that it is seen paused, as by SIGSTOP, and going on,
//! as well as ending, and says so to whoever asked to be told. Each thing
//! that can get stuck has a watcher that is not itself: what the client
//! starts, the client watches; what a host's monitor starts, it does. See
//! the [draft](../../../docs/drafts/silent-drops.md).
//!
//! Only that thread waits for the process, so nothing else may: the
//! standard library's own waiting is never called on it.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crossbeam_channel::Sender;

use super::scope::{Ended, Scope};
use crate::profile::State;

/// What a watched process did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// It was stopped, as by SIGSTOP or Ctrl-Z.
    Paused,
    /// It went on, as after SIGCONT.
    Resumed,
    /// It ended so.
    Exited(ExitStatus),
}

/// Where what a process does is told: with its ID.
pub type Told = Sender<(u32, Event)>;

/// A signal Ferry sends a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// A process Ferry started, watched: its standard input, output and error,
/// as the command piped them, taken by whoever uses them.
pub struct Child {
    pid: u32,
    shared: Arc<Shared>,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

struct Shared {
    status: Mutex<Status>,
    changed: Condvar,
    /// Ended as the process exits, for waits that also have a scope.
    exited: Scope,
}

#[derive(Default)]
struct Status {
    exited: Option<ExitStatus>,
    paused: bool,
}

impl Child {
    /// Starts `command`, and watches it: what it does goes to `told`, if
    /// given.
    pub fn spawn(command: &mut Command, told: Option<Told>) -> io::Result<Self> {
        let mut child = command.spawn()?;
        let pid = child.id();
        let shared = Arc::new(Shared {
            status: Mutex::new(Status::default()),
            changed: Condvar::new(),
            exited: Scope::new()?,
        });
        let watched = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("child {pid}"))
            .spawn(move || watch(pid, &watched, told.as_ref()))?;
        Ok(Self {
            pid,
            shared,
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        })
    }

    /// Its process ID.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// How it ended, if it has.
    pub fn try_wait(&self) -> Option<ExitStatus> {
        self.shared.status.lock().expect("not poisoned").exited
    }

    /// Whether it is stopped, as by SIGSTOP.
    pub fn paused(&self) -> bool {
        self.shared.status.lock().expect("not poisoned").paused
    }

    /// Waits for it to end.
    pub fn wait(&self) -> ExitStatus {
        let mut status = self.shared.status.lock().expect("not poisoned");
        loop {
            if let Some(exited) = status.exited {
                return exited;
            }
            status = self.shared.changed.wait(status).expect("not poisoned");
        }
    }

    /// Waits for it to end until `deadline`, or until `scope` ends: how it
    /// ended, if it did; the scope's end, if that came first.
    pub fn wait_in(
        &self,
        scope: &Scope,
        deadline: Option<Instant>,
    ) -> Result<Option<ExitStatus>, Ended> {
        if let Some(exited) = self.try_wait() {
            return Ok(Some(exited));
        }
        match scope.either(&self.shared.exited, deadline, State::Work) {
            Some(_) if self.try_wait().is_some() => Ok(self.try_wait()),
            Some(ended) => Err(ended),
            None => Ok(None),
        }
    }

    /// Waits for it to end until `deadline`: how it ended, if it did.
    pub fn wait_until(&self, deadline: Instant) -> Option<ExitStatus> {
        let mut status = self.shared.status.lock().expect("not poisoned");
        loop {
            if let Some(exited) = status.exited {
                return Some(exited);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            status = self
                .shared
                .changed
                .wait_timeout(status, left)
                .expect("not poisoned")
                .0;
        }
    }

    /// Sends it `signal`, unless it has ended; see [`Handle::signal`].
    pub fn signal(&self, signal: Signal) {
        self.handle().signal(signal);
    }

    /// A way to signal it from elsewhere, as when Ferry exits.
    pub fn handle(&self) -> Handle {
        Handle {
            pid: self.pid,
            shared: Arc::clone(&self.shared),
        }
    }

    /// Kills it, as the standard library's `kill` does, unless it has
    /// ended.
    pub fn kill(&self) {
        self.signal(Signal::Kill);
    }
}

/// A way to signal a watched process from elsewhere: clones signal the
/// same.
#[derive(Clone)]
pub struct Handle {
    pid: u32,
    shared: Arc<Shared>,
}

impl Handle {
    /// The process's ID.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Sends the process `signal`, unless it has ended. Its ID stays its
    /// own until the watching thread waits for it, which marks it ended at
    /// once after: a signal sent in between, which needs another process to
    /// have taken its ID that soon, is the one way this could go astray.
    pub fn signal(&self, signal: Signal) {
        let status = self.shared.status.lock().expect("not poisoned");
        if status.exited.is_none() {
            let signal = match signal {
                Signal::Term => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
            };
            // SAFETY: `kill` has no memory effects.
            unsafe {
                libc::kill(self.pid as libc::pid_t, signal);
            }
        }
    }
}

/// Waits for the process `pid`, as its only waiter, until it ends: says
/// each time it is paused, goes on, and how it ended.
fn watch(pid: u32, shared: &Shared, told: Option<&Told>) {
    let tell = |event| {
        if let Some(told) = told {
            let _ = told.send((pid, event));
        }
    };
    loop {
        let mut raw = 0;
        // SAFETY: `waitpid` writes the status to `raw`, and has no other
        // memory effects.
        let waited = unsafe {
            libc::waitpid(
                pid as libc::pid_t,
                &mut raw,
                libc::WUNTRACED | libc::WCONTINUED,
            )
        };
        if waited == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // Waited for by someone else, against the rule: it ended, how is
            // not known.
            raw = 0xff << 8;
        }
        if waited != -1 && libc::WIFSTOPPED(raw) {
            shared.status.lock().expect("not poisoned").paused = true;
            tell(Event::Paused);
            continue;
        }
        if waited != -1 && libc::WIFCONTINUED(raw) {
            shared.status.lock().expect("not poisoned").paused = false;
            tell(Event::Resumed);
            continue;
        }
        let exited = ExitStatus::from_raw(raw);
        let mut status = shared.status.lock().expect("not poisoned");
        status.exited = Some(exited);
        status.paused = false;
        drop(status);
        shared.changed.notify_all();
        shared.exited.end("it exited");
        tell(Event::Exited(exited));
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    fn sleeping(told: Option<Told>) -> Child {
        Child::spawn(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
            told,
        )
        .unwrap()
    }

    #[test]
    fn a_child_is_seen_paused_going_on_and_ending() {
        let (told, events) = crossbeam_channel::unbounded();
        let child = sleeping(Some(told));
        let pid = child.id();
        let next = || events.recv_timeout(Duration::from_secs(10)).unwrap();
        // SAFETY: `kill` has no memory effects.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) };
        assert_eq!(next(), (pid, Event::Paused));
        assert!(child.paused());
        // SAFETY: as above.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) };
        assert_eq!(next(), (pid, Event::Resumed));
        child.signal(Signal::Term);
        let (_, ended) = next();
        let Event::Exited(status) = ended else {
            panic!("expected it to end, got {ended:?}");
        };
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert_eq!(child.wait(), status);
        assert_eq!(child.try_wait(), Some(status));
        // Ended, it is sent nothing more.
        child.kill();
    }

    #[test]
    fn a_wait_for_a_child_returns_as_its_scope_ends_or_its_deadline() {
        let child = sleeping(None);
        let scope = Scope::new().unwrap();
        let deadline = Some(Instant::now() + Duration::from_millis(20));
        assert_eq!(child.wait_in(&scope, deadline).unwrap(), None);
        assert_eq!(
            child.wait_until(Instant::now() + Duration::from_millis(20)),
            None
        );
        scope.end("given up");
        assert_eq!(child.wait_in(&scope, None).unwrap_err().0, "given up");
        child.kill();
        let status = child.wait();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        // Ended, it says how, whatever the scope.
        assert_eq!(child.wait_in(&scope, None).unwrap(), Some(status));
    }
}
