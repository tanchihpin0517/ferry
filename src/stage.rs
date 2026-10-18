//! The threads of a peer's stages, each under the peer's [`Stop`]: an error
//! a stage returns, or a panic, stops every stage, and is kept as the cause
//! if it is the first. No stage's failure waits for another stage to meet a
//! channel closed under it, and none is lost in a thread nobody joins.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::thread::{Scope, ScopedJoinHandle};

use anyhow::{Result, anyhow};

use crate::profile;
use crate::stop::Stop;

/// Where a peer's stages are started: a scope, and the stop they run under.
pub struct Stages<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    stop: &'scope Stop,
}

/// A stage started, whose result, if it had one, its join gives.
pub struct Stage<'scope, T> {
    handle: ScopedJoinHandle<'scope, Option<T>>,
    role: &'static str,
    stop: &'scope Stop,
}

impl<'scope, 'env> Stages<'scope, 'env> {
    pub fn new(scope: &'scope Scope<'scope, 'env>, stop: &'scope Stop) -> Self {
        Self { scope, stop }
    }

    /// Runs `work` on a thread of its own, profiled as `role`: what it
    /// returns, or `None` once it failed, which stops every stage.
    pub fn spawn<T: Send + 'scope>(
        &self,
        role: &'static str,
        work: impl FnOnce(&Stop) -> Result<T> + Send + 'scope,
    ) -> Stage<'scope, T> {
        let stop = self.stop;
        let handle = self.scope.spawn(move || {
            let _thread = profile::thread(role);
            match panic::catch_unwind(AssertUnwindSafe(|| work(stop))) {
                Ok(Ok(value)) => Some(value),
                Ok(Err(error)) => {
                    stop.fail(error);
                    None
                }
                Err(panic) => {
                    stop.fail(panicked(role, &panic));
                    None
                }
            }
        });
        Stage {
            handle,
            role,
            stop: self.stop,
        }
    }
}

impl<T> Stage<'_, T> {
    /// Waits for the stage to end: what it returned, or `None` once it
    /// failed, the stop knowing why.
    pub fn join(self) -> Option<T> {
        // A panic is caught as the stage runs, so one here came after, as
        // its result was let go of.
        self.handle.join().unwrap_or_else(|panic| {
            self.stop.fail(panicked(self.role, &panic));
            None
        })
    }
}

/// The failure a panic in `role` is.
fn panicked(role: &str, panic: &Box<dyn Any + Send>) -> anyhow::Error {
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic");
    anyhow!("internal error in {role}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::Stopped;

    /// A stage that fails stops one that waits for work and for the stop,
    /// and the scope ends, with the first failure the cause.
    #[test]
    fn a_stage_that_fails_stops_the_others() {
        crate::test_support::within(
            "a stage that fails stops the others",
            crate::test_support::PATIENCE,
            move || {
                let stop = Stop::new(None);
                let (work, waiting) = crossbeam_channel::unbounded::<u32>();
                std::thread::scope(|scope| {
                    let stages = Stages::new(scope, &stop);
                    let waiter = stages.spawn("waiter", |stop| {
                        crossbeam_channel::select! {
                            recv(waiting) -> _ => Ok(()),
                            recv(stop.signal()) -> _ => Err(Stopped.into()),
                        }
                    });
                    let failer = stages.spawn("failer", |_| -> Result<()> {
                        Err(anyhow!("the disk is gone"))
                    });
                    // The waiter is woken as the failure comes, before anyone
                    // joins the stage that failed.
                    assert!(waiter.join().is_none());
                    assert!(failer.join().is_none());
                    drop(work);
                });
                assert_eq!(stop.result().unwrap_err().to_string(), "the disk is gone");
            },
        );
    }

    /// A stage that panics stops the others, as one that fails does, and
    /// names its role.
    #[test]
    fn a_stage_that_panics_stops_the_others() {
        crate::test_support::within(
            "a stage that panics stops the others",
            crate::test_support::PATIENCE,
            move || {
                let stop = Stop::new(None);
                std::thread::scope(|scope| {
                    let stages = Stages::new(scope, &stop);
                    let waiter = stages.spawn("waiter", |stop| {
                        let _ = stop.signal().recv();
                        stop.check()
                    });
                    let panicker = stages.spawn("parser", |_| -> Result<()> {
                        panic!("a record with no header")
                    });
                    assert!(waiter.join().is_none());
                    assert!(panicker.join().is_none());
                });
                assert_eq!(
                    stop.result().unwrap_err().to_string(),
                    "internal error in parser: a record with no header"
                );
            },
        );
    }

    #[test]
    fn a_stage_that_succeeds_gives_its_result() {
        let stop = Stop::new(None);
        std::thread::scope(|scope| {
            let stages = Stages::new(scope, &stop);
            assert_eq!(stages.spawn("counter", |_| Ok(3)).join(), Some(3));
        });
        stop.result().unwrap();
    }
}
