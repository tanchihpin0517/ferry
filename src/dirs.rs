//! Directories under a transfer's root, as the stages that work there share
//! them: the root is opened once, and each directory under it from there, a
//! name at a time, never through a symlink, nor by its path again. A
//! directory is the one it was when opened: renamed or replaced after, it is
//! still the one written into or read from, as the one listed is the one
//! deleted from.
//!
//! The handles most recently used are kept, a few, and each is shared by
//! whatever holds it, such as the files written into it waiting to be
//! renamed: a directory takes one descriptor however many files are in it.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, OnceLock};

use crate::fsys;

/// How many directories are kept open after their last use: few, as each
/// takes a descriptor, but enough for the directories being worked in at
/// once.
pub const KEPT: usize = 32;

/// The directories under one root.
pub struct DirCache {
    /// The root's path, as the user named it.
    path: OsString,
    /// The root, once opened, or given.
    root: OnceLock<Arc<OwnedFd>>,
    recent: Mutex<Recent>,
    kept: usize,
}

/// The directories kept, by path, each with when it was last used.
#[derive(Default)]
struct Recent {
    handles: HashMap<Vec<u8>, (Arc<OwnedFd>, u64)>,
    clock: u64,
    /// The paths of directories not made, never opened, nor what is under
    /// them: what their names find may be another's; see [`DirCache::refuse`].
    refused: Vec<Vec<u8>>,
}

impl DirCache {
    /// The directories under the root at `path`, keeping up to `kept`.
    pub fn new(path: &OsStr, kept: usize) -> Self {
        Self {
            path: path.to_owned(),
            root: OnceLock::new(),
            recent: Mutex::default(),
            kept,
        }
    }

    /// The root is `root`, already open, unless it was opened before: what
    /// is under it is reached from it.
    pub fn hold_root(&self, root: Arc<OwnedFd>) {
        let _ = self.root.set(root);
    }

    /// The root, opened by its path the first time, and only then.
    pub fn root(&self) -> io::Result<Arc<OwnedFd>> {
        if let Some(root) = self.root.get() {
            return Ok(Arc::clone(root));
        }
        let opened = Arc::new(fsys::open_root(&self.path)?);
        Ok(Arc::clone(self.root.get_or_init(|| opened)))
    }

    /// The directory at `path` under the root, the empty path being the
    /// root: kept, or opened from the deepest directory above it that is.
    pub fn open(&self, path: &[u8]) -> io::Result<Arc<OwnedFd>> {
        if path.is_empty() {
            return self.root();
        }
        let (mut directory, at) = {
            let mut recent = self.recent.lock().expect("not poisoned");
            if recent
                .refused
                .iter()
                .any(|refused| is_within(path, refused))
            {
                return Err(io::Error::other(
                    "it was not made: its name is another's here",
                ));
            }
            recent.clock += 1;
            let now = recent.clock;
            if let Some((handle, used)) = recent.handles.get_mut(path) {
                *used = now;
                return Ok(Arc::clone(handle));
            }
            // The deepest directory above it that is kept.
            let above = path
                .iter()
                .enumerate()
                .rev()
                .filter(|&(_, &byte)| byte == b'/')
                .find_map(|(slash, _)| {
                    recent
                        .handles
                        .get(&path[..slash])
                        .map(|(handle, _)| (Arc::clone(handle), slash + 1))
                });
            match above {
                Some(above) => above,
                None => (self.root()?, 0),
            }
        };
        // Opened a name at a time, without the lock: each may be a round
        // trip, as on NFS.
        for name in path[at..].split(|&byte| byte == b'/') {
            if name.is_empty() {
                continue;
            }
            directory = Arc::new(fsys::open_directory(&*directory, name)?);
        }
        self.keep(path, &directory);
        Ok(directory)
    }

    /// The directory at `path` was not made: it, and all under it, is never
    /// opened, as its name may find another directory, one the filesystem
    /// takes for the same name, which nothing is to be written into.
    pub fn refuse(&self, path: &[u8]) {
        let mut recent = self.recent.lock().expect("not poisoned");
        recent.handles.retain(|kept, _| !is_within(kept, path));
        recent.refused.push(path.to_vec());
    }

    /// Keeps `directory` as the one at `path`, letting go of the least
    /// recently used beyond what is kept.
    fn keep(&self, path: &[u8], directory: &Arc<OwnedFd>) {
        let mut recent = self.recent.lock().expect("not poisoned");
        recent.clock += 1;
        let now = recent.clock;
        recent
            .handles
            .insert(path.to_vec(), (Arc::clone(directory), now));
        while recent.handles.len() > self.kept {
            let oldest = recent
                .handles
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(path, _)| path.clone())
                .expect("more than none");
            recent.handles.remove(&oldest);
        }
    }
}

/// Whether `path` is `directory`, or under it.
fn is_within(path: &[u8], directory: &[u8]) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The identity of what a descriptor has open.
    fn identity(directory: &OwnedFd) -> (u64, u64) {
        let stat = rustix::fs::fstat(directory).unwrap();
        #[allow(clippy::unnecessary_cast)]
        (stat.st_dev as u64, stat.st_ino as u64)
    }

    fn of(path: &std::path::Path) -> (u64, u64) {
        identity(&fsys::open_root(path.as_os_str()).unwrap())
    }

    #[test]
    fn a_directory_is_shared_while_kept_and_opened_from_its_parent() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("a/b/c")).unwrap();
        let dirs = DirCache::new(temp.path().as_os_str(), 4);
        let b = dirs.open(b"a/b").unwrap();
        assert!(Arc::ptr_eq(&b, &dirs.open(b"a/b").unwrap()));
        assert_eq!(identity(&b), of(&temp.path().join("a/b")));
        assert_eq!(
            identity(&dirs.open(b"a/b/c").unwrap()),
            of(&temp.path().join("a/b/c"))
        );
        assert_eq!(identity(&dirs.open(b"").unwrap()), of(temp.path()));
    }

    /// Opened once, a directory is the one it was: one put where it was
    /// since is not reached, nor a root that its path names since.
    #[test]
    fn a_directory_is_the_one_it_was_when_opened() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("a")).unwrap();
        let dirs = DirCache::new(root.as_os_str(), 4);
        let a = identity(&dirs.open(b"a").unwrap());
        fs::rename(root.join("a"), root.join("moved")).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        assert_eq!(identity(&dirs.open(b"a").unwrap()), a);
        fs::rename(&root, temp.path().join("old-root")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        // Reached from the root it opened, not the one now at its path.
        assert!(dirs.open(b"b").is_err());
        assert!(dirs.open(b"moved").is_ok());
    }

    #[test]
    fn a_symlink_below_the_root_is_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("link")).unwrap();
        let dirs = DirCache::new(temp.path().as_os_str(), 4);
        assert!(dirs.open(b"link").is_err());
        assert!(dirs.open(b"real").is_ok());
    }

    /// A directory refused is not opened, nor anything under it, though
    /// kept before; one beside it, of a name it starts with, is.
    #[test]
    fn a_directory_refused_is_not_gone_into() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("a/b")).unwrap();
        fs::create_dir_all(temp.path().join("ab")).unwrap();
        let dirs = DirCache::new(temp.path().as_os_str(), 4);
        dirs.open(b"a/b").unwrap();
        dirs.refuse(b"a");
        assert!(dirs.open(b"a").is_err());
        assert!(dirs.open(b"a/b").is_err());
        assert!(dirs.open(b"ab").is_ok());
        assert!(dirs.open(b"").is_ok());
    }

    /// Beyond what it keeps, the least recently used is let go of; one
    /// still held stays open for whoever holds it.
    #[test]
    fn the_least_recently_used_is_let_go_of() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            fs::create_dir(temp.path().join(name)).unwrap();
        }
        let dirs = DirCache::new(temp.path().as_os_str(), 2);
        let a = dirs.open(b"a").unwrap();
        dirs.open(b"b").unwrap();
        dirs.open(b"a").unwrap();
        dirs.open(b"c").unwrap();
        let recent = dirs.recent.lock().unwrap();
        let mut kept: Vec<&[u8]> = recent.handles.keys().map(Vec::as_slice).collect();
        kept.sort();
        assert_eq!(kept, [&b"a"[..], b"c"]);
        drop(recent);
        assert!(rustix::fs::fstat(&*a).is_ok());
    }

    /// A root given is the one reached from, whatever its path names.
    #[test]
    fn a_root_given_is_the_one_used() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("given/a")).unwrap();
        fs::create_dir_all(temp.path().join("named")).unwrap();
        let dirs = DirCache::new(temp.path().join("named").as_os_str(), 4);
        dirs.hold_root(Arc::new(
            fsys::open_root(temp.path().join("given").as_os_str()).unwrap(),
        ));
        assert!(dirs.open(b"a").is_ok());
        assert_eq!(
            identity(&dirs.root().unwrap()),
            of(&temp.path().join("given"))
        );
    }
}
