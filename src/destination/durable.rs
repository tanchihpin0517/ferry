//! Renaming a batch of written files into place: one barrier per drive,
//! then the renames, in parallel.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::fsys;
use crate::pool;
use crate::protocol::FileKey;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::State;
use super::apply::clear_the_way;
use super::write::{Batched, Ready};

/// Renames a batch of files into place: one barrier per drive first, unless
/// told not to sync, then the renames, in parallel. Returns each file with
/// what became of its rename; [`Stopped`] if a renamer failed, which it said
/// itself.
pub(super) fn rename_batch(
    batch: Vec<Batched>,
    state: &State,
    fsync: bool,
    stop: &Stop,
) -> Result<Vec<(Batched, Result<()>)>> {
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
    let (delete, protect) = { (state.delete, Arc::clone(&state.protect)) };
    let threads = pool::threads(8).min(batch.len()).max(1);
    let chunk = batch.len().div_ceil(threads);
    let mut batch = batch;
    let mut chunks = Vec::new();
    while !batch.is_empty() {
        let rest = batch.split_off(chunk.min(batch.len()));
        chunks.push(std::mem::replace(&mut batch, rest));
    }
    std::thread::scope(|scope| {
        let stages = Stages::new(scope, stop);
        let renamers: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let protect = Arc::clone(&protect);
                stages.spawn("rename", move |_| {
                    Ok(chunk
                        .into_iter()
                        .map(|mut done| {
                            let result =
                                rename_into_place(&mut done.ready, delete, &protect, fsync);
                            (done, result)
                        })
                        .collect::<Vec<_>>())
                })
            })
            .collect();
        let mut renamed = Vec::new();
        for renamer in renamers {
            renamed.extend(renamer.join().ok_or(Stopped)?);
        }
        Ok(renamed)
    })
}

/// Renames one file of a batch into place, or removes it if that fails.
fn rename_into_place(
    ready: &mut Ready,
    delete: bool,
    protect: &[FileKey],
    fsync: bool,
) -> Result<()> {
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
