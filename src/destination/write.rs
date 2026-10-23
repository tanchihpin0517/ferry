//! Writing the files the Source sends: the parts of each block, checked by
//! the decoders, are handed to a pool of writers, a whole file or a segment
//! of a large one each; each file is written without a name where the
//! filesystem can make one, or under a temporary name, then given its own
//! in batches.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::block::{self, Part, Shared};
use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::inject;
use crate::interrupt;
use crate::manifest::{self, Entry, FileId};
use crate::pool;
use crate::profile;
use crate::protocol::FileHash;
use crate::report::{Count, Gauge, Reporter, Stage, TRANSFERRING_EVERY, Transfer};
use crate::source::display;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::apply::{Event, Setters, Warned, set_attributes};
use super::durable::{self, RENAMING, Renamed, Renaming};
use super::{State, root_parent};

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
#[derive(Clone)]
pub(super) struct Target {
    /// Shared by the files of a directory.
    pub(super) directory: Arc<[u8]>,
    pub(super) entry: Entry,
    /// The root itself, which is a file: said so, rather than taken from a
    /// name, which a manifest sets.
    pub(super) is_root: bool,
    /// Whether something had its name, as the comparison listed the
    /// directory: if not, a file without a name is given it at once.
    pub(super) exists: bool,
}

/// What the decoders hand on, in the order it came.
pub(super) enum Arrival {
    /// What a block holds of each file, checked.
    Parts(Vec<Part>),
    /// The hash of a file sent in segments, merged from theirs.
    Hash(FileHash),
    /// The Source sent every block it will.
    End,
}

/// What became of one file.
enum Written {
    /// Written and checked, without its name, and, once its fsync thread is
    /// done with it, pushed to the drive: ready to be given its name with its
    /// batch.
    Ready(Ready),
    Failed(anyhow::Error),
}

/// Where a file written is, until it takes its name.
pub(super) enum Unplaced {
    /// Without a name, open: it goes when it is closed, however the process
    /// ends, so it is not registered.
    Unnamed(File),
    /// Under a temporary name, registered: removed when dropped, unless it
    /// was renamed into place, so a batch that never comes to be renamed
    /// leaves nothing behind.
    Named {
        temporary: Vec<u8>,
        registered: interrupt::Registered,
    },
}

/// A file written, waiting for its batch.
pub(super) struct Ready {
    /// Shared with its registration, if any.
    pub(super) directory: Arc<OwnedFd>,
    pub(super) name: Vec<u8>,
    pub(super) unplaced: Unplaced,
    /// Whether something had its name, as compared.
    pub(super) exists: bool,
    /// Kept open to set a barrier on its drive, where that needs a file:
    /// one of a batch's on each drive, if any. Under a temporary name, kept
    /// for its fsync too, until that is done.
    pub(super) file: Option<File>,
    /// Its drive, for one barrier per drive.
    pub(super) device: u64,
    /// The root is this file: its directory is not one that is finished.
    pub(super) is_root: bool,
}

impl Ready {
    /// Pushes the file to the drive, then lets it go, but where its drive's
    /// barrier may need it.
    fn sync(&mut self) -> Result<()> {
        let file = match &self.unplaced {
            Unplaced::Unnamed(file) => Some(file),
            Unplaced::Named { .. } => self.file.as_ref(),
        };
        fsys::fsync(file.expect("kept for its fsync"))?;
        if !fsys::BARRIER_NEEDS_A_FILE {
            self.file = None;
        }
        Ok(())
    }
}

/// A file made, and not written whole yet: where it is, and how it goes.
struct Made {
    directory: Arc<OwnedFd>,
    name: Vec<u8>,
    file: File,
    /// Its temporary name, where it has one, registered.
    temporary: Option<(Vec<u8>, interrupt::Registered)>,
}

/// What a writer is handed.
enum Job {
    /// A whole file: made, written, and given its attributes.
    Whole {
        id: FileId,
        target: Target,
        shown: Vec<u8>,
        bytes: Shared,
    },
    /// A file whose segments come: made, for them to be written in.
    Make { id: FileId, target: Target },
    /// A segment, written where it goes in its file.
    Segment {
        id: FileId,
        made: Arc<Made>,
        offset: u64,
        bytes: Shared,
    },
    /// A file whose segments are all written, and its hash found to be the
    /// Source's: given its attributes.
    Finish {
        id: FileId,
        made: Made,
        target: Target,
        shown: Vec<u8>,
        size: u64,
    },
}

/// What a writer did, for the dispatcher.
enum Outcome {
    /// A file written whole, or given its attributes, or failed, and what
    /// could not be set.
    Written {
        id: FileId,
        shown: Vec<u8>,
        size: u64,
        written: Written,
        warned: Warned,
    },
    /// A file made for its segments, or not.
    Made { id: FileId, made: Result<Made> },
    /// A segment written, or not.
    Segment { id: FileId, written: Result<()> },
}

/// A file handed to a writer and not yet settled, for the dashboard.
pub(super) struct Transferring {
    pub(super) id: FileId,
    pub(super) path: String,
    pub(super) size: u64,
    /// Its bytes come so far.
    pub(super) done: Arc<AtomicU64>,
}

/// Reports the files being received and written every
/// [`TRANSFERRING_EVERY`] while there are any, until `stop` is dropped, and
/// then that none is, once some were reported.
fn report_transferring(
    transferring: &Mutex<Vec<Transferring>>,
    reporter: &Reporter,
    stop: &crossbeam_channel::Receiver<()>,
) {
    let mut reported = false;
    while let Err(crossbeam_channel::RecvTimeoutError::Timeout) =
        stop.recv_timeout(TRANSFERRING_EVERY)
    {
        let files: Vec<Transfer> = transferring
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|file| Transfer {
                id: file.id,
                path: file.path.clone(),
                size: file.size,
                done: file.done.load(Ordering::Relaxed).min(file.size),
            })
            .collect();
        let none = files.is_empty();
        if reported || !none {
            reporter.transferring(files);
        }
        reported = !none;
    }
    if reported {
        reporter.transferring(Vec::new());
    }
}

/// How many files are renamed together, after one barrier per drive.
const BATCH: usize = 256;

/// Descriptors left for all but the files waiting for their batch, or for
/// their fsync: the writers' files and directories, the directories being
/// compared, those kept for the stages to share, and the transport's.
const KEPT_OPEN: u64 = 128 + dirs::KEPT as u64;

/// At most how many files are pushed to the drive at once, each by a thread
/// of its own, beside the writers, which go on writing: an fsync waits on
/// the drive, not the CPU, and the more wait at once, the more one journal
/// commit takes. With 200,000 files of 4 KiB on ext4, 64 took 6.3 to 7.1 s
/// on 8 cores, against 17 s with each writer's own; 128, 5.8 to 6.2 s, but
/// with more system time on 96 cores. On NFS, it made no difference.
const SYNCING: usize = 64;

/// How many threads push files to the drive, where at most `open_files` may
/// be open: each holds one open, as does each file waiting for one, so
/// fewer where the limit is low, of what the rest leave.
fn syncers(open_files: u64) -> usize {
    (open_files.saturating_sub(KEPT_OPEN) / 4).clamp(1, SYNCING as u64) as usize
}

/// How many files a batch holds, where at most `open_files` may be open at
/// once, `syncing` of them by files being pushed to the drive: each waiting
/// in a batch holds its directory open, shared with the others there, and
/// itself, without a name, where `unnamed`; otherwise only one of them, the
/// file being the one of its drive kept for the barrier. The batch filling
/// waits beside those being renamed. So a smaller batch, where the limit is
/// low, rather than files failing for want of a descriptor.
fn batch_size(open_files: u64, syncing: u64, unnamed: bool) -> usize {
    let each = if unnamed { 2 } else { 1 };
    let room = open_files.saturating_sub(KEPT_OPEN + syncing) / (each * (RENAMING as u64 + 1));
    room.clamp(1, BATCH as u64) as usize
}

/// Puts a file written into its batch, keeping its file open only if none
/// of the batch on its drive is: one is enough for the drive's barrier.
fn into_batch(batch: &mut Vec<Batched>, mut file: Batched) {
    let device = file.ready.device;
    if batch
        .iter()
        .any(|kept| kept.ready.device == device && kept.ready.file.is_some())
    {
        file.ready.file = None;
    }
    batch.push(file);
}

/// A file of a batch, and what to report of it.
pub(super) struct Batched {
    pub(super) id: FileId,
    pub(super) shown: Vec<u8>,
    pub(super) size: u64,
    pub(super) ready: Ready,
}

/// Writes every file whose parts the decoders hand on, until the Source
/// sent every block, telling the applier of each file that fails, among
/// them those the Source could not read whole, and those not as it read
/// them.
///
/// The parts come in order, to this thread, which hands each to a pool of
/// writers: a whole file each, or a segment of a large one, written where it
/// goes in its file, so that many are written at once. A file in segments
/// is made first, and, once every segment is written and its hash, merged
/// from theirs, is the Source's, given its attributes.
///
/// A writer leaves its file without its name, pushed to the drive. This
/// thread gathers them in batches, for the renamers to give them their
/// names as it goes on: one barrier per drive, so that every file of the
/// batch reaches storage before any of their names, then the names.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_files(
    root: &Path,
    state: &State,
    dirs: &DirCache,
    arrivals: &Receiver<Arrival>,
    incoming: &Receiver<(FileId, Target)>,
    events: &Sender<Event>,
    fsync: bool,
    stop: &Stop,
) -> Result<()> {
    let writers = pool::threads(16);
    let reporter = state.reporter.clone();
    // The files handed to the writers and not yet settled, by when they
    // started, for the dashboard.
    let transferring: Mutex<Vec<Transferring>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| -> Result<()> {
        let stages = Stages::new(scope, stop);
        // Dropped when this returns, which stops the reports.
        let (reports_end, reports_ended) = crossbeam_channel::unbounded::<()>();
        if reporter.is_open() {
            let (transferring, reporter) = (&transferring, reporter.clone());
            scope.spawn(move || report_transferring(transferring, &reporter, &reports_ended));
        }
        let _reports_end = reports_end;
        let (jobs, queue) = crossbeam_channel::bounded::<Job>(writers);
        let (finished, outcomes) = crossbeam_channel::unbounded::<Outcome>();
        let open_files = crate::limits::open_files();
        let syncers = if fsync { syncers(open_files) } else { 0 };
        // Each file written waits here for its fsync, holding itself open.
        let (to_sync, syncing) = crossbeam_channel::bounded::<Outcome>(syncers);
        for _ in 0..syncers {
            let syncing = syncing.clone();
            let finished = finished.clone();
            stages.spawn("fsync", move |_| {
                while let Ok(outcome) = profile::wait(profile::State::Work, || syncing.recv()) {
                    let outcome = match outcome {
                        Outcome::Written {
                            id,
                            shown,
                            size,
                            written: Written::Ready(mut ready),
                            warned,
                        } => {
                            let written =
                                match profile::wait(profile::State::Fsync, || ready.sync()) {
                                    Ok(()) => Written::Ready(ready),
                                    // Gone with it, as is its temporary name.
                                    Err(error) => Written::Failed(error),
                                };
                            Outcome::Written {
                                id,
                                shown,
                                size,
                                written,
                                warned,
                            }
                        }
                        other => other,
                    };
                    if finished.send(outcome).is_err() {
                        break;
                    }
                }
                Ok(())
            });
        }
        drop(syncing);
        for _ in 0..writers {
            let queue = queue.clone();
            let to_sync = to_sync.clone();
            let finished = finished.clone();
            stages.spawn("writer", move |stop| {
                while let Ok(job) = profile::wait(profile::State::Work, || queue.recv()) {
                    let outcome = profile::wait(profile::State::WriteFile, || {
                        work(job, root, state, dirs, fsync, stop)
                    });
                    // The fsync threads end only once the writers have.
                    let ready = matches!(
                        outcome,
                        Outcome::Written {
                            written: Written::Ready(_),
                            ..
                        }
                    );
                    let sent = if fsync && ready {
                        profile::wait(profile::State::ToSync, || to_sync.send(outcome).is_ok())
                    } else {
                        finished.send(outcome).is_ok()
                    };
                    if !sent {
                        break;
                    }
                }
                Ok(())
            });
        }
        drop(finished);
        drop(to_sync);
        drop(queue);
        let batch_size = batch_size(open_files, 2 * syncers as u64, fsys::UNNAMED);
        let mut renaming = Renaming::new(
            durable::renamer(&stages, state.delete, &state.keep, fsync),
            batch_size,
        );
        let mut dispatch = Dispatch {
            root,
            state,
            stop,
            reporter: reporter.clone(),
            events,
            incoming,
            jobs,
            targets: HashMap::new(),
            arrived: Arrived::default(),
            segmented: HashMap::new(),
            hashes: HashMap::new(),
            in_flight: 0,
            batch: Vec::with_capacity(batch_size),
            batch_size,
            transferring: &transferring,
            // Jobs handed to the writers: a full queue, and every writer
            // busy.
            writing: reporter.gauge(Stage::Write, writers * 2),
        };
        dispatch.run(arrivals, &outcomes, &mut renaming)
    })
}

/// Does what a writer is handed.
fn work(
    job: Job,
    root: &Path,
    state: &State,
    dirs: &DirCache,
    fsync: bool,
    stop: &Stop,
) -> Outcome {
    match job {
        Job::Whole {
            id,
            target,
            shown,
            bytes,
        } => {
            let mut warned = Vec::new();
            let written = write_whole(
                root,
                &target,
                &shown,
                &bytes,
                state,
                dirs,
                fsync,
                stop,
                &mut warned,
            );
            Outcome::Written {
                id,
                shown,
                size: bytes.len() as u64,
                written,
                warned,
            }
        }
        Job::Make { id, target } => Outcome::Made {
            id,
            made: make(root, &target, dirs, stop),
        },
        Job::Segment {
            id,
            made,
            offset,
            bytes,
        } => {
            // Once the Destination has stopped, nothing more is written.
            let written = stop
                .check()
                .and_then(|()| Ok(made.file.write_all_at(&bytes, offset)?));
            Outcome::Segment { id, written }
        }
        Job::Finish {
            id,
            made,
            target,
            shown,
            size,
        } => {
            let mut warned = Vec::new();
            let written = finish(made, &target, &shown, state, fsync, &mut warned);
            Outcome::Written {
                id,
                shown,
                size,
                written,
                warned,
            }
        }
    }
}

/// Makes a file for `target`: without a name in its directory, or under a
/// temporary name beside it where the filesystem makes none. Once the
/// Destination has stopped, none is made.
fn make(root: &Path, target: &Target, dirs: &DirCache, stop: &Stop) -> Result<Made> {
    stop.check()?;
    let (directory, name) = target_directory(root, target, dirs)?;
    let unnamed = if inject::failing("unnamed") {
        None
    } else {
        fsys::create_unnamed(&*directory)?
    };
    let (file, temporary) = match unnamed {
        Some(file) => (file, None),
        None => {
            let (file, temporary, registered) =
                interrupt::create(&directory, || fsys::create_temporary(&directory, &name))?;
            (file, Some((temporary, registered)))
        }
    };
    Ok(Made {
        directory,
        name,
        file,
        temporary,
    })
}

/// Writes a whole file: made, its bytes written, given its attributes.
#[allow(clippy::too_many_arguments)]
fn write_whole(
    root: &Path,
    target: &Target,
    shown: &[u8],
    bytes: &[u8],
    state: &State,
    dirs: &DirCache,
    fsync: bool,
    stop: &Stop,
    warned: &mut Warned,
) -> Written {
    let made = match make(root, target, dirs, stop) {
        Ok(made) => made,
        Err(error) => return Written::Failed(error),
    };
    let written = inject::fail_if("write").and_then(|()| Ok((&made.file).write_all(bytes)?));
    if let Err(error) = written {
        discard(made);
        return Written::Failed(error);
    }
    finish(made, target, shown, state, fsync, warned)
}

/// Lets go of a file made that will not be given its name: one without a
/// name goes as it is closed, one under a temporary name is removed.
fn discard(made: Made) {
    if let Some((temporary, _)) = &made.temporary {
        let _ = fsys::remove(&made.directory, temporary, false);
    }
}

/// Gives a file written its attributes, and leaves it so, for an fsync
/// thread to push it to the drive, unless told not to sync, and for its
/// batch to give it its name.
fn finish(
    made: Made,
    target: &Target,
    shown: &[u8],
    state: &State,
    fsync: bool,
    warned: &mut Warned,
) -> Written {
    // Its drive, once it is ready.
    let finished = (|| -> Result<u64> {
        apply_file_attributes(&made.file, shown, &target.entry, state, warned)?;
        Ok(made.file.metadata()?.dev())
    })();
    let device = match finished {
        Ok(device) => device,
        Err(error) => {
            discard(made);
            return Written::Failed(error);
        }
    };
    let Made {
        directory,
        name,
        file,
        temporary,
    } = made;
    // What it holds open goes with it, not copied.
    let (unplaced, file) = match temporary {
        None => (Unplaced::Unnamed(file), None),
        Some((temporary, registered)) => (
            Unplaced::Named {
                temporary,
                registered,
            },
            // For its fsync, and its drive's barrier.
            fsync.then_some(file),
        ),
    };
    Written::Ready(Ready {
        directory,
        name,
        unplaced,
        exists: target.exists,
        file,
        device,
        is_root: target.is_root,
    })
}

/// A file the Source sends in segments, as they come.
struct Segmented {
    target: Target,
    shown: Vec<u8>,
    /// Its size as the Source found it when it opened it, as each of its
    /// segments says.
    size: u64,
    /// The file, once made; let go of once it is finished, or failed.
    made: Option<Arc<Made>>,
    /// Its segments that came before it was made, and where they go.
    waiting: Vec<(u64, Shared)>,
    /// Each segment's chaining value, by its place, as it came.
    values: BTreeMap<u64, [u8; 32]>,
    /// How many segments are written.
    written: u64,
    /// The hash the Source merged from its segments', once it comes.
    hash: Option<[u8; 32]>,
    /// How many of its jobs the writers have.
    jobs: usize,
    /// It failed: its segments still to come are dropped.
    failed: bool,
    /// Why, until it is told.
    why: Option<anyhow::Error>,
    /// It is finished, or its failure reported: either way settled.
    settled: bool,
    /// Its bytes come so far, for the dashboard.
    done: Arc<AtomicU64>,
}

impl Segmented {
    fn segments(&self) -> u64 {
        block::segments(self.size)
    }
}

/// What the dispatcher takes next.
enum Next {
    Arrival(Option<Arrival>),
    Outcome(Option<Outcome>),
    Stopped,
}

/// The thread that hands the writers each part of each block as it comes,
/// follows each file until it is settled, and gives files written their
/// names, in batches.
struct Dispatch<'a> {
    root: &'a Path,
    state: &'a State,
    stop: &'a Stop,
    reporter: Reporter,
    events: &'a Sender<Event>,
    incoming: &'a Receiver<(FileId, Target)>,
    jobs: Sender<Job>,
    /// The needed files none of whose bytes have come, by ID.
    targets: HashMap<FileId, Target>,
    /// The IDs whose bytes came: a whole file a second time, or a file in
    /// segments after it was whole, is the Source's bug, told at once.
    arrived: Arrived,
    /// The files coming in segments, and those finished or failed, whose
    /// segments still to come are dropped.
    segmented: HashMap<FileId, Segmented>,
    /// The hashes of files in segments that came before any of their
    /// segments did.
    hashes: HashMap<FileId, [u8; 32]>,
    /// How many jobs the writers have.
    in_flight: usize,
    batch: Vec<Batched>,
    batch_size: usize,
    transferring: &'a Mutex<Vec<Transferring>>,
    writing: Gauge,
}

impl Dispatch<'_> {
    /// Takes the decoders' parts, and the writers' outcomes, until every
    /// file the Source sent is settled.
    fn run(
        &mut self,
        arrivals: &Receiver<Arrival>,
        outcomes: &Receiver<Outcome>,
        renaming: &mut Renaming,
    ) -> Result<()> {
        let never = crossbeam_channel::never();
        let mut ended = false;
        loop {
            while let Ok(outcome) = outcomes.try_recv() {
                self.settle(outcome, renaming)?;
            }
            renaming.take_renamed(&|renamed| self.renamed(renamed))?;
            self.writing.set(self.in_flight);
            // Nothing more is written once the Destination has stopped.
            self.stop.check()?;
            if ended && self.in_flight == 0 {
                return self.end(renaming);
            }
            // The next block waits while the writers hold enough.
            let taking = !ended;
            let next = profile::wait(profile::State::Blocks, || {
                crossbeam_channel::select! {
                    recv(if taking { arrivals } else { &never }) -> arrival => {
                        Next::Arrival(arrival.ok())
                    }
                    recv(outcomes) -> outcome => Next::Outcome(outcome.ok()),
                    recv(self.stop.signal()) -> _ => Next::Stopped,
                }
            });
            match next {
                Next::Arrival(Some(Arrival::Parts(parts))) => {
                    for part in parts {
                        // What the writers did meanwhile, between the
                        // thousands of files a pack may hold.
                        while let Ok(outcome) = outcomes.try_recv() {
                            self.settle(outcome, renaming)?;
                        }
                        renaming.take_renamed(&|renamed| self.renamed(renamed))?;
                        if part.is_segment() {
                            self.segment(part)?;
                        } else {
                            self.whole(part)?;
                        }
                    }
                }
                Next::Arrival(Some(Arrival::Hash(hash))) => self.hash(hash)?,
                Next::Arrival(Some(Arrival::End)) => ended = true,
                Next::Arrival(None) => {
                    return Err(self.stop.consequence(self.ended_early()));
                }
                Next::Outcome(Some(outcome)) => self.settle(outcome, renaming)?,
                // The writers stopped, and say why themselves.
                Next::Outcome(None) | Next::Stopped => return Err(Stopped.into()),
            }
        }
    }

    /// The blocks ended before the Source said they did: where.
    fn ended_early(&self) -> anyhow::Error {
        let unfinished = self
            .segmented
            .values()
            .find(|file| !file.settled && file.values.len() as u64 != file.segments());
        match unfinished {
            Some(file) => anyhow!(
                "the data ended in the middle of {}",
                String::from_utf8_lossy(&file.shown)
            ),
            None => anyhow!("the data ended before its end"),
        }
    }

    /// The target of the file with `id`, which the Source sends for the
    /// first time, at the mtime it found it at; refused if it was never
    /// asked for, or has come already.
    fn arriving(&mut self, id: FileId, mtime: manifest::Timestamp) -> Result<(Target, Vec<u8>)> {
        if self.arrived.contains(id) {
            bail!("the Source sent ID {id} twice");
        }
        let target = target(id, &mut self.targets, self.incoming, self.stop)?
            .with_context(|| format!("the Source sent unneeded ID {id}"))?;
        self.arrived.insert(id);
        let mut entry = target.entry.clone();
        entry.mtime = mtime;
        let target = Target { entry, ..target };
        // The root, a file, by its path here, not as `.`.
        let shown = if target.is_root {
            self.root.as_os_str().as_encoded_bytes().to_vec()
        } else {
            display(
                &manifest::join(&target.directory, &target.entry.name),
                false,
            )
        };
        Ok((target, shown))
    }

    /// Takes a whole file: hands it to a writer, or, if it is not as the
    /// Source read it, fails it.
    fn whole(&mut self, part: Part) -> Result<()> {
        let id = part.file;
        let (target, shown) = self.arriving(id, part.mtime)?;
        self.reporter.add(Count::Arrived, 1);
        match part.bytes {
            Err(why) => self.failed(id, shown, anyhow::Error::msg(why)),
            Ok(bytes) => {
                let size = bytes.len() as u64;
                self.reporter.add(Count::Received, size);
                self.transferring
                    .lock()
                    .expect("not poisoned")
                    .push(Transferring {
                        id,
                        path: String::from_utf8_lossy(&shown).into_owned(),
                        size,
                        done: Arc::new(AtomicU64::new(size)),
                    });
                self.hand(Job::Whole {
                    id,
                    target,
                    shown,
                    bytes,
                })
            }
        }
    }

    /// Takes a segment: the first of its file's has the file made, and each
    /// is written once it is.
    fn segment(&mut self, part: Part) -> Result<()> {
        let id = part.file;
        let first = !self.segmented.contains_key(&id);
        if first {
            let (target, shown) = self.arriving(id, part.mtime)?;
            let done = Arc::new(AtomicU64::new(0));
            self.transferring
                .lock()
                .expect("not poisoned")
                .push(Transferring {
                    id,
                    path: String::from_utf8_lossy(&shown).into_owned(),
                    size: part.size,
                    done: Arc::clone(&done),
                });
            self.segmented.insert(
                id,
                Segmented {
                    target,
                    shown,
                    size: part.size,
                    made: None,
                    waiting: Vec::new(),
                    values: BTreeMap::new(),
                    written: 0,
                    hash: self.hashes.remove(&id),
                    jobs: 0,
                    failed: false,
                    why: None,
                    settled: false,
                    done,
                },
            );
        }
        let file = self.segmented.get_mut(&id).expect("just made");
        // Those of a file that failed still come, and are dropped.
        if file.failed {
            return Ok(());
        }
        if file.settled || file.values.len() as u64 == file.segments() {
            bail!("the Source sent a segment of ID {id} twice");
        }
        if part.size != file.size {
            bail!("the Source sent segments of ID {id} of different sizes");
        }
        let bytes = match part.bytes {
            Ok(bytes) => bytes,
            Err(why) => return self.fail_segmented(id, anyhow::Error::msg(why)),
        };
        let index = part.offset / block::SEGMENT;
        let value = part.chaining_value.expect("a segment checked has one");
        if file.values.insert(index, value).is_some() {
            bail!("the Source sent a segment of ID {id} twice");
        }
        let len = bytes.len();
        file.done.fetch_add(len as u64, Ordering::Relaxed);
        self.reporter.add(Count::Received, len as u64);
        if file.values.len() as u64 == file.segments() {
            self.reporter.add(Count::Arrived, 1);
        }
        let Some(made) = file.made.clone() else {
            file.waiting.push((part.offset, bytes));
            // Made once its first segment comes as the Source read it.
            if first {
                file.jobs += 1;
                let target = file.target.clone();
                return self.hand(Job::Make { id, target });
            }
            return Ok(());
        };
        file.jobs += 1;
        self.hand(Job::Segment {
            id,
            made,
            offset: part.offset,
            bytes,
        })
    }

    /// Takes the hash of a file in segments, which may come before any of
    /// them.
    fn hash(&mut self, FileHash { file: id, hash }: FileHash) -> Result<()> {
        if let Some(file) = self.segmented.get_mut(&id) {
            if file.failed {
                return Ok(());
            }
            if file.settled || file.hash.replace(hash).is_some() {
                bail!("the Source sent the hash of ID {id} twice");
            }
            return self.check_whole(id);
        }
        if self.arrived.contains(id) || !self.needed(id)? {
            bail!("the Source sent the hash of unneeded ID {id}");
        }
        if self.hashes.insert(id, hash).is_some() {
            bail!("the Source sent the hash of ID {id} twice");
        }
        Ok(())
    }

    /// Whether `id` is a needed file, waiting for its target, if it is still
    /// to be sent: each is sent before its file is asked for, so a Source
    /// has it here before it sends the file's hash; one that sends hashes
    /// ahead, as a test's may, waits for it.
    fn needed(&mut self, id: FileId) -> Result<bool> {
        loop {
            if self.targets.contains_key(&id) {
                return Ok(true);
            }
            let next = crossbeam_channel::select! {
                recv(self.incoming) -> next => next,
                recv(self.stop.signal()) -> _ => return Err(Stopped.into()),
            };
            let Ok((next, target)) = next else {
                return Ok(false);
            };
            self.targets.insert(next, target);
        }
    }

    /// Hands the writers a job, waiting for room: a writer that failed has
    /// stopped the Destination, and then this waits for room no more.
    fn hand(&mut self, job: Job) -> Result<()> {
        profile::wait(profile::State::ToWriters, || {
            crossbeam_channel::select! {
                send(self.jobs, job) -> sent => sent.map_err(|_| Stopped),
                recv(self.stop.signal()) -> _ => Err(Stopped),
            }
        })?;
        self.in_flight += 1;
        Ok(())
    }

    /// Takes what a writer did.
    fn settle(&mut self, outcome: Outcome, renaming: &mut Renaming) -> Result<()> {
        self.in_flight -= 1;
        match outcome {
            Outcome::Written {
                id,
                shown,
                size,
                written,
                warned,
            } => {
                // What a writer could not set is warned of here.
                self.state.warn(warned);
                self.transferring
                    .lock()
                    .expect("not poisoned")
                    .retain(|file| file.id != id);
                if let Some(file) = self.segmented.get_mut(&id) {
                    file.settled = true;
                }
                match written {
                    Written::Ready(ready) => {
                        into_batch(
                            &mut self.batch,
                            Batched {
                                id,
                                shown,
                                size,
                                ready,
                            },
                        );
                        if self.batch.len() >= self.batch_size {
                            let batch = std::mem::take(&mut self.batch);
                            renaming.hand(batch, self.stop, &|renamed| self.renamed(renamed))?;
                        }
                        Ok(())
                    }
                    Written::Failed(error) => self.failed(id, shown, error),
                }
            }
            Outcome::Made { id, made } => {
                let file = self
                    .segmented
                    .get_mut(&id)
                    .expect("made for a file in segments");
                file.jobs -= 1;
                let made = match made {
                    Ok(made) => made,
                    Err(error) => return self.fail_segmented(id, error),
                };
                // Its failure is told once nothing of it is with the writers.
                if file.failed {
                    discard(made);
                    return self.settle_failed(id);
                }
                let made = Arc::new(made);
                file.made = Some(Arc::clone(&made));
                let waiting = std::mem::take(&mut file.waiting);
                file.jobs += waiting.len();
                for (offset, bytes) in waiting {
                    self.hand(Job::Segment {
                        id,
                        made: Arc::clone(&made),
                        offset,
                        bytes,
                    })?;
                }
                drop(made);
                self.check_whole(id)
            }
            Outcome::Segment { id, written } => {
                let file = self
                    .segmented
                    .get_mut(&id)
                    .expect("written for a file in segments");
                file.jobs -= 1;
                if let Err(error) = written {
                    return self.fail_segmented(id, error);
                }
                if file.failed {
                    return self.settle_failed(id);
                }
                file.written += 1;
                self.check_whole(id)
            }
        }
    }

    /// Once every segment of a file is written, and its hash has come, has
    /// it given its attributes, if the hash merged from the segments' is the
    /// Source's.
    fn check_whole(&mut self, id: FileId) -> Result<()> {
        let file = self.segmented.get_mut(&id).expect("a file in segments");
        let whole = !file.failed
            && file.made.is_some()
            && file.written == file.segments()
            && file.hash.is_some();
        if !whole {
            return Ok(());
        }
        let values: Vec<[u8; 32]> = file.values.values().copied().collect();
        if Some(block::root(&values, file.size)) != file.hash {
            return self.fail_segmented(
                id,
                anyhow!("the content's hash does not match the Source's"),
            );
        }
        let made = Arc::try_unwrap(file.made.take().expect("made"))
            .ok()
            .expect("no writer holds a file whose segments are all written");
        let job = Job::Finish {
            id,
            made,
            target: file.target.clone(),
            shown: file.shown.clone(),
            size: file.size,
        };
        self.hand(job)
    }

    /// A file in segments failed: its segments still to come are dropped,
    /// and, once none of its jobs is with the writers, it is let go of and
    /// told.
    fn fail_segmented(&mut self, id: FileId, error: anyhow::Error) -> Result<()> {
        let file = self.segmented.get_mut(&id).expect("a file in segments");
        if !file.failed {
            file.failed = true;
            file.why = Some(error);
            file.waiting.clear();
        }
        self.settle_failed(id)
    }

    /// Tells of a file in segments that failed, once none of its jobs is with
    /// the writers.
    fn settle_failed(&mut self, id: FileId) -> Result<()> {
        let file = self.segmented.get_mut(&id).expect("a file in segments");
        if file.jobs > 0 || file.settled {
            return Ok(());
        }
        file.settled = true;
        if let Some(made) = file.made.take() {
            discard(Arc::try_unwrap(made).ok().expect("no writer holds it"));
        }
        file.values.clear();
        let error = file.why.take().expect("failed");
        let shown = file.shown.clone();
        self.transferring
            .lock()
            .expect("not poisoned")
            .retain(|transferring| transferring.id != id);
        self.failed(id, shown, error)
    }

    /// Reports each file renamed, or not, as it is taken back.
    fn renamed(&self, renamed: Renamed) -> Result<()> {
        for (done, result) in renamed {
            match result {
                Ok(()) => {
                    self.settled(done.id)?;
                    self.reporter.add(Count::Files, 1);
                    self.reporter.add(Count::Bytes, done.size);
                }
                Err(error) => self.failed(done.id, done.shown, error)?,
            }
        }
        Ok(())
    }

    /// The Source sent every block, and every file of them is settled:
    /// those in segments that did not come whole fail, the last batch is
    /// renamed, and the applier told which needed files never came.
    fn end(&mut self, renaming: &mut Renaming) -> Result<()> {
        let unfinished: Vec<FileId> = self
            .segmented
            .iter()
            .filter(|(_, file)| !file.settled)
            .map(|(&id, _)| id)
            .collect();
        for id in unfinished {
            self.fail_segmented(id, anyhow!("the Source never sent all of it"))?;
        }
        let batch = std::mem::take(&mut self.batch);
        renaming.hand(batch, self.stop, &|renamed| self.renamed(renamed))?;
        renaming.finish(self.stop, &|renamed| self.renamed(renamed))?;
        // The needed files that did not come are known once every target
        // is sent: the applier knows which of them the Source said it could
        // not open, and fails the rest.
        loop {
            let next = crossbeam_channel::select! {
                recv(self.incoming) -> next => next,
                recv(self.stop.signal()) -> _ => return Err(Stopped.into()),
            };
            let Ok((id, target)) = next else {
                break;
            };
            self.targets.insert(id, target);
        }
        let mut never: Vec<(FileId, Vec<u8>)> = self
            .targets
            .drain()
            .map(|(id, target)| {
                let path = manifest::join(&target.directory, &target.entry.name);
                (id, display(&path, false))
            })
            .collect();
        never.sort();
        let _ = self.events.send(Event::StreamEnded(never));
        Ok(())
    }

    /// A file written, for its directory, which may wait for it; once the
    /// applier is gone, it said why.
    fn settled(&self, id: FileId) -> Result<()> {
        self.events
            .send(Event::Settled(id))
            .map_err(|_| Stopped.into())
    }

    /// A file not written, or not given its name, and why: the applier says
    /// what that means, whether it fails the file or stops the transfer.
    fn failed(&self, id: FileId, shown: Vec<u8>, error: anyhow::Error) -> Result<()> {
        self.events
            .send(Event::Failed { id, shown, error })
            .map_err(|_| Stopped.into())
    }
}

/// The IDs whose bytes came, a bit each: few bytes for millions of files.
/// Only an ID that had a target is inserted, so a Source gone wrong cannot
/// make it grow beyond the manifest.
#[derive(Default)]
struct Arrived {
    bits: Vec<u64>,
}

impl Arrived {
    fn contains(&self, id: FileId) -> bool {
        usize::try_from(id / 64)
            .ok()
            .and_then(|word| self.bits.get(word))
            .is_some_and(|bits| bits & (1 << (id % 64)) != 0)
    }

    fn insert(&mut self, id: FileId) {
        let word = usize::try_from(id / 64).expect("an ID that had a target");
        if self.bits.len() <= word {
            self.bits.resize(word + 1, 0);
        }
        self.bits[word] |= 1 << (id % 64);
    }
}

/// Owner first, since setting it clears setuid and setgid, then permissions,
/// then the mtime, last since nothing may write to the file after it. What
/// this process may not set goes in `warned`, as `shown`, and the file kept.
fn apply_file_attributes(
    file: &File,
    shown: &[u8],
    entry: &Entry,
    state: &State,
    warned: &mut Warned,
) -> Result<()> {
    let metadata = file.metadata()?;
    let setters = Setters {
        chown: |uid, gid| fsys::set_owner(file, uid, gid),
        chmod: Some(|mode| fsys::set_mode(file, mode)),
        utimes: |mtime| fsys::set_mtime(file, mtime),
    };
    let current = (metadata.uid(), metadata.gid());
    Ok(set_attributes(
        state, shown, current, entry, setters, warned,
    )?)
}

/// The target of the file with `id`, the dispatcher's own, or one still to
/// be sent to it. Each is sent before its file is asked for, so a Source has
/// it here before it sends the file; one that sends files ahead, as a test's
/// may, waits for it. `None` once every target is sent, and none is it: it
/// was never asked for.
fn target(
    id: FileId,
    targets: &mut HashMap<FileId, Target>,
    incoming: &Receiver<(FileId, Target)>,
    stop: &Stop,
) -> Result<Option<Target>> {
    if let Some(target) = targets.remove(&id) {
        return Ok(Some(target));
    }
    loop {
        let next = crossbeam_channel::select! {
            recv(incoming) -> next => next,
            recv(stop.signal()) -> _ => return Err(Stopped.into()),
        };
        let Ok((next, target)) = next else {
            return Ok(None);
        };
        if next == id {
            return Ok(Some(target));
        }
        targets.insert(next, target);
    }
}

/// The directory a target is written in, and its name there.
fn target_directory(
    root: &Path,
    target: &Target,
    dirs: &DirCache,
) -> Result<(Arc<OwnedFd>, Vec<u8>)> {
    if target.is_root {
        let (directory, name) = root_parent(root)?;
        return Ok((Arc::new(directory), name));
    }
    Ok((dirs.open(&target.directory)?, target.entry.name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch shrinks where few files may be open, the more where each
    /// holds itself open, without a name, and is never empty.
    #[test]
    fn a_batch_fits_the_open_file_limit() {
        assert_eq!(batch_size(u64::MAX, 0, false), BATCH);
        assert_eq!(batch_size(1024, 0, false), BATCH);
        assert_eq!(batch_size(256, 0, false), 32);
        assert_eq!(batch_size(130, 0, false), 1);
        assert_eq!(batch_size(16, 0, false), 1);
        assert_eq!(batch_size(u64::MAX, 0, true), BATCH);
        assert_eq!(batch_size(1024, 0, true), 144);
        assert_eq!(batch_size(256, 0, true), 16);
        assert_eq!(batch_size(16, 0, true), 1);
        // Fewer still, beside the files waiting for their fsync.
        assert_eq!(batch_size(1024, 128, true), 122);
        assert_eq!(batch_size(256, 48, true), 8);
    }

    /// An ID is arrived once inserted, whatever its word; one far beyond
    /// any inserted, as a Source gone wrong may send, is not, and takes no
    /// room.
    #[test]
    fn arrived_ids_are_kept_a_bit_each() {
        let mut arrived = Arrived::default();
        for id in [0, 63, 64, 1_000] {
            assert!(!arrived.contains(id));
            arrived.insert(id);
            assert!(arrived.contains(id));
        }
        assert!(!arrived.contains(1));
        assert!(!arrived.contains(FileId::MAX));
        assert_eq!(arrived.bits.len(), 16);
    }

    /// The fsync threads are fewer where few files may be open, and never
    /// none.
    #[test]
    fn the_fsync_threads_fit_the_open_file_limit() {
        assert_eq!(syncers(u64::MAX), SYNCING);
        assert_eq!(syncers(1024), SYNCING);
        assert_eq!(syncers(256), 24);
        assert_eq!(syncers(16), 1);
    }

    /// A batch keeps one file open for each drive, of those kept open at
    /// all, for its barrier: the others are let go of as they come.
    #[test]
    fn a_batch_keeps_one_file_open_for_each_drive() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Arc::new(fsys::open_root(temp.path().as_os_str()).unwrap());
        let file = |id: FileId, device: u64, kept: bool| {
            let (file, temporary, registered) = interrupt::create(&directory, || {
                fsys::create_temporary(&*directory, format!("f{id}").as_bytes())
            })
            .unwrap();
            Batched {
                id,
                shown: Vec::new(),
                size: 0,
                ready: Ready {
                    directory: Arc::clone(&directory),
                    name: format!("f{id}").into_bytes(),
                    unplaced: Unplaced::Named {
                        temporary,
                        registered,
                    },
                    exists: false,
                    file: kept.then_some(file),
                    device,
                    is_root: false,
                },
            }
        };
        let mut batch = Vec::new();
        into_batch(&mut batch, file(1, 7, true));
        into_batch(&mut batch, file(2, 7, true));
        into_batch(&mut batch, file(3, 8, true));
        into_batch(&mut batch, file(4, 9, false));
        into_batch(&mut batch, file(5, 9, true));
        let kept: Vec<FileId> = batch
            .iter()
            .filter(|batched| batched.ready.file.is_some())
            .map(|batched| batched.id)
            .collect();
        assert_eq!(kept, [1, 3, 5]);
    }
}
