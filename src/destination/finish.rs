//! Finishing directories, once nothing more is made in them, and making
//! what was written durable at the end.

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::dirs::DirCache;
use crate::fsys;
use crate::manifest::Entry;
use crate::source::display;

use super::apply::{depth, set_owner};
use super::{State, root_parent};

/// Finishes each directory as it is done, in that order: a directory's
/// after all under it.
pub(super) fn finish_as_done(
    state: &Mutex<State>,
    dirs: &DirCache,
    done: mpsc::Receiver<(Vec<u8>, Entry)>,
    fsync: bool,
) -> Result<()> {
    for (path, entry) in done {
        finish_directory(state, dirs, &path, &entry, fsync)?;
    }
    Ok(())
}

/// Sets a directory's owner, permissions, and mtime, now that nothing more
/// is made in it nor under it, and makes it durable unless told not to.
fn finish_directory(
    state: &Mutex<State>,
    dirs: &DirCache,
    path: &[u8],
    entry: &Entry,
    fsync: bool,
) -> Result<()> {
    let ownership = Arc::clone(&state.lock().expect("not poisoned").ownership);
    let directory = dirs.open(path);
    let finished = (|| -> Result<()> {
        let directory = directory?;
        let stat = rustix::fs::fstat(&directory)?;
        #[allow(clippy::unnecessary_cast)]
        let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
        set_owner(&ownership, uid, gid, entry, |uid, gid| {
            fsys::set_owner(&directory, uid, gid)
        })?;
        fsys::set_mode(&directory, entry.mode & 0o7777)?;
        state.lock().expect("not poisoned").let_in.remove(path);
        fsys::set_mtime(&directory, entry.mtime)?;
        if fsync {
            fsys::fsync(&directory)?;
        }
        Ok(())
    })();
    finished.with_context(|| format!("finish {}", String::from_utf8_lossy(&display(path, true))))
}

/// Gives each directory let into, and not finished, the permissions it had,
/// deepest first, so that each is still reached through its parent: the
/// transfer failed, and will set them no more.
pub(super) fn restore_let_in(state: &Mutex<State>, dirs: &DirCache) {
    let mut let_in: Vec<(Vec<u8>, u32)> =
        state.lock().expect("not poisoned").let_in.drain().collect();
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
