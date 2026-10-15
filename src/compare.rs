//! The Destination's side of the comparison: what each manifest entry needs.
//!
//! For each entry the Destination looks at its own copy, at the same path under
//! its root, without following symlinks. A file's content is unchanged if it is
//! a file of the same size and mtime, as in rsync's quick check; other kinds'
//! are unchanged if they are of the same kind and, for symlinks and devices,
//! point to the same place. An entry whose content is unchanged may still need
//! its permissions, owner, or mtime set.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{FileType, Stat};
use rustix::io::Errno;

use crate::fsys;
use crate::manifest::{Entry, Kind, Timestamp};
use crate::owners::Ownership;

/// What an entry needs, from the most to the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Created, or replaced: missing, of another kind, or with other content.
    Content,
    /// Its permissions or owner set.
    Attributes,
    /// Only its mtime set: a directory, symlink, or special file whose time
    /// differs. A file whose time differs needs its content.
    Times,
    Same,
}

impl Verdict {
    /// Whether a dry run lists the entry.
    pub fn is_listed(self) -> bool {
        matches!(self, Verdict::Content | Verdict::Attributes)
    }
}

pub struct Comparer {
    root: Vec<u8>,
    modify_window: i64,
}

/// Whether two mtimes count as the same, as rsync's `--modify-window`
/// decides: a window of 0 compares whole seconds, a positive window allows
/// that many seconds apart, and a negative window compares nanoseconds too.
///
/// Whole seconds are the default because some filesystems and tools keep
/// coarser times than others. FAT keeps 2 seconds, HFS+ 1 second, and some
/// copies drop the nanoseconds; comparing nanoseconds there would send the
/// same unchanged files on every run.
pub fn same_mtime(a: Timestamp, b: Timestamp, modify_window: i64) -> bool {
    if modify_window < 0 {
        a == b
    } else {
        a.seconds.abs_diff(b.seconds) <= modify_window.unsigned_abs()
    }
}

impl Comparer {
    pub fn new(root: Vec<u8>, modify_window: i64) -> Self {
        Self {
            root,
            modify_window,
        }
    }

    fn root_path(&self) -> &OsStr {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(&self.root)
    }

    /// What the root needs.
    pub fn root(&self, entry: &Entry, ownership: &Ownership) -> io::Result<Verdict> {
        match fsys::stat_root(self.root_path()) {
            Ok(stat) => self.verdict(entry, &stat, ownership, || {
                std::fs::read_link(self.root_path())
                    .map(|target| target.into_os_string().into_encoded_bytes())
            }),
            Err(error) if is_missing(&error) => Ok(Verdict::Content),
            Err(error) => Err(error),
        }
    }

    /// What each entry of the directory at `path` needs, in order.
    pub fn directory(
        &self,
        path: &[u8],
        entries: &[Entry],
        ownership: &Ownership,
    ) -> io::Result<Vec<Verdict>> {
        let directory = match self.open(path) {
            Ok(directory) => directory,
            Err(error) if is_missing(&error) => return Ok(vec![Verdict::Content; entries.len()]),
            Err(error) => return Err(error),
        };
        entries
            .iter()
            .map(|entry| self.entry(&directory, entry, ownership))
            .collect()
    }

    /// What the Destination has in the directory at `path` that `entries`,
    /// and the names of `earlier` parts of it, do not: names, and whether
    /// each is a directory. A directory that is missing, or a symlink, has
    /// nothing.
    pub fn extras(
        &self,
        path: &[u8],
        entries: &[Entry],
        earlier: &[Vec<u8>],
    ) -> io::Result<Vec<(Vec<u8>, bool)>> {
        let directory = match self.open(path) {
            Ok(directory) => directory,
            Err(error) if is_missing(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let wanted: HashSet<&[u8]> = entries
            .iter()
            .map(|entry| entry.name.as_slice())
            .chain(earlier.iter().map(Vec::as_slice))
            .collect();
        let mut extras = Vec::new();
        for name in fsys::read_names(&directory)? {
            if !wanted.contains(name.as_slice()) {
                let is_directory = fsys::stat(&directory, &name)
                    .is_ok_and(|stat| fsys::file_type(&stat) == FileType::Directory);
                extras.push((name, is_directory));
            }
        }
        Ok(extras)
    }

    fn open(&self, path: &[u8]) -> io::Result<OwnedFd> {
        let root = fsys::open_root(self.root_path())?;
        fsys::open_path(root, path)
    }

    fn entry(
        &self,
        directory: impl AsFd,
        entry: &Entry,
        ownership: &Ownership,
    ) -> io::Result<Verdict> {
        match fsys::stat(&directory, &entry.name) {
            Ok(stat) => self.verdict(entry, &stat, ownership, || {
                fsys::read_link(&directory, &entry.name)
            }),
            Err(error) if is_missing(&error) => Ok(Verdict::Content),
            Err(error) => Err(error),
        }
    }

    fn verdict(
        &self,
        entry: &Entry,
        stat: &Stat,
        ownership: &Ownership,
        link_target: impl FnOnce() -> io::Result<Vec<u8>>,
    ) -> io::Result<Verdict> {
        #[allow(clippy::unnecessary_cast)]
        let rdev = stat.st_rdev as u64;
        let same_content = match (&entry.kind, fsys::file_type(stat)) {
            #[allow(clippy::unnecessary_cast)]
            (Kind::File, FileType::RegularFile) => {
                stat.st_size as u64 == entry.size
                    && same_mtime(fsys::mtime(stat), entry.mtime, self.modify_window)
            }
            (Kind::Directory, FileType::Directory)
            | (Kind::Fifo, FileType::Fifo)
            | (Kind::Socket, FileType::Socket) => true,
            (Kind::Symlink { target }, FileType::Symlink) => link_target()? == *target,
            (Kind::CharDevice { rdev: wanted }, FileType::CharacterDevice)
            | (Kind::BlockDevice { rdev: wanted }, FileType::BlockDevice) => rdev == *wanted,
            _ => false,
        };
        Ok(if !same_content {
            Verdict::Content
        } else if attributes_differ(entry, stat, ownership) {
            Verdict::Attributes
        } else if !same_mtime(fsys::mtime(stat), entry.mtime, self.modify_window) {
            Verdict::Times
        } else {
            Verdict::Same
        })
    }
}

/// Whether the permissions differ, or an owner that this process could set.
/// An owner it could not set is not a difference: no run could remove it.
fn attributes_differ(entry: &Entry, stat: &Stat, ownership: &Ownership) -> bool {
    #[allow(clippy::unnecessary_cast)]
    let (mode, uid, gid) = (
        stat.st_mode as u32 & 0o7777,
        stat.st_uid as u32,
        stat.st_gid as u32,
    );
    let mode_differs = !entry.kind.is_symlink() && mode != entry.mode;
    let wanted_uid = ownership.uid(entry.uid);
    let wanted_gid = ownership.gid(entry.gid);
    let user_differs = wanted_uid != uid && ownership.can_set_user(wanted_uid, uid);
    let group_differs = wanted_gid != gid && ownership.can_set_group(wanted_gid);
    mode_differs || user_differs || group_differs
}

/// A path that does not exist, or that runs through something that is not a
/// directory, is missing: the entry is needed.
fn is_missing(error: &io::Error) -> bool {
    Errno::from_io_error(error)
        .is_some_and(|errno| [Errno::NOENT, Errno::NOTDIR, Errno::LOOP].contains(&errno))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{FileId, Manifest};
    use crate::walk::{self, Sink};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    struct Collect(Vec<Manifest>);

    impl Sink for Collect {
        fn manifest(&mut self, message: Manifest) -> anyhow::Result<()> {
            self.0.push(message);
            Ok(())
        }

        fn problem(&mut self, path: &[u8], error: io::Error) -> anyhow::Result<()> {
            panic!("{}: {error}", String::from_utf8_lossy(path));
        }
    }

    /// The IDs the Destination at `destination` needs from the tree at `source`.
    fn needs(source: &Path, destination: &Path) -> Vec<FileId> {
        needs_within(source, destination, 0)
    }

    /// The IDs a dry run lists: those whose content or attributes differ.
    fn needs_within(source: &Path, destination: &Path, modify_window: i64) -> Vec<FileId> {
        let mut manifest = Collect(Vec::new());
        walk::walk(source.as_os_str(), &mut manifest).unwrap();
        let comparer = Comparer::new(destination.as_os_str().as_bytes().to_vec(), modify_window);
        let ownership = Ownership::here();
        let mut needs = Vec::new();
        for message in manifest.0 {
            match message {
                Manifest::Root(entry) => {
                    if comparer.root(&entry, &ownership).unwrap().is_listed() {
                        needs.push(entry.id);
                    }
                }
                Manifest::Directory { path, entries, .. } => {
                    let verdicts = comparer.directory(&path, &entries, &ownership).unwrap();
                    for (entry, verdict) in entries.iter().zip(verdicts) {
                        if verdict.is_listed() {
                            needs.push(entry.id);
                        }
                    }
                }
                Manifest::Names { .. } | Manifest::Problem { .. } | Manifest::End => {}
            }
        }
        needs
    }

    fn tree(root: &Path) {
        fs::create_dir(root).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/file"), "content").unwrap();
        symlink("dir/file", root.join("link")).unwrap();
    }

    /// Copies a file's mtime from `from` to `to`.
    fn copy_mtime(from: &Path, to: &Path) {
        let modified = fs::metadata(from).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(to)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn a_missing_destination_needs_everything() {
        let temp = tempfile::tempdir().unwrap();
        tree(&temp.path().join("source"));
        // Root, dir, link, dir/file.
        assert_eq!(
            needs(
                &temp.path().join("source"),
                &temp.path().join("destination")
            ),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn an_identical_destination_needs_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        tree(&source);
        tree(&destination);
        copy_mtime(&source.join("dir/file"), &destination.join("dir/file"));
        assert!(needs(&source, &destination).is_empty());
    }

    #[test]
    fn changed_entries_are_needed() {
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        tree(&source);
        tree(&destination);
        // Same size, different mtime.
        fs::File::options()
            .write(true)
            .open(destination.join("dir/file"))
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        // A symlink pointing elsewhere.
        fs::remove_file(destination.join("link")).unwrap();
        symlink("elsewhere", destination.join("link")).unwrap();
        assert_eq!(needs(&source, &destination), [2, 3]);
    }

    #[test]
    fn a_symlink_where_a_directory_belongs_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        tree(&source);
        // The destination's `dir` is a link to a copy of the source's dir.
        fs::create_dir(&destination).unwrap();
        tree(&temp.path().join("other"));
        copy_mtime(
            &source.join("dir/file"),
            &temp.path().join("other/dir/file"),
        );
        symlink(temp.path().join("other/dir"), destination.join("dir")).unwrap();
        symlink("dir/file", destination.join("link")).unwrap();
        // `dir` is needed, being a link, and so is everything inside it.
        assert_eq!(needs(&source, &destination), [1, 3]);
    }

    #[test]
    fn mtime_windows() {
        let at = |seconds, nanoseconds| Timestamp {
            seconds,
            nanoseconds,
        };
        // Whole seconds by default.
        assert!(same_mtime(at(10, 100), at(10, 900), 0));
        assert!(!same_mtime(at(10, 0), at(11, 0), 0));
        // A positive window allows that many seconds apart.
        assert!(same_mtime(at(10, 0), at(12, 0), 2));
        assert!(same_mtime(at(12, 0), at(10, 0), 2));
        assert!(!same_mtime(at(10, 0), at(13, 0), 2));
        // A negative window compares nanoseconds too.
        assert!(!same_mtime(at(10, 100), at(10, 900), -1));
        assert!(same_mtime(at(10, 100), at(10, 100), -1));
    }

    #[test]
    fn a_file_is_compared_within_the_window() {
        use std::time::{Duration, SystemTime};
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("file"), "content").unwrap();
        fs::write(destination.join("file"), "content").unwrap();
        let set = |path: &Path, after: Duration| {
            let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(base + after)
                .unwrap();
        };
        set(&source.join("file"), Duration::from_millis(500));

        // The same second, different nanoseconds.
        set(&destination.join("file"), Duration::from_millis(250));
        assert!(needs_within(&source, &destination, 0).is_empty());
        assert_eq!(needs_within(&source, &destination, -1), [1]);

        // One second apart.
        set(&destination.join("file"), Duration::from_millis(1500));
        assert_eq!(needs_within(&source, &destination, 0), [1]);
        assert!(needs_within(&source, &destination, 1).is_empty());
    }
}
