//! The Destination peer: compares the manifest with its own tree, says what it
//! needs, and makes its tree match.
//!
//! The main thread reads packets: it compares each manifest message as it
//! arrives, creates the directories, symlinks, and special files it needs,
//! sets attributes that differ, and hands data packets to a writer thread,
//! which writes the files in the record stream. When the stream ends, the
//! writer thread sets the directories' attributes and mtimes, deepest first,
//! since writing into a directory changes its mtime.

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
use crate::manifest::{Entry, FileId};
use crate::mux;
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::FileKey;
use crate::record::ChannelReader;
use crate::report::{Reporter, Stage};
use crate::stage::Stages;
use crate::stop::Stop;

mod apply;
mod durable;
mod finish;
mod receive;
#[cfg(test)]
mod tests;
mod warnings;
mod write;

use apply::Open;
use finish::{finish_as_done, finish_directories, make_durable, restore_let_in};
use receive::read_packets;
use warnings::Warnings;
use write::{Target, write_files};

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

/// What the main thread and the writer thread share.
struct State {
    ownership: Arc<Ownership>,
    /// Files whose content is coming, by ID: the directory each is in, for
    /// it to wait for them, or `None` for the root itself. The parser has
    /// the rest of each, sent on `targets` before the file is asked for.
    needed: HashMap<FileId, Option<Arc<[u8]>>>,
    targets: Option<crossbeam_channel::Sender<(FileId, Target)>>,
    /// The directories announced and not finished yet, by path; the root's
    /// is empty. See [`Open`].
    open: HashMap<Vec<u8>, Open>,
    /// The directories whose parts were applied, as the manifest went into
    /// them, depth first: each one's parent is below it.
    stack: Vec<Vec<u8>>,
    /// Where a directory done goes to be finished, in the order it is done.
    finishing: Option<mpsc::Sender<(Vec<u8>, Entry)>>,
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
            needed: HashMap::new(),
            targets: None,
            open: HashMap::new(),
            stack: Vec::new(),
            finishing: None,
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
        let files = (!options.dry_run).then(|| {
            let mux = mux.clone();
            let (targets, incoming) = crossbeam_channel::unbounded();
            state.lock().expect("not poisoned").targets = Some(targets);
            let reporter = reporter.clone();
            // Directories are finished as they are done, by a thread of
            // their own, in that order.
            let (finishing, done) = mpsc::channel();
            state.lock().expect("not poisoned").finishing = Some(finishing);
            let finisher = stages.spawn("finisher", move |_| {
                finish_as_done(root, state, done, options.fsync)
            });
            stages.spawn("parser", move |stop| {
                let mut stream = ChannelReader::new(stream, profile::State::Stream);
                let written = write_files(
                    root,
                    state,
                    &mux,
                    &mut stream,
                    &incoming,
                    options.fsync,
                    stop,
                );
                let result = match written {
                    Ok(()) => finish_directories(state, finisher),
                    // Nothing more is finished: the finisher ends.
                    Err(error) => {
                        state.lock().expect("not poisoned").finishing.take();
                        finisher.join();
                        Err(error)
                    }
                }
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
        let mut frames = frames;
        let read = read_packets(
            reader,
            &mux,
            &comparer,
            root,
            state,
            options,
            &reporter,
            &mut frames,
            &stop,
        );
        // Kept before the stream ends, which closing the frames does.
        if let Err(error) = read {
            stop.fail(error);
        }
        drop(frames);
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
