//! The Source's walk of its tree, which produces the manifest.
//!
//! The manifest has a fixed order: depth first, each directory's entries
//! together and sorted by name, before any of its subdirectories. That order
//! is the order of paths compared component by component, so every directory
//! waiting to be read or sent is kept in a map sorted that way.
//!
//! Worker threads read directories ahead, in parallel, always taking the
//! first one waiting. The calling thread sends them in order, waiting when the
//! next one is not read yet. Read-ahead is bounded, so memory is too; the
//! directory the calling thread waits for may always be read, so the walk
//! never stalls. IDs are given as directories are sent, so they follow the
//! manifest's order whatever order the reading happened in.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;

use crate::fsys;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};

/// How many directories may be read ahead of the one the walk sends next.
const READ_AHEAD: usize = 256;

/// Where the walk sends what it finds.
pub trait Sink {
    fn manifest(&mut self, message: Manifest) -> Result<()>;
    /// Something could not be read. The walk skips it and goes on.
    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()>;
}

/// Walks the tree at `root`, sending its manifest to `sink`, ending with
/// [`Manifest::End`]. IDs start at 0 for the root and follow manifest order.
pub fn walk(root: &OsStr, sink: &mut impl Sink) -> Result<()> {
    let threads = std::thread::available_parallelism().map_or(4, |count| count.get().min(16));
    walk_with(root, sink, threads)
}

/// A directory's path, as its components: the order of the manifest.
type Key = Vec<Vec<u8>>;

/// A directory found, waiting to be read or sent.
enum Node {
    /// To be read: by its name in its parent, open while it waits.
    Waiting {
        parent: Arc<OwnedFd>,
    },
    Reading,
    Read(Listing),
}

/// What reading a directory found.
struct Listing {
    /// The directory itself could not be read: it is left out of the
    /// manifest.
    unreadable: bool,
    /// Its entries, sorted by name, without IDs yet.
    entries: Vec<Entry>,
    /// What could not be read: the directory itself, or some of its entries.
    problems: Vec<(Vec<u8>, io::Error)>,
}

struct Shared {
    nodes: BTreeMap<Key, Node>,
    /// The nodes waiting to be read, in order.
    waiting: BTreeSet<Key>,
    /// Directories read but not yet sent.
    read_ahead: usize,
    /// The walk failed: the workers stop.
    stopped: bool,
}

/// Walks with `threads` workers reading directories.
fn walk_with(root: &OsStr, sink: &mut impl Sink, threads: usize) -> Result<()> {
    let stat = match fsys::stat_root(root) {
        Ok(stat) => stat,
        Err(error) => {
            sink.problem(b"", error)?;
            return sink.manifest(Manifest::End);
        }
    };
    let root_entry = fsys::entry(0, Vec::new(), &stat, || {
        std::fs::read_link(root).map(|target| target.into_os_string().into_encoded_bytes())
    })?;
    let is_directory = root_entry.kind == Kind::Directory;
    sink.manifest(Manifest::Root(root_entry))?;
    if is_directory {
        match fsys::open_root(root) {
            Ok(directory) => send_tree(directory, sink, threads)?,
            Err(error) => sink.problem(b"", error)?,
        }
    }
    sink.manifest(Manifest::End)
}

/// Reads the tree under the open root with workers, and sends it in order.
fn send_tree(root: OwnedFd, sink: &mut impl Sink, threads: usize) -> Result<()> {
    let shared = Mutex::new(Shared {
        nodes: BTreeMap::new(),
        waiting: BTreeSet::from([Vec::new()]),
        read_ahead: 0,
        stopped: false,
    });
    let changed = Condvar::new();
    // The root is read like any directory: by the name `.` in itself.
    shared.lock().expect("not poisoned").nodes.insert(
        Vec::new(),
        Node::Waiting {
            parent: Arc::new(root),
        },
    );

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| work(&shared, &changed));
        }
        let sent = send_in_order(&shared, &changed, sink);
        shared.lock().expect("not poisoned").stopped = true;
        changed.notify_all();
        sent
    })
}

/// Takes the first directory waiting, reads it, and records what it holds,
/// until the walk is done.
fn work(shared: &Mutex<Shared>, changed: &Condvar) {
    let mut guard = shared.lock().expect("not poisoned");
    loop {
        if guard.stopped {
            return;
        }
        // The first directory waiting; it may be read if read-ahead allows,
        // or if it is the very next to send.
        let first_key = guard.nodes.keys().next();
        let next = guard
            .waiting
            .first()
            .filter(|key| guard.read_ahead < READ_AHEAD || Some(*key) == first_key)
            .cloned();
        let Some(key) = next else {
            if guard.nodes.is_empty() {
                return;
            }
            guard = changed.wait(guard).expect("not poisoned");
            continue;
        };
        guard.waiting.remove(&key);
        let Some(Node::Waiting { parent }) = guard.nodes.insert(key.clone(), Node::Reading) else {
            unreachable!("the node was waiting");
        };
        drop(guard);

        let name = key.last().map_or(&b"."[..], |name| name.as_slice());
        let (listing, directory) = read_directory(&parent, name, &key);
        drop(parent);

        guard = shared.lock().expect("not poisoned");
        if let Some(directory) = directory {
            let directory = Arc::new(directory);
            for entry in listing
                .entries
                .iter()
                .filter(|entry| entry.kind == Kind::Directory)
            {
                let mut child = key.clone();
                child.push(entry.name.clone());
                guard.waiting.insert(child.clone());
                guard.nodes.insert(
                    child,
                    Node::Waiting {
                        parent: Arc::clone(&directory),
                    },
                );
            }
        }
        guard.nodes.insert(key, Node::Read(listing));
        guard.read_ahead += 1;
        changed.notify_all();
    }
}

/// Reads the directory `name` in `parent`: its entries, sorted by name, and
/// the directory, open, for reading its subdirectories.
fn read_directory(parent: &OwnedFd, name: &[u8], key: &Key) -> (Listing, Option<OwnedFd>) {
    let path = key.join(&b'/');
    let mut listing = Listing {
        unreadable: false,
        entries: Vec::new(),
        problems: Vec::new(),
    };
    let directory = if key.is_empty() {
        rustix::io::dup(parent).map_err(io::Error::from)
    } else {
        fsys::open_directory(parent, name)
    };
    let directory = match directory {
        Ok(directory) => directory,
        Err(error) => {
            listing.unreadable = true;
            listing.problems.push((path, error));
            return (listing, None);
        }
    };
    let names = match fsys::read_names(&directory) {
        Ok(names) => names,
        Err(error) => {
            listing.unreadable = true;
            listing.problems.push((path, error));
            return (listing, None);
        }
    };
    for name in names {
        match describe(&directory, name.clone()) {
            Ok(entry) => listing.entries.push(entry),
            Err(error) => listing.problems.push((manifest::join(&path, &name), error)),
        }
    }
    (listing, Some(directory))
}

/// An entry, with ID 0 until it is sent.
fn describe(directory: impl AsFd, name: Vec<u8>) -> io::Result<Entry> {
    let stat = fsys::stat(&directory, &name)?;
    let link_name = name.clone();
    fsys::entry(0, name, &stat, || fsys::read_link(&directory, &link_name))
}

/// Sends each directory in order, as soon as it is read, giving IDs as it
/// goes.
fn send_in_order(shared: &Mutex<Shared>, changed: &Condvar, sink: &mut impl Sink) -> Result<()> {
    let mut next_id: FileId = 1;
    loop {
        let (key, listing) = {
            let mut guard = shared.lock().expect("not poisoned");
            loop {
                let Some(first) = guard.nodes.first_entry() else {
                    return Ok(());
                };
                if matches!(first.get(), Node::Read(_)) {
                    let (key, node) = first.remove_entry();
                    guard.read_ahead -= 1;
                    changed.notify_all();
                    let Node::Read(listing) = node else {
                        unreachable!("the node was read");
                    };
                    break (key, listing);
                }
                guard = changed.wait(guard).expect("not poisoned");
            }
        };
        let path = key.join(&b'/');
        for (problem_path, error) in listing.problems {
            sink.problem(&problem_path, error)?;
        }
        if listing.unreadable {
            continue;
        }
        let mut entries = listing.entries;
        for entry in &mut entries {
            entry.id = next_id;
            next_id += 1;
        }
        sink.manifest(Manifest::Directory { path, entries })?;
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

    /// A tree wide and deep enough that workers read far ahead: 1,000
    /// directories, each with files, some nested ten deep.
    fn big_tree(root: &std::path::Path) {
        for i in 0..100 {
            let wide = root.join(format!("wide-{i:03}"));
            fs::create_dir_all(&wide).unwrap();
            for j in 0..5 {
                fs::write(wide.join(format!("file-{j}")), "x").unwrap();
                fs::create_dir(wide.join(format!("sub-{j}"))).unwrap();
                fs::write(wide.join(format!("sub-{j}/leaf")), "y").unwrap();
            }
        }
        let mut deep = root.to_path_buf();
        for depth in 0..10 {
            deep = deep.join(format!("deep-{depth}"));
            for k in 0..40 {
                fs::create_dir_all(deep.join(format!("side-{k}"))).unwrap();
            }
        }
    }

    #[test]
    fn workers_send_the_same_manifest_as_one_thread() {
        let root = tempfile::tempdir().unwrap();
        big_tree(root.path());
        let walked = |threads| {
            let mut sink = Collect::default();
            walk_with(root.path().as_os_str(), &mut sink, threads).unwrap();
            assert!(sink.problems.is_empty());
            sink.messages
        };
        let one = walked(1);
        assert!(one.len() > 1_000, "{} messages", one.len());
        for _ in 0..3 {
            assert_eq!(walked(8), one);
        }
    }

    #[test]
    fn an_unreadable_directory_is_left_out_and_the_walk_goes_on() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/locked")).unwrap();
        fs::write(root.path().join("a/locked/hidden"), "x").unwrap();
        fs::create_dir(root.path().join("b")).unwrap();
        fs::write(root.path().join("b/seen"), "y").unwrap();
        fs::set_permissions(
            root.path().join("a/locked"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let mut sink = Collect::default();
        walk_with(root.path().as_os_str(), &mut sink, 4).unwrap();
        fs::set_permissions(
            root.path().join("a/locked"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(sink.problems, [b"a/locked".to_vec()]);
        let paths: Vec<&[u8]> = sink
            .messages
            .iter()
            .filter_map(|message| match message {
                Manifest::Directory { path, .. } => Some(path.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, [&b""[..], b"a", b"b"]);
    }
}
