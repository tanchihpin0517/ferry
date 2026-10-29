//! Writing the files the Source sends: the writers, each handed by the
//! decider a whole file, or a segment of a large one, a file's segments one
//! after another, in the order they came; each file is written without a
//! name where the filesystem can make one, or under a temporary name, then
//! given its own by the renamers, in batches (`rename`). A writer only says
//! what became of what it was handed.

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::block::{Part, Shared};
use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::inject;
use crate::interrupt;
use crate::manifest::{Entry, FileId};
use crate::profile;
use crate::stage::Stages;
use crate::stop::Stop;

use super::apply::{Setters, Warned, set_attributes};
use super::decider::Event;
use super::rename::RENAMING;
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

/// What the checkers hand on, a record at a time, in the order the Source
/// sent them: what it holds of a file, checked.
pub(super) struct Arrival(pub(super) Vec<Part>);

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
}

/// A file made, and not written whole yet: where it is, and how it goes.
pub(super) struct Partial {
    directory: Arc<OwnedFd>,
    name: Vec<u8>,
    file: File,
    /// Its temporary name, where it has one, registered.
    temporary: Option<(Vec<u8>, interrupt::Registered)>,
    /// One of its segments is being written: never two at once, which a
    /// debug build checks.
    writing: AtomicBool,
}

#[cfg(test)]
impl Partial {
    /// A file made for its segments, as a writer makes one, of `file` open:
    /// for a test of what takes it, not of writers.
    pub(super) fn opened(file: File) -> Self {
        let directory = Arc::new(OwnedFd::from(File::open("/").expect("the root opens")));
        Self {
            directory,
            name: Vec::new(),
            file,
            temporary: None,
            writing: AtomicBool::new(false),
        }
    }
}

/// What a writer is handed.
pub(super) enum Job {
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
        partial: Arc<Partial>,
        offset: u64,
        bytes: Shared,
    },
    /// A file whose segments are all written, and its hash found to be the
    /// Source's: given its attributes.
    Finish {
        id: FileId,
        partial: Partial,
        target: Target,
        shown: Vec<u8>,
        size: u64,
    },
    /// A file made that will not be given its name: let go of, and removed,
    /// where it has a temporary name. Nothing is said of it.
    Discard(Partial),
}

/// How many files are handed to the renamers together.
pub(super) const BATCH: usize = 256;

/// How long a batch not full may wait for more before it is handed over
/// all the same: so that files written while no more come, as while a
/// large file is still sent, take their names within a second rather than
/// at the end. Small files coming fill a batch long before.
pub(super) const BATCH_WAIT: Duration = Duration::from_secs(1);

/// Descriptors left for all but the files waiting for their batch: the
/// writers' files and directories, the directories being compared, those
/// kept for the stages to share, and the transport's.
const KEPT_OPEN: u64 = 128 + dirs::KEPT as u64;

/// How many files a batch holds, where at most `open_files` may be open at
/// once: each waiting in a batch holds its directory open, shared with the
/// others there, and itself, without a name, where `unnamed`. The batch
/// filling waits beside those being renamed. So a smaller batch, where the
/// limit is low, rather than files failing for want of a descriptor.
pub(super) fn batch_size(open_files: u64, unnamed: bool) -> usize {
    let each = if unnamed { 2 } else { 1 };
    let room = open_files.saturating_sub(KEPT_OPEN) / (each * (RENAMING as u64 + 1));
    room.clamp(1, BATCH as u64) as usize
}

/// At most how many files written may wait for their part to be made, to
/// take their names.
const WAITING: usize = 1 << 16;

/// How many files written may wait for their part to be made, beside the
/// batches, where at most `open_files` may be open at once: as many as the
/// descriptors left over from those, each holding itself open, without a
/// name, where `unnamed`, and its directory; but at least one, and no more
/// than [`WAITING`].
pub(super) fn waiting_size(open_files: u64, unnamed: bool, batch_size: usize) -> usize {
    let each = if unnamed { 2 } else { 1 };
    let room = open_files.saturating_sub(KEPT_OPEN) / each;
    let batches = (RENAMING as u64 + 1) * batch_size as u64;
    room.saturating_sub(batches).clamp(1, WAITING as u64) as usize
}

/// A file of a batch, and what to report of it.
pub(super) struct Batched {
    pub(super) id: FileId,
    pub(super) shown: Vec<u8>,
    pub(super) size: u64,
    pub(super) ready: Ready,
}

/// Starts `writers` writers, under `stages`: each does a job at a time, and
/// says what became of it on `reported`. They end once `jobs`, returned,
/// closes.
pub(super) fn writers<'scope>(
    stages: &Stages<'scope, '_>,
    writers: usize,
    root: &'scope Path,
    state: &'scope State,
    dirs: &'scope DirCache,
    reported: &Sender<Event>,
) -> Sender<Job> {
    let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
    for _ in 0..writers {
        let queue = queue.clone();
        let reported = reported.clone();
        stages.spawn("writer", move |stop| {
            while let Ok(job) = profile::wait(profile::State::Work, || queue.recv()) {
                let path;
                let _on = match &job {
                    Job::Whole { shown, .. } | Job::Finish { shown, .. } => {
                        Some(profile::on(shown))
                    }
                    Job::Make { target, .. } => {
                        path = crate::manifest::join(&target.directory, &target.entry.name);
                        Some(profile::on(&path))
                    }
                    // Its name: its directory is open, its path not at hand.
                    Job::Segment { partial, .. } => Some(profile::on(&partial.name)),
                    _ => None,
                };
                let done = profile::wait(profile::State::WriteFile, || {
                    work(job, root, state, dirs, stop)
                });
                if let Some(done) = done
                    && reported.send(done).is_err()
                {
                    break;
                }
            }
            Ok(())
        });
    }
    jobs
}

/// Does what a writer is handed, and says what became of it, but for a
/// file let go of.
fn work(job: Job, root: &Path, state: &State, dirs: &DirCache, stop: &Stop) -> Option<Event> {
    Some(match job {
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
                stop,
                &mut warned,
            );
            Event::Written {
                id,
                shown,
                size: bytes.len() as u64,
                written,
                warned,
            }
        }
        Job::Make { id, target } => Event::FileMade {
            id,
            made: make(root, &target, dirs, stop),
        },
        Job::Segment {
            id,
            partial,
            offset,
            bytes,
        } => {
            let busy = partial.writing.swap(true, Ordering::Relaxed);
            debug_assert!(!busy, "two segments of a file written at once");
            // Once the Destination has stopped, nothing more is written.
            let written = stop
                .check()
                .and_then(|()| Ok(partial.file.write_all_at(&bytes, offset)?));
            partial.writing.store(false, Ordering::Relaxed);
            Event::SegmentWritten { id, written }
        }
        Job::Finish {
            id,
            partial,
            target,
            shown,
            size,
        } => {
            let mut warned = Vec::new();
            let written = finish(partial, &target, &shown, state, &mut warned);
            Event::Written {
                id,
                shown,
                size,
                written,
                warned,
            }
        }
        Job::Discard(partial) => {
            discard(partial);
            return None;
        }
    })
}

/// Makes a file for `target`: without a name in its directory, or under a
/// temporary name beside it where the filesystem makes none. Once the
/// Destination has stopped, none is made.
fn make(root: &Path, target: &Target, dirs: &DirCache, stop: &Stop) -> Result<Partial> {
    stop.check()?;
    let (directory, name) = target_directory(root, target, dirs)?;
    let unnamed = if inject::failing("unnamed") || inject::failing("close") {
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
    Ok(Partial {
        directory,
        name,
        file,
        temporary,
        writing: AtomicBool::new(false),
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
    stop: &Stop,
    warned: &mut Warned,
) -> Result<Ready> {
    let partial = make(root, target, dirs, stop)?;
    let written = inject::fail_if("write").and_then(|()| Ok((&partial.file).write_all(bytes)?));
    if let Err(error) = written {
        discard(partial);
        return Err(error);
    }
    finish(partial, target, shown, state, warned)
}

/// Lets go of a file made that will not be given its name: one without a
/// name goes as it is closed, one under a temporary name is removed.
fn discard(partial: Partial) {
    if let Some((temporary, _)) = &partial.temporary {
        let _ = fsys::remove(&partial.directory, temporary, false);
    }
}

/// Gives a file written its attributes, and leaves it so, for its batch to
/// give it its name.
fn finish(
    partial: Partial,
    target: &Target,
    shown: &[u8],
    state: &State,
    warned: &mut Warned,
) -> Result<Ready> {
    if let Err(error) = apply_file_attributes(&partial.file, shown, &target.entry, state, warned) {
        discard(partial);
        return Err(error);
    }
    let Partial {
        directory,
        name,
        file,
        temporary,
        ..
    } = partial;
    // What it holds open goes with it, not copied. One under a temporary
    // name is closed here, and fails if closing it does, as rsync checks it:
    // on NFS, closing it is where an error writing it back, such as the
    // server's drive being full, is told. One without a name is written on
    // a filesystem of this machine, where closing it writes nothing back.
    let unplaced = match temporary {
        None => Unplaced::Unnamed(file),
        Some((temporary, registered)) => {
            if let Err(error) = inject::fail_if("close").and_then(|()| Ok(fsys::close(file)?)) {
                let _ = fsys::remove(&directory, &temporary, false);
                return Err(error);
            }
            Unplaced::Named {
                temporary,
                registered,
            }
        }
    };
    Ok(Ready {
        directory,
        name,
        unplaced,
        exists: target.exists,
    })
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
        assert_eq!(batch_size(u64::MAX, false), BATCH);
        assert_eq!(batch_size(1024, false), BATCH);
        assert_eq!(batch_size(256, false), 32);
        assert_eq!(batch_size(130, false), 1);
        assert_eq!(batch_size(16, false), 1);
        assert_eq!(batch_size(u64::MAX, true), BATCH);
        assert_eq!(batch_size(1024, true), 144);
        assert_eq!(batch_size(256, true), 16);
        assert_eq!(batch_size(16, true), 1);
    }

    /// As many files may wait for their part to be made as descriptors are
    /// left over from the batches, but at least one, and at most
    /// [`WAITING`].
    #[test]
    fn files_waiting_fit_what_is_left_of_the_open_file_limit() {
        assert_eq!(waiting_size(u64::MAX, true, BATCH), WAITING);
        assert_eq!(waiting_size(1 << 20, false, BATCH), WAITING);
        // 864 descriptors left: 3 batches of 144, two each, take them all.
        assert_eq!(waiting_size(1024, true, 144), 1);
        // 3 batches of 256, one each, leave 96.
        assert_eq!(waiting_size(1024, false, 256), 96);
        assert_eq!(waiting_size(16, true, 1), 1);
    }
}
