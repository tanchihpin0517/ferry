//! What the Source tells the Destination about its files.
//!
//! The manifest is sent one directory at a time: every entry of a directory,
//! sorted by name, in one message. Paths are bytes with `/` between
//! components, relative to the transfer's root; the root itself is the empty
//! path. Names are not required to be UTF-8.

use std::collections::VecDeque;

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
/// and only once, its parts together, depth first, by name, as a scan goes:
/// one announced may never come, as one the Source could not list, but none
/// comes after another that comes after it; names come in order, strictly;
/// IDs follow manifest order, from 0 for the root.
#[derive(Debug, Default)]
pub struct Check {
    /// A root came.
    root: bool,
    /// The root is a directory that has not come yet.
    root_expected: bool,
    ended: bool,
    /// The ID the next entry has.
    next: FileId,
    /// The directories come, from the root down to the latest, each with
    /// the names of its own announced and not come yet, in order: what may
    /// come next is one of those, of the latest or of one above it.
    expected: Vec<(Vec<u8>, VecDeque<Vec<u8>>)>,
    /// The directory whose parts are coming.
    open: Option<Open>,
}

/// A directory whose parts are coming: its path, the last name it sent,
/// and the names of its directories so far.
#[derive(Debug)]
struct Open {
    path: Vec<u8>,
    last: Vec<u8>,
    directories: VecDeque<Vec<u8>>,
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
        self.root_expected = entry.kind == Kind::Directory;
        Ok(())
    }

    /// Takes the directory at `path` for come, if it may come now: see
    /// [`Check`].
    fn arrive(&mut self, path: &[u8]) -> Result<()> {
        let shown = || String::from_utf8_lossy(path).into_owned();
        if path.is_empty() {
            ensure!(
                self.root_expected,
                "the root directory, which is not one announced, or came before"
            );
            self.root_expected = false;
            return Ok(());
        }
        let (parent, name) = match path.iter().rposition(|&byte| byte == b'/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => (&b""[..], path),
        };
        while self
            .expected
            .last()
            .is_some_and(|(directory, _)| directory != parent)
        {
            self.expected.pop();
        }
        let Some((_, names)) = self.expected.last_mut() else {
            anyhow::bail!(
                "the directory {:?}, which is not one announced, or came before, or out of \
                 depth-first order",
                shown()
            );
        };
        while let Some(next) = names.pop_front() {
            if next == name {
                return Ok(());
            }
        }
        anyhow::bail!(
            "the directory {:?}, which is not one announced, or came before, or out of \
             depth-first order",
            shown()
        )
    }

    pub fn directory(&mut self, path: &[u8], entries: &[Entry], more: bool) -> Result<()> {
        ensure!(!self.ended, "a directory after the end");
        let shown = || String::from_utf8_lossy(path).into_owned();
        let (mut last, mut directories) = match self.open.take() {
            Some(Open {
                path: open,
                last,
                directories,
            }) => {
                ensure!(
                    open == path,
                    "{} before the rest of {}",
                    shown(),
                    String::from_utf8_lossy(&open)
                );
                (Some(last), directories)
            }
            None => {
                self.arrive(path)?;
                (None, VecDeque::new())
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
                directories.push_back(name.clone());
            }
            last = Some(name.clone());
        }
        if more {
            self.open = Some(Open {
                path: path.to_vec(),
                last: last.unwrap_or_default(),
                directories,
            });
        } else {
            self.expected.push((path.to_vec(), directories));
        }
        Ok(())
    }

    /// Whether the manifest has come to its end.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    pub fn end(&mut self) -> Result<()> {
        ensure!(!self.ended, "a second end");
        if let Some(open) = &self.open {
            anyhow::bail!(
                "the end before the rest of {}",
                String::from_utf8_lossy(&open.path)
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

    /// Directories come depth first, by name: one announced may never come,
    /// but none comes after one that comes after it.
    #[test]
    fn directories_come_depth_first() {
        let tree = |check: &mut Check| {
            check.root(&named(0, b"", Kind::Directory)).unwrap();
            check
                .directory(
                    b"",
                    &[
                        named(1, b"a", Kind::Directory),
                        named(2, b"b", Kind::Directory),
                    ],
                    false,
                )
                .unwrap();
        };
        // Deeper ones before the next beside them.
        let mut check = Check::default();
        tree(&mut check);
        check
            .directory(b"a", &[named(3, b"x", Kind::Directory)], false)
            .unwrap();
        check.directory(b"a/x", &[], false).unwrap();
        check.directory(b"b", &[], false).unwrap();
        check.end().unwrap();
        // Not after the next beside them.
        let mut check = Check::default();
        tree(&mut check);
        check
            .directory(b"a", &[named(3, b"x", Kind::Directory)], false)
            .unwrap();
        check.directory(b"b", &[], false).unwrap();
        assert!(check.directory(b"a/x", &[], false).is_err());
        // One never come, as one the Source could not list.
        let mut check = Check::default();
        tree(&mut check);
        check.directory(b"b", &[], false).unwrap();
        check.end().unwrap();
        // Nor back to one passed over.
        let mut check = Check::default();
        tree(&mut check);
        check.directory(b"b", &[], false).unwrap();
        assert!(check.directory(b"a", &[], false).is_err());
        // Nor the root again.
        let mut check = Check::default();
        tree(&mut check);
        assert!(check.directory(b"", &[], false).is_err());
    }

    #[test]
    fn join_paths() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a", b"b"), b"a/b");
        assert_eq!(join(b"a/b", b"c"), b"a/b/c");
    }
}
