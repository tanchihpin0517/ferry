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
//! in batches (`write`, `durable`). The finisher sets each directory's
//! attributes and mtime once it is done, deepest first, since writing into a
//! directory changes its mtime (`finish`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::compare::Comparer;
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
    warnings: Warnings,
    /// Whether to delete what the Source does not have.
    delete: bool,
    /// What is never deleted; see [`Options::protect`].
    protect: Arc<Vec<FileKey>>,
    /// Why the transfer is to stop: something to be deleted was one to
    /// protect.
    refused: Option<String>,
    /// Entries that could not be made to match, or deleted.
    failures: u64,
    /// Paths the Source could not read.
    source_problems: u64,
    /// The directories let into, and not finished yet, with the permissions
    /// they had: given back should the transfer fail before.
    let_in: HashMap<Vec<u8>, u32>,
    /// The root, as the comparison opened it, once: what is under it is
    /// reached from it, never from its path again, which may have come to
    /// name another since, as a symlink switched or a disk mounted.
    root_directory: Option<Arc<OwnedFd>>,
    reporter: Reporter,
}

impl State {
    fn new(delete: bool) -> Self {
        Self {
            ownership: Arc::new(Ownership::here()),
            warnings: Warnings::default(),
            delete,
            protect: Arc::new(Vec::new()),
            refused: None,
            failures: 0,
            source_problems: 0,
            let_in: HashMap::new(),
            root_directory: None,
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
    let mut state = State::new(options.delete);
    state.protect = Arc::new(options.protect.clone());
    let state = Mutex::new(state);
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));
    let state = &state;
    let (mux, outlet) = mux::channel();
    // The first failure of any stage stops them all, and is the one told.
    let stop = Stop::new(Some(mux.clone()));

    std::thread::scope(|scope| {
        let stages = Stages::new(scope, &stop);
        let pump = stages.spawn("pump", move |_| mux::pump(writer, outlet));
        let reporter = if options.report {
            Reporter::to_mux(mux.clone())
        } else {
            Reporter::none()
        };
        state.lock().expect("not poisoned").reporter = reporter.clone();
        let (data, stream) = mpsc::sync_channel::<Vec<u8>>(DECODED);
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
            let (finishing, done) = mpsc::channel();
            directories = Directories::new(Some(finishing), Some(targets));
            let finisher = stages.spawn("finisher", move |_| {
                finish_as_done(root, state, done, options.fsync)
            });
            stages.spawn("parser", move |stop| {
                let mut stream = ChannelReader::new(stream, profile::State::Stream);
                // Kept before the finisher is waited for: it ends as the
                // applier does, once every directory is done, or once the
                // Destination stops.
                let written = stop.failing(write_files(
                    root,
                    state,
                    &mux,
                    &mut stream,
                    &incoming,
                    &events,
                    options.fsync,
                    stop,
                ));
                let finished = finisher.join().ok_or_else(|| anyhow::Error::from(Stopped));
                let result = written
                    .and(finished)
                    .and_then(|()| make_durable(root, state, options.fsync));
                for message in state.lock().expect("not poisoned").warnings.messages() {
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
            scope,
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
        restore_let_in(root, state);
        return Err(error);
    }
    let state = state.lock().expect("not poisoned");
    Ok(Summary {
        failed: state.failures,
        problems: state.source_problems,
    })
}

/// The directory at `path` under the root: from the root as the comparison
/// opened it, once there is one, never from its path again; before, as for
/// a root that is not a directory, from its path.
fn open_under(state: &State, root: &Path, path: &[u8]) -> io::Result<OwnedFd> {
    match &state.root_directory {
        Some(directory) => fsys::open_path(&**directory, path),
        None => fsys::open_path_from(root, path),
    }
}

/// The directory a root that is not a directory is in, and its name there.
fn root_parent(root: &Path) -> Result<(OwnedFd, Vec<u8>)> {
    let parent = match root.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = root
        .file_name()
        .with_context(|| format!("{} has no file name", root.display()))?;
    Ok((
        fsys::open_root(parent.as_os_str())?,
        name.as_bytes().to_vec(),
    ))
}

/// Whether an error is, at its root, a deletion refused because what was to
/// be deleted is to be protected; see [`fsys::Protected`].
fn is_protected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(fsys::is_protected)
    })
}
