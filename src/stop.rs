//! A peer's first failure, and the stop of every stage once there is one.
//!
//! The stages of a peer run on threads of their own, and pass work on over
//! channels. A stage that fails says so here, before it lets go of what the
//! others wait on: the first failure is kept as the cause, the peer is
//! stopped, and the other side is told, once. What a stage meets because
//! another stopped, such as a channel closed under it, is [`Stopped`], a
//! consequence, never taken for a cause.
//!
//! A stage checks before it changes anything, and a stage that waits on a
//! channel waits on [`Stop::signal`] too, which wakes it once the peer stops.

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::mux::Mux;
use crate::protocol::Control;

/// What a stage meets because the peer stopped: a consequence of another's
/// failure, never its cause.
#[derive(Debug)]
pub struct Stopped;

impl fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("stopped, as another part of this side failed")
    }
}

impl std::error::Error for Stopped {}

/// The first failure, the other side already told of it: sent as
/// `Control::Error`, or come from the other side.
#[derive(Debug)]
pub struct Reported(pub anyhow::Error);

impl fmt::Display for Reported {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "{:#}", self.0)
    }
}

impl std::error::Error for Reported {}

/// What never comes on the stop's signal: it only closes.
pub enum Never {}

/// A peer's stop: its first failure, and whether its stages should go on.
pub struct Stop {
    /// Set once there is a failure, while `inner` is locked.
    stopped: AtomicBool,
    inner: Mutex<Inner>,
    /// Dropped as the peer stops, which closes `signal`.
    raised: Mutex<Option<Sender<Never>>>,
    signal: Receiver<Never>,
}

struct Inner {
    /// The first failure: the cause.
    first: Option<anyhow::Error>,
    /// The other side knows of it.
    told: bool,
    /// Where the one `Control::Error` goes, until the stop is closed.
    mux: Option<Mux>,
}

impl Stop {
    /// A stop that tells the other side of the first failure through `mux`,
    /// if given.
    pub fn new(mux: Option<Mux>) -> Self {
        let (raised, signal) = crossbeam_channel::bounded(0);
        Self {
            stopped: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                first: None,
                told: false,
                mux,
            }),
            raised: Mutex::new(Some(raised)),
            signal,
        }
    }

    /// A stage failed: if it is the first failure, and not [`Stopped`], it
    /// is the cause, the peer stops, and the other side is told.
    pub fn fail(&self, error: anyhow::Error) {
        self.keep(error, false);
    }

    /// The other side failed, and said so: that is the cause, and the peer
    /// stops, with nothing to tell.
    pub fn peer_failed(&self, error: anyhow::Error) {
        self.keep(error, true);
    }

    fn keep(&self, error: anyhow::Error, told: bool) {
        if error.is::<Stopped>() {
            return;
        }
        let mut inner = self.inner.lock().expect("not poisoned");
        if inner.first.is_some() {
            return;
        }
        let message = format!("{error:#}");
        inner.first = Some(error);
        self.stopped.store(true, Ordering::SeqCst);
        inner.told = told;
        // Messages never wait, so this sends while locked: only the first
        // failure gets here, and before `done` can.
        if !told
            && let Some(mux) = &inner.mux
            && mux.message(&Control::Error { message }).is_ok()
        {
            inner.told = true;
        }
        drop(inner);
        self.raised.lock().expect("not poisoned").take();
    }

    /// What a stage does with its result before it lets go of what others
    /// wait on: a failure is kept, and [`Stopped`] given in its place, so
    /// that whatever the others then meet is a consequence.
    pub fn failing<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            self.fail(error);
            Stopped.into()
        })
    }

    /// An error met once the peer has stopped, as a stream that ended under
    /// a stage: a consequence, [`Stopped`].
    pub fn consequence(&self, error: anyhow::Error) -> anyhow::Error {
        if self.is_stopped() {
            Stopped.into()
        } else {
            error
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// [`Stopped`] once the peer has stopped: for a stage to call before it
    /// changes anything.
    pub fn check(&self) -> Result<()> {
        if self.is_stopped() {
            return Err(Stopped.into());
        }
        Ok(())
    }

    /// Closed once the peer stops: a stage waiting on a channel waits on it
    /// too, in a `select!`.
    pub fn signal(&self) -> &Receiver<Never> {
        &self.signal
    }

    /// Tells the other side that this one is done, unless it has stopped:
    /// `Control::Done` never follows `Control::Error`. A failure after it is
    /// still told.
    pub fn done(&self) -> Result<()> {
        let inner = self.inner.lock().expect("not poisoned");
        if self.is_stopped() {
            return Err(Stopped.into());
        }
        match &inner.mux {
            Some(mux) => mux.message(&Control::Done),
            None => Err(Stopped.into()),
        }
    }

    /// Lets go of the mux, so that the pump can end once the stages have
    /// let go of theirs. A failure after it is kept, but not told.
    pub fn close(&self) {
        self.inner.lock().expect("not poisoned").mux.take();
    }

    /// The first failure, if any: as [`Reported`] once the other side knows
    /// of it, so that it is not told again.
    pub fn result(self) -> Result<()> {
        let inner = self.inner.into_inner().expect("not poisoned");
        match inner.first {
            None => Ok(()),
            Some(first) if inner.told => Err(Reported(first).into()),
            Some(first) => Err(first),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux;
    use crate::protocol::{self, PacketType};
    use anyhow::anyhow;
    use std::time::Duration;

    /// What a stop with a mux sends, once `stop` is done with it.
    fn sent(run: impl FnOnce(&Stop)) -> (Vec<Control>, Result<()>) {
        let (mux, outlet) = mux::channel();
        let stop = Stop::new(Some(mux));
        run(&stop);
        stop.close();
        let mut written = Vec::new();
        mux::pump(&mut written, outlet).unwrap();
        let mut reader = written.as_slice();
        let mut controls = Vec::new();
        while let Some(packet) = protocol::receive(&mut reader).unwrap() {
            assert_eq!(packet.kind, PacketType::Control);
            controls.push(packet.decode::<Control>().unwrap());
        }
        (controls, stop.result())
    }

    #[test]
    fn the_first_failure_is_the_cause_and_is_told_once() {
        let (controls, result) = sent(|stop| {
            assert!(stop.check().is_ok());
            stop.fail(anyhow!("first"));
            stop.fail(anyhow!("second"));
            assert!(stop.is_stopped());
            assert!(stop.check().unwrap_err().is::<Stopped>());
        });
        assert_eq!(
            controls,
            [Control::Error {
                message: "first".to_owned()
            }]
        );
        let error = result.unwrap_err();
        assert!(error.is::<Reported>());
        assert_eq!(error.to_string(), "first");
    }

    #[test]
    fn a_consequence_is_never_the_cause() {
        let (controls, result) = sent(|stop| {
            stop.fail(Stopped.into());
            stop.fail(anyhow::Error::from(Stopped).context("write a file"));
            assert!(!stop.is_stopped());
            stop.fail(anyhow!("the cause"));
        });
        assert_eq!(
            controls,
            [Control::Error {
                message: "the cause".to_owned()
            }]
        );
        assert_eq!(result.unwrap_err().to_string(), "the cause");
    }

    #[test]
    fn done_never_follows_an_error() {
        let (controls, result) = sent(|stop| {
            stop.fail(anyhow!("failed"));
            assert!(stop.done().unwrap_err().is::<Stopped>());
        });
        assert_eq!(
            controls,
            [Control::Error {
                message: "failed".to_owned()
            }]
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_failure_after_done_is_still_told() {
        let (controls, result) = sent(|stop| {
            stop.done().unwrap();
            stop.fail(anyhow!("after"));
        });
        assert_eq!(
            controls,
            [
                Control::Done,
                Control::Error {
                    message: "after".to_owned()
                }
            ]
        );
        assert!(result.unwrap_err().is::<Reported>());
    }

    #[test]
    fn the_other_sides_failure_is_not_told_back() {
        let (controls, result) = sent(|stop| {
            stop.peer_failed(anyhow!("the Source failed: gone"));
            stop.fail(anyhow!("a consequence there was no stage to see"));
        });
        assert_eq!(controls, []);
        let error = result.unwrap_err();
        assert!(error.is::<Reported>());
        assert_eq!(error.to_string(), "the Source failed: gone");
    }

    /// Once the mux is let go of, a failure is kept, for the caller to tell.
    #[test]
    fn a_failure_after_close_is_kept_untold() {
        let stop = Stop::new(Some(mux::channel().0));
        stop.close();
        stop.fail(anyhow!("late").context("finish"));
        let error = stop.result().unwrap_err();
        assert!(!error.is::<Reported>());
        assert_eq!(format!("{error:#}"), "finish: late");
    }

    #[test]
    fn without_a_failure_the_result_is_ok() {
        let (controls, result) = sent(|stop| stop.done().unwrap());
        assert_eq!(controls, [Control::Done]);
        result.unwrap();
    }

    /// A thread that waits for work, and for the stop, wakes as the peer
    /// stops, though no work comes.
    #[test]
    fn stopping_wakes_whoever_waits_on_the_signal() {
        crate::test_support::within(
            "stopping wakes whoever waits on the signal",
            crate::test_support::PATIENCE,
            move || {
                let stop = Stop::new(None);
                let (_work, waiting) = crossbeam_channel::unbounded::<u32>();
                std::thread::scope(|scope| {
                    let woken = scope.spawn(|| {
                        crossbeam_channel::select! {
                            recv(waiting) -> _ => false,
                            recv(stop.signal()) -> _ => true,
                        }
                    });
                    std::thread::sleep(Duration::from_millis(50));
                    stop.fail(anyhow!("failed").context("a stage"));
                    assert!(woken.join().unwrap());
                });
            },
        );
    }

    /// A stop without a mux, as the client's, keeps the cause untold.
    #[test]
    fn without_a_mux_the_cause_is_kept_untold() {
        let stop = Stop::new(None);
        stop.fail(anyhow!("cut").context("read a path"));
        assert!(stop.done().is_err());
        let error = stop.result().unwrap_err();
        assert!(!error.is::<Reported>());
        assert_eq!(format!("{error:#}"), "read a path: cut");
    }
}
