//! What the peers tell the user: progress counts, the paths a dry run lists,
//! and lines about what goes wrong. A peer reports through a [`Reporter`],
//! which a server's peer sends to the client in report packets, and the
//! client shows the reports; a peer never prints.
//!
//! Counts are kept in the reporter and sent as they change, at most every
//! [`EVERY`], so that counting a file costs no more than an atomic add, and
//! the reports stay few however many files there are.
//!
//! A peer also reports how full the queue before each of its stages is, for
//! the dashboard: each queue has a [`Gauge`], and a reporter with gauges
//! sends their levels every [`QUEUES_EVERY`]. The Destination reports, every
//! [`TRANSFERRING_EVERY`], the files it is receiving and writing.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::mux::Mux;

/// How often changed counts are sent, at most.
const EVERY: Duration = Duration::from_millis(100);

/// How often the queues' levels are sent.
const QUEUES_EVERY: Duration = Duration::from_millis(250);

/// The most bytes of listed lines in one report, well under a packet's
/// limit, however long the paths.
const LISTED_BYTES: usize = 1 << 20;

/// How often the files being received and written are sent. Often in unit
/// tests, to see them.
pub const TRANSFERRING_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(1)
} else {
    Duration::from_secs(1)
};

/// One report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Report {
    /// Counts to add to those so far.
    Counts(Counts),
    /// Lines a dry run lists, in order, for standard output: a part's at a
    /// time, so that millions of lines are not millions of packets.
    Listed(Vec<Vec<u8>>),
    /// A line for standard error, such as a warning or what could not be
    /// copied, without the `ferry: ` the client puts before it.
    Said(String),
    /// How full the queue before each of a peer's stages is, in percent.
    Queues(Vec<(Stage, u8)>),
    /// The files the Destination is receiving and writing, those it started
    /// first first. Empty once none is.
    Transferring(Vec<Transfer>),
    /// Where a server's threads spent their time, sent once it is done,
    /// when the run is profiled.
    Profile(crate::profile::Profile),
    /// Where a server's threads are, and for how long, as the client asked
    /// on the control connection.
    Snapshot(Vec<crate::profile::Now>),
    /// Where a server's threads spent the last interval, sent on the control
    /// connection every second.
    Metrics(crate::profile::Metrics),
    /// What holds the transfer back, as the client sees it: a side's role at
    /// work, and at what, or empty if none is. The client's own, for its
    /// display.
    Busy(String),
}

/// A file the Destination is receiving and writing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: u64,
    /// Its path, relative to the destination.
    pub path: String,
    pub size: u64,
    /// Its bytes received so far, handed to its writer.
    pub done: u64,
}

/// A stage of the pipeline, by the queue before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Stage {
    /// The Source's files handed to its readers.
    Read,
    /// The Source's frames waiting to be compressed.
    Compress,
    /// The Source's data packets waiting for the connection.
    Network,
    /// The Destination's frames waiting to be decompressed.
    Decompress,
    /// The Destination's files handed to its writers.
    Write,
}

impl Stage {
    /// Every stage, in the order data goes through them.
    pub const ALL: [Stage; 5] = [
        Stage::Read,
        Stage::Compress,
        Stage::Network,
        Stage::Decompress,
        Stage::Write,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::Read => "read",
            Stage::Compress => "compress",
            Stage::Network => "network",
            Stage::Decompress => "decompress",
            Stage::Write => "write",
        }
    }
}

/// How full a queue is: what waits in it, out of the room it has. Clones
/// measure the same queue.
#[derive(Clone)]
pub struct Gauge(Arc<GaugeLevel>);

struct GaugeLevel {
    level: AtomicUsize,
    capacity: usize,
}

impl Gauge {
    /// A gauge for a queue with room for `capacity`.
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(GaugeLevel {
            level: AtomicUsize::new(0),
            capacity: capacity.max(1),
        }))
    }

    /// Something went into the queue.
    pub fn up(&self) {
        self.0.level.fetch_add(1, Ordering::Relaxed);
    }

    /// Something came out of it.
    pub fn down(&self) {
        let _ = self
            .0
            .level
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |level| {
                level.checked_sub(1)
            });
    }

    /// How much waits in it now.
    pub fn set(&self, level: usize) {
        self.0.level.store(level, Ordering::Relaxed);
    }

    /// How many wait in it now.
    pub fn level(&self) -> usize {
        self.0.level.load(Ordering::Relaxed)
    }

    /// How full it is, in percent.
    pub fn percent(&self) -> u8 {
        let level = self.0.level.load(Ordering::Relaxed).min(self.0.capacity);
        (level * 100 / self.0.capacity) as u8
    }
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
    /// The bytes of the files whose content is needed.
    pub to_send: u64,
    /// The bytes of content received so far, as they come, before their
    /// files are done.
    pub received: u64,
    /// The files whose content has all been received, before they are done:
    /// counted one by one, where `files` goes up a batch of renames at a
    /// time.
    pub arrived: u64,
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
        self.to_send += other.to_send;
        self.received += other.received;
        self.arrived += other.arrived;
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
    ToSend,
    Received,
    Arrived,
    Wire,
    Problems,
    Failed,
}

/// Where a peer reports to; or nowhere, for a peer whose reports no one
/// shows. Clones report to the same place.
#[derive(Clone, Default)]
pub struct Reporter(Option<Arc<Shared>>);

struct Shared {
    counts: [AtomicU64; 10],
    /// The queues reported, once any is.
    gauges: Mutex<Vec<(Stage, Gauge)>>,
    /// Where reports go, until the reporter is closed.
    out: Mutex<Option<Out>>,
    started: Instant,
    /// When counts were last sent, in milliseconds since `started`.
    sent: AtomicU64,
}

#[derive(Clone)]
enum Out {
    Channel(Sender<Report>),
    /// In report packets, among the peer's messages.
    Mux(Mux),
}

impl Reporter {
    /// A reporter whose reports go nowhere.
    pub fn none() -> Self {
        Self(None)
    }

    /// A reporter whose reports go to `out`.
    pub fn new(out: Sender<Report>) -> Self {
        Self::to(Out::Channel(out))
    }

    /// A reporter whose reports go over a peer's connection, in report
    /// packets. It must be closed before the connection's pump can end.
    pub fn to_mux(mux: Mux) -> Self {
        Self::to(Out::Mux(mux))
    }

    fn to(out: Out) -> Self {
        Self(Some(Arc::new(Shared {
            counts: Default::default(),
            gauges: Mutex::new(Vec::new()),
            out: Mutex::new(Some(out)),
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

    /// A gauge for the queue before `stage`, with room for `capacity`, whose
    /// level is reported every [`QUEUES_EVERY`] until the reporter is closed.
    pub fn gauge(&self, stage: Stage, capacity: usize) -> Gauge {
        let gauge = Gauge::new(capacity);
        self.watch(stage, &gauge);
        gauge
    }

    /// Reports the level of `gauge`, the queue before `stage`, every
    /// [`QUEUES_EVERY`] until the reporter is closed.
    pub fn watch(&self, stage: Stage, gauge: &Gauge) {
        let Some(shared) = &self.0 else {
            return;
        };
        let mut gauges = shared.gauges.lock().expect("not poisoned");
        gauges.push((stage, gauge.clone()));
        if gauges.len() == 1 {
            // A weak handle, so that the thread never keeps the reports'
            // way out alive: it ends once the reporter is closed or gone.
            let weak = Arc::downgrade(shared);
            std::thread::spawn(move || report_queues(&weak));
        }
    }

    /// Reports lines a dry run lists, in reports of at most
    /// [`LISTED_BYTES`]. A dry run lists faster than the client may show, so
    /// they wait, as the manifest does, while 8 MiB of messages wait to be
    /// sent, rather than piling up in memory.
    pub fn listed(&self, lines: Vec<Vec<u8>>) {
        let mut report = Vec::new();
        let mut bytes = 0;
        for line in lines {
            if bytes + line.len() > LISTED_BYTES && !report.is_empty() {
                self.send_open(Report::Listed(std::mem::take(&mut report)), true);
                bytes = 0;
            }
            bytes += line.len();
            report.push(line);
        }
        if !report.is_empty() {
            self.send_open(Report::Listed(report), true);
        }
    }

    /// Reports a line for standard error.
    pub fn say(&self, line: impl Into<String>) {
        self.send(Report::Said(line.into()));
    }

    /// Reports the files being received and written, or with none, that none
    /// is.
    pub fn transferring(&self, files: Vec<Transfer>) {
        self.send(Report::Transferring(files));
    }

    /// Whether reports go anywhere.
    pub fn is_open(&self) -> bool {
        self.0.is_some()
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
            to_send: take(Count::ToSend),
            received: take(Count::Received),
            arrived: take(Count::Arrived),
            wire: take(Count::Wire),
            problems: take(Count::Problems),
            failed: take(Count::Failed),
        };
        if !counts.is_empty() {
            self.send(Report::Counts(counts));
        }
    }

    /// Sends the last counts, and lets go of where reports go, for every
    /// clone: a peer's pump ends only once nothing can hand it a packet.
    pub fn close(&self) {
        self.flush();
        if let Some(shared) = &self.0 {
            shared.out.lock().expect("not poisoned").take();
        }
    }

    fn send(&self, report: Report) {
        self.send_open(report, false);
    }

    /// Sends a report, `paced` or not, and says whether the reporter was
    /// still open. The way out is taken from the lock first, so that a paced
    /// report waits without holding up the others.
    fn send_open(&self, report: Report, paced: bool) -> bool {
        let Some(shared) = &self.0 else {
            return false;
        };
        let Some(out) = shared.out.lock().expect("not poisoned").clone() else {
            return false;
        };
        // No one listening any more is no reason to stop a transfer.
        match out {
            Out::Channel(out) => {
                let _ = out.send(report);
            }
            Out::Mux(mux) if paced => {
                let _ = mux.message_paced(&report);
            }
            Out::Mux(mux) => {
                let _ = mux.message(&report);
            }
        }
        true
    }
}

/// Sends the queues' levels every [`QUEUES_EVERY`] until the reporter is
/// closed.
fn report_queues(shared: &Weak<Shared>) {
    loop {
        std::thread::sleep(QUEUES_EVERY);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let levels: Vec<(Stage, u8)> = shared
            .gauges
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|(stage, gauge)| (*stage, gauge.percent()))
            .collect();
        if !Reporter(Some(Arc::clone(&shared))).send_open(Report::Queues(levels), false) {
            return;
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
        reporter.listed(vec![b"dir/".to_vec(), b"dir/file".to_vec()]);
        reporter.listed(Vec::new());
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
                Report::Listed(vec![b"dir/".to_vec(), b"dir/file".to_vec()])
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
    fn queues_are_reported_until_closed() {
        let (out, reports) = mpsc::channel();
        let reporter = Reporter::new(out);
        let gauge = reporter.gauge(Stage::Compress, 4);
        gauge.up();
        gauge.up();
        gauge.up();
        gauge.down();
        let report = reports.recv_timeout(QUEUES_EVERY * 4).unwrap();
        assert_eq!(report, Report::Queues(vec![(Stage::Compress, 50)]));
        reporter.close();
        std::thread::sleep(QUEUES_EVERY * 2);
        while reports.try_recv().is_ok() {}
        std::thread::sleep(QUEUES_EVERY * 2);
        assert!(reports.try_recv().is_err());
    }

    #[test]
    fn a_reporter_to_nowhere_does_nothing() {
        let reporter = Reporter::none();
        reporter.add(Count::Failed, 1);
        reporter.say("ignored");
        reporter.flush();
    }

    #[test]
    fn listed_lines_go_in_reports_of_at_most_a_mebibyte() {
        let (out, reports) = mpsc::channel();
        let reporter = Reporter::new(out);
        let line = vec![b'x'; 4096];
        reporter.listed(vec![line.clone(); 1000]);
        let sizes: Vec<usize> = reports
            .try_iter()
            .map(|report| match report {
                Report::Listed(lines) => {
                    assert!(lines.iter().map(Vec::len).sum::<usize>() <= LISTED_BYTES);
                    lines.len()
                }
                other => panic!("{other:?}"),
            })
            .collect();
        // 256 lines of 4 KiB make a mebibyte.
        assert_eq!(sizes, [256, 256, 256, 232]);
    }
}
