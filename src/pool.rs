//! A pool of worker threads whose results come back in the order the work
//! went in, for the stages that must keep an order: frames of the record
//! stream, and directories of the manifest.
//!
//! At most a pool's capacity of items are in it at once, from when they are
//! submitted to when their results are taken: waiting, being worked on, or
//! done and waiting to be taken. A fast submitter, or a slow taker, makes the
//! submitter wait, rather than piling work or results up; results are put
//! back in order as they arrive.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::profile::{self, State};
use crate::report::Gauge;
use crate::stage::Stages;
use crate::stop::{Never, Stopped};

/// How a pool's threads are profiled: their role, the state of their work,
/// and the state of a submitter waiting for room.
#[derive(Clone, Copy)]
pub struct Profiled {
    pub role: &'static str,
    pub work: State,
    pub full: State,
}

/// How many items are in a pool, submitted and their results not taken, and
/// a way to wait for fewer.
struct InPool {
    state: Mutex<Counted>,
    fewer: Condvar,
    /// At most how many.
    limit: usize,
}

struct Counted {
    items: usize,
    /// The collector is gone: nothing more is taken.
    closed: bool,
}

impl InPool {
    /// Waits for room for one more, and takes it; false once the collector
    /// is gone.
    fn enter(&self, full: State) -> bool {
        let mut state = self.state.lock().expect("not poisoned");
        let limit = self.limit;
        if state.items >= limit && !state.closed {
            state = profile::wait(full, || {
                self.fewer
                    .wait_while(state, |state| state.items >= limit && !state.closed)
            })
            .expect("not poisoned");
        }
        if state.closed {
            return false;
        }
        state.items += 1;
        true
    }

    fn leave(&self) {
        let mut state = self.state.lock().expect("not poisoned");
        state.items = state.items.saturating_sub(1);
        self.fewer.notify_all();
    }

    fn close(&self) {
        self.state.lock().expect("not poisoned").closed = true;
        self.fewer.notify_all();
    }
}

/// Where work goes in.
pub struct Submitter<T> {
    queue: Sender<(u64, T)>,
    in_pool: Arc<InPool>,
    next: u64,
    full: State,
    /// How full the queue is, for the dashboard.
    gauge: Option<Gauge>,
}

/// Another submitter of the same pool, for another thread: the collector
/// ends once every one is gone. Numbering their items is theirs, with
/// [`Submitter::submit_at`].
impl<T> Clone for Submitter<T> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            in_pool: Arc::clone(&self.in_pool),
            next: self.next,
            full: self.full,
            gauge: self.gauge.clone(),
        }
    }
}

impl<T> Submitter<T> {
    /// Hands `item` to the workers, waiting for room; [`Stopped`] once the
    /// workers or the collector are gone: why is for whatever stopped them
    /// to say.
    pub fn submit(&mut self, item: T) -> Result<()> {
        let stopped = || anyhow::Error::from(Stopped);
        if !self.in_pool.enter(self.full) {
            return Err(stopped());
        }
        if let Some(gauge) = &self.gauge {
            gauge.up();
        }
        profile::wait(self.full, || self.queue.send((self.next, item))).map_err(|_| stopped())?;
        self.next += 1;
        Ok(())
    }

    /// As [`Submitter::submit`], but `item` takes the place `sequence`
    /// says, given where the items come from, rather than the next: they
    /// may come out of order, and come out in it. Each place is taken once,
    /// from 0 up, and a submitter numbers all its items so or none.
    pub fn submit_at(&mut self, sequence: u64, item: T) -> Result<()> {
        self.next = sequence;
        self.submit(item)
    }

    /// Waits for room for one more item, and takes it, for one to be handed
    /// in later with [`Submitter::submit_reserved`], by this submitter or
    /// another: those who number items handed in out of order take their
    /// places so, in order, as one handed in late would otherwise find the
    /// pool full of those after it, which wait for it.
    pub fn reserve(&self) -> Result<()> {
        if self.in_pool.enter(self.full) {
            Ok(())
        } else {
            Err(Stopped.into())
        }
    }

    /// Hands in `item`, at the place `sequence` says, for which room was
    /// taken with [`Submitter::reserve`].
    pub fn submit_reserved(&mut self, sequence: u64, item: T) -> Result<()> {
        if let Some(gauge) = &self.gauge {
            gauge.up();
        }
        self.queue
            .send((sequence, item))
            .map_err(|_| anyhow::Error::from(Stopped))
    }
}

/// Where results come out, in the order the work went in.
pub struct Collector<U> {
    results: Receiver<(u64, U)>,
    /// Closed once the peer stops: a worker that failed, or panicked, never
    /// sends its result, so none more is waited for.
    stopped: Receiver<Never>,
    waiting: BTreeMap<u64, U>,
    next: u64,
    in_pool: Arc<InPool>,
}

impl<U> Drop for Collector<U> {
    /// Nothing more is taken: a submitter waiting for room waits no more.
    fn drop(&mut self) {
        self.in_pool.close();
    }
}

impl<U> Iterator for Collector<U> {
    type Item = U;

    /// The next result, waiting for it; `None` once every submitter is gone
    /// and every result has come out, or once the peer stops: it says why.
    fn next(&mut self) -> Option<U> {
        loop {
            if let Some(result) = self.waiting.remove(&self.next) {
                self.next += 1;
                self.in_pool.leave();
                return Some(result);
            }
            let (sequence, result) = profile::wait(State::Result, || {
                crossbeam_channel::select! {
                    recv(self.results) -> arrived => arrived.ok(),
                    recv(self.stopped) -> _ => None,
                }
            })?;
            self.waiting.insert(sequence, result);
        }
    }
}

/// Starts `threads` workers running `work`, with room for `capacity` items
/// in the pool.
pub fn ordered<'scope, T, U>(
    stages: &Stages<'scope, '_>,
    threads: usize,
    capacity: usize,
    profiled: Profiled,
    work: impl Fn(T) -> U + Send + Sync + 'scope,
) -> (Submitter<T>, Collector<U>)
where
    T: Send + 'scope,
    U: Send + 'scope,
{
    start(stages, threads, capacity, None, profiled, work)
}

/// As [`ordered`], with `gauge` measuring what waits for the workers.
pub fn gauged<'scope, T, U>(
    stages: &Stages<'scope, '_>,
    threads: usize,
    capacity: usize,
    gauge: Option<Gauge>,
    profiled: Profiled,
    work: impl Fn(T) -> U + Send + Sync + 'scope,
) -> (Submitter<T>, Collector<U>)
where
    T: Send + 'scope,
    U: Send + 'scope,
{
    start(stages, threads, capacity, gauge, profiled, work)
}

fn start<'scope, T, U>(
    stages: &Stages<'scope, '_>,
    threads: usize,
    capacity: usize,
    gauge: Option<Gauge>,
    profiled: Profiled,
    work: impl Fn(T) -> U + Send + Sync + 'scope,
) -> (Submitter<T>, Collector<U>)
where
    T: Send + 'scope,
    U: Send + 'scope,
{
    let in_pool = Arc::new(InPool {
        state: Mutex::new(Counted {
            items: 0,
            closed: false,
        }),
        fewer: Condvar::new(),
        limit: capacity,
    });
    let (queue, input) = crossbeam_channel::bounded::<(u64, T)>(capacity);
    let (output, results) = crossbeam_channel::unbounded::<(u64, U)>();
    let work = Arc::new(work);
    // Each worker takes the next item as it is free, from a receiver of its
    // own.
    for _ in 0..threads.max(1) {
        let input = input.clone();
        let output = output.clone();
        let work = Arc::clone(&work);
        let taken = gauge.clone();
        // A worker that panics stops the peer, as any stage does.
        stages.spawn(profiled.role, move |_| {
            while let Ok((sequence, item)) = profile::wait(State::Work, || input.recv()) {
                if let Some(gauge) = &taken {
                    gauge.down();
                }
                let result = profile::wait(profiled.work, || work(item));
                if output.send((sequence, result)).is_err() {
                    break;
                }
            }
            Ok(())
        });
    }
    (
        Submitter {
            queue,
            in_pool: Arc::clone(&in_pool),
            next: 0,
            full: profiled.full,
            gauge,
        },
        Collector {
            results,
            stopped: stages.stop().signal().clone(),
            waiting: BTreeMap::new(),
            next: 0,
            in_pool,
        },
    )
}

/// How many threads a stage should use: the machine's cores, within limits,
/// however few of them [`crate::cpus::limit`] keeps the process to, as a
/// stage's threads mostly wait on the filesystem.
pub fn threads(at_most: usize) -> usize {
    crate::cpus::machine().min(at_most)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop::Stop;
    use std::time::Duration;

    #[test]
    fn results_come_back_in_order() {
        crate::test_support::within(
            "results come back in order",
            crate::test_support::PATIENCE,
            move || {
                let stop = Stop::new(None);
                std::thread::scope(|scope| {
                    let stages = Stages::new(scope, &stop);
                    let profiled = Profiled {
                        role: "test",
                        work: State::Run,
                        full: State::Work,
                    };
                    let (mut submitter, mut collector) =
                        ordered(&stages, 8, 4, profiled, |item: u64| {
                            // Later items finish first.
                            std::thread::sleep(Duration::from_micros(1_000 - item * 10));
                            item * 2
                        });
                    let submitting = scope.spawn(move || {
                        for item in 0..100 {
                            submitter.submit(item).unwrap();
                        }
                    });
                    let results: Vec<u64> = std::iter::from_fn(|| collector.next()).collect();
                    submitting.join().unwrap();
                    assert_eq!(results, (0..100).map(|item| item * 2).collect::<Vec<_>>());
                });
            },
        );
    }

    /// Results not taken count against the pool's capacity: a submitter
    /// whose results are not taken waits, however fast the workers, until
    /// one is; and once the collector is gone, it waits no more.
    #[test]
    fn a_submitter_waits_while_its_results_are_not_taken() {
        crate::test_support::within(
            "a submitter waits while its results are not taken",
            crate::test_support::PATIENCE,
            move || {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let profiled = Profiled {
                    role: "test",
                    work: State::Run,
                    full: State::Work,
                };
                let submitted = AtomicUsize::new(0);
                let stop = Stop::new(None);
                std::thread::scope(|scope| {
                    let stages = Stages::new(scope, &stop);
                    let (mut submitter, mut collector) =
                        ordered(&stages, 4, 3, profiled, |item: u64| item);
                    let submitted = &submitted;
                    let submitting = scope.spawn(move || {
                        for item in 0..10 {
                            if submitter.submit(item).is_err() {
                                return false;
                            }
                            submitted.fetch_add(1, Ordering::SeqCst);
                        }
                        true
                    });
                    // It gets as far as the capacity, however slowly, and no further.
                    let reaches = |count: usize| {
                        let started = std::time::Instant::now();
                        while submitted.load(Ordering::SeqCst) < count {
                            assert!(started.elapsed() < Duration::from_secs(30));
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        std::thread::sleep(Duration::from_millis(100));
                        assert_eq!(submitted.load(Ordering::SeqCst), count);
                    };
                    reaches(3);
                    assert_eq!(collector.next(), Some(0));
                    reaches(4);
                    drop(collector);
                    assert!(!submitting.join().unwrap(), "told the collector is gone");
                });
            },
        );
    }

    /// A worker that panics stops the pool, as SRC-03's `poolpanic` did not:
    /// the collector stops waiting for the result that never comes, the
    /// submitter for room, and the stop says why.
    #[test]
    fn a_worker_that_panics_stops_the_pool() {
        crate::test_support::within(
            "a worker that panics stops the pool",
            crate::test_support::PATIENCE,
            move || {
                let profiled = Profiled {
                    role: "test",
                    work: State::Run,
                    full: State::Work,
                };
                let stop = Stop::new(None);
                std::thread::scope(|scope| {
                    let stages = Stages::new(scope, &stop);
                    let (mut submitter, collector) =
                        ordered(&stages, 8, 64, profiled, |item: u64| {
                            assert_ne!(item, 3, "a worker's bug");
                            item
                        });
                    let submitting = scope.spawn(move || {
                        for item in 0..1_000 {
                            if submitter.submit(item).is_err() {
                                return;
                            }
                        }
                    });
                    let results: Vec<u64> = collector.collect();
                    assert!(results.len() < 1_000);
                    submitting.join().unwrap();
                });
                let error = stop.result().unwrap_err().to_string();
                assert!(error.starts_with("internal error in test"), "{error}");
            },
        );
    }
}
