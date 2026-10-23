//! Finishing directories, once nothing more is made in them, and making
//! what was written durable at the end.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::dirs::DirCache;
use crate::fsys;
use crate::manifest::Entry;
use crate::mux::Mux;
use crate::profile;
use crate::source::display;
use crate::stop::Stop;

use super::apply::{Setters, Warned, depth, file_failed_in, parent_and_name, set_attributes};
use super::{State, ends_the_transfer, parent_of, root_parent};

/// How many directories are finished at once: as many as gained on the
/// filesystems measured, each in a directory of its own, as for deleting;
/// see [`crate::remove::DELETERS`].
const FINISHERS: usize = 16;

/// Finishes each directory as it is done, by up to [`FINISHERS`] threads,
/// each a directory: one only once every directory under it that is to be
/// finished is, so that one that cannot be gone into after is finished
/// last. They come done deepest first, so all under one come before it. One
/// that cannot be finished fails alone, reported as its entry, and the rest
/// are finished all the same; but where the drive is full or read-only, the
/// transfer stops.
pub(super) fn finish_as_done(
    state: &State,
    dirs: &DirCache,
    mux: &Mux,
    stop: &Stop,
    done: crossbeam_channel::Receiver<(Vec<u8>, Entry)>,
    fsync: bool,
) -> Result<()> {
    std::thread::scope(|scope| {
        let (jobs, taken) = crossbeam_channel::unbounded::<(Vec<u8>, Entry)>();
        let (finished, results) = crossbeam_channel::unbounded::<Finished>();
        for _ in 0..FINISHERS {
            let taken = taken.clone();
            let finished = finished.clone();
            scope.spawn(move || {
                let _thread = profile::thread("finish");
                while let Ok((path, entry)) = profile::wait(profile::State::Work, || taken.recv()) {
                    let shown = display(&path, true);
                    let mut warned = Vec::new();
                    let result = profile::wait(profile::State::Finish, || {
                        finish_directory(state, dirs, &path, &shown, &entry, fsync, &mut warned)
                    });
                    if finished.send((path, entry, result, warned)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(finished);
        // For each directory, how many under it, come to be finished, are
        // not yet; and those waiting for theirs.
        let mut unfinished: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut waiting: HashMap<Vec<u8>, Entry> = HashMap::new();
        let mut finishing = 0usize;
        let mut incoming = Some(done);
        let never = crossbeam_channel::never();
        loop {
            if incoming.is_none() && finishing == 0 {
                // A directory still waiting waits for one that never came,
                // as none should: it is finished all the same, deepest first.
                let Some(path) = waiting.keys().max_by_key(|path| depth(path)).cloned() else {
                    break;
                };
                let entry = waiting.remove(&path).expect("there");
                unfinished.remove(&path);
                finishing += 1;
                let _ = jobs.send((path, entry));
            }
            let next = profile::wait(profile::State::Work, || {
                crossbeam_channel::select! {
                    recv(incoming.as_ref().unwrap_or(&never)) -> came => Next::Came(came.ok()),
                    recv(results) -> result => Next::Finished(result.ok()),
                }
            });
            match next {
                Next::Came(Some((path, entry))) => {
                    if let Some((parent, _)) = parent_and_name(&path) {
                        *unfinished.entry(parent.to_vec()).or_default() += 1;
                    }
                    if unfinished.contains_key(&path) {
                        waiting.insert(path, entry);
                    } else {
                        finishing += 1;
                        let _ = jobs.send((path, entry));
                    }
                }
                Next::Came(None) => incoming = None,
                Next::Finished(None) => break,
                Next::Finished(Some((path, entry, result, warned))) => {
                    finishing -= 1;
                    // What a finisher could not set is warned of here.
                    state.warn(warned);
                    if let Err(error) = result {
                        let shown = display(&path, true);
                        if ends_the_transfer(&error) {
                            return Err(error
                                .context(format!("finish {}", String::from_utf8_lossy(&shown))));
                        }
                        let message = format!("could not be finished: {error:#}");
                        file_failed_in(
                            state,
                            &state.reporter,
                            mux,
                            stop,
                            entry.id,
                            &shown,
                            message,
                        )?;
                    }
                    if let Some((parent, _)) = parent_and_name(&path) {
                        let left = unfinished.get_mut(parent).map(|left| {
                            *left -= 1;
                            *left
                        });
                        if left == Some(0) {
                            unfinished.remove(parent);
                            if let Some(entry) = waiting.remove(parent) {
                                finishing += 1;
                                let _ = jobs.send((parent.to_vec(), entry));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

/// A directory finished, what became of it, and what could not be set.
type Finished = (Vec<u8>, Entry, Result<()>, Warned);

/// What the finisher takes next: a directory done, or one finished, with
/// what became of it; none once either is over.
enum Next {
    Came(Option<(Vec<u8>, Entry)>),
    Finished(Option<Finished>),
}

/// Sets a directory's owner, permissions, and mtime, now that nothing more
/// is made in it nor under it, and makes it durable unless told not to. Each
/// is tried, whatever came of those before it; what this process may not
/// set goes in `warned`, as `shown`.
fn finish_directory(
    state: &State,
    dirs: &DirCache,
    path: &[u8],
    shown: &[u8],
    entry: &Entry,
    fsync: bool,
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
/// only ordered on the way reaches storage. A root this transfer made,
/// `root_made`, is a new name in the directory it is in, which is synced
/// first, so that the tree is found after a power loss too.
pub(super) fn make_durable(
    root: &Path,
    dirs: &DirCache,
    fsync: bool,
    root_made: bool,
) -> Result<()> {
    if !fsync {
        return Ok(());
    }
    if root_made {
        let parent = parent_of(root);
        fsys::open_root(parent.as_os_str())
            .and_then(fsys::fsync)
            .with_context(|| format!("make {} durable in {}", root.display(), parent.display()))?;
    }
    let directory = match dirs.held_root() {
        Some(directory) => directory,
        // A root that is a file: its directory.
        None => Arc::new(root_parent(root)?.0),
    };
    fsys::sync_durable(&*directory).context("make what was written durable")
}
