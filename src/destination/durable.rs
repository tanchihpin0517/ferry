//! Renaming written files into place, by one renamer that lasts the
//! transfer: a batch at a time, one barrier per drive first, then the
//! renames, while the parser goes on reading the record stream.
//!
//! One renamer, rather than several: renames in one directory contend for
//! its lock, and with fsync, for the journal the writers' fsyncs commit.
//! Measured with files of 4 KiB, 200,000 on ext4 and 20,000 on NFS, one
//! renamed them faster than 2 or 8 did, with fsync and without.

use std::collections::HashSet;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::fsys;
use crate::profile::{self, State};
use crate::protocol::FileKey;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::apply::clear_the_way;
use super::write::Batched;

/// How many batches may be handed to the renamer at once, beside the one
/// filling: each holds its files' directories open as it waits.
pub(super) const RENAMING: usize = 2;

/// What became of each file of a batch.
pub(super) type Renamed = Vec<(Batched, Result<()>)>;

/// Starts the renamer, under `stages`: it renames a batch at a time, and
/// sends back what became of its files. It ends once `batches` closes.
pub(super) fn renamer<'scope>(
    stages: &Stages<'scope, '_>,
    delete: bool,
    protect: &'scope [FileKey],
    fsync: bool,
) -> (Sender<Vec<Batched>>, Receiver<Renamed>) {
    let (batches, queue) = crossbeam_channel::unbounded::<Vec<Batched>>();
    let (done, renamed) = crossbeam_channel::unbounded();
    stages.spawn("rename", move |_| {
        while let Ok(batch) = profile::wait(State::Work, || queue.recv()) {
            let files = profile::wait(State::Rename, || {
                rename_batch(batch, delete, protect, fsync)
            })?;
            if done.send(files).is_err() {
                break;
            }
        }
        Ok(())
    });
    (batches, renamed)
}

/// Renames a batch of files into place: one barrier per drive first, unless
/// told not to sync, then the renames. Returns each file with what became
/// of its rename.
fn rename_batch(
    mut batch: Vec<Batched>,
    delete: bool,
    protect: &[FileKey],
    fsync: bool,
) -> Result<Renamed> {
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
    Ok(batch
        .into_iter()
        .map(|mut done| {
            let result = rename_into_place(&mut done, delete, protect, fsync);
            (done, result)
        })
        .collect())
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
    use std::sync::{Arc, Mutex};

    /// Batches of three, handed to the renamer as they fill, more than
    /// [`RENAMING`] of them: each file is renamed into place, and taken
    /// back, once, and nothing is left under a temporary name.
    #[test]
    fn the_renamer_renames_every_batch_handed_to_it() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Arc::new(fsys::open_root(temp.path().as_os_str()).unwrap());
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
            let mut renaming = Renaming::new(renamer(&stages, false, &[], true), 3);
            let mut batch = Vec::new();
            for id in 0..10 {
                let name = format!("f{id}").into_bytes();
                let (file, temporary, registered) =
                    interrupt::create(&directory, || fsys::create_temporary(&*directory, &name))
                        .unwrap();
                batch.push(Batched {
                    id,
                    shown: Vec::new(),
                    size: 0,
                    ready: Ready {
                        directory: Arc::clone(&directory),
                        name,
                        temporary,
                        file: Some(file),
                        device: 1,
                        is_root: false,
                        registered,
                    },
                });
                if batch.len() == 3 {
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
        assert_eq!(taken, (0..10).collect::<Vec<_>>());
        let mut names: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut wanted: Vec<String> = (0..10).map(|id| format!("f{id}")).collect();
        wanted.sort();
        assert_eq!(names, wanted);
    }
}
