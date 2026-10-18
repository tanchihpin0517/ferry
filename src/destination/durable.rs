//! Renaming a batch of written files into place: one barrier per drive,
//! then the renames, in parallel.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::fsys;
use crate::pool::{self};
use crate::profile;
use crate::protocol::FileKey;

use super::State;
use super::apply::clear_the_way;
use super::write::{Batched, Ready};

/// Renames a batch of files into place: one barrier per drive first, unless
/// told not to sync, then the renames, in parallel. Returns each file with
/// what became of its rename.
pub(super) fn rename_batch(
    batch: Vec<Batched>,
    state: &Mutex<State>,
    fsync: bool,
) -> Result<Vec<(Batched, Result<()>)>> {
    if fsync {
        let mut barriered = HashSet::new();
        for done in &batch {
            if barriered.insert(done.ready.device) {
                fsys::barrier(&done.ready.file).context("order the batch's writes")?;
            }
        }
    }
    let (delete, protect) = {
        let state = state.lock().expect("not poisoned");
        (state.delete, Arc::clone(&state.protect))
    };
    let threads = pool::threads(8).min(batch.len()).max(1);
    let chunk = batch.len().div_ceil(threads);
    let mut batch = batch;
    let mut chunks = Vec::new();
    while !batch.is_empty() {
        let rest = batch.split_off(chunk.min(batch.len()));
        chunks.push(std::mem::replace(&mut batch, rest));
    }
    let renamed = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let protect = Arc::clone(&protect);
                scope.spawn(move || {
                    let _thread = profile::thread("rename");
                    chunk
                        .into_iter()
                        .map(|mut done| {
                            let result =
                                rename_into_place(&mut done.ready, delete, &protect, fsync);
                            (done, result)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a renamer panicked"))
            .collect()
    });
    Ok(renamed)
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
