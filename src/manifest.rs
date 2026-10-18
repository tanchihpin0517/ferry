//! What the Source tells the Destination about its files.
//!
//! The manifest is sent one directory at a time: every entry of a directory,
//! sorted by name, in one message. Paths are bytes with `/` between
//! components, relative to the transfer's root; the root itself is the empty
//! path. Names are not required to be UTF-8.

use std::collections::HashSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// A file's ID, assigned in manifest order. Needs and results refer to files
/// by ID rather than by path.
pub type FileId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: FileId,
    /// The name within its directory; empty for the root.
    pub name: Vec<u8>,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Timestamp,
    /// Permission bits, including setuid, setgid, and sticky.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Directory,
    Symlink {
        target: Vec<u8>,
    },
    Fifo,
    Socket,
    CharDevice {
        rdev: u64,
    },
    BlockDevice {
        rdev: u64,
    },
    /// A name the Source listed but could not look at: its metadata is
    /// unknown, and zero. The Destination leaves what it has under the name
    /// as it is: never needed, never extra.
    Unreadable,
}

impl Kind {
    pub fn is_symlink(&self) -> bool {
        matches!(self, Kind::Symlink { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Manifest {
    /// The root of the transfer. Always the first message, with ID 0.
    Root(Entry),
    /// Entries of the directory at `path`. A large directory comes in parts,
    /// one after another, each but the last with `more`, so that no message
    /// grows with the directory.
    Directory {
        path: Vec<u8>,
        entries: Vec<Entry>,
        more: bool,
    },
    /// The names of user and group IDs that the entries after it use, each
    /// sent once, before its first use. ID 0 is never named.
    Names {
        users: Vec<(u32, String)>,
        groups: Vec<(u32, String)>,
    },
    /// Something at `path` could not be read: as the scan listed it, so it
    /// is in the manifest as unreadable, or, as `file`, a needed file that
    /// could not be opened to be sent, so it never comes.
    Problem { path: Vec<u8>, file: Option<FileId> },
    /// The scan is complete.
    End,
}

/// Joins a directory path and a name into a path.
pub fn join(directory: &[u8], name: &[u8]) -> Vec<u8> {
    if directory.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(directory.len() + 1 + name.len());
    path.extend_from_slice(directory);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

/// Checks the manifest as it comes, against what a Source sends, so that
/// the Destination acts on nothing else: whatever a Source sends, by a bug
/// or ill will, it can then make the Destination touch only what is under
/// its root, by names it has listed. A root comes before any directory, and
/// once; each name is one, not `.` or `..`, nor with a `/` or a NUL in it;
/// a directory comes only once announced as one, by its parent's entries,
/// and only once, its parts together; names come in order, strictly; IDs
/// follow manifest order, from 0 for the root.
#[derive(Debug, Default)]
pub struct Check {
    /// A root came.
    root: bool,
    ended: bool,
    /// The ID the next entry has.
    next: FileId,
    /// Directories announced, and not come yet.
    expected: HashSet<Vec<u8>>,
    /// The directory whose parts are coming, and the last name it sent.
    open: Option<(Vec<u8>, Vec<u8>)>,
}

impl Check {
    pub fn root(&mut self, entry: &Entry) -> Result<()> {
        ensure!(!self.ended, "a root after the end");
        ensure!(!self.root, "a second root");
        ensure!(
            entry.id == 0 && entry.name.is_empty(),
            "a root with ID {} named {}",
            entry.id,
            String::from_utf8_lossy(&entry.name)
        );
        self.root = true;
        self.next = 1;
        if entry.kind == Kind::Directory {
            self.expected.insert(Vec::new());
        }
        Ok(())
    }

    pub fn directory(&mut self, path: &[u8], entries: &[Entry], more: bool) -> Result<()> {
        ensure!(!self.ended, "a directory after the end");
        let shown = || String::from_utf8_lossy(path).into_owned();
        let mut last = match self.open.take() {
            Some((open, last)) => {
                ensure!(
                    open == path,
                    "{} before the rest of {}",
                    shown(),
                    String::from_utf8_lossy(&open)
                );
                Some(last)
            }
            None => {
                ensure!(
                    self.expected.remove(path),
                    "the directory {:?}, which is not one announced, or came before",
                    shown()
                );
                None
            }
        };
        for entry in entries {
            let name = &entry.name;
            ensure!(
                !name.is_empty()
                    && name != b"."
                    && name != b".."
                    && !name.contains(&b'/')
                    && !name.contains(&0),
                "the name {:?} in {:?}",
                String::from_utf8_lossy(name),
                shown()
            );
            if let Some(last) = &last {
                ensure!(
                    last < name,
                    "{:?} after {:?} in {:?}, out of order",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(last),
                    shown()
                );
            }
            ensure!(
                entry.id == self.next,
                "ID {} where {} was due, in {:?}",
                entry.id,
                self.next,
                shown()
            );
            self.next += 1;
            if entry.kind == Kind::Directory {
                self.expected.insert(join(path, name));
            }
            last = Some(name.clone());
        }
        if more {
            self.open = Some((path.to_vec(), last.unwrap_or_default()));
        }
        Ok(())
    }

    /// Whether the manifest has come to its end.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    pub fn end(&mut self) -> Result<()> {
        ensure!(!self.ended, "a second end");
        if let Some((open, _)) = &self.open {
            anyhow::bail!(
                "the end before the rest of {}",
                String::from_utf8_lossy(open)
            );
        }
        self.ended = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(id: FileId, name: &[u8], kind: Kind) -> Entry {
        Entry {
            id,
            name: name.to_vec(),
            kind,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0,
            uid: 0,
            gid: 0,
        }
    }

    /// A Source's manifest, as a scan sends it, passes.
    #[test]
    fn a_manifest_as_a_scan_sends_it_passes() {
        let mut check = Check::default();
        check.root(&named(0, b"", Kind::Directory)).unwrap();
        check
            .directory(
                b"",
                &[named(1, b"a", Kind::Directory), named(2, b"b", Kind::File)],
                true,
            )
            .unwrap();
        check
            .directory(b"", &[named(3, b"c", Kind::Unreadable)], false)
            .unwrap();
        check.directory(b"a", &[], false).unwrap();
        check.end().unwrap();
    }

    /// Whatever else, it does not.
    #[test]
    fn anything_else_does_not() {
        let root = || {
            let mut check = Check::default();
            check.root(&named(0, b"", Kind::Directory)).unwrap();
            check
        };
        // A file root announces no directory, not even itself.
        let mut check = Check::default();
        check.root(&named(0, b"", Kind::File)).unwrap();
        assert!(check.directory(b"", &[], false).is_err());
        // Nothing before the root, nor after the end.
        assert!(Check::default().directory(b"", &[], false).is_err());
        let mut check = root();
        check.directory(b"", &[], false).unwrap();
        check.end().unwrap();
        assert!(check.root(&named(0, b"", Kind::Directory)).is_err());
        assert!(check.end().is_err());
        // An unread directory announces nothing.
        let mut check = root();
        check
            .directory(b"", &[named(1, b"x", Kind::Unreadable)], false)
            .unwrap();
        assert!(check.directory(b"x", &[], false).is_err());
        // The end before a directory's last part.
        let mut check = root();
        check
            .directory(b"", &[named(1, b"a", Kind::File)], true)
            .unwrap();
        assert!(check.end().is_err());
        // Another directory before a directory's last part.
        let mut check = root();
        check
            .directory(b"", &[named(1, b"a", Kind::Directory)], true)
            .unwrap();
        assert!(check.directory(b"a", &[], false).is_err());
    }

    #[test]
    fn join_paths() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a", b"b"), b"a/b");
        assert_eq!(join(b"a/b", b"c"), b"a/b/c");
    }
}
