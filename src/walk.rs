//! The Source's walk of its tree, which produces the manifest.
//!
//! One thread, depth first: a directory's entries are sent together, sorted by
//! name, before any of its subdirectories are read. The walk is made parallel
//! in a later step without changing this order.

use std::ffi::OsStr;
use std::io;
use std::os::fd::AsFd;

use anyhow::Result;

use crate::fsys;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};

/// Where the walk sends what it finds.
pub trait Sink {
    fn manifest(&mut self, message: Manifest) -> Result<()>;
    /// Something could not be read. The walk skips it and goes on.
    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()>;
}

/// Walks the tree at `root`, sending its manifest to `sink`, ending with
/// [`Manifest::End`]. IDs start at 0 for the root and follow manifest order.
pub fn walk(root: &OsStr, sink: &mut impl Sink) -> Result<()> {
    let mut walker = Walker { next_id: 0, sink };
    let stat = match fsys::stat_root(root) {
        Ok(stat) => stat,
        Err(error) => {
            walker.sink.problem(b"", error)?;
            return walker.sink.manifest(Manifest::End);
        }
    };
    let id = walker.next_id();
    let root_entry = fsys::entry(id, Vec::new(), &stat, || {
        std::fs::read_link(root).map(|target| target.into_os_string().into_encoded_bytes())
    })?;
    let is_directory = root_entry.kind == Kind::Directory;
    walker.sink.manifest(Manifest::Root(root_entry))?;
    if is_directory {
        match fsys::open_root(root) {
            Ok(directory) => walker.directory(directory, Vec::new())?,
            Err(error) => walker.sink.problem(b"", error)?,
        }
    }
    walker.sink.manifest(Manifest::End)
}

struct Walker<'a, S> {
    next_id: FileId,
    sink: &'a mut S,
}

impl<S: Sink> Walker<'_, S> {
    fn next_id(&mut self) -> FileId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn directory(&mut self, directory: impl AsFd, path: Vec<u8>) -> Result<()> {
        let names = match fsys::read_names(&directory) {
            Ok(names) => names,
            Err(error) => return self.sink.problem(&path, error),
        };
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            match self.describe(&directory, name.clone()) {
                Ok(entry) => entries.push(entry),
                Err(error) => self.sink.problem(&manifest::join(&path, &name), error)?,
            }
        }
        let subdirectories: Vec<Vec<u8>> = entries
            .iter()
            .filter(|entry| entry.kind == Kind::Directory)
            .map(|entry| entry.name.clone())
            .collect();
        self.sink.manifest(Manifest::Directory {
            path: path.clone(),
            entries,
        })?;

        for name in subdirectories {
            let child = manifest::join(&path, &name);
            match fsys::open_directory(&directory, &name) {
                Ok(subdirectory) => self.directory(subdirectory, child)?,
                Err(error) => self.sink.problem(&child, error)?,
            }
        }
        Ok(())
    }

    fn describe(&mut self, directory: impl AsFd, name: Vec<u8>) -> io::Result<Entry> {
        let stat = fsys::stat(&directory, &name)?;
        let link_name = name.clone();
        let id = self.next_id();
        fsys::entry(id, name, &stat, || fsys::read_link(&directory, &link_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[derive(Default)]
    struct Collect {
        messages: Vec<Manifest>,
        problems: Vec<Vec<u8>>,
    }

    impl Sink for Collect {
        fn manifest(&mut self, message: Manifest) -> Result<()> {
            self.messages.push(message);
            Ok(())
        }

        fn problem(&mut self, path: &[u8], _error: io::Error) -> Result<()> {
            self.problems.push(path.to_vec());
            Ok(())
        }
    }

    fn names(entries: &[Entry]) -> Vec<(&[u8], FileId)> {
        entries
            .iter()
            .map(|entry| (entry.name.as_slice(), entry.id))
            .collect()
    }

    #[test]
    fn walks_depth_first_in_name_order() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("b/inner")).unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::write(root.path().join("c"), "c").unwrap();
        fs::write(root.path().join("a/file"), "file").unwrap();
        symlink("../c", root.path().join("b/link")).unwrap();

        let mut sink = Collect::default();
        walk(root.path().as_os_str(), &mut sink).unwrap();
        assert!(sink.problems.is_empty());

        let messages = sink.messages;
        assert_eq!(messages.len(), 6);
        assert!(
            matches!(&messages[0], Manifest::Root(entry) if entry.id == 0 && entry.kind == Kind::Directory)
        );
        let Manifest::Directory { path, entries } = &messages[1] else {
            panic!()
        };
        assert_eq!(path, b"");
        assert_eq!(names(entries), [(&b"a"[..], 1), (b"b", 2), (b"c", 3)]);
        let Manifest::Directory { path, entries } = &messages[2] else {
            panic!()
        };
        assert_eq!(path, b"a");
        assert_eq!(names(entries), [(&b"file"[..], 4)]);
        assert_eq!(entries[0].size, 4);
        let Manifest::Directory { path, entries } = &messages[3] else {
            panic!()
        };
        assert_eq!(path, b"b");
        assert_eq!(names(entries), [(&b"inner"[..], 5), (b"link", 6)]);
        assert_eq!(
            entries[1].kind,
            Kind::Symlink {
                target: b"../c".to_vec()
            }
        );
        let Manifest::Directory { path, entries } = &messages[4] else {
            panic!()
        };
        assert_eq!(path, b"b/inner");
        assert!(entries.is_empty());
        assert_eq!(messages[5], Manifest::End);
    }

    #[test]
    fn does_not_follow_a_symlinked_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        let mut sink = Collect::default();
        walk(root.path().as_os_str(), &mut sink).unwrap();
        // The link is an entry; what it points to is never read.
        assert_eq!(sink.messages.len(), 3);
        let Manifest::Directory { entries, .. } = &sink.messages[1] else {
            panic!()
        };
        assert!(matches!(entries[0].kind, Kind::Symlink { .. }));
    }

    #[test]
    fn a_file_root_is_one_entry() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        fs::write(&file, "content").unwrap();

        let mut sink = Collect::default();
        walk(file.as_os_str(), &mut sink).unwrap();
        assert!(
            matches!(&sink.messages[0], Manifest::Root(entry) if entry.kind == Kind::File && entry.size == 7)
        );
        assert_eq!(sink.messages[1], Manifest::End);
    }

    #[test]
    fn a_missing_root_is_a_problem() {
        let root = tempfile::tempdir().unwrap();
        let mut sink = Collect::default();
        walk(root.path().join("missing").as_os_str(), &mut sink).unwrap();
        assert_eq!(sink.problems, [b""]);
        assert_eq!(sink.messages, [Manifest::End]);
    }
}
