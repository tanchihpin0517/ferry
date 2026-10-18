//! Writing the files of the record stream: parsed in order, written by a
//! pool of writers, each under a temporary name, then renamed in batches.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::fsys;
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

use super::apply::{Event, file_failed_in, set_owner};
use super::durable::rename_batch;
use super::{State, is_protected, open_under, root_parent};

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
pub(super) struct Target {
    /// Shared by the files of a directory.
    pub(super) directory: Arc<[u8]>,
    pub(super) entry: Entry,
    /// The root itself, which is a file: said so, rather than taken from a
    /// name, which a manifest sets.
    pub(super) is_root: bool,
}

/// What became of one record.
enum Written {
    /// Written, checked, and pushed to the drive, under its temporary name:
    /// ready to be renamed into place with its batch.
    Ready(Ready),
    Failed(anyhow::Error),
}

/// A file written under its temporary name, waiting for its batch.
pub(super) struct Ready {
    /// Shared with its registration.
    pub(super) directory: Arc<OwnedFd>,
    pub(super) name: Vec<u8>,
    pub(super) temporary: Vec<u8>,
    /// Kept open, to set a barrier on its drive.
    pub(super) file: File,
    /// Its drive, for one barrier per drive.
    pub(super) device: u64,
    /// The root is this file: its directory is not one that is finished.
    pub(super) is_root: bool,
    /// Removes the file when dropped, unless it was renamed into place: so
    /// a batch that never comes to be renamed leaves nothing behind.
    pub(super) registered: interrupt::Registered,
}

/// A file handed to a writer: where it goes, and its record's bytes, which
/// the parser forwards as it reads them.
struct FileJob {
    pub(super) id: FileId,
    pub(super) target: Target,
    pub(super) shown: Vec<u8>,
    pub(super) size: u64,
    pub(super) bytes: mpsc::Receiver<Vec<u8>>,
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
    stop: &mpsc::Receiver<()>,
) {
    let mut reported = false;
    while let Err(mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(TRANSFERRING_EVERY) {
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

/// Descriptors left for all but the files waiting for their batch: the
/// writers' files and directories, the directories being compared, and the
/// transport's.
const KEPT_OPEN: u64 = 128;

/// How many files a batch holds, where at most `open_files` may be open at
/// once: each waiting in it holds itself and its directory open, so a
/// smaller batch, where the limit is low, rather than files failing for
/// want of a descriptor.
fn batch_size(open_files: u64) -> usize {
    let room = open_files.saturating_sub(KEPT_OPEN) / 2;
    room.clamp(1, BATCH as u64) as usize
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
/// A writer leaves its file under its temporary name, pushed to the drive.
/// This thread renames them in batches: one barrier per drive, so that every
/// file of the batch reaches storage before any of their names, then the
/// renames, in parallel.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_files(
    root: &Path,
    state: &Mutex<State>,
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
    let reporter = state.lock().expect("not poisoned").reporter.clone();
    // The files handed to the writers and not yet settled, by when they
    // started, for the dashboard.
    let transferring: Mutex<Vec<Transferring>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| -> Result<()> {
        let stages = Stages::new(scope, stop);
        // Dropped when this returns, which stops the reports.
        let (reports_end, reports_ended) = mpsc::channel::<()>();
        if reporter.is_open() {
            let (transferring, reporter) = (&transferring, reporter.clone());
            scope.spawn(move || report_transferring(transferring, &reporter, &reports_ended));
        }
        let _reports_end = reports_end;
        let (jobs, queue) = crossbeam_channel::bounded::<FileJob>(writers);
        let (finished, outcomes) =
            crossbeam_channel::unbounded::<(FileId, Vec<u8>, u64, Written)>();
        for _ in 0..writers {
            let queue = queue.clone();
            let finished = finished.clone();
            stages.spawn("writer", move |stop| {
                while let Ok(job) = profile::wait(profile::State::Work, || queue.recv()) {
                    let mut bytes = ChannelReader::new(job.bytes, profile::State::Bytes);
                    let written = profile::wait(profile::State::WriteFile, || {
                        write_file(root, &job.target, job.size, &mut bytes, state, fsync, stop)
                    })
                    .unwrap_or_else(Written::Failed);
                    if finished
                        .send((job.id, job.shown, job.size, written))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(())
            });
        }
        drop(finished);
        drop(queue);

        let mut in_flight = 0usize;
        // Files handed to the writers: a full queue, and every writer busy.
        let writing = reporter.gauge(Stage::Write, writers * 2);
        let batch_size = batch_size(crate::limits::open_files());
        let mut batch: Vec<Batched> = Vec::with_capacity(batch_size);
        let fail = |id: FileId, shown: &[u8], message: String| {
            file_failed_in(state, &reporter, mux, stop, id, shown, message)
        };
        // What became of each file, for its directory, which may wait for
        // it; once the applier is gone, it said why.
        let settled = |id: FileId| events.send(Event::Settled(id)).map_err(|_| Stopped);
        // Renames the batch into place, and reports each file.
        let flush = |batch: &mut Vec<Batched>| -> Result<()> {
            if batch.is_empty() {
                return Ok(());
            }
            let renamed = profile::wait(profile::State::Rename, || {
                rename_batch(std::mem::take(batch), state, fsync, stop)
            })?;
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
                    Err(error) => fail(done.id, &done.shown, format!("{error:#}"))?,
                }
            }
            Ok(())
        };
        // What becomes of a file once its writer is done with it.
        let settle = |(id, shown, size, written): (FileId, Vec<u8>, u64, Written),
                      batch: &mut Vec<Batched>|
         -> Result<()> {
            transferring
                .lock()
                .expect("not poisoned")
                .retain(|file| file.id != id);
            match written {
                Written::Ready(ready) => {
                    batch.push(Batched {
                        id,
                        shown,
                        size,
                        ready,
                    });
                    if batch.len() >= batch_size {
                        flush(batch)?;
                    }
                    Ok(())
                }
                Written::Failed(error) => {
                    settled(id)?;
                    fail(id, &shown, format!("{error:#}"))
                }
            }
        };
        loop {
            while let Ok(outcome) = outcomes.try_recv() {
                in_flight -= 1;
                settle(outcome, &mut batch)?;
            }
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
                    let (forward, bytes) = mpsc::sync_channel(4);
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
                        settle(outcome, &mut batch)?;
                    }
                    flush(&mut batch)?;
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
    forward: &mpsc::SyncSender<Vec<u8>>,
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

/// Writes one file: into a temporary file beside it, checked against the
/// Source's hash, given its attributes, and pushed to the drive unless told
/// not to sync. It is left under its temporary name, for its batch to rename.
///
/// The record is read to its end whatever happens to the file, so that the
/// stream stays in step; only an error reading the stream itself is returned
/// as an error.
fn write_file(
    root: &Path,
    target: &Target,
    size: u64,
    stream: &mut impl Read,
    state: &Mutex<State>,
    fsync: bool,
    stop: &Stop,
) -> Result<Written> {
    // Once the Destination has stopped, no temporary file is made: the
    // record is only read to its end.
    let opened = stop.check().and_then(|()| {
        let (directory, name) = target_directory(root, target, state)?;
        let directory = Arc::new(directory);
        let (file, temporary, registered) =
            interrupt::create(&directory, || fsys::create_temporary(&directory, &name))?;
        Ok((directory, name, file, temporary, registered))
    });
    let mut local_error: Option<anyhow::Error> = None;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut remaining = size;
    let (mut file, opened) = match opened {
        Ok((directory, name, file, temporary, registered)) => {
            (Some(file), Some((directory, name, temporary, registered)))
        }
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

    let Some((directory, name, temporary, registered)) = opened else {
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
        apply_file_attributes(&file, &target.entry, state)?;
        if fsync {
            fsys::fsync(&file)?;
        }
        Ok(file.metadata()?.dev())
    })();
    match finished {
        // What it holds open goes with it, not copied.
        Ok(device) => Ok(Written::Ready(Ready {
            directory,
            name,
            temporary,
            file,
            device,
            is_root: target.is_root,
            registered,
        })),
        Err(error) => {
            let _ = fsys::remove(&directory, &temporary, false);
            Ok(Written::Failed(error))
        }
    }
}

/// Owner first, since setting it clears setuid and setgid, then permissions,
/// then the mtime, last since nothing may write to the file after it.
fn apply_file_attributes(file: &File, entry: &Entry, state: &Mutex<State>) -> Result<()> {
    let metadata = file.metadata()?;
    let ownership = Arc::clone(&state.lock().expect("not poisoned").ownership);
    set_owner(
        &ownership,
        metadata.uid(),
        metadata.gid(),
        entry,
        |uid, gid| fsys::set_owner(file, uid, gid),
    )?;
    fsys::set_mode(file, entry.mode & 0o7777)?;
    fsys::set_mtime(file, entry.mtime)?;
    Ok(())
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
    state: &Mutex<State>,
) -> Result<(OwnedFd, Vec<u8>)> {
    if target.is_root {
        return root_parent(root);
    }
    let directory = open_under(
        &state.lock().expect("not poisoned"),
        root,
        &target.directory,
    )?;
    Ok((directory, target.entry.name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch shrinks where few files may be open, and is never empty.
    #[test]
    fn a_batch_fits_the_open_file_limit() {
        assert_eq!(batch_size(u64::MAX), BATCH);
        assert_eq!(batch_size(1024), BATCH);
        assert_eq!(batch_size(256), 64);
        assert_eq!(batch_size(130), 1);
        assert_eq!(batch_size(16), 1);
    }
}
