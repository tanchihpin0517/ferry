//! Writing the files of the record stream: parsed in order, written by a
//! pool of writers, each without a name where the filesystem can make one,
//! or under a temporary name, then given its own in batches.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::inject;
use crate::interrupt;
use crate::manifest::{self, Entry, FileId};
use crate::mux::Mux;
use crate::pool;
use crate::profile;
use crate::record::{self, ChannelReader, Header};
use crate::report::{Count, Reporter, Stage, TRANSFERRING_EVERY, Transfer};
use crate::source::display;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::apply::{Event, Setters, file_failed_in, set_attributes};
use super::durable::{self, RENAMING, Renamed, Renaming};
use super::{MissingParent, State, ends_the_transfer, is_protected, root_parent};

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
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

/// What became of one record.
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

/// A file handed to a writer: where it goes, and its record's bytes, which
/// the parser forwards as it reads them.
struct FileJob {
    pub(super) id: FileId,
    pub(super) target: Target,
    pub(super) shown: Vec<u8>,
    pub(super) size: u64,
    pub(super) bytes: crossbeam_channel::Receiver<Vec<u8>>,
}

/// A file handed to a writer and not yet settled, for the dashboard.
pub(super) struct Transferring {
    pub(super) id: FileId,
    pub(super) path: String,
    pub(super) size: u64,
    /// Its bytes forwarded to its writer so far.
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

/// Writes every file in the record stream, until it ends, reporting each file
/// that fails, among them those the Source could not read whole.
///
/// The stream is parsed in order, by this thread; the files are written by a
/// pool of writers, one file each, so that many are written at once. A
/// file's bytes go to its writer through a channel of their own, as they are
/// parsed, so a large file is never held in memory whole.
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
    mux: &Mux,
    stream: &mut ChannelReader,
    incoming: &Receiver<(FileId, Target)>,
    events: &Sender<Event>,
    fsync: bool,
    stop: &Stop,
) -> Result<()> {
    let writers = pool::threads(16);
    // The needed files whose records have not come, by ID.
    let mut targets: HashMap<FileId, Target> = HashMap::new();
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
        let (jobs, queue) = crossbeam_channel::bounded::<FileJob>(writers);
        let (finished, outcomes) =
            crossbeam_channel::unbounded::<(FileId, Vec<u8>, u64, Written)>();
        let open_files = crate::limits::open_files();
        let syncers = if fsync { syncers(open_files) } else { 0 };
        // Each file written waits here for its fsync, holding itself open.
        let (to_sync, syncing) =
            crossbeam_channel::bounded::<(FileId, Vec<u8>, u64, Written)>(syncers);
        for _ in 0..syncers {
            let syncing = syncing.clone();
            let finished = finished.clone();
            stages.spawn("fsync", move |_| {
                while let Ok((id, shown, size, written)) =
                    profile::wait(profile::State::Work, || syncing.recv())
                {
                    let written = match written {
                        Written::Ready(mut ready) => {
                            match profile::wait(profile::State::Fsync, || ready.sync()) {
                                Ok(()) => Written::Ready(ready),
                                // Gone with it, as is its temporary name.
                                Err(error) => Written::Failed(error),
                            }
                        }
                        failed => failed,
                    };
                    if finished.send((id, shown, size, written)).is_err() {
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
                    let mut bytes = ChannelReader::new(job.bytes, profile::State::Bytes);
                    let written = profile::wait(profile::State::WriteFile, || {
                        write_file(
                            root,
                            &job.target,
                            &job.shown,
                            job.size,
                            &mut bytes,
                            state,
                            dirs,
                            fsync,
                            stop,
                        )
                    })
                    .unwrap_or_else(Written::Failed);
                    let outcome = (job.id, job.shown, job.size, written);
                    // The fsync threads end only once the writers have.
                    let sent = if fsync && matches!(outcome.3, Written::Ready(_)) {
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
            durable::renamer(&stages, state.delete, &state.protect, fsync),
            batch_size,
        );

        let mut in_flight = 0usize;
        // Files handed to the writers: a full queue, and every writer busy.
        let writing = reporter.gauge(Stage::Write, writers * 2);
        let mut batch: Vec<Batched> = Vec::with_capacity(batch_size);
        let fail = |id: FileId, shown: &[u8], message: String| {
            file_failed_in(state, &reporter, mux, stop, id, shown, message)
        };
        // What became of each file, for its directory, which may wait for
        // it; once the applier is gone, it said why.
        let settled = |id: FileId| events.send(Event::Settled(id)).map_err(|_| Stopped);
        // Reports each file renamed, or not, as it is taken back.
        let take = |renamed: Renamed| -> Result<()> {
            for (done, result) in renamed {
                settled(done.id)?;
                match result {
                    Ok(()) => {
                        reporter.add(Count::Files, 1);
                        reporter.add(Count::Bytes, done.size);
                    }
                    // Never deleted, and the transfer stops: the Source's
                    // root, or a directory above it, is where this file goes.
                    Err(error) if is_protected(&error) => {
                        return Err(error.context(format!(
                            "will not delete {}/ to make {}",
                            String::from_utf8_lossy(&done.shown),
                            String::from_utf8_lossy(&done.shown),
                        )));
                    }
                    // Every file after it would fail the same way.
                    Err(error) if ends_the_transfer(&error) => {
                        return Err(error
                            .context(format!("write {}", String::from_utf8_lossy(&done.shown))));
                    }
                    Err(error) => fail(done.id, &done.shown, format!("{error:#}"))?,
                }
            }
            Ok(())
        };
        // What becomes of a file once its writer is done with it.
        let settle = |(id, shown, size, written): (FileId, Vec<u8>, u64, Written),
                      batch: &mut Vec<Batched>,
                      renaming: &mut Renaming|
         -> Result<()> {
            transferring
                .lock()
                .expect("not poisoned")
                .retain(|file| file.id != id);
            match written {
                Written::Ready(ready) => {
                    into_batch(
                        batch,
                        Batched {
                            id,
                            shown,
                            size,
                            ready,
                        },
                    );
                    if batch.len() >= batch_size {
                        renaming.hand(std::mem::take(batch), stop, &take)?;
                    }
                    Ok(())
                }
                Written::Failed(error) => {
                    settled(id)?;
                    // The root, which is this file, has nowhere to go.
                    if error.is::<MissingParent>() {
                        return Err(error);
                    }
                    // Every file after it would fail the same way.
                    if ends_the_transfer(&error) {
                        return Err(
                            error.context(format!("write {}", String::from_utf8_lossy(&shown)))
                        );
                    }
                    fail(id, &shown, format!("{error:#}"))
                }
            }
        };
        loop {
            while let Ok(outcome) = outcomes.try_recv() {
                in_flight -= 1;
                settle(outcome, &mut batch, &mut renaming)?;
            }
            renaming.take_renamed(&take)?;
            writing.set(in_flight);
            // Nothing more is written once the Destination has stopped.
            stop.check()?;
            let header = record::read_header(stream)
                .map_err(|error| stop.consequence(ended(error, "before its end")))?;
            match header {
                Header::File { id, size, mtime } => {
                    let target = target(id, &mut targets, incoming, stop)?
                        .with_context(|| format!("the Source sent unneeded ID {id}"))?;
                    let mut entry = target.entry.clone();
                    entry.mtime = mtime;
                    let target = Target { entry, ..target };
                    let shown = display(
                        &manifest::join(&target.directory, &target.entry.name),
                        false,
                    );
                    let done = Arc::new(AtomicU64::new(0));
                    transferring
                        .lock()
                        .expect("not poisoned")
                        .push(Transferring {
                            id,
                            path: String::from_utf8_lossy(&shown).into_owned(),
                            size,
                            done: Arc::clone(&done),
                        });
                    let (forward, bytes) = crossbeam_channel::bounded(4);
                    let job = FileJob {
                        id,
                        target,
                        shown,
                        size,
                        bytes,
                    };
                    // A writer that failed has stopped the Destination: then
                    // this waits for room no more.
                    profile::wait(profile::State::ToWriters, || {
                        crossbeam_channel::select! {
                            send(jobs, job) -> sent => sent.map_err(|_| Stopped),
                            recv(stop.signal()) -> _ => Err(Stopped),
                        }
                    })?;
                    in_flight += 1;
                    forward_record(stream, size, &forward, &reporter, &done).map_err(|error| {
                        // Named as the dashboard has it, rather than kept for
                        // every file in case.
                        let transferring = transferring.lock().expect("not poisoned");
                        let path = transferring
                            .iter()
                            .find(|file| file.id == id)
                            .map_or("a file", |file| file.path.as_str());
                        stop.consequence(ended(error, &format!("in the middle of {path}")))
                    })?;
                }
                Header::End => {
                    drop(jobs);
                    while in_flight > 0 {
                        let outcome = profile::wait(profile::State::Outcome, || {
                            crossbeam_channel::select! {
                                recv(outcomes) -> outcome => outcome.map_err(|_| Stopped),
                                recv(stop.signal()) -> _ => Err(Stopped),
                            }
                        })?;
                        in_flight -= 1;
                        settle(outcome, &mut batch, &mut renaming)?;
                    }
                    renaming.hand(std::mem::take(&mut batch), stop, &take)?;
                    renaming.finish(stop, &take)?;
                    // The needed files that did not come are known once
                    // every target is sent: the applier knows which of them
                    // the Source said it could not open, and fails the rest.
                    loop {
                        let next = crossbeam_channel::select! {
                            recv(incoming) -> next => next,
                            recv(stop.signal()) -> _ => return Err(Stopped.into()),
                        };
                        let Ok((id, target)) = next else {
                            break;
                        };
                        targets.insert(id, target);
                    }
                    let mut never: Vec<(FileId, Vec<u8>)> = targets
                        .drain()
                        .map(|(id, target)| {
                            let path = manifest::join(&target.directory, &target.entry.name);
                            (id, display(&path, false))
                        })
                        .collect();
                    never.sort();
                    let _ = events.send(Event::StreamEnded(never));
                    return Ok(());
                }
            }
        }
    })
}

/// A record stream that ended, said as where it did; any other error as it
/// is.
fn ended(error: anyhow::Error, at: &str) -> anyhow::Error {
    let eof = error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof)
    });
    if eof {
        anyhow!("the record stream ended {at}")
    } else {
        error
    }
}

/// Forwards one record's content and trailer from the stream to its writer.
/// A writer reads its record to the end whatever happens, so this never waits
/// on one that gave up; if one stopped altogether, the rest is read and
/// dropped, keeping the stream in step.
fn forward_record(
    stream: &mut impl Read,
    size: u64,
    forward: &crossbeam_channel::Sender<Vec<u8>>,
    reporter: &Reporter,
    done: &AtomicU64,
) -> Result<()> {
    let mut remaining = size;
    while remaining > 0 {
        let count = (256 * 1024).min(remaining as usize);
        let mut chunk = vec![0; count];
        stream
            .read_exact(&mut chunk)
            .context("read the record stream")?;
        let _ = profile::wait(profile::State::ToWriter, || forward.send(chunk));
        reporter.add(Count::Received, count as u64);
        done.fetch_add(count as u64, Ordering::Relaxed);
        remaining -= count as u64;
    }
    let trailer = record::read_trailer(stream)?;
    let mut bytes = Vec::new();
    record::write_trailer(&mut bytes, &trailer)?;
    let _ = profile::wait(profile::State::ToWriter, || forward.send(bytes));
    reporter.add(Count::Arrived, 1);
    Ok(())
}

/// Writes one file: into a file without a name in its directory, or under a
/// temporary name beside it where the filesystem makes none, checked
/// against the Source's hash, and given its attributes. It is left so, for
/// an fsync thread to push it to the drive, unless told not to sync, and
/// for its batch to give it its name.
///
/// The record is read to its end whatever happens to the file, so that the
/// stream stays in step; only an error reading the stream itself is returned
/// as an error.
#[allow(clippy::too_many_arguments)]
fn write_file(
    root: &Path,
    target: &Target,
    shown: &[u8],
    size: u64,
    stream: &mut impl Read,
    state: &State,
    dirs: &DirCache,
    fsync: bool,
    stop: &Stop,
) -> Result<Written> {
    // Once the Destination has stopped, no file is made: the record is only
    // read to its end.
    let opened = stop.check().and_then(|()| {
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
        Ok((directory, name, file, temporary))
    });
    let mut local_error: Option<anyhow::Error> = None;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut remaining = size;
    let (mut file, opened) = match opened {
        Ok((directory, name, file, temporary)) => (Some(file), Some((directory, name, temporary))),
        Err(error) => {
            local_error = Some(error);
            (None, None)
        }
    };
    while remaining > 0 {
        let count = buffer.len().min(remaining as usize);
        stream
            .read_exact(&mut buffer[..count])
            .context("read the record stream")?;
        hasher.update(&buffer[..count]);
        if let (Some(writing), None) = (&mut file, &local_error)
            && let Err(error) = writing.write_all(&buffer[..count])
        {
            local_error = Some(error.into());
        }
        remaining -= count as u64;
    }
    let trailer = record::read_trailer(stream)?;

    let Some((directory, name, temporary)) = opened else {
        return Ok(Written::Failed(
            local_error.expect("set when nothing was opened"),
        ));
    };
    let file = file.expect("open when the directory is");
    // Its drive, once it is ready.
    let finished = (|| -> Result<u64> {
        if let Some(error) = local_error {
            return Err(error);
        }
        if let Some(why) = trailer.unread {
            bail!("the Source could not read it whole: {why}");
        }
        if *hasher.finalize().as_bytes() != trailer.hash {
            bail!("the content's hash does not match the Source's");
        }
        apply_file_attributes(&file, shown, &target.entry, state)?;
        Ok(file.metadata()?.dev())
    })();
    match finished {
        // What it holds open goes with it, not copied.
        Ok(device) => {
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
            Ok(Written::Ready(Ready {
                directory,
                name,
                unplaced,
                exists: target.exists,
                file,
                device,
                is_root: target.is_root,
            }))
        }
        // One without a name goes as it is closed.
        Err(error) => {
            if let Some((temporary, _)) = &temporary {
                let _ = fsys::remove(&directory, temporary, false);
            }
            Ok(Written::Failed(error))
        }
    }
}

/// Owner first, since setting it clears setuid and setgid, then permissions,
/// then the mtime, last since nothing may write to the file after it. What
/// this process may not set is warned of, as `shown`, and the file kept.
fn apply_file_attributes(file: &File, shown: &[u8], entry: &Entry, state: &State) -> Result<()> {
    let metadata = file.metadata()?;
    let setters = Setters {
        chown: |uid, gid| fsys::set_owner(file, uid, gid),
        chmod: Some(|mode| fsys::set_mode(file, mode)),
        utimes: |mtime| fsys::set_mtime(file, mtime),
    };
    let current = (metadata.uid(), metadata.gid());
    Ok(set_attributes(state, shown, current, entry, setters)?)
}

/// The target of the file with `id`, the parser's own, or one still to be
/// sent to it. Each is sent before its file is asked for, so a Source has
/// it here before it sends the record; one that sends records ahead, as a
/// test's may, waits for it. `None` once every target is sent, and none is
/// it: it was never asked for.
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
