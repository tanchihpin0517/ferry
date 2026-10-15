//! What the peers tell the user: progress counts, the paths a dry run lists,
//! and lines about what goes wrong. A peer reports through a [`Reporter`],
//! and the client shows the reports; a peer never prints.
//!
//! Counts are kept in the reporter and sent as they change, at most every
//! [`EVERY`], so that counting a file costs no more than an atomic add, and
//! the reports stay few however many files there are.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How often changed counts are sent, at most.
const EVERY: Duration = Duration::from_millis(100);

/// One report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Report {
    /// Counts to add to those so far.
    Counts(Counts),
    /// A line a dry run lists, for standard output.
    Listed(Vec<u8>),
    /// A line for standard error, such as a warning or what could not be
    /// copied, without the `ferry: ` the client puts before it.
    Said(String),
}

/// What a transfer counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Entries compared.
    pub entries: u64,
    /// Files whose content is needed.
    pub needed: u64,
    /// Files sent, or written.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
    /// The bytes that crossed the connection for them, compressed.
    pub wire: u64,
    /// Paths the Source could not read.
    pub problems: u64,
    /// Entries that could not be made to match, or deleted.
    pub failed: u64,
}

impl Counts {
    pub fn add(&mut self, other: &Counts) {
        self.entries += other.entries;
        self.needed += other.needed;
        self.files += other.files;
        self.bytes += other.bytes;
        self.wire += other.wire;
        self.problems += other.problems;
        self.failed += other.failed;
    }

    fn is_empty(&self) -> bool {
        *self == Counts::default()
    }
}

/// One of the [`Counts`].
#[derive(Debug, Clone, Copy)]
pub enum Count {
    Entries,
    Needed,
    Files,
    Bytes,
    Wire,
    Problems,
    Failed,
}

/// Where a peer reports to; or nowhere, for a peer whose reports no one
/// shows. Clones report to the same place.
#[derive(Clone, Default)]
pub struct Reporter(Option<Arc<Shared>>);

struct Shared {
    counts: [AtomicU64; 7],
    out: Mutex<Sender<Report>>,
    started: Instant,
    /// When counts were last sent, in milliseconds since `started`.
    sent: AtomicU64,
}

impl Reporter {
    /// A reporter whose reports go nowhere.
    pub fn none() -> Self {
        Self(None)
    }

    /// A reporter whose reports go to `out`.
    pub fn new(out: Sender<Report>) -> Self {
        Self(Some(Arc::new(Shared {
            counts: Default::default(),
            out: Mutex::new(out),
            started: Instant::now(),
            sent: AtomicU64::new(0),
        })))
    }

    /// Adds to a count, and sends the counts if they are due.
    pub fn add(&self, count: Count, amount: u64) {
        let Some(shared) = &self.0 else {
            return;
        };
        if amount == 0 {
            return;
        }
        shared.counts[count as usize].fetch_add(amount, Ordering::Relaxed);
        let now = shared.started.elapsed().as_millis() as u64;
        let sent = shared.sent.load(Ordering::Relaxed);
        if now.saturating_sub(sent) >= EVERY.as_millis() as u64
            && shared
                .sent
                .compare_exchange(sent, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.flush();
        }
    }

    /// Reports a line a dry run lists.
    pub fn listed(&self, line: Vec<u8>) {
        self.send(Report::Listed(line));
    }

    /// Reports a line for standard error.
    pub fn say(&self, line: impl Into<String>) {
        self.send(Report::Said(line.into()));
    }

    /// Sends the counts added since they were last sent, if any. A peer
    /// flushes when it is done, so that the last counts are not left behind.
    pub fn flush(&self) {
        let Some(shared) = &self.0 else {
            return;
        };
        let take = |count: Count| shared.counts[count as usize].swap(0, Ordering::Relaxed);
        let counts = Counts {
            entries: take(Count::Entries),
            needed: take(Count::Needed),
            files: take(Count::Files),
            bytes: take(Count::Bytes),
            wire: take(Count::Wire),
            problems: take(Count::Problems),
            failed: take(Count::Failed),
        };
        if !counts.is_empty() {
            self.send(Report::Counts(counts));
        }
    }

    fn send(&self, report: Report) {
        if let Some(shared) = &self.0 {
            // No one listening any more is no reason to stop a transfer.
            let _ = shared.out.lock().expect("not poisoned").send(report);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn received(reports: &mpsc::Receiver<Report>) -> (Counts, Vec<Report>) {
        let mut counts = Counts::default();
        let mut others = Vec::new();
        for report in reports.try_iter() {
            match report {
                Report::Counts(more) => counts.add(&more),
                other => others.push(other),
            }
        }
        (counts, others)
    }

    #[test]
    fn counts_arrive_once_flushed_and_lines_at_once() {
        let (out, reports) = mpsc::channel();
        let reporter = Reporter::new(out);
        reporter.add(Count::Files, 2);
        reporter.add(Count::Bytes, 10);
        reporter.say("a warning");
        reporter.listed(b"dir/".to_vec());
        reporter.clone().add(Count::Files, 1);
        reporter.flush();
        reporter.flush();
        let (counts, others) = received(&reports);
        assert_eq!(counts.files, 3);
        assert_eq!(counts.bytes, 10);
        assert_eq!(
            others,
            [
                Report::Said("a warning".into()),
                Report::Listed(b"dir/".to_vec())
            ]
        );
    }

    #[test]
    fn counts_are_sent_as_they_change_but_not_each_time() {
        let (out, reports) = mpsc::channel();
        let reporter = Reporter::new(out);
        std::thread::sleep(EVERY);
        for _ in 0..1_000 {
            reporter.add(Count::Entries, 1);
        }
        let sent = reports.try_iter().count();
        assert!((1..10).contains(&sent), "{sent} reports");
        reporter.flush();
    }

    #[test]
    fn a_reporter_to_nowhere_does_nothing() {
        let reporter = Reporter::none();
        reporter.add(Count::Failed, 1);
        reporter.say("ignored");
        reporter.flush();
    }
}
