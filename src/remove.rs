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
//! round trip, not two. A directory is listed a read at a time, each read's
//! names deleted before the next, so that what a directory holds, however
//! many, is never held whole.

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
    trees_with(
        directory,
        names,
        keep,
        deleters,
        stopped,
        fsys::LIST_BYTES,
        &|_| {},
    )
    .0
}

/// As [`trees`], each read of a directory asking for `room` bytes of names,
/// and `pass_ended` called on each directory as a pass over it ends; with
/// the most names that waited at once to be gone into.
fn trees_with(
    directory: BorrowedFd,
    names: &[(Vec<u8>, bool)],
    keep: &Keep,
    deleters: usize,
    stopped: &(dyn Fn() -> bool + Sync),
    room: usize,
    pass_ended: &(dyn Fn(BorrowedFd) + Sync),
) -> (Vec<io::Result<()>>, usize) {
    let work = Work {
        top: directory,
        keep,
        deepest: crate::inject::remove_depth().unwrap_or(fsys::REMOVE_DEPTH),
        deleters: deleters.max(1),
        stopped,
        room,
        pass_ended,
        removals: names.iter().map(|_| Removal::default()).collect(),
        queue: Mutex::new(Queue {
            tasks: Vec::new(),
            unfinished: 0,
            idle: 0,
            helpers: 0,
            most: 0,
        }),
        changed: Condvar::new(),
    };
    std::thread::scope(|scope| {
        // The first name is taken first.
        for (removal, (name, directory)) in names.iter().enumerate().rev() {
            work.push(
                scope,
                Task::Name(Name {
                    parent: None,
                    name: name.clone(),
                    directory: *directory,
                    removal,
                    depth: 0,
                    path: Vec::new(),
                }),
            );
        }
        work.deleter(scope, State::ToDelete);
    });
    let most = work.queue.into_inner().expect("not poisoned").most;
    let results = work
        .removals
        .into_iter()
        .map(|removal| {
            removal
                .result
                .into_inner()
                .expect("not poisoned")
                .expect("every name settled")
        })
        .collect();
    (results, most)
}

/// What a deleter does.
enum Task {
    Name(Name),
    /// Reads on in a directory being emptied, and deletes what the read
    /// brings.
    More(Arc<Node>),
}

/// A name to go into: one of the names given, or one in a directory being
/// emptied.
struct Name {
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
    /// Its subdirectories not done with yet, and one more while its names
    /// are read and deleted.
    pending: AtomicUsize,
    /// It holds a directory on another filesystem, and so stays.
    pinned: AtomicBool,
    /// Where its names are read, a read at a time; held while a read's names
    /// are deleted, so that one deleter at a time deletes in it.
    listing: Mutex<fsys::Batches>,
    /// The pass over it read some names: should it then not be empty, it is
    /// read again from its start.
    read: AtomicBool,
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

    /// Keeps the least path by bytes, whatever the order the paths are met
    /// in.
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
    /// The most tasks queued at once.
    most: usize,
}

struct Work<'a> {
    top: BorrowedFd<'a>,
    keep: &'a Keep,
    deepest: usize,
    deleters: usize,
    stopped: &'a (dyn Fn() -> bool + Sync),
    /// How many bytes of names a read of a directory asks for.
    room: usize,
    pass_ended: &'a (dyn Fn(BorrowedFd) + Sync),
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
            queue.most = queue.most.max(queue.tasks.len());
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
            profile::wait(State::Delete, || match task {
                Task::Name(name) => self.go_into(scope, name),
                Task::More(node) => self.more(scope, node),
            });
            let mut queue = self.queue.lock().expect("not poisoned");
            queue.unfinished -= 1;
            if queue.unfinished == 0 {
                self.changed.notify_all();
            }
        }
    }

    /// Deletes `task`'s name, if it is no directory, or empties it, queueing
    /// its subdirectories.
    fn go_into<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, task: Name) {
        let removal = &self.removals[task.removal];
        if (self.stopped)() {
            removal.fail(io::Error::other("the transfer stopped"));
        }
        if removal.failed() {
            return self.done_with(scope, task, Ok(None));
        }
        let parent = task
            .parent
            .as_ref()
            .map_or(self.top, |parent| parent.directory.as_fd());
        if !task.directory {
            match fsys::remove(parent, &task.name, false) {
                Ok(()) => return self.done_with(scope, task, Ok(None)),
                // Made a directory since it was listed.
                Err(error) if is_a_directory(&error) => {}
                Err(error) => return self.done_with(scope, task, Err(error)),
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
                scope,
                task,
                Err(io::Error::other(format!(
                    "it is more than {deepest} directories deep"
                ))),
            );
        }
        match entered {
            Ok(Entered::Removed) => self.done_with(scope, task, Ok(None)),
            Ok(Entered::Elsewhere) => {
                removal.elsewhere(task.path.clone());
                self.done_with(scope, task, Ok(Some(())))
            }
            Ok(Entered::Opened { directory, let_in }) => {
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
                    listing: Mutex::new(fsys::Batches::new(self.room)),
                    read: AtomicBool::new(false),
                });
                self.more(scope, node);
            }
            Err(error) => self.done_with(scope, task, Err(error)),
        }
    }

    /// Reads on in `node`, and deletes what the reads bring, one name at a
    /// time, as the directory's lock allows no more. A read that brings
    /// directories queues them, and beneath them the next read, which waits
    /// for the rest of this one to be deleted: so one deleter at a time
    /// deletes in it, and what waits to be gone into is never more than a
    /// read's worth for each directory being emptied. One that brings none
    /// is read on from at once. Past its end, it is done with, once all it
    /// held is.
    fn more<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, node: Arc<Node>) {
        let removal = &self.removals[node.removal];
        let mut listing = node.listing.lock().expect("not poisoned");
        let (mut buffer, mut listed) = (Vec::new(), Vec::new());
        loop {
            if (self.stopped)() {
                removal.fail(io::Error::other("the transfer stopped"));
            }
            let more = !removal.failed()
                && listing
                    .next(node.directory.as_fd(), &mut buffer, &mut listed)
                    .unwrap_or_else(|error| {
                        removal.fail(error);
                        false
                    });
            if !more {
                (self.pass_ended)(node.directory.as_fd());
                drop(listing);
                return self.release(scope, node);
            }
            if !listed.is_empty() {
                node.read.store(true, Ordering::Release);
            }
            let (directories, others): (Vec<Listed>, Vec<Listed>) =
                listed.drain(..).partition(Listed::may_be_directory);
            let queue = |name: Vec<u8>| {
                node.pending.fetch_add(1, Ordering::AcqRel);
                let path = crate::manifest::join(&node.path, &name);
                self.push(
                    scope,
                    Task::Name(Name {
                        parent: Some(Arc::clone(&node)),
                        name,
                        directory: true,
                        removal: node.removal,
                        depth: node.depth,
                        path,
                    }),
                );
            };
            let beneath = !directories.is_empty();
            if beneath {
                self.push(scope, Task::More(Arc::clone(&node)));
                for entry in directories.into_iter().rev() {
                    queue(entry.name);
                }
            }
            for entry in others {
                if removal.failed() || (self.stopped)() {
                    break;
                }
                match fsys::remove(&node.directory, &entry.name, false) {
                    Ok(()) => {}
                    Err(error) if is_a_directory(&error) => queue(entry.name),
                    Err(error) => {
                        removal.fail(error);
                        break;
                    }
                }
            }
            if beneath {
                return;
            }
        }
    }

    /// One less for `node` to wait for: it is finished once none is left.
    fn release<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, node: Arc<Node>) {
        if node.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish(scope, node);
        }
    }

    /// Settles `task`, whose name was deleted, `Ok(None)`, found on another
    /// filesystem, `Ok(Some(()))`, or not deleted, `Err`: for one of the names
    /// given, that is its result; under a directory, it is one less to wait
    /// for there.
    fn done_with<'scope>(
        &'scope self,
        scope: &'scope Scope<'scope, '_>,
        task: Name,
        result: io::Result<Option<()>>,
    ) {
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
                self.release(scope, parent);
            }
        }
    }

    /// Removes `node`, emptied, from its directory, unless it stays: when its
    /// removal failed, or it holds another filesystem. Then its directory
    /// has one less to wait for, and so on up, a level at a time. One found
    /// not empty, as a filesystem may skip names while others are deleted,
    /// or with a name made since, is read again from its start, as long as
    /// the pass before read some names.
    fn finish<'scope>(&'scope self, scope: &'scope Scope<'scope, '_>, mut node: Arc<Node>) {
        loop {
            let removal = &self.removals[node.removal];
            let pinned = node.pinned.load(Ordering::Acquire);
            let mut stays = pinned || removal.failed();
            if !stays {
                let parent = node.parent.lock().expect("not poisoned");
                let directory = parent
                    .as_ref()
                    .map_or(self.top, |parent| parent.directory.as_fd());
                let removed = fsys::remove(directory, &node.name, true);
                drop(parent);
                if let Err(error) = removed {
                    if is_not_empty(&error) && node.read.swap(false, Ordering::AcqRel) {
                        let rewound = node
                            .listing
                            .lock()
                            .expect("not poisoned")
                            .rewind(node.directory.as_fd());
                        match rewound {
                            Ok(()) => {
                                node.pending.fetch_add(1, Ordering::AcqRel);
                                self.push(scope, Task::More(node));
                                return;
                            }
                            Err(error) => removal.fail(error),
                        }
                    } else {
                        removal.fail(error);
                    }
                    stays = true;
                }
            }
            if stays && let Some(mode) = node.let_in {
                let _ = fsys::set_mode(&node.directory, mode);
            }
            let parent = node.parent.lock().expect("not poisoned").take();
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
    Ok(Entered::Opened { directory, let_in })
}

/// Whether removing a directory failed as it is not empty: `ENOTEMPTY`, or
/// `EEXIST`, which POSIX allows.
fn is_not_empty(error: &io::Error) -> bool {
    matches!(
        rustix::io::Errno::from_io_error(error),
        Some(rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST)
    )
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

    /// Room for a read of a few dozen names.
    const SMALL: usize = 1024;

    /// A directory of 300 files and 300 directories with a file each, read a
    /// few dozen names at a time, is deleted whole, by one deleter or many,
    /// with never more than a few reads' worth waiting to be gone into.
    #[test]
    fn a_large_directory_is_read_and_deleted_a_read_at_a_time() {
        for deleters in [1, DELETERS] {
            let temp = tempfile::tempdir().unwrap();
            let extra = temp.path().join("extra");
            std::fs::create_dir(&extra).unwrap();
            for index in 0..300 {
                std::fs::write(extra.join(format!("file-{index:03}")), "x").unwrap();
                let directory = extra.join(format!("dir-{index:03}"));
                std::fs::create_dir(&directory).unwrap();
                std::fs::write(directory.join("in"), "x").unwrap();
            }
            let root = opened(temp.path());
            let (results, most) = trees_with(
                root.as_fd(),
                &[(b"extra".to_vec(), true)],
                &Keep::default(),
                deleters,
                &|| false,
                SMALL,
                &|_| {},
            );
            assert!(results.iter().all(Result::is_ok), "{results:?}");
            assert!(!extra.exists());
            assert!(most < 100, "{most} waited, with {deleters} deleters");
        }
    }

    /// A directory found not empty once its names are deleted, as one made
    /// since, is read again from its start, as long as the pass before read
    /// some; an empty one whose pass read none is not, and fails so.
    #[test]
    fn a_directory_found_not_empty_is_read_again_while_a_pass_reads_some() {
        let temp = tempfile::tempdir().unwrap();
        tree(&temp.path().join("extra"), 4, 2, 3);
        let made = AtomicUsize::new(0);
        let make = |directory: BorrowedFd| {
            let index = made.fetch_add(1, Ordering::AcqRel);
            if index < 5 {
                let name = format!("late-{index}");
                let flags = rustix::fs::OFlags::CREATE | rustix::fs::OFlags::WRONLY;
                rustix::fs::openat(directory, name.as_str(), flags, 0o644.into()).unwrap();
            }
        };
        let root = opened(temp.path());
        for deleters in [1, DELETERS] {
            tree(&temp.path().join("extra"), 4, 2, 3);
            made.store(0, Ordering::Release);
            let (results, _) = trees_with(
                root.as_fd(),
                &[(b"extra".to_vec(), true)],
                &Keep::default(),
                deleters,
                &|| false,
                SMALL,
                &make,
            );
            assert!(results.iter().all(Result::is_ok), "{results:?}");
            assert!(made.load(Ordering::Acquire) > 5);
            assert!(!temp.path().join("extra").exists());
        }
        std::fs::create_dir(temp.path().join("empty")).unwrap();
        made.store(0, Ordering::Release);
        let (results, _) = trees_with(
            root.as_fd(),
            &[(b"empty".to_vec(), true)],
            &Keep::default(),
            1,
            &|| false,
            SMALL,
            &make,
        );
        let error = results[0].as_ref().unwrap_err();
        assert!(is_not_empty(error), "{error}");
        assert!(temp.path().join("empty/late-0").exists());
        assert_eq!(made.load(Ordering::Acquire), 1);
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
