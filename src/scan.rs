//! The Source's scan of its tree, which produces the manifest.
//!
//! The manifest has a fixed order: depth first, each directory's entries
//! together and sorted by name, before any of its subdirectories. That order
//! is the order of paths compared component by component.
//!
//! One thread lists directories, one at a time, in that order: it opens a
//! directory, reads its names, hands them to the stat threads, and goes on to
//! the next. Which names are directories, and so where to go next, it learns
//! from the listing itself, without waiting for their stats. A pool of
//! [`STATS`] threads stats the names, a part at a time, in the order they were
//! handed over, and the calling thread sends what they find in that order,
//! giving IDs as it goes. So the filesystem is asked at most one listing and
//! [`STATS`] stats at once, and memory holds only the directories still to
//! visit, and the parts waiting for the stat threads.
//!
//! A large directory is sent in parts of at most [`PART`] entries, so that no
//! message grows with it.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use anyhow::Result;

use crate::fsys::{self, Listed};
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};

/// How many threads stat entries.
pub const STATS: usize = 8;

/// The most entries in one message: a directory with more is sent in parts.
pub const PART: usize = 10_000;

/// How many parts may wait for the stat threads.
const WAITING: usize = 64;

/// Where the scan sends what it finds.
pub trait Sink {
    fn manifest(&mut self, message: Manifest) -> Result<()>;
    /// Something could not be read. The scan skips it and goes on.
    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()>;
}

/// Scans the tree at `root`, sending its manifest to `sink`, ending with
/// [`Manifest::End`]. IDs start at 0 for the root and follow manifest order.
pub fn scan(root: &OsStr, sink: &mut impl Sink) -> Result<()> {
    scan_with(root, sink, STATS, PART)
}

/// As [`scan`], with at most `part` entries in a message; 0 for [`PART`].
pub fn scan_in_parts(root: &OsStr, sink: &mut impl Sink, part: usize) -> Result<()> {
    scan_with(root, sink, STATS, if part == 0 { PART } else { part })
}

/// Scans with `stats` threads statting entries, sending at most `part`
/// entries in a message.
fn scan_with(root: &OsStr, sink: &mut impl Sink, stats: usize, part: usize) -> Result<()> {
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
            Ok(directory) => send_tree(directory, sink, stats, part)?,
            Err(error) => sink.problem(b"", error)?,
        }
    }
    sink.manifest(Manifest::End)
}

/// What the lister hands the stat threads, in manifest order.
enum Job {
    /// Names of the directory at `path`, open as `directory`: a part.
    Part {
        directory: Arc<OwnedFd>,
        path: Arc<Vec<u8>>,
        names: Vec<Vec<u8>>,
        first: bool,
        last: bool,
    },
    /// The directory at `path` could not be read.
    Unreadable { path: Vec<u8>, error: io::Error },
}

/// What the stat threads hand back, in the same order.
enum Found {
    Part {
        path: Arc<Vec<u8>>,
        entries: Vec<Entry>,
        /// Entries that could not be read.
        problems: Vec<(Vec<u8>, io::Error)>,
        first: bool,
        last: bool,
    },
    Unreadable {
        path: Vec<u8>,
        error: io::Error,
    },
}

/// Lists the tree under the open root, with the stat threads, and sends it in
/// order.
fn send_tree(root: OwnedFd, sink: &mut impl Sink, stats: usize, part: usize) -> Result<()> {
    std::thread::scope(|scope| {
        let profiled = Profiled {
            role: "stat",
            work: State::Stat,
            full: State::ToStat,
        };
        let (mut parts, found) = pool::ordered(scope, stats, WAITING, profiled, stat_part);
        scope.spawn(move || {
            let _thread = profile::thread("scan");
            list_tree(root, &mut parts, part);
        });
        send_found(found, sink)
    })
}

/// A directory waiting to be listed: its name in its parent, which is open.
struct Waiting {
    parent: Arc<OwnedFd>,
    name: Vec<u8>,
    path: Vec<u8>,
}

/// Lists every directory, depth first in name order, handing each one's
/// names to the stat threads in parts of at most `part`, until done or the
/// stat threads stop.
fn list_tree(root: OwnedFd, parts: &mut pool::Submitter<Job>, part: usize) {
    let mut waiting: Vec<Waiting> = Vec::new();
    let mut next = Some((root, Vec::new()));
    loop {
        let handed = match next.take() {
            Some((directory, path)) => list(directory, path, parts, part, &mut waiting),
            None => {
                let Some(Waiting { parent, name, path }) = waiting.pop() else {
                    return;
                };
                let opened = profile::wait(State::List, || fsys::open_directory(&parent, &name));
                match opened {
                    Ok(directory) => {
                        next = Some((directory, path));
                        continue;
                    }
                    Err(error) => parts.submit(Job::Unreadable { path, error }),
                }
            }
        };
        if handed.is_err() {
            // The stat threads stopped: the scan failed, and says why.
            return;
        }
    }
}

/// Lists the open directory at `path`, hands its names to the stat threads,
/// and adds its subdirectories to `waiting`, so that the first by name is
/// listed next.
fn list(
    directory: OwnedFd,
    path: Vec<u8>,
    parts: &mut pool::Submitter<Job>,
    part: usize,
    waiting: &mut Vec<Waiting>,
) -> Result<()> {
    let listed = match profile::wait(State::List, || fsys::read_entries(&directory)) {
        Ok(listed) => listed,
        Err(error) => return parts.submit(Job::Unreadable { path, error }),
    };
    let directory = Arc::new(directory);
    let subdirectories: Vec<Vec<u8>> = listed
        .iter()
        .filter(|entry| is_directory(&directory, entry))
        .map(|entry| entry.name.clone())
        .collect();
    let names: Vec<Vec<u8>> = listed.into_iter().map(|entry| entry.name).collect();
    let path = Arc::new(path);
    // An empty directory is one empty part.
    let count = names.len().div_ceil(part).max(1);
    let mut names = names.into_iter();
    for index in 0..count {
        parts.submit(Job::Part {
            directory: Arc::clone(&directory),
            path: Arc::clone(&path),
            names: names.by_ref().take(part).collect(),
            first: index == 0,
            last: index + 1 == count,
        })?;
    }
    for name in subdirectories.into_iter().rev() {
        waiting.push(Waiting {
            parent: Arc::clone(&directory),
            path: manifest::join(&path, &name),
            name,
        });
    }
    Ok(())
}

/// Whether a listed name is a directory: as the listing says, or, where it
/// does not say, as a stat of it does.
fn is_directory(directory: &OwnedFd, entry: &Listed) -> bool {
    match entry.file_type {
        rustix::fs::FileType::Unknown => profile::wait(State::Stat, || {
            fsys::stat(directory, &entry.name)
                .is_ok_and(|stat| fsys::file_type(&stat) == rustix::fs::FileType::Directory)
        }),
        file_type => file_type == rustix::fs::FileType::Directory,
    }
}

/// Stats a part's names, for a stat thread.
fn stat_part(job: Job) -> Found {
    match job {
        Job::Part {
            directory,
            path,
            names,
            first,
            last,
        } => {
            let mut entries = Vec::with_capacity(names.len());
            let mut problems = Vec::new();
            for name in names {
                match describe(&*directory, name.clone()) {
                    Ok(entry) => entries.push(entry),
                    // Listed, so it is there: sent as unreadable, for the
                    // Destination to leave what it has as it is.
                    Err(error) => {
                        problems.push((manifest::join(&path, &name), error));
                        entries.push(unreadable(name));
                    }
                }
            }
            Found::Part {
                path,
                entries,
                problems,
                first,
                last,
            }
        }
        Job::Unreadable { path, error } => Found::Unreadable { path, error },
    }
}

/// An entry, with ID 0 until it is sent.
/// An entry listed whose metadata could not be read: all of it zero.
fn unreadable(name: Vec<u8>) -> Entry {
    Entry {
        id: 0,
        name,
        kind: Kind::Unreadable,
        size: 0,
        mtime: manifest::Timestamp {
            seconds: 0,
            nanoseconds: 0,
        },
        mode: 0,
        uid: 0,
        gid: 0,
    }
}

fn describe(directory: impl AsFd, name: Vec<u8>) -> io::Result<Entry> {
    let stat = fsys::stat(&directory, &name)?;
    let link_name = name.clone();
    fsys::entry(0, name, &stat, || fsys::read_link(&directory, &link_name))
}

/// Sends what the stat threads found, in order, giving IDs as it goes.
///
/// A directory is sent only if its parent's entry for it, as its stat found,
/// is a directory: the lister went by the listing, and the name may have
/// changed since. The parts of one that is not are left out, and so is
/// everything under it.
fn send_found(found: impl Iterator<Item = Found>, sink: &mut impl Sink) -> Result<()> {
    let mut next_id: FileId = 1;
    // Directories sent as entries, and not listed yet; the root's is empty.
    let mut expected: HashSet<Vec<u8>> = HashSet::from([Vec::new()]);
    // Whether the directory whose parts are coming is being sent.
    let mut sending = false;
    for found in found {
        match found {
            Found::Unreadable { path, error } => {
                if expected.remove(&path) {
                    sink.problem(&path, error)?;
                }
            }
            Found::Part {
                path,
                mut entries,
                problems,
                first,
                last,
            } => {
                if first {
                    sending = expected.remove(path.as_slice());
                }
                if !sending {
                    continue;
                }
                for (problem_path, error) in problems {
                    sink.problem(&problem_path, error)?;
                }
                for entry in &mut entries {
                    entry.id = next_id;
                    next_id += 1;
                    if entry.kind == Kind::Directory {
                        expected.insert(manifest::join(&path, &entry.name));
                    }
                }
                sink.manifest(Manifest::Directory {
                    path: path.to_vec(),
                    entries,
                    more: !last,
                })?;
            }
        }
    }
    Ok(())
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
    fn scans_depth_first_in_name_order() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("b/inner")).unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::write(root.path().join("c"), "c").unwrap();
        fs::write(root.path().join("a/file"), "file").unwrap();
        symlink("../c", root.path().join("b/link")).unwrap();

        let mut sink = Collect::default();
        scan(root.path().as_os_str(), &mut sink).unwrap();
        assert!(sink.problems.is_empty());

        let messages = sink.messages;
        assert_eq!(messages.len(), 6);
        assert!(
            matches!(&messages[0], Manifest::Root(entry) if entry.id == 0 && entry.kind == Kind::Directory)
        );
        let Manifest::Directory { path, entries, .. } = &messages[1] else {
            panic!()
        };
        assert_eq!(path, b"");
        assert_eq!(names(entries), [(&b"a"[..], 1), (b"b", 2), (b"c", 3)]);
        let Manifest::Directory { path, entries, .. } = &messages[2] else {
            panic!()
        };
        assert_eq!(path, b"a");
        assert_eq!(names(entries), [(&b"file"[..], 4)]);
        assert_eq!(entries[0].size, 4);
        let Manifest::Directory { path, entries, .. } = &messages[3] else {
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
        let Manifest::Directory { path, entries, .. } = &messages[4] else {
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
        scan(root.path().as_os_str(), &mut sink).unwrap();
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
        scan(file.as_os_str(), &mut sink).unwrap();
        assert!(
            matches!(&sink.messages[0], Manifest::Root(entry) if entry.kind == Kind::File && entry.size == 7)
        );
        assert_eq!(sink.messages[1], Manifest::End);
    }

    #[test]
    fn a_missing_root_is_a_problem() {
        let root = tempfile::tempdir().unwrap();
        let mut sink = Collect::default();
        scan(root.path().join("missing").as_os_str(), &mut sink).unwrap();
        assert_eq!(sink.problems, [b""]);
        assert_eq!(sink.messages, [Manifest::End]);
    }

    /// A tree wide and deep enough that the stat threads work on many parts
    /// at once: 1,000 directories, each with files, some nested ten deep.
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

    /// Seven files, in parts of three: three messages for the one
    /// directory, one after another, IDs following on; an empty directory is
    /// one empty message.
    #[test]
    fn a_large_directory_is_sent_in_parts() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("big")).unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();
        for i in 0..7 {
            fs::write(root.path().join(format!("big/f{i}")), "x").unwrap();
        }
        let mut sink = Collect::default();
        scan_with(root.path().as_os_str(), &mut sink, 4, 3).unwrap();
        let parts: Vec<(&[u8], Vec<FileId>, bool)> = sink
            .messages
            .iter()
            .filter_map(|message| match message {
                Manifest::Directory {
                    path,
                    entries,
                    more,
                } => Some((
                    path.as_slice(),
                    entries.iter().map(|entry| entry.id).collect(),
                    *more,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            parts,
            [
                (&b""[..], vec![1, 2], false),
                (&b"big"[..], vec![3, 4, 5], true),
                (&b"big"[..], vec![6, 7, 8], true),
                (&b"big"[..], vec![9], false),
                (&b"empty"[..], vec![], false),
            ]
        );
    }

    #[test]
    fn stat_threads_send_the_same_manifest_as_one() {
        let root = tempfile::tempdir().unwrap();
        big_tree(root.path());
        let scanned = |stats, part| {
            let mut sink = Collect::default();
            scan_with(root.path().as_os_str(), &mut sink, stats, part).unwrap();
            assert!(sink.problems.is_empty());
            sink.messages
        };
        let one = scanned(1, PART);
        assert!(one.len() > 1_000, "{} messages", one.len());
        for _ in 0..3 {
            assert_eq!(scanned(8, PART), one);
        }
        // In small parts, too.
        let small = scanned(1, 2);
        for _ in 0..3 {
            assert_eq!(scanned(8, 2), small);
        }
    }

    #[test]
    fn a_name_that_cannot_be_statted_is_sent_as_unreadable() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("here"), "x").unwrap();
        let directory = fsys::open_root(root.path().as_os_str()).unwrap();
        // Listed, then gone before its stat.
        let found = stat_part(Job::Part {
            directory: Arc::new(directory),
            path: Arc::new(b"d".to_vec()),
            names: vec![b"gone".to_vec(), b"here".to_vec()],
            first: true,
            last: true,
        });
        let Found::Part {
            entries, problems, ..
        } = found
        else {
            panic!("not a part");
        };
        let kinds: Vec<(&[u8], &Kind)> = entries
            .iter()
            .map(|entry| (entry.name.as_slice(), &entry.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (&b"gone"[..], &Kind::Unreadable),
                (&b"here"[..], &Kind::File)
            ]
        );
        let paths: Vec<&[u8]> = problems.iter().map(|(path, _)| path.as_slice()).collect();
        assert_eq!(paths, [b"d/gone"]);
    }

    #[test]
    fn an_unreadable_directory_is_left_out_and_the_scan_goes_on() {
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
        scan_with(root.path().as_os_str(), &mut sink, 4, PART).unwrap();
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

    /// A name listed as a directory, but no longer one by the time it is
    /// statted: its parts, and everything under it, are left out.
    #[test]
    fn a_directory_that_is_no_longer_one_is_left_out() {
        let entry = |name: &[u8], kind: Kind| Entry {
            id: 0,
            name: name.to_vec(),
            kind,
            size: 0,
            mtime: manifest::Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o755,
            uid: 0,
            gid: 0,
        };
        let part = |path: &[u8], entries: Vec<Entry>| Found::Part {
            path: Arc::new(path.to_vec()),
            entries,
            problems: Vec::new(),
            first: true,
            last: true,
        };
        let found = vec![
            part(
                b"",
                vec![entry(b"dir", Kind::Directory), entry(b"was", Kind::File)],
            ),
            part(b"dir", vec![entry(b"inner", Kind::File)]),
            part(b"was", vec![entry(b"gone", Kind::Directory)]),
            part(b"was/gone", vec![]),
            Found::Unreadable {
                path: b"was/other".to_vec(),
                error: io::Error::other("unreadable"),
            },
        ];
        let mut sink = Collect::default();
        send_found(found.into_iter(), &mut sink).unwrap();
        let paths: Vec<&[u8]> = sink
            .messages
            .iter()
            .filter_map(|message| match message {
                Manifest::Directory { path, .. } => Some(path.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, [&b""[..], b"dir"]);
        assert!(sink.problems.is_empty());
    }
}
