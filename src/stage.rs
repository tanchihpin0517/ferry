//! The threads of a peer's stages, each under the peer's [`Stop`]: an error
//! a stage returns, or a panic, stops every stage, and is kept as the cause
//! if it is the first. No stage's failure waits for another stage to meet a
//! channel closed under it, and none is lost in a thread nobody joins. A
//! stage may run on the scope's own thread, as [`run`] runs it.
//!
//! What a stage sends on, it is handed as what it holds, which is let go of
//! only once its failure, or its panic, is kept: a stage that finds what it
//! waits on ended then finds the peer stopped, if it is, and never takes a
//! failure's end for the end of the work. What a stage's work captures is
//! let go of as the work returns, or unwinds, before that.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::thread::{Scope, ScopedJoinHandle};

use anyhow::{Result, anyhow};

use crate::profile;
use crate::stop::{Stop, Stopped};

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

    /// The stop the stages run under.
    pub fn stop(&self) -> &'scope Stop {
        self.stop
    }

    /// Runs `work` on a thread of its own, profiled as `role`, with `held`,
    /// what it sends on, let go of once its failure is kept: what it
    /// returns, or `None` once it failed, which stops every stage.
    pub fn spawn<H, T>(
        &self,
        role: &'static str,
        held: H,
        work: impl FnOnce(&Stop, &mut H) -> Result<T> + Send + 'scope,
    ) -> Stage<'scope, T>
    where
        H: Send + 'scope,
        T: Send + 'scope,
    {
        let stop = self.stop;
        let handle = crate::threads::spawn_scoped(self.scope, role, move || {
            let work = |held: &mut H| {
                crate::inject::stall_if(role);
                work(stop, held)
            };
            run(role, stop, held, work).ok()
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

/// Runs `work` on this thread as a stage of `role` runs on a thread of its
/// own, profiled as `role`, with `held`, what it sends on: what it returns,
/// or, once its failure, or the failure a panic in it is, is kept as the
/// cause, [`Stopped`], and then `held` is let go of. Without this, a panic
/// on the scope's own thread leaves the scope waiting for stages that wait
/// for the stop.
pub fn run<H, T>(
    role: &'static str,
    stop: &Stop,
    mut held: H,
    work: impl FnOnce(&mut H) -> Result<T>,
) -> Result<T> {
    let _thread = profile::thread(role);
    let work = || {
        crate::inject::panic_if(role);
        work(&mut held)
    };
    let result = panic::catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|panic| Err(panicked(role, &panic)));
    let result = result.map_err(|error| {
        stop.fail(error);
        Stopped.into()
    });
    drop(held);
    result
}

/// A worker's loop, every stage's that takes its work an item at a time:
/// the next item, as `next` gives it, waited for as work, then `work` done
/// on it, until the items end, `work` says to go no further, or the peer
/// stops. The stop is checked before each item, so that no worker changes
/// anything once the peer has stopped: a maker, a finisher, a renamer, a
/// writer finishing a file, as each did, once.
pub fn each<T>(
    stop: &Stop,
    mut next: impl FnMut() -> Option<T>,
    mut work: impl FnMut(T) -> Result<bool>,
) -> Result<()> {
    while let Some(item) = profile::wait(profile::State::Work, &mut next) {
        stop.check()?;
        if !work(item)? {
            break;
        }
    }
    Ok(())
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
                    let waiter = stages.spawn("waiter", (), |stop, ()| {
                        crossbeam_channel::select! {
                            recv(waiting) -> _ => Ok(()),
                            recv(stop.signal()) -> _ => Err(Stopped.into()),
                        }
                    });
                    let failer = stages.spawn("failer", (), |_, ()| -> Result<()> {
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
                    let waiter = stages.spawn("waiter", (), |stop, ()| {
                        let _ = stop.signal().recv();
                        stop.check()
                    });
                    let panicker = stages.spawn("writing", (), |_, ()| -> Result<()> {
                        panic!("a block with no header")
                    });
                    assert!(waiter.join().is_none());
                    assert!(panicker.join().is_none());
                });
                assert_eq!(
                    stop.result().unwrap_err().to_string(),
                    "internal error in writing: a block with no header"
                );
            },
        );
    }

    /// A stage run on the scope's own thread that panics stops the others
    /// too, once its failure is kept, and the scope ends.
    #[test]
    fn a_stage_run_on_the_scopes_thread_that_panics_stops_the_others() {
        crate::test_support::within(
            "a stage run on the scope's thread that panics stops the others",
            crate::test_support::PATIENCE,
            move || {
                let stop = Stop::new(None);
                std::thread::scope(|scope| {
                    let stages = Stages::new(scope, &stop);
                    let waiter = stages.spawn("waiter", (), |stop, ()| {
                        let _ = stop.signal().recv();
                        stop.check()
                    });
                    let ran = run("receive", &stop, (), |()| -> Result<()> {
                        panic!("a block past the last")
                    });
                    assert!(ran.unwrap_err().is::<Stopped>());
                    assert!(waiter.join().is_none());
                });
                assert_eq!(
                    stop.result().unwrap_err().to_string(),
                    "internal error in receive: a block past the last"
                );
            },
        );
    }

    /// How a stage ends, holding what another waits on.
    #[derive(Clone, Copy, Debug)]
    enum Ending {
        Failing,
        Panicking,
        FailingOnTheScopesThread,
    }

    /// A stage that fails, or panics, lets go of what it sends on only once
    /// its failure is kept: a stage that waits on that, finding it ended,
    /// finds the peer stopped, never ended as though all was sent. So does
    /// one run on the scope's own thread.
    #[test]
    fn a_stage_lets_go_of_what_it_sends_on_once_its_failure_is_kept() {
        let endings = [
            Ending::Failing,
            Ending::Panicking,
            Ending::FailingOnTheScopesThread,
        ];
        for ending in endings.into_iter().cycle().take(3000) {
            let stop = Stop::new(None);
            let (sending, sent) = crossbeam_channel::bounded::<()>(0);
            std::thread::scope(|scope| {
                let stages = Stages::new(scope, &stop);
                let waiter = stages.spawn("waiter", sent, |stop, sent| {
                    let _ = sent.recv();
                    stop.check()
                });
                let failing = |_: &mut _| -> Result<()> { Err(anyhow!("the disk is gone")) };
                match ending {
                    Ending::Failing => {
                        stages.spawn("failer", sending, move |_, sending| failing(sending));
                    }
                    Ending::Panicking => {
                        stages.spawn("failer", sending, |_, _| -> Result<()> {
                            panic!("a block with no header")
                        });
                    }
                    Ending::FailingOnTheScopesThread => {
                        let _ = run("failer", &stop, sending, failing);
                    }
                }
                assert!(
                    waiter.join().is_none(),
                    "{ending:?}: ended as though all was sent"
                );
            });
        }
    }

    /// A worker takes no more work once the peer has stopped, though more
    /// waits: what waits is let go of, nothing done with it.
    #[test]
    fn a_worker_takes_no_work_once_the_peer_stopped() {
        let stop = Stop::new(None);
        let (items, waiting) = crossbeam_channel::unbounded();
        for item in [1, 2, 3] {
            items.send(item).unwrap();
        }
        let mut done = Vec::new();
        let worked = each(
            &stop,
            || waiting.try_recv().ok(),
            |item| {
                done.push(item);
                stop.fail(anyhow!("the disk is gone"));
                Ok(true)
            },
        );
        assert_eq!(done, [1]);
        assert!(worked.unwrap_err().is::<Stopped>());
        // The rest is let go of as the queue is.
        assert_eq!(waiting.len(), 1);
    }

    #[test]
    fn a_stage_that_succeeds_gives_its_result() {
        let stop = Stop::new(None);
        std::thread::scope(|scope| {
            let stages = Stages::new(scope, &stop);
            assert_eq!(stages.spawn("counter", (), |_, ()| Ok(3)).join(), Some(3));
        });
        stop.result().unwrap();
    }
}
