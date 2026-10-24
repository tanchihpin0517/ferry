//! Finishing directories, once nothing more is made in them, nor under
//! them: the finishers set each one's owner, permissions and mtime, as the
//! decider hands it to them, and only report what became of it.

use anyhow::Result;

use crate::dirs::DirCache;
use crate::fsys;
use crate::manifest::Entry;

use super::State;
use super::apply::{Setters, Warned, set_attributes};
use super::decider::depth;

/// How many directories are finished at once: as many as gained on the
/// filesystems measured, each in a directory of its own, as for deleting;
/// see [`crate::remove::DELETERS`].
pub(super) const FINISHERS: usize = 16;

/// Sets a directory's owner, permissions, and mtime, now that nothing more
/// is made in it nor under it. Each is tried, whatever came of those before
/// it; what this process may not set goes in `warned`, as `shown`.
pub(super) fn finish_directory(
    state: &State,
    dirs: &DirCache,
    path: &[u8],
    shown: &[u8],
    entry: &Entry,
    warned: &mut Warned,
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
        warned,
    );
    // Its permissions were set, or were never to be: it is let in no more.
    state.let_in.lock().expect("not poisoned").remove(path);
    Ok(set?)
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
