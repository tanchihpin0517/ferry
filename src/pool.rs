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
use std::thread::Scope;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::profile::{self, State};
use crate::report::Gauge;
use crate::stop::Stopped;

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
    /// At most how many; `None` for no bound, as [`ordered_unbounded`] has.
    limit: Option<usize>,
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
        if let Some(limit) = self.limit
            && state.items >= limit
            && !state.closed
        {
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
}

/// Where results come out, in the order the work went in.
pub struct Collector<U> {
    results: Receiver<(u64, U)>,
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
    /// and every result has come out.
    fn next(&mut self) -> Option<U> {
        loop {
            if let Some(result) = self.waiting.remove(&self.next) {
                self.next += 1;
                self.in_pool.leave();
                return Some(result);
            }
            let (sequence, result) = profile::wait(State::Result, || self.results.recv()).ok()?;
            self.waiting.insert(sequence, result);
        }
    }
}

impl<U> Collector<U> {
    /// The next result, if it is ready.
    pub fn try_next(&mut self) -> Option<U> {
        while let Ok((sequence, result)) = self.results.try_recv() {
            self.waiting.insert(sequence, result);
        }
        let result = self.waiting.remove(&self.next)?;
        self.next += 1;
        self.in_pool.leave();
        Some(result)
    }
}

/// Starts `threads` workers running `work`, with room for `capacity` items
/// in the pool.
pub fn ordered<'scope, T, U>(
    scope: &'scope Scope<'scope, '_>,
    threads: usize,
    capacity: usize,
    profiled: Profiled,
    work: impl Fn(T) -> U + Send + Sync + 'scope,
) -> (Submitter<T>, Collector<U>)
where
    T: Send + 'scope,
    U: Send + 'scope,
{
    start(
        scope,
        threads,
        capacity,
        Some(capacity),
        None,
        profiled,
        work,
    )
}

/// As [`ordered`], with `gauge` measuring what waits for the workers.
pub fn gauged<'scope, T, U>(
    scope: &'scope Scope<'scope, '_>,
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
    start(
        scope,
        threads,
        capacity,
        Some(capacity),
        gauge,
        profiled,
        work,
    )
}

/// As [`ordered`], but with room for only `capacity` items waiting for the
/// workers, and none bound on results not taken yet: for a collector that
/// also feeds its submitter, which waiting for its results to be taken would
/// deadlock. Its taker must take what is ready each time it feeds the
/// submitter more, so that what waits stays as little as that.
pub fn ordered_unbounded<'scope, T, U>(
    scope: &'scope Scope<'scope, '_>,
    threads: usize,
    capacity: usize,
    profiled: Profiled,
    work: impl Fn(T) -> U + Send + Sync + 'scope,
) -> (Submitter<T>, Collector<U>)
where
    T: Send + 'scope,
    U: Send + 'scope,
{
    start(scope, threads, capacity, None, None, profiled, work)
}

fn start<'scope, T, U>(
    scope: &'scope Scope<'scope, '_>,
    threads: usize,
    capacity: usize,
    limit: Option<usize>,
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
        limit,
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
        scope.spawn(move || {
            let _thread = profile::thread(profiled.role);
            loop {
                let item = profile::wait(State::Work, || input.recv());
                let Ok((sequence, item)) = item else {
                    return;
                };
                if let Some(gauge) = &taken {
                    gauge.down();
                }
                let result = profile::wait(profiled.work, || work(item));
                if output.send((sequence, result)).is_err() {
                    return;
                }
            }
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
            waiting: BTreeMap::new(),
            next: 0,
            in_pool,
        },
    )
}

/// How many threads a stage should use: the machine's cores, within limits.
pub fn threads(at_most: usize) -> usize {
    std::thread::available_parallelism().map_or(4, |count| count.get().min(at_most))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn results_come_back_in_order() {
        crate::test_support::within(
            "results come back in order",
            crate::test_support::PATIENCE,
            move || {
                std::thread::scope(|scope| {
                    let profiled = Profiled {
                        role: "test",
                        work: State::Run,
                        full: State::Work,
                    };
                    let (mut submitter, mut collector) =
                        ordered(scope, 8, 4, profiled, |item: u64| {
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
                std::thread::scope(|scope| {
                    let (mut submitter, mut collector) =
                        ordered(scope, 4, 3, profiled, |item: u64| item);
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

    /// Without a bound on results, only those waiting for the workers are
    /// bounded: a collector that also feeds its submitter never waits on
    /// itself.
    #[test]
    fn an_unbounded_pool_takes_work_while_its_results_are_not_taken() {
        crate::test_support::within(
            "an unbounded pool takes work while its results are not taken",
            crate::test_support::PATIENCE,
            move || {
                let profiled = Profiled {
                    role: "test",
                    work: State::Run,
                    full: State::Work,
                };
                std::thread::scope(|scope| {
                    let (mut submitter, mut collector) =
                        ordered_unbounded(scope, 2, 1, profiled, |item: u64| item);
                    for item in 0..50 {
                        submitter.submit(item).unwrap();
                    }
                    drop(submitter);
                    let results: Vec<u64> = std::iter::from_fn(|| collector.next()).collect();
                    assert_eq!(results, (0..50).collect::<Vec<_>>());
                });
            },
        );
    }
}
