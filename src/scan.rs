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
//! [`STATS`] threads stats the names, a batch of at most [`BATCH`] at a
//! time, in the order they were handed over, and the calling thread sends
//! what they find in that order, giving IDs as it goes. So the filesystem is
//! asked at most one listing and [`STATS`] stats at once, and memory holds
//! only the directories still to visit, and the batches waiting for the stat
//! threads.
//!
//! A directory the scan announced, as one among its parent's entries, and
//! does not list, it says is unlistable: one it could not read; one on
//! another filesystem, with `-x`; one the lister found no directory, as one
//! that changed between its listing and its stat. So each directory
//! announced ends, listed or not.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::dirs::{self, DirCache};
use crate::fsys::{self, Listed};
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::FileKey;
use crate::stage::Stages;
use crate::stop::Stop;

/// How many threads stat entries.
pub const STATS: usize = 8;

/// How many names a stat thread stats at a time, of one directory: the
/// scan's, here, and the Destination's, comparing; the Source writes each
/// batch's records at once.
pub const BATCH: usize = 256;

/// How many batches may wait for the stat threads.
const WAITING: usize = 64;

/// How many directories on the way down may be held open, for their
/// subdirectories still to be listed. Below that depth, a directory is opened
/// again from the root, so that a deep tree does not run out of descriptors.
const MAX_OPEN_PARENTS: usize = 128;

/// The next entries of a directory, in order, with their IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch<'a> {
    /// The directory's ID: the root's 0, or its entry's.
    pub dir: FileId,
    pub path: &'a [u8],
    pub entries: Vec<Entry>,
    /// The directory's first batch, and its last: an empty directory is one
    /// batch of none, both.
    pub first: bool,
    pub last: bool,
}

impl Batch<'_> {
    /// The manifest's records of it: an `In` before its first, its entries,
    /// and a `Listed` after its last.
    pub fn into_records(self) -> Vec<Manifest> {
        let mut records = Vec::with_capacity(self.entries.len() + 2);
        if self.first {
            records.push(Manifest::In { dir: self.dir });
        }
        records.extend(self.entries.into_iter().map(Manifest::Entry));
        if self.last {
            records.push(Manifest::Listed { dir: self.dir });
        }
        records
    }
}

/// Where the scan sends what it finds, in manifest order.
pub trait Sink {
    /// The root, first; IDs start at 0 for it.
    fn root(&mut self, entry: Entry) -> Result<()>;
    /// The next entries of a directory, its batches one after another,
    /// IDs following on.
    fn entries(&mut self, batch: Batch) -> Result<()>;
    /// A directory announced, by its ID, is not listed.
    fn unlistable(&mut self, dir: FileId) -> Result<()>;
    /// Something could not be read. The scan skips it and goes on.
    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()>;
    /// Something listed was gone before it could be looked at: it is left
    /// out, as it is no more.
    fn vanished(&mut self, path: &[u8]) -> Result<()>;
    /// The scan is complete: every directory announced has ended.
    fn end(&mut self) -> Result<()>;
}

/// Scans the tree at `root`, sending its manifest to `sink`.
pub fn scan(root: &OsStr, sink: &mut impl Sink) -> Result<()> {
    let dirs = DirCache::new(root, dirs::KEPT);
    scan_with(
        root,
        &dirs,
        &Stop::new(None),
        sink,
        STATS,
        BATCH,
        Bounds::default(),
    )
}

/// Where a scan goes no further.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bounds<'a> {
    /// A directory among these, which is the Destination's root, fails the
    /// scan where it is reached, rather than be gone into.
    pub avoid: &'a [FileKey],
    /// Stay on the root's filesystem, as rsync's `-x` does: a directory on
    /// another, a mount point, is sent, but not gone into.
    pub one_file_system: bool,
}

/// As [`scan`], in batches of at most `batch` names; 0 for [`BATCH`],
/// going no further than `bounds`. The root is opened from `dirs`, which the
/// readers of what it finds share; its stages run under `stop`.
pub fn scan_in_batches(
    root: &OsStr,
    dirs: &DirCache,
    stop: &Stop,
    sink: &mut impl Sink,
    batch: usize,
    bounds: Bounds,
) -> Result<()> {
    scan_with(
        root,
        dirs,
        stop,
        sink,
        STATS,
        if batch == 0 { BATCH } else { batch },
        bounds,
    )
}

/// Scans with `stats` threads statting entries, `batch` names at a time,
/// going no further than `bounds`.
fn scan_with(
    root: &OsStr,
    dirs: &DirCache,
    stop: &Stop,
    sink: &mut impl Sink,
    stats: usize,
    batch: usize,
    bounds: Bounds,
) -> Result<()> {
    let stat = match fsys::stat_source_root(root) {
        Ok(stat) => stat,
        Err(error) => {
            sink.problem(b"", error)?;
            return sink.end();
        }
    };
    let root_entry = fsys::entry(0, Vec::new(), &stat, || {
        std::fs::read_link(root).map(|target| target.into_os_string().into_encoded_bytes())
    })?;
    let is_directory = root_entry.kind == Kind::Directory;
    if bounds.avoid.contains(&fsys::key(&stat)) {
        bail!("the Destination's root is the Source's root");
    }
    sink.root(root_entry)?;
    if is_directory {
        match dirs.root() {
            Ok(directory) => {
                let expected = send_tree(directory, stop, sink, stats, batch, bounds)?;
                // Those never come were never gone into, by the lister,
                // which went past them; but not those of a scan cut short.
                stop.check()?;
                for dir in expected.rest() {
                    sink.unlistable(dir)?;
                }
            }
            Err(error) => {
                sink.problem(b"", error)?;
                sink.unlistable(0)?;
            }
        }
    }
    // A scan cut short as the transfer stops is not the manifest's end.
    stop.check()?;
    sink.end()
}

/// What the lister hands the stat threads, in manifest order.
enum Job {
    /// Names of the directory at `path`, open as `directory`: a batch.
    Batch {
        directory: Arc<OwnedFd>,
        path: Arc<Vec<u8>>,
        names: Vec<Vec<u8>>,
        first: bool,
        last: bool,
    },
    /// The directory at `path` could not be read.
    Unreadable { path: Vec<u8>, error: io::Error },
    /// The directory at `path` is one to avoid, and was not gone into.
    Avoided { path: Vec<u8> },
}

/// What the stat threads hand back, in the same order.
enum Found {
    Batch {
        path: Arc<Vec<u8>>,
        entries: Vec<Entry>,
        /// Entries that could not be read.
        problems: Vec<(Vec<u8>, io::Error)>,
        /// Entries listed, and gone before they were looked at.
        vanished: Vec<Vec<u8>>,
        first: bool,
        last: bool,
    },
    Unreadable {
        path: Vec<u8>,
        error: io::Error,
    },
    Avoided {
        path: Vec<u8>,
    },
}

/// Lists the tree under the open root, with the stat threads, and sends it in
/// order: what directories it announced that never came.
fn send_tree(
    root: Arc<OwnedFd>,
    stop: &Stop,
    sink: &mut impl Sink,
    stats: usize,
    batch: usize,
    bounds: Bounds,
) -> Result<Expected> {
    std::thread::scope(|scope| {
        let stages = Stages::new(scope, stop);
        let profiled = Profiled {
            role: "stat",
            work: State::Stat,
            full: State::ToStat,
        };
        let (mut batches, found) = pool::ordered(&stages, stats, WAITING, profiled, stat_batch);
        stages.spawn("scan", move |stop| {
            list_tree(root, &mut batches, batch, bounds, stop);
            Ok(())
        });
        send_found(found, sink)
    })
}

/// A directory waiting to be listed: its name in its parent, and its
/// parent's path, which its siblings share, rather than each a path of its
/// own: a directory may have millions.
struct Waiting {
    parent: Parent,
    name: Vec<u8>,
    parent_path: Arc<Vec<u8>>,
}

/// Where a waiting directory is opened from.
enum Parent {
    /// Its parent, held open.
    Open(Arc<OwnedFd>),
    /// The root, one component at a time, never following a symlink: deeper
    /// than [`MAX_OPEN_PARENTS`], its parent is not held open.
    Root,
}

/// Lists every directory, depth first in name order, handing each one's
/// names to the stat threads in batches of at most `batch`, until done or
/// the stat threads stop. A directory beyond `bounds` is not gone into.
fn list_tree(
    root: Arc<OwnedFd>,
    batches: &mut pool::Submitter<Job>,
    batch: usize,
    bounds: Bounds,
    stop: &Stop,
) {
    // The root's filesystem, to stay on; one the root's cannot be had for
    // is not stayed on.
    #[allow(clippy::unnecessary_cast)]
    let device = bounds
        .one_file_system
        .then(|| {
            rustix::fs::fstat(&*root)
                .ok()
                .map(|stat| stat.st_dev as u64)
        })
        .flatten();
    let mut waiting: Vec<Waiting> = Vec::new();
    let mut next = Some((Arc::clone(&root), Vec::new()));
    loop {
        // Nothing more is gone into once the transfer has stopped: the
        // batches already listed end the scan, which says it was cut short.
        if stop.is_stopped() {
            return;
        }
        let handed = match next.take() {
            Some((directory, path)) => list(directory, path, batches, batch, &mut waiting),
            None => {
                let Some(Waiting {
                    parent,
                    name,
                    parent_path,
                }) = waiting.pop()
                else {
                    return;
                };
                let path = manifest::join(&parent_path, &name);
                drop(parent_path);
                // Its entry was sent, as a mount point is; what is in it is
                // not, which leaves what the Destination has there as it is:
                // it is unlistable, as the lister goes past it.
                if let (Some(device), Parent::Open(parent)) = (device, &parent)
                    && is_elsewhere(parent, &name, device)
                {
                    continue;
                }
                let on = profile::on(&path);
                let opened = profile::wait(State::List, || match &parent {
                    Parent::Open(parent) => fsys::open_directory(parent, &name),
                    Parent::Root => fsys::open_path(&root, &path),
                });
                drop(on);
                drop(parent);
                match opened {
                    Ok(directory) if is_among(&directory, bounds.avoid) => {
                        batches.submit(Job::Avoided { path })
                    }
                    Ok(directory)
                        if device.is_some_and(|device| is_open_elsewhere(&directory, device)) =>
                    {
                        continue;
                    }
                    Ok(directory) => {
                        next = Some((Arc::new(directory), path));
                        continue;
                    }
                    Err(error) => batches.submit(Job::Unreadable { path, error }),
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
    directory: Arc<OwnedFd>,
    path: Vec<u8>,
    batches: &mut pool::Submitter<Job>,
    batch: usize,
    waiting: &mut Vec<Waiting>,
) -> Result<()> {
    let _on = profile::on(&path);
    let listed = match profile::wait(State::List, || fsys::read_entries(&directory)) {
        Ok(listed) => listed,
        Err(error) => return batches.submit(Job::Unreadable { path, error }),
    };
    // Only the directories on the way down are held for their waiting
    // subdirectories, so at most one per level of depth.
    let held = depth(&path) < MAX_OPEN_PARENTS;
    let subdirectories: Vec<Vec<u8>> = listed
        .iter()
        .filter(|entry| is_directory(&directory, entry))
        .map(|entry| entry.name.clone())
        .collect();
    let names: Vec<Vec<u8>> = listed.into_iter().map(|entry| entry.name).collect();
    let path = Arc::new(path);
    // An empty directory is one empty batch.
    let count = names.len().div_ceil(batch).max(1);
    let mut names = names.into_iter();
    for index in 0..count {
        batches.submit(Job::Batch {
            directory: Arc::clone(&directory),
            path: Arc::clone(&path),
            names: names.by_ref().take(batch).collect(),
            first: index == 0,
            last: index + 1 == count,
        })?;
    }
    for name in subdirectories.into_iter().rev() {
        waiting.push(Waiting {
            parent: if held {
                Parent::Open(Arc::clone(&directory))
            } else {
                Parent::Root
            },
            parent_path: Arc::clone(&path),
            name,
        });
    }
    Ok(())
}

/// How many directories down from the root `path` is.
fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        path.iter().filter(|&&byte| byte == b'/').count() + 1
    }
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

/// Stats a batch's names, for a stat thread.
fn stat_batch(job: Job) -> Found {
    match job {
        Job::Batch {
            directory,
            path,
            names,
            first,
            last,
        } => {
            let mut entries = Vec::with_capacity(names.len());
            let mut problems = Vec::new();
            let mut vanished = Vec::new();
            for name in names {
                match describe(&*directory, name.clone()) {
                    Ok(entry) => entries.push(entry),
                    // Gone since it was listed: left out, as rsync does, so
                    // that `sync` deletes what the Destination has of it.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        vanished.push(manifest::join(&path, &name));
                    }
                    // Listed, so it is there: sent as unreadable, for the
                    // Destination to leave what it has as it is.
                    Err(error) => {
                        problems.push((manifest::join(&path, &name), error));
                        entries.push(unreadable(name));
                    }
                }
            }
            Found::Batch {
                path,
                entries,
                problems,
                vanished,
                first,
                last,
            }
        }
        Job::Unreadable { path, error } => Found::Unreadable { path, error },
        Job::Avoided { path } => Found::Avoided { path },
    }
}

/// Whether `name` in `parent` is on another filesystem than `device`, as a
/// stat that mounts nothing says: an automounter's mount point not mounted
/// yet is not mounted to find out.
fn is_elsewhere(parent: &OwnedFd, name: &[u8], device: u64) -> bool {
    #[allow(clippy::unnecessary_cast)]
    fsys::stat(parent, name).is_ok_and(|stat| stat.st_dev as u64 != device)
}

/// Whether an open directory is on another filesystem than `device`.
fn is_open_elsewhere(directory: &OwnedFd, device: u64) -> bool {
    #[allow(clippy::unnecessary_cast)]
    rustix::fs::fstat(directory).is_ok_and(|stat| stat.st_dev as u64 != device)
}

/// Whether an open directory is among `keys`.
fn is_among(directory: &OwnedFd, keys: &[FileKey]) -> bool {
    !keys.is_empty()
        && rustix::fs::fstat(directory).is_ok_and(|stat| keys.contains(&fsys::key(&stat)))
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

/// Sends what the stat threads found, in order, giving IDs as it goes: what
/// directories it announced that never came.
///
/// A directory is sent only if its parent's entry for it, as its stat found,
/// is a directory: the lister went by the listing, and the name may have
/// changed since. The batches of one that is not are left out, and so is
/// everything under it. One whose entry is a directory, and that the lister
/// went past, as it was no directory as listed, or on another filesystem,
/// is unlistable.
fn send_found(found: impl Iterator<Item = Found>, sink: &mut impl Sink) -> Result<Expected> {
    let mut next_id: FileId = 1;
    let mut expected = Expected::default();
    // The directory whose batches are coming, if it is sent, and the
    // directories among its entries so far.
    let mut sending = None;
    let mut subdirectories = VecDeque::new();
    for found in found {
        match found {
            Found::Unreadable { path, error } => {
                if let Some(dir) = arrive(&mut expected, &path, sink)? {
                    sink.problem(&path, error)?;
                    sink.unlistable(dir)?;
                }
            }
            // Gone into, it would have the Source read what the Destination
            // writes, and `sync` delete it as it goes.
            Found::Avoided { path } => {
                if arrive(&mut expected, &path, sink)?.is_some() {
                    bail!(
                        "the Destination's root is inside the Source, at {}/",
                        String::from_utf8_lossy(&path)
                    );
                }
            }
            Found::Batch {
                path,
                mut entries,
                problems,
                vanished,
                first,
                last,
            } => {
                if first {
                    sending = arrive(&mut expected, &path, sink)?;
                    subdirectories = VecDeque::new();
                }
                let Some(dir) = sending else {
                    continue;
                };
                for (problem_path, error) in problems {
                    sink.problem(&problem_path, error)?;
                }
                for gone in vanished {
                    sink.vanished(&gone)?;
                }
                for entry in &mut entries {
                    entry.id = next_id;
                    next_id += 1;
                    if entry.kind == Kind::Directory {
                        subdirectories.push_back((entry.name.clone(), entry.id));
                    }
                }
                sink.entries(Batch {
                    dir,
                    path: &path,
                    entries,
                    first,
                    last,
                })?;
                if last {
                    expected.listed(path.to_vec(), std::mem::take(&mut subdirectories));
                }
            }
        }
    }
    Ok(expected)
}

/// As [`Expected::arrive`], telling `sink` those the lister went past are
/// unlistable.
fn arrive(expected: &mut Expected, path: &[u8], sink: &mut impl Sink) -> Result<Option<FileId>> {
    let mut skipped = Vec::new();
    let arrived = expected.arrive(path, &mut skipped);
    for dir in skipped {
        sink.unlistable(dir)?;
    }
    Ok(arrived)
}

/// The names of a directory's own directories, in order, with their IDs.
type Subdirectories = VecDeque<(Vec<u8>, FileId)>;

/// The directories sent as entries and not listed yet, as the lister goes,
/// depth first, by name: for each directory sent, from the root down to the
/// latest, the names of its own, in order, a name each rather than a path,
/// with their IDs.
#[derive(Default)]
struct Expected {
    /// The root is listed.
    root_listed: bool,
    sent: Vec<(Vec<u8>, Subdirectories)>,
}

impl Expected {
    /// The ID of the directory at `path`, which the lister comes to, if it
    /// was sent as one: then it is taken as come. One its parent's entry
    /// did not send as a directory, as one changed since it was listed, or
    /// one under it, was not, and changes nothing. Those the lister went
    /// past, which never come, go in `skipped`.
    fn arrive(&mut self, path: &[u8], skipped: &mut Vec<FileId>) -> Option<FileId> {
        if path.is_empty() {
            return (!std::mem::replace(&mut self.root_listed, true)).then_some(0);
        }
        let (parent, name) = match path.iter().rposition(|&byte| byte == b'/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => (&b""[..], path),
        };
        let at = self
            .sent
            .iter()
            .rposition(|(directory, _)| directory == parent)?;
        // Those after its parent are done with: the lister went past them.
        for (_, names) in self.sent.drain(at + 1..) {
            skipped.extend(names.into_iter().map(|(_, id)| id));
        }
        let names = &mut self.sent[at].1;
        // Those before it never come, as the lister found them no
        // directories; one after it is still to come.
        while names
            .front()
            .is_some_and(|(next, _)| next.as_slice() < name)
        {
            skipped.push(names.pop_front().expect("one").1);
        }
        if names.front().is_some_and(|(next, _)| next == name) {
            return names.pop_front().map(|(_, id)| id);
        }
        None
    }

    /// The directory at `path` was sent whole, with `subdirectories`.
    fn listed(&mut self, path: Vec<u8>, subdirectories: Subdirectories) {
        self.sent.push((path, subdirectories));
    }

    /// The directories sent and never come: the lister went past them all.
    fn rest(self) -> impl Iterator<Item = FileId> {
        self.sent
            .into_iter()
            .flat_map(|(_, names)| names.into_iter().map(|(_, id)| id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn the_scan_stops_at_a_directory_to_avoid() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/destination/deeper")).unwrap();
        fs::write(root.path().join("a/file"), "x").unwrap();
        let avoid = [fsys::key(
            &fsys::stat_root(root.path().join("a/destination").as_os_str()).unwrap(),
        )];
        let mut sink = Collect::default();
        let error = scan_in_batches(
            root.path().as_os_str(),
            &DirCache::new(root.path().as_os_str(), dirs::KEPT),
            &Stop::new(None),
            &mut sink,
            0,
            Bounds {
                avoid: &avoid,
                ..Bounds::default()
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inside the Source, at a/destination/"),
            "{error}"
        );
        // Nothing of it is in the manifest.
        assert!(
            !sink
                .batches
                .iter()
                .any(|(path, ..)| path.starts_with(b"a/destination"))
        );

        // As the root itself, it is not scanned at all.
        let avoid = [fsys::key(
            &fsys::stat_root(root.path().as_os_str()).unwrap(),
        )];
        let mut sink = Collect::default();
        let error = scan_in_batches(
            root.path().as_os_str(),
            &DirCache::new(root.path().as_os_str(), dirs::KEPT),
            &Stop::new(None),
            &mut sink,
            0,
            Bounds {
                avoid: &avoid,
                ..Bounds::default()
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("is the Source's root"),
            "{error}"
        );
        assert!(sink.records.is_empty());
    }

    /// What a scan sends: the manifest's records, as the Source writes
    /// them, and its batches, its problems and what vanished, as they came.
    #[derive(Default)]
    struct Collect {
        records: Vec<Manifest>,
        /// Each batch's path, IDs, and whether it is its directory's first
        /// and last.
        batches: Vec<(Vec<u8>, Vec<FileId>, bool, bool)>,
        unlistable: Vec<FileId>,
        problems: Vec<Vec<u8>>,
        vanished: Vec<Vec<u8>>,
    }

    impl Sink for Collect {
        fn root(&mut self, entry: Entry) -> Result<()> {
            self.records.push(Manifest::Root(entry));
            Ok(())
        }

        fn entries(&mut self, batch: Batch) -> Result<()> {
            self.batches.push((
                batch.path.to_vec(),
                batch.entries.iter().map(|entry| entry.id).collect(),
                batch.first,
                batch.last,
            ));
            self.records.extend(batch.into_records());
            Ok(())
        }

        fn unlistable(&mut self, dir: FileId) -> Result<()> {
            self.unlistable.push(dir);
            self.records.push(Manifest::Unlistable { dir });
            Ok(())
        }

        fn problem(&mut self, path: &[u8], _error: io::Error) -> Result<()> {
            self.problems.push(path.to_vec());
            self.records.push(Manifest::Problem {
                path: path.to_vec(),
                file: None,
            });
            Ok(())
        }

        fn vanished(&mut self, path: &[u8]) -> Result<()> {
            self.vanished.push(path.to_vec());
            Ok(())
        }

        fn end(&mut self) -> Result<()> {
            self.records.push(Manifest::End);
            Ok(())
        }
    }

    /// Whether `records` pass the Destination's check, whole.
    fn passes_the_check(records: &[Manifest]) {
        let mut check = manifest::Check::default();
        for record in records {
            match record {
                Manifest::Root(entry) => check.root(entry).unwrap(),
                Manifest::In { dir } => drop(check.enter(*dir).unwrap()),
                Manifest::Entry(entry) => drop(check.entry(entry).unwrap()),
                Manifest::Listed { dir } => drop(check.listed(*dir).unwrap()),
                Manifest::Unlistable { dir } => drop(check.unlistable(*dir).unwrap()),
                Manifest::End => check.end().unwrap(),
                Manifest::User { .. } | Manifest::Group { .. } | Manifest::Problem { .. } => {}
            }
        }
        assert!(check.is_ended());
    }

    /// The batches' paths, each once, as their first batches come.
    fn paths(sink: &Collect) -> Vec<&[u8]> {
        sink.batches
            .iter()
            .filter(|(_, _, first, _)| *first)
            .map(|(path, ..)| path.as_slice())
            .collect()
    }

    /// A deep tree is scanned whole, holding at most one directory open per
    /// level down to [`MAX_OPEN_PARENTS`], however deep it goes. Each level
    /// has a sibling still to list, which is what holds its parent open.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_deep_tree_holds_a_bounded_number_of_directories_open() {
        struct Counting {
            root: std::path::PathBuf,
            directories: usize,
            most_open: usize,
        }
        impl Sink for Counting {
            fn root(&mut self, _entry: Entry) -> Result<()> {
                Ok(())
            }
            fn entries(&mut self, batch: Batch) -> Result<()> {
                if batch.first {
                    self.directories += 1;
                }
                self.most_open = self.most_open.max(open_under(&self.root));
                Ok(())
            }
            fn unlistable(&mut self, dir: FileId) -> Result<()> {
                panic!("{dir} unlistable");
            }
            fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()> {
                panic!("{}: {error}", String::from_utf8_lossy(path));
            }
            fn vanished(&mut self, path: &[u8]) -> Result<()> {
                panic!("{} vanished", String::from_utf8_lossy(path));
            }
            fn end(&mut self) -> Result<()> {
                Ok(())
            }
        }
        /// The descriptors open on what is under `root`: the scan's alone,
        /// whatever other tests open meanwhile.
        fn open_under(root: &std::path::Path) -> usize {
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|fd| fs::read_link(fd.path()).ok())
                .filter(|target| target.starts_with(root))
                .count()
        }

        const DEPTH: usize = 600;
        let root = tempfile::tempdir().unwrap();
        let mut level = root.path().to_path_buf();
        for _ in 0..DEPTH {
            fs::create_dir(level.join("z")).unwrap();
            level.push("a");
            fs::create_dir(&level).unwrap();
        }
        let mut sink = Counting {
            root: root.path().canonicalize().unwrap(),
            directories: 0,
            most_open: 0,
        };
        scan(root.path().as_os_str(), &mut sink).unwrap();
        assert_eq!(sink.directories, 2 * DEPTH + 1);
        // Room for the batches at the stat threads, and for those put back
        // in order.
        let most = MAX_OPEN_PARENTS + WAITING + 64;
        assert!(sink.most_open <= most, "{} open", sink.most_open);
        // The count is of the scan's directories, or it would say nothing.
        assert!(sink.most_open > 1, "{} open", sink.most_open);
    }

    #[test]
    fn a_path_is_as_deep_as_its_components() {
        assert_eq!(depth(b""), 0);
        assert_eq!(depth(b"a"), 1);
        assert_eq!(depth(b"a/b/c"), 3);
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
        assert!(sink.problems.is_empty() && sink.unlistable.is_empty());
        assert_eq!(
            sink.batches,
            [
                (b"".to_vec(), vec![1, 2, 3], true, true),
                (b"a".to_vec(), vec![4], true, true),
                (b"b".to_vec(), vec![5, 6], true, true),
                (b"b/inner".to_vec(), vec![], true, true),
            ]
        );
        let records = &sink.records;
        assert!(
            matches!(&records[0], Manifest::Root(entry) if entry.id == 0 && entry.kind == Kind::Directory)
        );
        assert_eq!(records[1], Manifest::In { dir: 0 });
        assert!(matches!(&records[5], Manifest::Listed { dir: 0 }));
        assert!(
            matches!(&records[7], Manifest::Entry(entry) if entry.name == b"file" && entry.size == 4)
        );
        let link = records.iter().find_map(|record| match record {
            Manifest::Entry(entry) if entry.name == b"link" => Some(entry),
            _ => None,
        });
        assert_eq!(
            link.unwrap().kind,
            Kind::Symlink {
                target: b"../c".to_vec()
            }
        );
        assert_eq!(records.last(), Some(&Manifest::End));
        passes_the_check(records);
    }

    /// What a scan sends of a deep tree, in batches, with a directory it
    /// cannot list, passes the Destination's check.
    #[test]
    fn what_a_scan_sends_passes_the_check() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        for path in ["a/x/deep/er", "a/y", "b", "c/z", "a/locked/inside", "a/w"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        for (index, path) in ["f1", "a/f2", "a/x/f3", "a/x/deep/f4", "c/f5"]
            .iter()
            .enumerate()
        {
            fs::write(root.path().join(path), index.to_string()).unwrap();
        }
        let locked = root.path().join("a/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let mut sink = Collect::default();
        let dirs = DirCache::new(root.path().as_os_str(), dirs::KEPT);
        let scanned = scan_in_batches(
            root.path().as_os_str(),
            &dirs,
            &Stop::new(None),
            &mut sink,
            2,
            Bounds::default(),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        scanned.unwrap();
        passes_the_check(&sink.records);
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
        assert_eq!(sink.batches.len(), 1);
        assert!(matches!(
            &sink.records[2],
            Manifest::Entry(entry) if matches!(entry.kind, Kind::Symlink { .. })
        ));
    }

    #[test]
    fn a_file_root_is_one_entry() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        fs::write(&file, "content").unwrap();

        let mut sink = Collect::default();
        scan(file.as_os_str(), &mut sink).unwrap();
        assert!(
            matches!(&sink.records[0], Manifest::Root(entry) if entry.kind == Kind::File && entry.size == 7)
        );
        assert_eq!(sink.records[1..], [Manifest::End]);
    }

    #[test]
    fn a_missing_root_is_a_problem() {
        let root = tempfile::tempdir().unwrap();
        let mut sink = Collect::default();
        scan(root.path().join("missing").as_os_str(), &mut sink).unwrap();
        assert_eq!(sink.problems, [b""]);
        assert!(matches!(
            &sink.records[..],
            [Manifest::Problem { .. }, Manifest::End]
        ));
    }

    /// A tree wide and deep enough that the stat threads work on many
    /// batches at once: 1,000 directories, each with files, some nested ten
    /// deep.
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

    /// Seven files, in batches of three: three batches for the one
    /// directory, one after another, IDs following on, its records between
    /// one `In` and its `Listed`; an empty directory is one empty batch.
    #[test]
    fn a_large_directory_goes_in_batches() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("big")).unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();
        for i in 0..7 {
            fs::write(root.path().join(format!("big/f{i}")), "x").unwrap();
        }
        let mut sink = Collect::default();
        scan_with(
            root.path().as_os_str(),
            &DirCache::new(root.path().as_os_str(), dirs::KEPT),
            &Stop::new(None),
            &mut sink,
            4,
            3,
            Bounds::default(),
        )
        .unwrap();
        assert_eq!(
            sink.batches,
            [
                (b"".to_vec(), vec![1, 2], true, true),
                (b"big".to_vec(), vec![3, 4, 5], true, false),
                (b"big".to_vec(), vec![6, 7, 8], false, false),
                (b"big".to_vec(), vec![9], false, true),
                (b"empty".to_vec(), vec![], true, true),
            ]
        );
        let ins = sink
            .records
            .iter()
            .filter(|record| matches!(record, Manifest::In { dir: 1 }))
            .count();
        assert_eq!(ins, 1);
        passes_the_check(&sink.records);
    }

    #[test]
    fn stat_threads_send_the_same_manifest_as_one() {
        let root = tempfile::tempdir().unwrap();
        big_tree(root.path());
        let scanned = |stats, batch| {
            let mut sink = Collect::default();
            scan_with(
                root.path().as_os_str(),
                &DirCache::new(root.path().as_os_str(), dirs::KEPT),
                &Stop::new(None),
                &mut sink,
                stats,
                batch,
                Bounds::default(),
            )
            .unwrap();
            assert!(sink.problems.is_empty());
            sink.records
        };
        let one = scanned(1, BATCH);
        assert!(one.len() > 1_000, "{} records", one.len());
        for _ in 0..3 {
            assert_eq!(scanned(8, BATCH), one);
        }
        // In small batches, too.
        let small = scanned(1, 2);
        for _ in 0..3 {
            assert_eq!(scanned(8, 2), small);
        }
    }

    /// A name listed, then gone before its stat, has vanished: left out,
    /// and not a problem.
    #[test]
    fn a_name_gone_before_its_stat_has_vanished() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("here"), "x").unwrap();
        let directory = fsys::open_root(root.path().as_os_str()).unwrap();
        let found = stat_batch(Job::Batch {
            directory: Arc::new(directory),
            path: Arc::new(b"d".to_vec()),
            names: vec![b"gone".to_vec(), b"here".to_vec()],
            first: true,
            last: true,
        });
        let Found::Batch {
            entries,
            problems,
            vanished,
            ..
        } = found
        else {
            panic!("not a batch");
        };
        let names: Vec<&[u8]> = entries.iter().map(|entry| entry.name.as_slice()).collect();
        assert_eq!(names, [b"here"]);
        assert!(problems.is_empty());
        assert_eq!(vanished, [b"d/gone"]);
    }

    /// A name that cannot be statted for another reason, here a directory
    /// that may be listed but not searched, is sent as unreadable, for the
    /// Destination to leave what it has as it is.
    #[test]
    fn a_name_that_cannot_be_statted_is_sent_as_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("there"), "x").unwrap();
        let directory = fsys::open_root(root.path().as_os_str()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let found = stat_batch(Job::Batch {
            directory: Arc::new(directory),
            path: Arc::new(b"d".to_vec()),
            names: vec![b"there".to_vec()],
            first: true,
            last: true,
        });
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let Found::Batch {
            entries,
            problems,
            vanished,
            ..
        } = found
        else {
            panic!("not a batch");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, Kind::Unreadable);
        let paths: Vec<&[u8]> = problems.iter().map(|(path, _)| path.as_slice()).collect();
        assert_eq!(paths, [b"d/there"]);
        assert!(vanished.is_empty());
    }

    /// A directory that cannot be read is a problem, and unlistable, and the
    /// scan goes on.
    #[test]
    fn an_unreadable_directory_is_unlistable_and_the_scan_goes_on() {
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
        scan_with(
            root.path().as_os_str(),
            &DirCache::new(root.path().as_os_str(), dirs::KEPT),
            &Stop::new(None),
            &mut sink,
            4,
            BATCH,
            Bounds::default(),
        )
        .unwrap();
        fs::set_permissions(
            root.path().join("a/locked"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(sink.problems, [b"a/locked".to_vec()]);
        assert_eq!(paths(&sink), [&b""[..], b"a", b"b"]);
        // a/ is 1, b/ 2, and a/locked 3.
        assert_eq!(sink.unlistable, [3]);
        passes_the_check(&sink.records);
    }

    /// A name listed as a directory, but no longer one by the time it is
    /// statted: its batches, and everything under it, are left out. One
    /// statted as a directory, which the lister went past, as it was no
    /// directory as listed, is unlistable, once the lister is past it.
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
        let batch = |path: &[u8], entries: Vec<Entry>| Found::Batch {
            path: Arc::new(path.to_vec()),
            entries,
            problems: Vec::new(),
            vanished: Vec::new(),
            first: true,
            last: true,
        };
        let found = vec![
            batch(
                b"",
                vec![entry(b"dir", Kind::Directory), entry(b"was", Kind::File)],
            ),
            batch(b"dir", vec![entry(b"inner", Kind::File)]),
            batch(b"was", vec![entry(b"gone", Kind::Directory)]),
            batch(b"was/gone", vec![]),
            Found::Unreadable {
                path: b"was/other".to_vec(),
                error: io::Error::other("unreadable"),
            },
        ];
        let mut sink = Collect::default();
        let expected = send_found(found.into_iter(), &mut sink).unwrap();
        assert_eq!(paths(&sink), [&b""[..], b"dir"]);
        assert!(sink.problems.is_empty() && sink.unlistable.is_empty());
        assert_eq!(expected.rest().count(), 0);

        // One before a directory, left out with all under it, leaves the
        // directory after it to be sent.
        let found = vec![
            batch(
                b"",
                vec![entry(b"a", Kind::File), entry(b"b", Kind::Directory)],
            ),
            batch(b"a", vec![entry(b"x", Kind::Directory)]),
            batch(b"a/x", vec![]),
            batch(b"b", vec![entry(b"c", Kind::Directory)]),
            batch(b"b/c", vec![]),
            Found::Unreadable {
                path: b"b/d".to_vec(),
                error: io::Error::other("unreadable"),
            },
        ];
        let mut sink = Collect::default();
        let expected = send_found(found.into_iter(), &mut sink).unwrap();
        assert_eq!(paths(&sink), [&b""[..], b"b", b"b/c"]);
        assert!(sink.problems.is_empty() && sink.unlistable.is_empty());
        assert_eq!(expected.rest().count(), 0);

        // Directories as statted, x/ and z/, that the lister never comes
        // to: x/, before y/, which it comes to, at once; z/, after all, at
        // the end.
        let found = vec![
            batch(
                b"",
                vec![
                    entry(b"x", Kind::Directory),
                    entry(b"y", Kind::Directory),
                    entry(b"z", Kind::Directory),
                ],
            ),
            batch(b"y", vec![]),
        ];
        let mut sink = Collect::default();
        let expected = send_found(found.into_iter(), &mut sink).unwrap();
        assert_eq!(paths(&sink), [&b""[..], b"y"]);
        assert_eq!(sink.unlistable, [1]);
        assert_eq!(expected.rest().collect::<Vec<_>>(), [3]);
    }

    /// A scan whose transfer has stopped goes into nothing more, and does
    /// not end the manifest as though it were whole.
    #[test]
    fn a_scan_stopped_goes_no_further_nor_ends_the_manifest() {
        let root = tempfile::tempdir().unwrap();
        big_tree(root.path());
        let directories = |stop: &Stop| {
            let mut sink = Collect::default();
            let scanned = scan_in_batches(
                root.path().as_os_str(),
                &DirCache::new(root.path().as_os_str(), dirs::KEPT),
                stop,
                &mut sink,
                0,
                Bounds::default(),
            );
            let count = paths(&sink).len();
            (scanned, count, sink.records.last().cloned())
        };
        let (scanned, all, last) = directories(&Stop::new(None));
        scanned.unwrap();
        assert_eq!(last, Some(Manifest::End));
        let stopped = Stop::new(None);
        stopped.fail(anyhow::anyhow!("the Destination failed"));
        let (scanned, some, last) = directories(&stopped);
        assert!(scanned.unwrap_err().is::<crate::stop::Stopped>());
        assert!(some < all, "{some} of {all}");
        assert_ne!(last, Some(Manifest::End));
    }
}
