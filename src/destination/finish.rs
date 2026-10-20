//! Finishing directories, once nothing more is made in them, and making
//! what was written durable at the end.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::dirs::DirCache;
use crate::fsys;
use crate::manifest::Entry;
use crate::mux::Mux;
use crate::source::display;
use crate::stop::Stop;

use super::apply::{Setters, depth, file_failed_in, set_attributes};
use super::{State, ends_the_transfer, root_parent};

/// Finishes each directory as it is done, in that order: a directory's
/// after all under it. One that cannot be finished fails alone, reported
/// as its entry, and the rest are finished all the same; but where the drive
/// is full or read-only, the transfer stops.
pub(super) fn finish_as_done(
    state: &State,
    dirs: &DirCache,
    mux: &Mux,
    stop: &Stop,
    done: crossbeam_channel::Receiver<(Vec<u8>, Entry)>,
    fsync: bool,
) -> Result<()> {
    while let Ok((path, entry)) = crate::profile::wait(crate::profile::State::Work, || done.recv())
    {
        let shown = display(&path, true);
        if let Err(error) = finish_directory(state, dirs, &path, &shown, &entry, fsync) {
            if ends_the_transfer(&error) {
                return Err(error.context(format!("finish {}", String::from_utf8_lossy(&shown))));
            }
            let message = format!("could not be finished: {error:#}");
            file_failed_in(state, &state.reporter, mux, stop, entry.id, &shown, message)?;
        }
    }
    Ok(())
}

/// Sets a directory's owner, permissions, and mtime, now that nothing more
/// is made in it nor under it, and makes it durable unless told not to. Each
/// is tried, whatever came of those before it; what this process may not
/// set is warned of, as `shown`.
fn finish_directory(
    state: &State,
    dirs: &DirCache,
    path: &[u8],
    shown: &[u8],
    entry: &Entry,
    fsync: bool,
) -> Result<()> {
    let directory = dirs.open(path)?;
    let stat = rustix::fs::fstat(&*directory)?;
    #[allow(clippy::unnecessary_cast)]
    let current = (stat.st_uid as u32, stat.st_gid as u32);
    let set = set_attributes(
        state,
        shown,
        current,
        entry,
        Setters {
            chown: |uid, gid| fsys::set_owner(&*directory, uid, gid),
            chmod: Some(|mode| fsys::set_mode(&*directory, mode)),
            utimes: |mtime| fsys::set_mtime(&*directory, mtime),
        },
    );
    // Its permissions were set, or were never to be: it is let in no more.
    state.let_in.lock().expect("not poisoned").remove(path);
    let synced = if fsync {
        fsys::fsync(&*directory)
    } else {
        Ok(())
    };
    set?;
    Ok(synced?)
}

/// Gives each directory let into, and not finished, the permissions it had,
/// deepest first, so that each is still reached through its parent: the
/// transfer failed, and will set them no more.
pub(super) fn restore_let_in(state: &State, dirs: &DirCache) {
    let mut let_in: Vec<(Vec<u8>, u32)> =
        state.let_in.lock().expect("not poisoned").drain().collect();
    let_in.sort_by_key(|(path, _)| std::cmp::Reverse(depth(path)));
    for (path, mode) in let_in {
        if let Ok(directory) = dirs.open(&path) {
            let _ = fsys::set_mode(&*directory, mode);
        }
    }
}

/// Makes everything this transfer wrote durable, once, at the end: what was
/// only ordered on the way reaches storage.
pub(super) fn make_durable(root: &Path, dirs: &DirCache, fsync: bool) -> Result<()> {
    if !fsync {
        return Ok(());
    }
    let directory = match dirs.held_root() {
        Some(directory) => directory,
        // A root that is a file: its directory.
        None => Arc::new(root_parent(root)?.0),
    };
    fsys::sync_durable(&*directory).context("make what was written durable")
}
