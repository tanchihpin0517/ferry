//! A scope as the waits in it see it: once it ends, every wait in it
//! returns at once, saying why, however long it would have waited. Each
//! monitor's core says when a scope ends; see [`crate::monitor`]. A wait
//! polls what it waits for and its scope together, so that ending a scope
//! needs no wait to look again now and then, and no timer.
//!
//! A scope is a socket pair: the waits poll one end, and ending it closes
//! the other, which wakes every one of them, on every system, as a peer
//! closing does.

use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rustix::event::{PollFd, PollFlags, Timespec};

use crate::profile::{self, State};

/// A scope, as the waits in it see it: clones are the same scope.
#[derive(Clone)]
pub struct Scope(Arc<Inner>);

struct Inner {
    /// Why it ended, as first said.
    why: OnceLock<String>,
    /// What the waits poll.
    watched: UnixStream,
    /// Closed as the scope ends.
    ender: Mutex<Option<UnixStream>>,
}

/// A scope ended, and why: the error a wait in it returns.
#[derive(Debug, Clone)]
pub struct Ended(pub String);

impl std::fmt::Display for Ended {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Ended {}

impl Ended {
    /// The scope's end, as `error` says, if that is what it is.
    pub fn of(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }

    fn into_io(self) -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionAborted, self)
    }
}

/// What a wait came to, short of its scope ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// What it waited for is ready.
    Ready,
    /// Its deadline passed first.
    TimedOut,
}

impl Scope {
    /// A new scope, not ended.
    pub fn new() -> io::Result<Self> {
        let (watched, ender) = UnixStream::pair()?;
        Ok(Self(Arc::new(Inner {
            why: OnceLock::new(),
            watched,
            ender: Mutex::new(Some(ender)),
        })))
    }

    /// Ends the scope, for `why`, unless it ended already: every wait in it
    /// returns.
    pub fn end(&self, why: &str) {
        let _ = self.0.why.set(why.to_owned());
        drop(self.0.ender.lock().expect("not poisoned").take());
    }

    /// Why it ended, once it has.
    pub fn ended(&self) -> Option<Ended> {
        self.0.why.get().map(|why| Ended(why.clone()))
    }

    /// What a wait that polls more than one thing polls, with what else it
    /// waits for, to stop as the scope ends.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.watched.as_fd()
    }

    /// Fails with its end, once it has ended.
    pub fn check(&self) -> io::Result<()> {
        match self.ended() {
            Some(ended) => Err(ended.into_io()),
            None => Ok(()),
        }
    }

    /// Waits until `fd` is ready for `flags`, its deadline passes, or the
    /// scope ends, in `state` as profiled: an error with its end, then,
    /// which [`Ended::of`] finds. A signal that interrupts the wait does not
    /// end it.
    pub fn wait(
        &self,
        fd: impl AsFd,
        flags: PollFlags,
        deadline: Option<Instant>,
        state: State,
    ) -> io::Result<Waited> {
        loop {
            self.check()?;
            // Past its deadline, it still looks once: what is there already
            // is ready.
            let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let timeout = left.map(|left| Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            });
            let mut polled = [
                PollFd::new(&fd, flags),
                PollFd::new(&self.0.watched, PollFlags::IN),
            ];
            match profile::wait(state, || rustix::event::poll(&mut polled, timeout.as_ref())) {
                Ok(_) if !polled[1].revents().is_empty() => {
                    self.check()?;
                }
                Ok(_) if !polled[0].revents().is_empty() => return Ok(Waited::Ready),
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
            if left.is_some_and(|left| left.is_zero()) {
                return Ok(Waited::TimedOut);
            }
        }
    }

    /// Waits until this scope or `other` ends, or `deadline` passes: which
    /// ended, if one did.
    pub fn either(&self, other: &Self, deadline: Option<Instant>, state: State) -> Option<Ended> {
        match self.wait(&other.0.watched, PollFlags::IN, deadline, state) {
            Err(error) => Ended::of(&error).cloned(),
            Ok(_) => other.ended(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_wait_returns_once_its_scope_ends_saying_why() {
        let scope = Scope::new().unwrap();
        let (quiet, _other) = UnixStream::pair().unwrap();
        let ender = scope.clone();
        let started = Instant::now();
        let ending = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            ender.end("the host was lost");
            ender.end("said second, not kept");
        });
        let error = scope
            .wait(&quiet, PollFlags::IN, None, State::ReadConnection)
            .unwrap_err();
        ending.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(Ended::of(&error).unwrap().0, "the host was lost");
        // Once ended, a wait returns at once.
        assert!(
            scope
                .wait(&quiet, PollFlags::IN, None, State::ReadConnection)
                .is_err()
        );
    }

    #[test]
    fn a_wait_returns_what_is_ready_or_its_deadline() {
        let scope = Scope::new().unwrap();
        let (mut talking, listening) = UnixStream::pair().unwrap();
        let deadline = Some(Instant::now() + Duration::from_millis(20));
        assert_eq!(
            scope
                .wait(&listening, PollFlags::IN, deadline, State::ReadConnection)
                .unwrap(),
            Waited::TimedOut
        );
        io::Write::write_all(&mut talking, b"x").unwrap();
        assert_eq!(
            scope
                .wait(&listening, PollFlags::IN, None, State::ReadConnection)
                .unwrap(),
            Waited::Ready
        );
    }

    #[test]
    fn either_says_which_scope_ended() {
        let (first, second) = (Scope::new().unwrap(), Scope::new().unwrap());
        second.end("second");
        assert_eq!(
            first.either(&second, None, State::Work).unwrap().0,
            "second"
        );
        let third = Scope::new().unwrap();
        first.end("first");
        assert_eq!(first.either(&third, None, State::Work).unwrap().0, "first");
        let deadline = Some(Instant::now() + Duration::from_millis(10));
        assert!(
            Scope::new()
                .unwrap()
                .either(&third, deadline, State::Work)
                .is_none()
        );
    }
}
