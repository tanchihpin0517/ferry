//! The Destination peer: compares the manifest with its own tree, says what it
//! needs, and makes its tree match.
//!
//! Its stages run under one [`Stop`], and pass work on over channels. The
//! main thread reads packets, checks them, and hands them on
//! (`receive`). A compare thread and a pool of stat workers compare each
//! part of the manifest with the tree; the applier applies each comparison
//! as it comes, creating the directories, symlinks, and special files it
//! needs and setting attributes that differ, asks for the files it needs,
//! and alone follows each directory until it is done (`apply`). A pool
//! decompresses the record stream's frames, which a feeder hands to the
//! parser in order; the parser hands each file to a writer, and renames them
//! in batches (`write`, `durable`). Up to 16 finishers set each directory's
//! attributes and mtime once it is done, one only after every directory under
//! it, since writing into a directory changes its mtime (`finish`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::compare::Comparer;
use crate::dirs::{self, DirCache};
use crate::frame;
use crate::fsys;
use crate::mux;
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::FileKey;
use crate::record::ChannelReader;
use crate::report::{Reporter, Stage};
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

mod apply;
mod durable;
mod finish;
mod receive;
#[cfg(test)]
mod tests;
mod warnings;
mod write;

use apply::{Applying, Directories};
use finish::{finish_as_done, make_durable, restore_let_in};
use receive::read_packets;
use warnings::Warnings;
use write::write_files;

pub struct Options {
    /// The Destination's root, as path bytes.
    pub root: Vec<u8>,
    pub dry_run: bool,
    pub modify_window: i64,
    pub numeric_ids: bool,
    pub fsync: bool,
    /// Delete what the Source does not have.
    pub delete: bool,
    /// Report what is found and what goes wrong, to the client, in report
    /// packets.
    pub report: bool,
    /// The Source's root, then each directory above it, when the client
    /// found them on the same kernel: none of them is deleted, nor the
    /// Source's root gone into. Reaching one stops the transfer.
    pub protect: Vec<FileKey>,
    /// How many entries the Source may have sent and this side not compared
    /// yet, as the hello says, which sets how often it is told how far
    /// this side is: see [`crate::protocol::through_every`].
    pub in_flight_entries: u32,
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

/// What the stages share; each directory's progress is the applier's
/// alone, in [`Directories`].
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
    /// Whether this transfer made the root: the directory it is in then
    /// holds a new name, made durable at the end too.
    root_made: AtomicBool,
    reporter: Reporter,
}

impl State {
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
            root_made: AtomicBool::new(false),
            reporter: Reporter::none(),
        }
    }
}

/// How many decompressed frames, of up to [`frame::FRAME`] each, may wait for
/// the parser: 16 MiB. Beyond, decompressing waits, and so reading the
/// connection, as for a disk slower than the network.
const DECODED: usize = 8;

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
    let reporter = if options.report {
        Reporter::to_mux(mux.clone())
    } else {
        Reporter::none()
    };
    // What the stages share, each part of it shared as it may be.
    let state = &State {
        keep: Arc::new(fsys::Keep::protecting(options.protect.clone())),
        one_file_system: options.one_file_system,
        reporter: reporter.clone(),
        ..State::new(options.delete)
    };

    std::thread::scope(|scope| {
        let stages = Stages::new(scope, &stop);
        let pump = stages.spawn("pump", move |_| mux::pump(writer, outlet));
        let (data, stream) = crossbeam_channel::bounded::<Vec<u8>>(DECODED);
        // What the parser and the main thread tell the applier.
        let (events, told) = crossbeam_channel::unbounded();
        let mut directories = Directories::new(None, None);
        let files = (!options.dry_run).then(|| {
            let mux = mux.clone();
            let events = events.clone();
            let reporter = reporter.clone();
            // Each needed file's target goes to the parser, before the file
            // is asked for.
            let (targets, incoming) = crossbeam_channel::unbounded();
            // Directories are finished as they are done, by a thread of
            // their own, in that order.
            let (finishing, done) = crossbeam_channel::unbounded();
            directories = Directories::new(Some(finishing), Some(targets));
            let finishing_mux = mux.clone();
            let finisher = stages.spawn("finisher", move |stop| {
                finish_as_done(state, dirs, &finishing_mux, stop, done, options.fsync)
            });
            stages.spawn("parser", move |stop| {
                let mut stream = ChannelReader::new(stream, profile::State::Stream);
                // Kept before the finisher is waited for: it ends as the
                // applier does, once every directory is done, or once the
                // Destination stops.
                let written = stop.failing(write_files(
                    root,
                    state,
                    dirs,
                    &mux,
                    &mut stream,
                    &incoming,
                    &events,
                    options.fsync,
                    stop,
                ));
                let finished = profile::wait(profile::State::ToFinish, || finisher.join())
                    .ok_or_else(|| anyhow::Error::from(Stopped));
                let result = written.and(finished).and_then(|()| {
                    let root_made = state.root_made.load(Ordering::Relaxed);
                    make_durable(root, dirs, options.fsync, root_made)
                });
                let warnings = state.warnings.lock().expect("not poisoned").messages();
                for message in warnings {
                    tell(&reporter, format_args!("warning: {message}"));
                }
                // Kept before the stream is let go of, which ends the
                // feeder's.
                stop.failing(result)?;
                drop(stream);
                // In order after the results, which must arrive first.
                stop.done()
            })
        });

        // Frames are decompressed by a pool, and fed to the file writer in
        // order by a thread of their own.
        let (frames, decoded) = pool::gauged(
            &stages,
            pool::threads(8),
            16,
            Some(reporter.gauge(Stage::Decompress, 16)),
            Profiled {
                role: "decompress",
                work: profile::State::Decompress,
                full: profile::State::ToDecompress,
            },
            |encoded: Vec<u8>| frame::decode(&encoded),
        );
        let feeding = stages.spawn("feeder", move |stop| {
            for bytes in decoded {
                // Kept before the parser's stream ends, as this returns.
                let bytes = stop.failing(bytes)?;
                if profile::wait(profile::State::ToParser, || data.send(bytes)).is_err() {
                    // The parser stopped, and says why itself.
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
        let read = read_packets(reader, &applying, directories, events, told, frames);
        // Kept before the stream ends, which closing the frames does.
        if let Err(error) = read {
            stop.fail(error);
        }
        drop(mux);
        feeding.join();
        if let Some(files) = files {
            files.join();
        }
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
