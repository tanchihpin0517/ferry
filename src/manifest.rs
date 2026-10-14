//! What the Source tells the Destination about its files.
//!
//! The manifest is sent one directory at a time: every entry of a directory,
//! sorted by name, in one message. Paths are bytes with `/` between
//! components, relative to the transfer's root; the root itself is the empty
//! path. Names are not required to be UTF-8.

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
    Symlink { target: Vec<u8> },
    Fifo,
    Socket,
    CharDevice { rdev: u64 },
    BlockDevice { rdev: u64 },
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
    /// Every entry of the directory at `path`.
    Directory { path: Vec<u8>, entries: Vec<Entry> },
    /// The names of user and group IDs that the entries after it use, each
    /// sent once, before its first use. ID 0 is never named.
    Names {
        users: Vec<(u32, String)>,
        groups: Vec<(u32, String)>,
    },
    /// Something at `path` could not be read, so it is missing from the
    /// manifest: `sync` must not take it for deleted.
    Problem { path: Vec<u8> },
    /// The walk is complete.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_paths() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a", b"b"), b"a/b");
        assert_eq!(join(b"a/b", b"c"), b"a/b/c");
    }
}
