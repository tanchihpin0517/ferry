//! Finishing directories, once nothing more is made in them, and making
//! what was written durable at the end.

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::fsys;
use crate::manifest::Entry;
use crate::source::display;
use crate::stage::Stage;
use crate::stop::Stopped;

use super::apply::set_owner;
use super::{State, open_under, root_parent};

/// Finishes each directory as it is done, in that order: a directory's
/// after all under it.
pub(super) fn finish_as_done(
    root: &Path,
    state: &Mutex<State>,
    done: mpsc::Receiver<(Vec<u8>, Entry)>,
    fsync: bool,
) -> Result<()> {
    for (path, entry) in done {
        finish_directory(root, state, &path, &entry, fsync)?;
    }
    Ok(())
}

/// Finishes what is still open once everything is written, deepest first,
/// should anything be: every directory is finished as it is done, but one
/// with a file that never came, not even as one the Source could not open,
/// would otherwise never be. Then waits for every directory to be finished:
/// [`Stopped`] if the finisher failed, which it said itself.
pub(super) fn finish_directories(state: &Mutex<State>, finisher: Stage<'_, ()>) -> Result<()> {
    {
        let mut state = state.lock().expect("not poisoned");
        state.leave_all();
        let mut left: Vec<(Vec<u8>, Entry)> = state
            .open
            .drain()
            .filter(|(_, open)| open.finish)
            .map(|(path, open)| (path, open.entry))
            .collect();
        left.sort_by_key(|(path, _)| std::cmp::Reverse(depth(path)));
        if let Some(finishing) = state.finishing.take() {
            for directory in left {
                let _ = finishing.send(directory);
            }
        }
    }
    finisher.join().ok_or_else(|| Stopped.into())
}

/// Sets a directory's owner, permissions, and mtime, now that nothing more
/// is made in it nor under it, and makes it durable unless told not to.
fn finish_directory(
    root: &Path,
    state: &Mutex<State>,
    path: &[u8],
    entry: &Entry,
    fsync: bool,
) -> Result<()> {
    let (directory, ownership) = {
        let state = state.lock().expect("not poisoned");
        (open_under(&state, root, path), Arc::clone(&state.ownership))
    };
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
pub(super) fn restore_let_in(root: &Path, state: &Mutex<State>) {
    let mut state = state.lock().expect("not poisoned");
    let mut let_in: Vec<(Vec<u8>, u32)> = state.let_in.drain().collect();
    let_in.sort_by_key(|(path, _)| std::cmp::Reverse(depth(path)));
    for (path, mode) in let_in {
        if let Ok(directory) = open_under(&state, root, &path) {
            let _ = fsys::set_mode(&directory, mode);
        }
    }
}

/// Makes everything this transfer wrote durable, once, at the end: what was
/// only ordered on the way reaches storage.
pub(super) fn make_durable(root: &Path, state: &Mutex<State>, fsync: bool) -> Result<()> {
    if !fsync {
        return Ok(());
    }
    let held = state.lock().expect("not poisoned").root_directory.clone();
    let directory = match held {
        Some(directory) => rustix::io::dup(&*directory)?,
        // A root that is a file: its directory.
        None => root_parent(root)?.0,
    };
    fsys::sync_durable(&directory).context("make what was written durable")
}

pub(super) fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        1 + path.iter().filter(|&&byte| byte == b'/').count()
    }
}
