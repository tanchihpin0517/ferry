//! The Destination peer: compares the manifest with its own tree, says what it
//! needs, and makes its tree match.
//!
//! Its stages run under one [`Stop`], and pass work on over channels. The
//! main thread reads packets, checks them, applies the root, and hands the
//! rest on (`receive`). A compare thread and a pool of stat workers compare
//! each part of the manifest with the tree. The decider takes each
//! comparison as it comes, and what the other stages tell it, one at a
//! time, on a thread of its own, and says what to do, doing no I/O itself
//! (`decider`): it has what a part changes made by up to 16 makers, a
//! directory each, which create the directories, symlinks, and special
//! files it needs and set attributes that differ; it asks for a part's
//! files as it is compared, and alone follows each directory until it is
//! done, and each file until it is settled (`apply`). A directory's extras
//! are deleted on a lane of its own, which only that directory's parts
//! wait for (`apply`, `crate::remove`). A pool decodes the blocks,
//! decompressing each and checking what it holds, which a feeder hands to
//! the decider in order; it hands each file, or segment of a large one, to
//! one of 16 writers, and the files written to the renamers in batches,
//! each named once its part is made (`write`, `rename`). Up to 16
//! finishers set each directory's attributes and mtime once it is done, as
//! the decider hands it over, one only after every directory under it,
//! since writing into a directory changes its mtime (`finish`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::block;
use crate::compare::Comparer;
use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::mux::{self, Mux};
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::{Credit, FileKey};
use crate::report::{Reporter, Stage};
use crate::stage::Stages;
use crate::stop::Stop;

mod apply;
mod decider;
mod finish;
mod receive;
mod rename;
#[cfg(test)]
mod tests;
mod warnings;
mod write;

use apply::{Applying, Outlets};
use finish::restore_let_in;
use receive::{Incoming, read_packets};
use warnings::Warnings;
use write::Arrival;

pub struct Options {
    /// The Destination's root, as path bytes.
    pub root: Vec<u8>,
    pub dry_run: bool,
    pub modify_window: i64,
    pub numeric_ids: bool,
    /// Delete what the Source does not have.
    pub delete: bool,
    /// Where to report what is found and what goes wrong: to the client, on
    /// the server's control connection; or nowhere.
    pub report: Reporter,
    /// The Source's root, then each directory above it, when the client
    /// found them on the same kernel: none of them is deleted, nor the
    /// Source's root gone into. Reaching one stops the transfer.
    pub protect: Vec<FileKey>,
    /// How many entries the Source may have sent and this side not compared
    /// yet, as the hello says, which sets how often it is told how far
    /// this side is: see [`crate::protocol::through_every`].
    pub in_flight_entries: u32,
    /// The data credit to give the Source, as the hello says; see
    /// [`crate::protocol::data_credit`].
    pub data_credit: u64,
    /// Delete in no directory on another filesystem than the root's: `-x`.
    pub one_file_system: bool,
}

/// What went wrong, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    /// Entries that could not be made to match, or deleted.
    pub failed: u64,
    /// Paths the Source could not read.
    pub problems: u64,
}

/// Reports a line for standard error.
fn tell(reporter: &Reporter, message: std::fmt::Arguments) {
    reporter.say(message.to_string());
}

/// What the stages share; each directory's progress is the decider's
/// alone.
struct State {
    ownership: Arc<Ownership>,
    /// What could not be done, from any stage, told at the end.
    warnings: Mutex<Warnings>,
    /// Whether to delete what the Source does not have.
    delete: bool,
    /// What is never deleted; see [`Options::protect`]. With `-x`, the
    /// root's filesystem, to delete on no other, once the root is opened.
    keep: Arc<fsys::Keep>,
    /// Stay on the root's filesystem: [`Options::one_file_system`].
    one_file_system: bool,
    /// Entries that could not be made to match, or deleted.
    failures: AtomicU64,
    /// Paths the Source could not read.
    source_problems: AtomicU64,
    /// The directories let into, and not finished yet, with the permissions
    /// they had: given back should the transfer fail before.
    let_in: Mutex<HashMap<Vec<u8>, u32>>,
}

impl State {
    /// Warns of what an executor could not set: its owner, told, says so.
    fn warn(&self, warned: apply::Warned) {
        if warned.is_empty() {
            return;
        }
        let mut warnings = self.warnings.lock().expect("not poisoned");
        for (warning, shown) in warned {
            warnings.add(warning, &shown);
        }
    }

    fn new(delete: bool) -> Self {
        Self {
            ownership: Arc::new(Ownership::here()),
            warnings: Mutex::default(),
            delete,
            keep: Arc::new(fsys::Keep::default()),
            one_file_system: false,
            failures: AtomicU64::new(0),
            source_problems: AtomicU64::new(0),
            let_in: Mutex::default(),
        }
    }
}

/// Decodes what the receiver hands on: a block decompressed, and what it
/// holds of each file checked. The block's share of the data credit is
/// given back once its bytes are let go of: once its files are written, or
/// failed. So what the Source has sent and this side holds, in whatever
/// stage, never exceeds the credit, and nothing here waits for room.
fn decode(incoming: Incoming, giving: &Arc<Giving>) -> Result<Arrival> {
    Ok(match incoming {
        Incoming::Block(encoded) => {
            let block = block::decode(encoded)?;
            let bytes = block::cost(block.payload().len());
            let giving = Arc::clone(giving);
            Arrival::Parts(block.into_parts_releasing(move || giving.give(bytes)))
        }
        Incoming::Hash(hash) => Arrival::Hash(hash),
        Incoming::End => Arrival::End,
    })
}

/// Gives the Source back its credit as blocks are let go of, an eighth of
/// it at a time: a message for each block, each a hop through every thread
/// between the two peers, made a copy of a large file a tenth slower. Never
/// more than an eighth is kept back, so the Source, which waits only once
/// it has used most of the credit, always gets enough back to go on.
struct Giving {
    mux: Mux,
    kept: AtomicU64,
    /// How much is given back at a time.
    each: u64,
}

impl Giving {
    fn new(mux: Mux, credit: u64) -> Self {
        Self {
            mux,
            kept: AtomicU64::new(0),
            each: credit / 8,
        }
    }

    fn give(&self, bytes: u64) {
        let kept = self.kept.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if kept >= self.each {
            let bytes = self.kept.swap(0, Ordering::Relaxed);
            // Once the Destination is gone, there is no one to tell.
            if bytes > 0 {
                let _ = self.mux.message(&Credit { bytes });
            }
        }
    }
}

/// Runs the Destination over a connection. A dry run reports each path it
/// lists, in manifest order. The Destination reports the counts and what it
/// cannot do; the Source reports the paths it cannot read.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
) -> Result<Summary> {
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));
    // The directories under the root, shared by the stages that work there.
    let dirs = &DirCache::new(root.as_os_str(), dirs::KEPT);
    let (mux, outlet) = mux::channel();
    // The first failure of any stage stops them all, and is the one told.
    let stop = Stop::new(Some(mux.clone()));
    let reporter = options.report.clone();
    // What the stages share, each part of it shared as it may be.
    let state = &State {
        keep: Arc::new(fsys::Keep::protecting(options.protect.clone())),
        one_file_system: options.one_file_system,
        ..State::new(options.delete)
    };

    std::thread::scope(|scope| {
        let stages = Stages::new(scope, &stop);
        let pump = stages.spawn("pump", move |_| mux::pump(writer, outlet));
        // The Source may send this much, and more as it is given back.
        let credit = crate::protocol::data_credit(options.data_credit);
        if !options.dry_run {
            let _ = mux.message(&Credit { bytes: credit });
        }
        // Bounded by the credit, as everything after the connection is.
        let (data, arrivals) = crossbeam_channel::unbounded::<Arrival>();
        // What the main thread tells the decider.
        let (events, told) = crossbeam_channel::unbounded();
        // The blocks go to the decider, in a run.
        let outlets = Outlets {
            arrivals: (!options.dry_run).then_some(arrivals),
        };

        // Blocks are decoded by a pool, and fed to the decider in order by a
        // thread of their own. The pool has room for as many blocks as the
        // credit, so the receiver never waits for it.
        let decoding = (credit / block::cost(0)) as usize + 1;
        let giving = Arc::new(Giving::new(mux.clone(), credit));
        let (blocks, decoded) = pool::gauged(
            &stages,
            pool::threads(8),
            decoding,
            Some(reporter.gauge(Stage::Decompress, 16)),
            Profiled {
                role: "decompress",
                work: profile::State::Decompress,
                full: profile::State::ToDecompress,
            },
            move |incoming: Incoming| decode(incoming, &giving),
        );
        let feeding = stages.spawn("feeder", move |stop| {
            for arrival in decoded {
                // Kept before the decider's blocks end, as this returns.
                let arrival = stop.failing(arrival)?;
                if profile::wait(profile::State::ToDecider, || data.send(arrival)).is_err() {
                    // The decider stopped, and says why itself.
                    break;
                }
            }
            Ok(())
        });
        let applying = Applying {
            mux: &mux,
            comparer: &comparer,
            root,
            state,
            options,
            reporter: &reporter,
            dirs,
            stop: &stop,
        };
        let read = read_packets(reader, &applying, outlets, events, told, blocks);
        // Kept before the blocks end, which closing them does.
        if let Err(error) = read {
            stop.fail(error);
        }
        drop(mux);
        feeding.join();
        // With every handle dropped, the pump writes what is queued and ends.
        stop.close();
        reporter.close();
        pump.join();
    });
    if let Err(error) = stop.result() {
        restore_let_in(state, dirs);
        return Err(error);
    }
    Ok(Summary {
        failed: state.failures.load(Ordering::Relaxed),
        problems: state.source_problems.load(Ordering::Relaxed),
    })
}

/// The directory a root is in, as its path names it: `.` for one of a name
/// alone, as `b` or `b/`.
fn parent_of(root: &Path) -> &Path {
    match root.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The directory a root that is not a directory is in, and its name there.
fn root_parent(root: &Path) -> Result<(OwnedFd, Vec<u8>)> {
    let parent = parent_of(root);
    let name = root
        .file_name()
        .with_context(|| format!("{} has no file name", root.display()))?;
    let parent =
        fsys::open_root(parent.as_os_str()).map_err(|error| missing_parent(root, error))?;
    Ok((parent, name.as_bytes().to_vec()))
}

/// An error making or opening the root, said as its directory missing where
/// it is: then nothing can be copied, and the transfer stops.
fn missing_parent(root: &Path, error: io::Error) -> anyhow::Error {
    if error.kind() != io::ErrorKind::NotFound {
        return error.into();
    }
    let parent = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    MissingParent(format!(
        "the directory {}, where {} goes, does not exist",
        parent.display(),
        root.display()
    ))
    .into()
}

/// The directory the root goes in does not exist.
#[derive(Debug)]
struct MissingParent(String);

impl std::fmt::Display for MissingParent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MissingParent {}

/// Whether an error is, at its root, a deletion refused because what was to
/// be deleted is to be protected; see [`fsys::Protected`].
fn is_protected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(fsys::is_protected)
    })
}

/// Whether an error ends the transfer, rather than failing its entry alone:
/// the drive is full, or out of the user's quota, or read-only, so that
/// every entry after it would fail the same way.
fn ends_the_transfer(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(io_ends_the_transfer)
    })
}

/// As [`ends_the_transfer`], for an I/O error.
fn io_ends_the_transfer(error: &io::Error) -> bool {
    use rustix::io::Errno;
    Errno::from_io_error(error)
        .is_some_and(|errno| [Errno::NOSPC, Errno::DQUOT, Errno::ROFS].contains(&errno))
}
