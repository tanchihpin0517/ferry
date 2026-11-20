//! What the client shows, on its way to its display, the dashboard or the
//! progress line, from what follows the servers, the client's monitor, and
//! a watch between its rounds.
//!
//! What is sent never waits for the display, but for a dry run's listing,
//! which should hold the transfer back rather than fill memory: at most
//! [`LISTED`] parts of it wait, beyond which the one sending waits for
//! room. What can be merged is merged as it waits, so that no more than one
//! of each waits: counts are added together, and the latest of the files
//! being transferred, of what was deleted, of where the time goes, and of
//! each stage's queue, takes the place of the one before. What is said
//! waits in order, as it came. A watch's round is where merging stops: what
//! comes after it is never merged into what came before.
//!
//! Once the display is gone, ended, failed, or panicked, what is said goes
//! to standard error, as it waited and as it comes, and what is listed is
//! let go of. The counts are kept, whatever becomes of the display, for the
//! summary.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub use crossbeam_channel::RecvTimeoutError;

use crate::profile::QueueMetrics;
use crate::report::{Counts, Listed, Report, Stage, Transfer};

/// How many parts of a dry run's listing may wait for the display: each of
/// up to 1 MiB, so at most 16 MiB of it.
pub const LISTED: usize = 16;

/// What the display is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Counts to add to those so far.
    Counts(Counts),
    /// What a dry run lists, in order, for standard output.
    Listed(Vec<Listed>),
    /// A line for standard error, without the `ferry: ` put before it.
    Said(String),
    /// How full the queue before each of a side's stages is, in percent.
    Queues(Vec<(Stage, u8)>),
    /// The files the Destination is receiving and writing; empty once none
    /// is.
    Transferring(Vec<Transfer>),
    /// The latest of what a run of `sync` deleted.
    Deleted(Vec<Vec<u8>>),
    /// What holds the transfer back: a side's role at work, and at what, or
    /// empty if none is.
    Busy(String),
    /// How full a side's queues were over its latest second, as its metrics
    /// said.
    Levels {
        side: String,
        queues: Vec<QueueMetrics>,
    },
    /// A watch's round starts, its number from 1: the display shows it from
    /// its start, the round before over.
    Round(u64),
    /// Between a watch's rounds: what it waits for.
    Waiting(String),
}

impl Item {
    /// What of a server's report goes to the display, if anything.
    pub fn of(report: Report) -> Option<Self> {
        Some(match report {
            Report::Counts(counts) => Self::Counts(counts),
            Report::Listed(listed) => Self::Listed(listed),
            Report::Said(said) => Self::Said(said),
            Report::Queues(queues) => Self::Queues(queues),
            Report::Transferring(files) => Self::Transferring(files),
            Report::Deleted(paths) => Self::Deleted(paths),
            Report::Failed(_)
            | Report::Done
            | Report::Profile(_)
            | Report::Snapshot(_)
            | Report::Metrics(_)
            | Report::Carried(_)
            | Report::Watching
            | Report::Changed
            | Report::Unwatched { .. } => return None,
        })
    }
}

/// A way to the display: each clone is one, and the display ends once none
/// is left, and it has taken what waits.
pub struct Display(Arc<Shared>);

/// The display's own end: once it is let go of, the display is gone.
pub struct Shows(Arc<Shared>);

/// The counts, whatever becomes of the display: a way to it that keeps
/// nothing open.
#[derive(Clone)]
pub struct Tally(Arc<Shared>);

struct Shared {
    waiting: Mutex<Waiting>,
    /// Woken as something comes, or the last way to the display goes.
    came: Condvar,
    /// Woken as a part of the listing is taken, or the display goes.
    room: Condvar,
}

struct Waiting {
    items: VecDeque<Item>,
    /// How many of `items` are parts of the listing.
    listed: usize,
    /// The ways to the display there are.
    ways: usize,
    /// The display's end is held.
    shown: bool,
    /// The counts of the round, or the transfer, so far.
    counts: Counts,
}

/// A way to the display, its own end, and its tally.
pub fn channel() -> (Display, Shows, Tally) {
    let shared = Arc::new(Shared {
        waiting: Mutex::new(Waiting {
            items: VecDeque::new(),
            listed: 0,
            ways: 1,
            shown: true,
            counts: Counts::default(),
        }),
        came: Condvar::new(),
        room: Condvar::new(),
    });
    (
        Display(Arc::clone(&shared)),
        Shows(Arc::clone(&shared)),
        Tally(shared),
    )
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.waiting.lock().expect("not poisoned")
    }
}

impl Clone for Display {
    fn clone(&self) -> Self {
        self.0.lock().ways += 1;
        Self(Arc::clone(&self.0))
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let mut waiting = self.0.lock();
        waiting.ways -= 1;
        if waiting.ways == 0 {
            self.0.came.notify_all();
        }
    }
}

impl Display {
    /// Hands the display `item`, merged with one of its kind that waits, if
    /// it can be. It never waits, but for a part of the listing, which
    /// waits for room.
    pub fn send(&self, item: Item) {
        let _ = self.try_send(item, Duration::MAX);
    }

    /// As [`Display::send`], but a part of the listing waits for room only
    /// `within`, and comes back if there was none.
    pub fn list_within(&self, listed: Vec<Listed>, within: Duration) -> Result<(), Vec<Listed>> {
        match self.try_send(Item::Listed(listed), within) {
            Err(Item::Listed(listed)) => Err(listed),
            _ => Ok(()),
        }
    }

    /// Says `line` on the display, or on standard error once it is gone.
    pub fn say(&self, line: impl Into<String>) {
        self.send(Item::Said(line.into()));
    }

    /// Hands `item` on: a part of the listing waits for room as long as
    /// `within`, and comes back if there was none.
    fn try_send(&self, item: Item, within: Duration) -> Result<(), Item> {
        let mut waiting = self.0.lock();
        match &item {
            Item::Counts(more) => waiting.counts.add(more),
            Item::Round(_) => waiting.counts = Counts::default(),
            _ => {}
        }
        if let Item::Listed(_) = item {
            // Past what an instant can say, it waits for good.
            let deadline = Instant::now().checked_add(within);
            while waiting.shown && waiting.listed >= LISTED {
                waiting = match deadline {
                    None => self.0.room.wait(waiting).expect("not poisoned"),
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(item);
                        }
                        self.0
                            .room
                            .wait_timeout(waiting, left)
                            .expect("not poisoned")
                            .0
                    }
                };
            }
            waiting.listed += usize::from(waiting.shown);
        }
        if !waiting.shown {
            if let Item::Said(line) = item {
                drop(waiting);
                crate::status::say(&line);
            }
            return Ok(());
        }
        waiting.put(item);
        self.0.came.notify_one();
        Ok(())
    }
}

impl Waiting {
    /// Puts `item` among those waiting: into one of its kind since the last
    /// round, if it merges, or last.
    fn put(&mut self, item: Item) {
        let since_round = self
            .items
            .iter()
            .rposition(|waiting| matches!(waiting, Item::Round(_)))
            .map_or(0, |round| round + 1);
        let mut item = item;
        for waiting in self.items.range_mut(since_round..).rev() {
            item = match merge(waiting, item) {
                None => return,
                Some(item) => item,
            };
        }
        self.items.push_back(item);
    }
}

/// Merges `item` into `waiting`, if it is of its kind and merges: what does
/// not, comes back.
fn merge(waiting: &mut Item, item: Item) -> Option<Item> {
    match (waiting, item) {
        (Item::Counts(counts), Item::Counts(more)) => counts.add(&more),
        (Item::Queues(levels), Item::Queues(more)) => {
            for (stage, level) in more {
                match levels.iter_mut().find(|(waiting, _)| *waiting == stage) {
                    Some((_, waiting)) => *waiting = level,
                    None => levels.push((stage, level)),
                }
            }
        }
        (Item::Transferring(files), Item::Transferring(later)) => *files = later,
        (Item::Deleted(paths), Item::Deleted(later)) => *paths = later,
        (Item::Busy(busy), Item::Busy(later)) => *busy = later,
        (Item::Waiting(what), Item::Waiting(later)) => *what = later,
        (
            Item::Levels { side, queues },
            Item::Levels {
                side: later,
                queues: latest,
            },
        ) if *side == later => *queues = latest,
        (_, item) => return Some(item),
    }
    None
}

impl Shows {
    /// The next item, as it comes within `timeout`: disconnected once no
    /// way to the display is left, and none waits.
    pub fn next(&self, timeout: Duration) -> Result<Item, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut waiting = self.0.lock();
        loop {
            if let Some(item) = waiting.items.pop_front() {
                if let Item::Listed(_) = item {
                    waiting.listed -= 1;
                    self.0.room.notify_one();
                }
                return Ok(item);
            }
            if waiting.ways == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            waiting = self
                .0
                .came
                .wait_timeout(waiting, left)
                .expect("not poisoned")
                .0;
        }
    }
}

impl Drop for Shows {
    /// The display is gone: what was said and waits goes to standard error,
    /// in order, and so does what is said from now on; the rest is let go
    /// of, and whoever waits for room stops waiting.
    fn drop(&mut self) {
        let said: Vec<String> = {
            let mut waiting = self.0.lock();
            waiting.shown = false;
            waiting.listed = 0;
            self.0.room.notify_all();
            std::mem::take(&mut waiting.items)
                .into_iter()
                .filter_map(|item| match item {
                    Item::Said(line) => Some(line),
                    _ => None,
                })
                .collect()
        };
        for line in said {
            crate::status::say(&line);
        }
    }
}

impl Tally {
    /// The counts of the round, or the transfer, so far.
    pub fn counts(&self) -> Counts {
        self.0.lock().counts
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts threads as it likes"
)]
mod tests {
    use super::*;

    fn counted(files: u64) -> Item {
        Item::Counts(Counts {
            files,
            ..Counts::default()
        })
    }

    fn listed(name: &str) -> Item {
        Item::Listed(vec![Listed {
            path: name.as_bytes().to_vec(),
            deleting: false,
        }])
    }

    fn taken(shows: &Shows) -> Vec<Item> {
        std::iter::from_fn(|| shows.next(Duration::ZERO).ok()).collect()
    }

    /// What can be merged is, as it waits, into one of its kind, in the
    /// place of the first; what is said and listed waits in order.
    #[test]
    fn what_waits_is_merged_but_what_is_said_and_listed() {
        let (display, shows, tally) = channel();
        display.send(counted(1));
        display.say("first");
        display.send(Item::Busy("the Source: read".to_owned()));
        display.send(listed("a"));
        display.send(counted(2));
        display.send(Item::Queues(vec![(Stage::Read, 10), (Stage::Send, 20)]));
        display.send(Item::Busy("the Destination: write".to_owned()));
        display.send(Item::Queues(vec![(Stage::Read, 30)]));
        display.say("second");
        display.send(listed("b"));
        assert_eq!(
            taken(&shows),
            [
                counted(3),
                Item::Said("first".to_owned()),
                Item::Busy("the Destination: write".to_owned()),
                listed("a"),
                Item::Queues(vec![(Stage::Read, 30), (Stage::Send, 20)]),
                Item::Said("second".to_owned()),
                listed("b"),
            ]
        );
        assert_eq!(tally.counts().files, 3);
    }

    /// Nothing after a round is merged into what came before it, and the
    /// tally counts the round alone.
    #[test]
    fn a_round_is_where_merging_stops() {
        let (display, shows, tally) = channel();
        display.send(counted(1));
        display.send(Item::Round(2));
        display.send(counted(4));
        display.send(counted(5));
        assert_eq!(taken(&shows), [counted(1), Item::Round(2), counted(9)]);
        assert_eq!(tally.counts().files, 9);
    }

    /// The display ends once no way to it is left, and it took what waits.
    #[test]
    fn the_display_ends_once_no_way_to_it_is_left() {
        let (display, shows, _) = channel();
        let other = display.clone();
        display.say("one");
        drop(display);
        assert_eq!(shows.next(Duration::ZERO), Ok(Item::Said("one".to_owned())));
        assert_eq!(shows.next(Duration::ZERO), Err(RecvTimeoutError::Timeout));
        drop(other);
        assert_eq!(
            shows.next(Duration::ZERO),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    /// At most [`LISTED`] parts of the listing wait: the next waits for
    /// room, or comes back once it waited as long as it may.
    #[test]
    fn the_listing_waits_for_room() {
        let (display, shows, _) = channel();
        for _ in 0..LISTED {
            display.send(listed("a"));
        }
        let back = display.list_within(vec![], Duration::from_millis(10));
        assert_eq!(back, Err(vec![]));
        // Counts never wait.
        display.send(counted(1));
        std::thread::scope(|scope| {
            let sending = scope.spawn(|| display.send(listed("b")));
            std::thread::sleep(Duration::from_millis(20));
            assert!(!sending.is_finished());
            assert_eq!(shows.next(Duration::ZERO), Ok(listed("a")));
            sending.join().unwrap();
        });
    }

    /// Once the display is gone, the counts are still kept, and what waits
    /// for room stops waiting.
    #[test]
    fn once_the_display_is_gone_nothing_waits_for_it() {
        let (display, shows, tally) = channel();
        for _ in 0..LISTED {
            display.send(listed("a"));
        }
        std::thread::scope(|scope| {
            let sending = scope.spawn(|| display.send(listed("b")));
            std::thread::sleep(Duration::from_millis(20));
            drop(shows);
            sending.join().unwrap();
        });
        display.send(counted(7));
        display.send(listed("c"));
        assert_eq!(tally.counts().files, 7);
    }
}
