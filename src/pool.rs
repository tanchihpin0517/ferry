//! A pool of worker threads whose results come back in the order the work
//! went in, for the stages that must keep an order: frames of the record
//! stream, and directories of the manifest.
//!
//! Work goes in through a bounded queue, so a fast submitter waits rather
//! than piling work up; results are put back in order as they arrive.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::Scope;

use anyhow::{Result, anyhow};

/// Where work goes in.
pub struct Submitter<T> {
    queue: SyncSender<(u64, T)>,
    next: u64,
}

impl<T> Submitter<T> {
    pub fn submit(&mut self, item: T) -> Result<()> {
        self.queue
            .send((self.next, item))
            .map_err(|_| anyhow!("the workers stopped"))?;
        self.next += 1;
        Ok(())
    }
}

/// Where results come out, in the order the work went in.
pub struct Collector<U> {
    results: Receiver<(u64, U)>,
    waiting: BTreeMap<u64, U>,
    next: u64,
}

impl<U> Iterator for Collector<U> {
    type Item = U;

    /// The next result, waiting for it; `None` once every submitter is gone
    /// and every result has come out.
    fn next(&mut self) -> Option<U> {
        loop {
            if let Some(result) = self.waiting.remove(&self.next) {
                self.next += 1;
                return Some(result);
            }
            let (sequence, result) = self.results.recv().ok()?;
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
        Some(result)
    }
}

/// Starts `threads` workers running `work`, with room for `capacity` items
/// waiting.
pub fn ordered<'scope, T, U>(
    scope: &'scope Scope<'scope, '_>,
    threads: usize,
    capacity: usize,
    work: impl Fn(T) -> U + Send + Sync + 'scope,
) -> (Submitter<T>, Collector<U>)
where
    T: Send + 'scope,
    U: Send + 'scope,
{
    let (queue, input) = mpsc::sync_channel::<(u64, T)>(capacity);
    let (output, results) = mpsc::channel::<(u64, U)>();
    let input = Arc::new(Mutex::new(input));
    let work = Arc::new(work);
    for _ in 0..threads.max(1) {
        let input = Arc::clone(&input);
        let output: Sender<(u64, U)> = output.clone();
        let work = Arc::clone(&work);
        scope.spawn(move || {
            loop {
                let item = input.lock().expect("not poisoned").recv();
                let Ok((sequence, item)) = item else {
                    return;
                };
                if output.send((sequence, work(item))).is_err() {
                    return;
                }
            }
        });
    }
    (
        Submitter { queue, next: 0 },
        Collector {
            results,
            waiting: BTreeMap::new(),
            next: 0,
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
        std::thread::scope(|scope| {
            let (mut submitter, mut collector) = ordered(scope, 8, 4, |item: u64| {
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
    }
}
