//! Deleting trees, for `sync` and for what is in the way of something else.
//!
//! Every directory is emptied by whichever deleter is free, so a deletion
//! spreads over directories. That is the only way it goes faster: within one
//! directory the kernel takes its lock for each name, and on a filesystem over
//! the network each name is a round trip, some 0.65 ms on WekaFS, so one
//! thread deletes a million files in about 11 minutes. Sixteen, each in a
//! directory of its own, were 14 to 16 times as fast there, and 3 to 10 times
//! on NFS.
//!
//! What a directory holds is known from its listing: what is no directory is
//! unlinked at once, and only a directory is opened, so that a file costs one
//! round trip, not two.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::Scope;

use rustix::fs::FileType;

use crate::fsys::{self, Elsewhere, Keep, Listed, Protected};
use crate::profile::{self, State};

/// How many threads delete at once, the calling one included: as many as
/// gained, each in a directory of its own, on the filesystems measured.
pub const DELETERS: usize = 16;

/// Deletes each of `names` from `directory`, with everything under it, as
/// [`fsys::remove_tree`] says, with up to `deleters` threads, the calling one
/// included. With each name, whether it may be a directory: one that is not
/// is unlinked at once, and gone into only if it is one after all. Returns
/// what became of each name, in their order. Once `stopped` says so, nothing
/// more is deleted, and what was not is an error.
pub fn trees(
    directory: BorrowedFd,
    names: &[(Vec<u8>, bool)],
    keep: &Keep,
    deleters: usize,
    stopped: &(dyn Fn() -> bool + Sync),
) -> Vec<io::Result<()>> {
    let work = Work {
        top: directory,
        keep,
        deepest: crate::inject::remove_depth().unwrap_or(fsys::REMOVE_DEPTH),
        deleters: deleters.max(1),
        stopped,
        removals: names.iter().map(|_| Removal::default()).collect(),
        queue: Mutex::new(Queue {
            tasks: Vec::new(),
            unfinished: 0,
            idle: 0,
            helpers: 0,
        }),
        changed: Condvar::new(),
    };
    std::thread::scope(|scope| {
        // The first name is taken first.
        for (removal, (name, directory)) in names.iter().enumerate().rev() {
            work.push(
                scope,
                Task {
                    parent: None,
                    name: name.clone(),
                    directory: *directory,
                    removal,
                    depth: 0,
                    path: Vec::new(),
                },
            );
        }
        work.deleter(scope, State::ToDelete);
    });
    work.removals
        .into_iter()
        .map(|removal| {
            removal
                .result
                .into_inner()
                .expect("not poisoned")
                .expect("every name settled")
        })
        .collect()
}

/// A name to go into: one of the names given, or one in a directory being
/// emptied.
struct Task {
    /// The directory it is in; none for one of the names given, which is in
    /// the caller's.
    parent: Option<Arc<Node>>,
    name: Vec<u8>,
    /// Whether it may be a directory, as the listing says.
    directory: bool,
    /// Which of the names given it is under.
    removal: usize,
    /// How deep its directory is: 0 for the caller's.
    depth: usize,
    /// Where it is under the name given it is under, for an [`Elsewhere`].
    path: Vec<u8>,
}

/// A directory being emptied.
struct Node {
    name: Vec<u8>,
    path: Vec<u8>,
    directory: OwnedFd,
    /// Its permissions, if it was let into: given back if it stays.
    let_in: Option<u32>,
    /// The directory it is in, taken when it is done with, so that no chain
    /// of them is dropped by recursion; none for one of the names given.
    parent: Mutex<Option<Arc<Node>>>,
    removal: usize,
    depth: usize,
    /// Its subdirectories not done with yet, and one more while its own names
    /// are deleted.
    pending: AtomicUsize,
    /// It holds a directory on another filesystem, and so stays.
    pinned: AtomicBool,
}

/// What becomes of one of the names given.
#[derive(Default)]
struct Removal {
    /// The first error under it: nothing more under it is deleted.
    error: Mutex<Option<io::Error>>,
    failed: AtomicBool,
    /// The first directory under it, by path, on another filesystem.
    elsewhere: Mutex<Option<Vec<u8>>>,
    result: Mutex<Option<io::Result<()>>>,
}

impl Removal {
    fn fail(&self, error: io::Error) {
        let mut first = self.error.lock().expect("not poisoned");
        if first.is_none() {
            *first = Some(error);
        }
        self.failed.store(true, Ordering::Release);
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Keeps the first path by bytes, which is the one a removal going down
    /// in name order meets first.
    fn elsewhere(&self, path: Vec<u8>) {
        let mut first = self.elsewhere.lock().expect("not poisoned");
        if first.as_ref().is_none_or(|kept| path < *kept) {
            *first = Some(path);
        }
    }

    fn settle(&self, result: io::Result<()>) {
        *self.result.lock().expect("not poisoned") = Some(result);
    }

    /// The result of a name whose removal is over: its first error, or, if
    /// something under it stays on another filesystem, that.
    fn outcome(&self, pinned: bool) -> io::Result<()> {
        if let Some(error) = self.error.lock().expect("not poisoned").take() {
            return Err(error);
        }
        if pinned {
            let path = self.elsewhere.lock().expect("not poisoned").take();
            return Err(io::Error::other(Elsewhere {
                path: path.unwrap_or_default(),
            }));
        }
        Ok(())
    }
}

/// Tasks to take, last in first out, so that each deleter goes down a tree
/// as far as it can, and holds few directories open.
struct Queue {
    tasks: Vec<Task>,
    /// Tasks queued or being done.
    unfinished: usize,
    /// Deleters waiting for a task.
    idle: usize,
    /// Threads started besides the calling one.
    helpers: usize,
}

struct Work<'a> {
    top: BorrowedFd<'a>,
    keep: &'a Keep,
    deepest: usize,
    deleters: usize,
    stopped: &'a (dyn Fn() -> bool + Sync),
    removals: Vec<Removal>,
    queue: Mutex<Queue>,
    changed: Condvar,
}

/// What became of a name gone into.
enum Entered {
    Removed,
    Opened {
        directory: OwnedFd,
        let_in: Option<u32>,
        listed: Vec<Listed>,
    },
    Elsewhere,
}

impl<'a> Work<'a> {
    /// Queues `task`, and starts another deleter for it if none is free and
    /// there may be more.
    fn push<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, task: Task) {
        let start = {
            let mut queue = self.queue.lock().expect("not poisoned");
            queue.tasks.push(task);
            queue.unfinished += 1;
            let start = queue.idle == 0 && queue.helpers + 1 < self.deleters;
            if start {
                queue.helpers += 1;
            }
            start
        };
        if start {
            scope.spawn(move || {
                let _thread = profile::thread("delete");
                self.deleter(scope, State::Work);
            });
        } else {
            self.changed.notify_one();
        }
    }

    /// Takes tasks and does them, waiting in `idle` for one, until there are
    /// none left anywhere.
    fn deleter<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, idle: State) {
        loop {
            let task = {
                let mut queue = self.queue.lock().expect("not poisoned");
                loop {
                    if let Some(task) = queue.tasks.pop() {
                        break Some(task);
                    }
                    if queue.unfinished == 0 {
                        break None;
                    }
                    queue.idle += 1;
                    queue = profile::wait(idle, || self.changed.wait(queue)).expect("not poisoned");
                    queue.idle -= 1;
                }
            };
            let Some(task) = task else {
                self.changed.notify_all();
                return;
            };
            profile::wait(State::Delete, || self.go_into(scope, task));
            let mut queue = self.queue.lock().expect("not poisoned");
            queue.unfinished -= 1;
            if queue.unfinished == 0 {
                self.changed.notify_all();
            }
        }
    }

    /// Deletes `task`'s name, if it is no directory, or empties it, queueing
    /// its subdirectories.
    fn go_into<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, task: Task) {
        let removal = &self.removals[task.removal];
        if (self.stopped)() {
            removal.fail(io::Error::other("the transfer stopped"));
        }
        if removal.failed() {
            return self.done_with(task, Ok(None));
        }
        let parent = task
            .parent
            .as_ref()
            .map_or(self.top, |parent| parent.directory.as_fd());
        if !task.directory {
            match fsys::remove(parent, &task.name, false) {
                Ok(()) => return self.done_with(task, Ok(None)),
                // Made a directory since it was listed.
                Err(error) if is_a_directory(&error) => {}
                Err(error) => return self.done_with(task, Err(error)),
            }
        }
        let entered = enter(parent, &task.name, self.keep);
        if let Ok(Entered::Opened {
            directory, let_in, ..
        }) = &entered
            && task.depth >= self.deepest
        {
            if let Some(mode) = let_in {
                let _ = fsys::set_mode(directory, *mode);
            }
            let deepest = self.deepest;
            return self.done_with(
                task,
                Err(io::Error::other(format!(
                    "it is more than {deepest} directories deep"
                ))),
            );
        }
        match entered {
            Ok(Entered::Removed) => self.done_with(task, Ok(None)),
            Ok(Entered::Elsewhere) => {
                removal.elsewhere(task.path.clone());
                self.done_with(task, Ok(Some(())))
            }
            Ok(Entered::Opened {
                directory,
                let_in,
                listed,
            }) => {
                let node = Arc::new(Node {
                    name: task.name,
                    path: task.path,
                    directory,
                    let_in,
                    parent: Mutex::new(task.parent),
                    removal: task.removal,
                    depth: task.depth + 1,
                    pending: AtomicUsize::new(1),
                    pinned: AtomicBool::new(false),
                });
                self.empty(scope, &node, listed);
                if node.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                    self.finish(node);
                }
            }
            Err(error) => self.done_with(task, Err(error)),
        }
    }

    /// Queues `node`'s subdirectories, then deletes the rest of what it holds,
    /// one name at a time, as its directory's lock allows no more.
    fn empty<'scope>(
        &'scope self,
        scope: &'scope Scope<'scope, '_>,
        node: &Arc<Node>,
        listed: Vec<Listed>,
    ) {
        let removal = &self.removals[node.removal];
        let (directories, others): (Vec<Listed>, Vec<Listed>) = listed
            .into_iter()
            .partition(|entry| matches!(entry.file_type, FileType::Directory | FileType::Unknown));
        let queue = |name: Vec<u8>| {
            node.pending.fetch_add(1, Ordering::AcqRel);
            let path = crate::manifest::join(&node.path, &name);
            self.push(
                scope,
                Task {
                    parent: Some(Arc::clone(node)),
                    name,
                    directory: true,
                    removal: node.removal,
                    depth: node.depth,
                    path,
                },
            );
        };
        for entry in directories.into_iter().rev() {
            queue(entry.name);
        }
        for entry in others {
            if removal.failed() || (self.stopped)() {
                return;
            }
            match fsys::remove(&node.directory, &entry.name, false) {
                Ok(()) => {}
                Err(error) if is_a_directory(&error) => queue(entry.name),
                Err(error) => {
                    removal.fail(error);
                    return;
                }
            }
        }
    }

    /// Settles `task`, whose name was deleted, `Ok(None)`, found on another
    /// filesystem, `Ok(Some(()))`, or not deleted, `Err`: for one of the names
    /// given, that is its result; under a directory, it is one less to wait
    /// for there.
    fn done_with(&self, task: Task, result: io::Result<Option<()>>) {
        let removal = &self.removals[task.removal];
        let elsewhere = matches!(result, Ok(Some(())));
        if let Err(error) = result {
            removal.fail(error);
        }
        match task.parent {
            None => removal.settle(removal.outcome(elsewhere)),
            Some(parent) => {
                if elsewhere {
                    parent.pinned.store(true, Ordering::Release);
                }
                if parent.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                    self.finish(parent);
                }
            }
        }
    }

    /// Removes `node`, emptied, from its directory, unless it stays: when its
    /// removal failed, or it holds another filesystem. Then its directory
    /// has one less to wait for, and so on up, a level at a time.
    fn finish(&self, mut node: Arc<Node>) {
        loop {
            let removal = &self.removals[node.removal];
            let parent = node.parent.lock().expect("not poisoned").take();
            let directory = parent
                .as_ref()
                .map_or(self.top, |parent| parent.directory.as_fd());
            let pinned = node.pinned.load(Ordering::Acquire);
            let mut stays = pinned || removal.failed();
            if !stays && let Err(error) = fsys::remove(directory, &node.name, true) {
                removal.fail(error);
                stays = true;
            }
            if stays && let Some(mode) = node.let_in {
                let _ = fsys::set_mode(&node.directory, mode);
            }
            // Dropped before its directory, which nothing else then drops.
            drop(node);
            match parent {
                None => {
                    removal.settle(removal.outcome(pinned));
                    return;
                }
                Some(parent) => {
                    if pinned {
                        parent.pinned.store(true, Ordering::Release);
                    }
                    if parent.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                        return;
                    }
                    node = parent;
                }
            }
        }
    }
}

/// `name` in `parent` removed, if it is no directory, or opened to be
/// emptied and listed.
fn enter(parent: BorrowedFd, name: &[u8], keep: &Keep) -> io::Result<Entered> {
    fsys::one_name(name)?;
    if (!keep.protect.is_empty() || keep.device.get().is_some())
        && let Ok(stat) = fsys::stat(parent, name)
        && fsys::file_type(&stat) == FileType::Directory
    {
        if keep.protect.contains(&fsys::key(&stat)) {
            return Err(io::Error::other(Protected));
        }
        if keep.is_elsewhere(&stat) {
            return Ok(Entered::Elsewhere);
        }
    }
    let (directory, let_in) = match fsys::open_directory_to_work_in(parent, name) {
        Ok(opened) => opened,
        // Not a directory, or a symlink to one: removed as it is.
        Err(error)
            if matches!(
                rustix::io::Errno::from_io_error(&error),
                Some(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
            ) =>
        {
            return fsys::remove(parent, name, false).map(|()| Entered::Removed);
        }
        Err(error) => return Err(error),
    };
    match fsys::read_entries(&directory) {
        Ok(listed) => Ok(Entered::Opened {
            directory,
            let_in,
            listed,
        }),
        Err(error) => {
            if let Some(mode) = let_in {
                let _ = fsys::set_mode(&directory, mode);
            }
            Err(error)
        }
    }
}

/// Whether unlinking a name failed as it is a directory: `EISDIR` on Linux,
/// `EPERM` on macOS, which may also mean what it says, and then is said again
/// when the name is gone into.
fn is_a_directory(error: &io::Error) -> bool {
    matches!(
        rustix::io::Errno::from_io_error(error),
        Some(rustix::io::Errno::ISDIR | rustix::io::Errno::PERM)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// `directories` directories under `top`, each `depth` levels deep, with
    /// `files` files at every level.
    fn tree(top: &Path, directories: usize, depth: usize, files: usize) {
        for directory in 0..directories {
            let mut level = top.join(format!("d{directory:02}"));
            for _ in 0..depth {
                std::fs::create_dir_all(&level).unwrap();
                for file in 0..files {
                    std::fs::write(level.join(format!("f{file}")), "x").unwrap();
                }
                level.push("deeper");
            }
        }
    }

    fn opened(path: &Path) -> OwnedFd {
        fsys::open_root(path.as_os_str()).unwrap()
    }

    #[test]
    fn a_wide_tree_is_deleted_over_threads() {
        let temp = tempfile::tempdir().unwrap();
        tree(&temp.path().join("extra"), 40, 4, 3);
        std::fs::write(temp.path().join("loose"), "x").unwrap();
        let root = opened(temp.path());
        let names = [(b"extra".to_vec(), true), (b"loose".to_vec(), false)];
        let results = trees(root.as_fd(), &names, &Keep::default(), DELETERS, &|| false);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    /// Each name has its own result, in order: one gone already fails, the
    /// rest are deleted; one listed as no directory that is one is gone into.
    #[test]
    fn each_name_is_deleted_or_failed_on_its_own() {
        let temp = tempfile::tempdir().unwrap();
        tree(&temp.path().join("dir"), 3, 2, 2);
        tree(&temp.path().join("made-a-directory"), 2, 2, 1);
        std::fs::write(temp.path().join("file"), "x").unwrap();
        let root = opened(temp.path());
        let names = [
            (b"already-gone".to_vec(), false),
            (b"dir".to_vec(), true),
            (b"made-a-directory".to_vec(), false),
            (b"file".to_vec(), false),
        ];
        let results = trees(root.as_fd(), &names, &Keep::default(), DELETERS, &|| false);
        let error = results[0].as_ref().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(results[1..].iter().all(Result::is_ok), "{results:?}");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    /// However many threads delete, a protected directory stops its removal
    /// and is neither deleted nor let into; what holds it stays.
    #[test]
    fn a_protected_directory_stays_whatever_the_threads() {
        let temp = tempfile::tempdir().unwrap();
        let extra = temp.path().join("extra");
        tree(&extra, 20, 3, 2);
        let kept = extra.join("d07/deeper/kept");
        std::fs::create_dir_all(kept.join("inner")).unwrap();
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o500)).unwrap();
        let protect = vec![fsys::key(&fsys::stat_root(kept.as_os_str()).unwrap())];
        let root = opened(temp.path());
        let results = trees(
            root.as_fd(),
            &[(b"extra".to_vec(), true)],
            &Keep::protecting(protect),
            DELETERS,
            &|| false,
        );
        let error = results[0].as_ref().unwrap_err();
        assert!(fsys::is_protected(error), "{error}");
        assert!(kept.join("inner").is_dir());
        assert_eq!(
            std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
            0o500
        );
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// With `-x`, a name on another filesystem is not gone into, over threads
    /// as on one. A test can make no other filesystem without root, so here
    /// the one to stay on is another.
    #[test]
    fn a_directory_on_another_filesystem_stays_whatever_the_threads() {
        let temp = tempfile::tempdir().unwrap();
        tree(&temp.path().join("extra"), 4, 2, 1);
        let root = opened(temp.path());
        #[allow(clippy::unnecessary_cast)]
        let device = rustix::fs::fstat(&root).unwrap().st_dev as u64;
        let elsewhere = Keep::default();
        elsewhere.device.set(device + 1).unwrap();
        let results = trees(
            root.as_fd(),
            &[(b"extra".to_vec(), true)],
            &elsewhere,
            DELETERS,
            &|| false,
        );
        let error = results[0].as_ref().unwrap_err();
        assert!(fsys::is_elsewhere(error), "{error}");
        assert_eq!(error.to_string(), "it is on another filesystem");
        assert!(temp.path().join("extra/d00/f0").exists());
    }

    #[test]
    fn nothing_is_deleted_once_stopped() {
        let temp = tempfile::tempdir().unwrap();
        tree(&temp.path().join("extra"), 4, 2, 1);
        let root = opened(temp.path());
        let results = trees(
            root.as_fd(),
            &[(b"extra".to_vec(), true)],
            &Keep::default(),
            DELETERS,
            &|| true,
        );
        assert!(results[0].is_err());
        assert!(temp.path().join("extra/d00/f0").exists());
    }

    /// A chain of directories is deleted a level at a time, and done with a
    /// level at a time too, on a thread of 64 KiB, however many may help.
    #[test]
    fn a_deep_chain_is_deleted_without_running_out_of_stack() {
        const DEPTH: usize = 300;
        let temp = tempfile::tempdir().unwrap();
        let mut level = temp.path().join("deep");
        for _ in 0..DEPTH {
            level.push("d");
        }
        std::fs::create_dir_all(&level).unwrap();
        std::fs::write(level.join("file"), "f").unwrap();
        let root = opened(temp.path());
        std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                trees(
                    root.as_fd(),
                    &[(b"deep".to_vec(), true)],
                    &Keep::default(),
                    DELETERS,
                    &|| false,
                )
                .pop()
                .unwrap()
            })
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
        assert!(!temp.path().join("deep").exists());
    }
}
