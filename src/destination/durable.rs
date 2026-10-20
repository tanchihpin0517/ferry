//! Renaming written files into place, by renamers that last the transfer:
//! a batch at a time, one barrier per drive first, then the renames, while
//! the parser goes on reading the record stream.
//!
//! A directory's files are renamed by one renamer at a time: the kernel
//! renames in a directory one at a time under its lock, so two renamers in
//! one directory only contend for it. Renamers beyond the first rename in
//! the batch's other directories, so a batch in one directory is renamed by
//! one alone. Measured with files of 4 KiB, 100 to a directory: 20,000 on
//! WekaFS, where each rename waits on a server, 26% faster than one renamer;
//! 200,000 on ext4, as fast. With 1,000 to a directory, 2 to 7% slower on
//! ext4: a batch's second directory is one the writers still create in.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::fsys;
use crate::pool;
use crate::profile::{self, State};
use crate::protocol::FileKey;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::apply::clear_the_way;
use super::write::Batched;

/// How many batches may be handed to the renamers at once, beside the one
/// filling: each holds its files' directories open as it waits.
pub(super) const RENAMING: usize = 2;

/// At most how many directories a batch is renamed in at once: a few, as
/// 2, 4, 8, and 16 renamed as fast as each other on WekaFS and NFS.
const RENAMERS: usize = 4;

/// What became of each file of a batch.
pub(super) type Renamed = Vec<(Batched, Result<()>)>;

/// Starts the renamers, under `stages`: one takes a batch at a time, sets
/// its barrier, and renames its largest directory's files, while the
/// others rename the rest, a directory each; then it sends back what became
/// of the batch's files. They end once `batches` closes.
pub(super) fn renamer<'scope>(
    stages: &Stages<'scope, '_>,
    delete: bool,
    protect: &'scope [FileKey],
    fsync: bool,
) -> (Sender<Vec<Batched>>, Receiver<Renamed>) {
    let (batches, queue) = crossbeam_channel::unbounded::<Vec<Batched>>();
    let (done, renamed) = crossbeam_channel::unbounded();
    let (to_others, in_directories) = crossbeam_channel::unbounded::<Vec<Batched>>();
    let (directory_done, directories_renamed) = crossbeam_channel::unbounded::<Renamed>();
    let renamers = pool::threads(RENAMERS);
    for _ in 1..renamers {
        let in_directories = in_directories.clone();
        let directory_done = directory_done.clone();
        stages.spawn("rename", move |_| {
            while let Ok(files) = profile::wait(State::Work, || in_directories.recv()) {
                let files =
                    profile::wait(State::Rename, || rename_all(files, delete, protect, fsync));
                if directory_done.send(files).is_err() {
                    break;
                }
            }
            Ok(())
        });
    }
    stages.spawn("rename", move |_| {
        while let Ok(batch) = profile::wait(State::Work, || queue.recv()) {
            let batch = profile::wait(State::Rename, || barrier(batch, fsync))?;
            let mut directories = by_directory(batch).into_iter();
            let mut mine = directories.next().unwrap_or_default();
            let mut handed = 0;
            for directory in directories {
                if renamers > 1 {
                    // They end only after this does.
                    let _ = to_others.send(directory);
                    handed += 1;
                } else {
                    mine.extend(directory);
                }
            }
            let mut files =
                profile::wait(State::Rename, || rename_all(mine, delete, protect, fsync));
            for _ in 0..handed {
                match profile::wait(State::ToRenamers, || directories_renamed.recv()) {
                    Ok(renamed) => files.extend(renamed),
                    Err(_) => return Err(Stopped.into()),
                }
            }
            if done.send(files).is_err() {
                break;
            }
        }
        Ok(())
    });
    (batches, renamed)
}

/// Sets a batch's barrier, one per drive, unless told not to sync: so that
/// its files are written before any is renamed.
fn barrier(mut batch: Vec<Batched>, fsync: bool) -> Result<Vec<Batched>> {
    if fsync {
        // One file of each drive is kept for its barrier, where that needs
        // one.
        let mut barriered = HashSet::new();
        for done in &batch {
            if let Some(file) = &done.ready.file
                && barriered.insert(done.ready.device)
            {
                fsys::barrier(file).context("order the batch's writes")?;
            }
        }
    }
    for done in &mut batch {
        done.ready.file = None;
    }
    Ok(batch)
}

/// A batch's files by their directory, the directory of most files first.
/// A directory evicted from the cache and opened again is two here, which
/// only costs their renamers a wait for its lock.
fn by_directory(batch: Vec<Batched>) -> Vec<Vec<Batched>> {
    let mut index = HashMap::new();
    let mut directories: Vec<Vec<Batched>> = Vec::new();
    for done in batch {
        let at = *index
            .entry(Arc::as_ptr(&done.ready.directory))
            .or_insert_with(|| {
                directories.push(Vec::new());
                directories.len() - 1
            });
        directories[at].push(done);
    }
    directories.sort_by_key(|files| std::cmp::Reverse(files.len()));
    directories
}

/// Renames files into place, one after another. Returns each file with
/// what became of its rename.
fn rename_all(files: Vec<Batched>, delete: bool, protect: &[FileKey], fsync: bool) -> Renamed {
    files
        .into_iter()
        .map(|mut done| {
            let result = rename_into_place(&mut done, delete, protect, fsync);
            (done, result)
        })
        .collect()
}

/// The renamer, as the parser hands it batches, and takes back what became
/// of each file.
pub(super) struct Renaming {
    batches: Sender<Vec<Batched>>,
    renamed: Receiver<Renamed>,
    /// Files handed over and not taken back.
    files: usize,
    /// At most how many may be, as another batch is handed over: so that
    /// at most [`RENAMING`] batches are.
    room: usize,
}

impl Renaming {
    pub(super) fn new(
        (batches, renamed): (Sender<Vec<Batched>>, Receiver<Renamed>),
        batch_size: usize,
    ) -> Self {
        Self {
            batches,
            renamed,
            files: 0,
            room: (RENAMING - 1) * batch_size,
        }
    }

    /// Hands `batch` over, once there is room for it, to be renamed into
    /// place. What is renamed meanwhile goes to `take`.
    pub(super) fn hand(
        &mut self,
        batch: Vec<Batched>,
        stop: &Stop,
        take: &impl Fn(Renamed) -> Result<()>,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        while self.files > self.room {
            self.wait(stop, take)?;
        }
        self.files += batch.len();
        // The renamer is gone only once it failed, and said why.
        self.batches.send(batch).map_err(|_| Stopped.into())
    }

    /// Gives `take` what is renamed already, without waiting.
    pub(super) fn take_renamed(&mut self, take: &impl Fn(Renamed) -> Result<()>) -> Result<()> {
        while let Ok(renamed) = self.renamed.try_recv() {
            self.files -= renamed.len();
            take(renamed)?;
        }
        Ok(())
    }

    /// Waits for every file handed over to be renamed, giving each to
    /// `take`.
    pub(super) fn finish(
        &mut self,
        stop: &Stop,
        take: &impl Fn(Renamed) -> Result<()>,
    ) -> Result<()> {
        while self.files > 0 {
            self.wait(stop, take)?;
        }
        Ok(())
    }

    /// Waits for the next batch renamed, and gives it to `take`.
    fn wait(&mut self, stop: &Stop, take: &impl Fn(Renamed) -> Result<()>) -> Result<()> {
        let renamed = profile::wait(State::ToRename, || {
            crossbeam_channel::select! {
                recv(self.renamed) -> renamed => renamed.map_err(|_| Stopped),
                recv(stop.signal()) -> _ => Err(Stopped),
            }
        })?;
        self.files -= renamed.len();
        take(renamed)
    }
}

/// Renames one file of a batch into place, or removes it if that fails.
fn rename_into_place(
    done: &mut Batched,
    delete: bool,
    protect: &[FileKey],
    fsync: bool,
) -> Result<()> {
    let ready = &mut done.ready;
    let renamed = (|| -> Result<()> {
        clear_the_way(&ready.directory, &ready.name, delete, protect)?;
        fsys::rename(&ready.directory, &ready.temporary, &ready.name)?;
        // At its name now: whatever follows, it is not removed.
        ready.registered.disarm();
        // A root that is a file has no directory of its own that is
        // finished; its name is made durable here.
        if fsync && ready.is_root {
            fsys::fsync(&ready.directory)?;
        }
        Ok(())
    })();
    if renamed.is_err() {
        let _ = fsys::remove(&ready.directory, &ready.temporary, false);
    }
    renamed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::write::Ready;
    use crate::interrupt;
    use std::sync::Mutex;

    /// Batches of five, in three directories, handed to the renamers as
    /// they fill, more than [`RENAMING`] of them: each file is renamed into
    /// place in its directory, and taken back, once, and nothing is left
    /// under a temporary name.
    #[test]
    fn the_renamers_rename_every_batch_handed_to_them() {
        let temp = tempfile::tempdir().unwrap();
        let paths: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                let path = temp.path().join(name);
                std::fs::create_dir(&path).unwrap();
                path
            })
            .collect();
        let directories: Vec<_> = paths
            .iter()
            .map(|path| Arc::new(fsys::open_root(path.as_os_str()).unwrap()))
            .collect();
        let stop = Stop::new(None);
        let taken = Mutex::new(Vec::new());
        let take = |renamed: Renamed| -> Result<()> {
            for (done, result) in renamed {
                result.unwrap();
                taken.lock().unwrap().push(done.id);
            }
            Ok(())
        };
        std::thread::scope(|scope| {
            let stages = Stages::new(scope, &stop);
            let mut renaming = Renaming::new(renamer(&stages, false, &[], true), 5);
            let mut batch = Vec::new();
            for id in 0..30 {
                let directory = &directories[id as usize % 3];
                let name = format!("f{id:02}").into_bytes();
                let (file, temporary, registered) =
                    interrupt::create(directory, || fsys::create_temporary(&**directory, &name))
                        .unwrap();
                batch.push(Batched {
                    id,
                    shown: Vec::new(),
                    size: 0,
                    ready: Ready {
                        directory: Arc::clone(directory),
                        name,
                        temporary,
                        file: Some(file),
                        device: 1,
                        is_root: false,
                        registered,
                    },
                });
                if batch.len() == 5 {
                    renaming
                        .hand(std::mem::take(&mut batch), &stop, &take)
                        .unwrap();
                }
            }
            renaming.hand(batch, &stop, &take).unwrap();
            renaming.finish(&stop, &take).unwrap();
        });
        let mut taken = taken.into_inner().unwrap();
        taken.sort();
        assert_eq!(taken, (0..30).collect::<Vec<_>>());
        for (at, path) in paths.iter().enumerate() {
            let mut names: Vec<String> = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            let wanted: Vec<String> = (0..30)
                .filter(|id| id % 3 == at)
                .map(|id| format!("f{id:02}"))
                .collect();
            assert_eq!(names, wanted);
        }
    }
}
