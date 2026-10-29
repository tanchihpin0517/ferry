//! The Source's scan of its tree, which produces the manifest.
//!
//! The manifest has a fixed order: depth first, each directory's entries
//! together and sorted by name, before any of its subdirectories. That order
//! is the order of paths compared component by component.
//!
//! A lister lists directories, one at a time, in that order: it opens a
//! directory, reads its names, hands them to the stat threads, and goes on to
//! the next. Which names are directories, and so where to go next, it learns
//! from the listing itself, without waiting for their stats. A pool of
//! [`STATS`] threads stats the names, a batch of at most [`BATCH`] at a
//! time, in the order they were handed over, and the calling thread sends
//! what they find in that order, giving IDs as it goes. So the filesystem is
//! asked at most one listing and [`STATS`] stats at once, and memory holds
//! only the directories still to visit, and the batches waiting for the stat
//! threads. A directory's names, and its subdirectories still to visit, are
//! held within a bound, past which they are sorted, and kept, on disk; see
//! [`crate::sorted`].
//!
//! Several listers may list at once, each a directory: they take the next
//! from those still to visit, shared, the first of the latest directory's,
//! so that each goes down as far as it can, and they go down together.
//! Their directories' batches then come interleaved, in the order handed
//! over; a directory's all go before any of its subdirectories is visited,
//! so that each directory comes after the batch that announced it. With
//! one, the manifest has the order above.
//!
//! A directory the scan announced, as one among its parent's entries, and
//! does not list, it says is unlistable: one it could not read; one on
//! another filesystem, with `-x`; one the lister found no directory, as one
//! that changed between its listing and its stat. So each directory
//! announced ends, listed or not.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::dirs::{self, DirCache};
use crate::fsys::{self, Listed};
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::FileKey;
use crate::sorted::{self, Keyed, Sorted, Sorter, Spill};
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

/// How many threads stat entries.
pub const STATS: usize = 8;

/// How many names a stat thread stats at a time, of one directory: the
/// scan's, here, and the Destination's, comparing; the Source writes each
/// batch's records at once.
pub const BATCH: usize = 256;

/// How many batches may wait for the stat threads.
const WAITING: usize = 64;

/// How many directories may be held open, for their subdirectories still to
/// be listed: the listers' together. Past that, a directory is opened again
/// from the root, so that a deep tree, or several listers, do not run out
/// of descriptors.
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
    /// Its entries go after another directory's, or none: an `In` goes
    /// before them.
    pub enter: bool,
}

impl Batch<'_> {
    /// The manifest's records of it: an `In` before it, where it is entered,
    /// its entries, and a `Listed` after its last.
    pub fn into_records(self) -> Vec<Manifest> {
        let mut records = Vec::with_capacity(self.entries.len() + 2);
        if self.enter {
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
        &Spill::default(),
        1,
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
/// going no further than `bounds`, with `listers` listing at once. The root
/// is opened from `dirs`, which the readers of what it finds share; its
/// stages run under `stop`.
pub fn scan_in_batches(
    root: &OsStr,
    dirs: &DirCache,
    stop: &Stop,
    sink: &mut impl Sink,
    batch: usize,
    bounds: Bounds,
    listers: usize,
) -> Result<()> {
    scan_with(
        root,
        dirs,
        stop,
        sink,
        STATS,
        if batch == 0 { BATCH } else { batch },
        bounds,
        &Spill::default(),
        listers,
    )
}

/// Scans with `stats` threads statting entries, `batch` names at a time,
/// going no further than `bounds`, a directory's names past `spill`'s
/// limits sorted on disk, `listers` listing at once.
#[allow(clippy::too_many_arguments)]
fn scan_with(
    root: &OsStr,
    dirs: &DirCache,
    stop: &Stop,
    sink: &mut impl Sink,
    stats: usize,
    batch: usize,
    bounds: Bounds,
    spill: &Spill,
    listers: usize,
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
                let listing = Listing {
                    batch,
                    bounds,
                    spill,
                    listers: listers.max(1),
                };
                send_tree(directory, stop, sink, stats, listing)?;
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

/// What the listers hand the stat threads, in manifest order.
enum Job {
    /// Names of the directory at `path`, open as `directory`: a batch; with
    /// each, whether the lister goes into it, as it is a directory as
    /// listed.
    Batch {
        directory: Arc<OwnedFd>,
        path: Arc<Vec<u8>>,
        names: Vec<Vec<u8>>,
        into: Vec<bool>,
        first: bool,
        last: bool,
    },
    /// The directory at `path` could not be read.
    Unreadable { path: Vec<u8>, error: io::Error },
    /// The directory at `path` is one to avoid, and was not gone into.
    Avoided { path: Vec<u8> },
    /// The directory at `path` is on another filesystem, with `-x`, and was
    /// not gone into.
    Elsewhere { path: Vec<u8> },
}

/// What the stat threads hand back, in the same order.
enum Found {
    Batch {
        path: Arc<Vec<u8>>,
        entries: Vec<Entry>,
        /// For each entry, whether the lister goes into it.
        into: Vec<bool>,
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
    Elsewhere {
        path: Vec<u8>,
    },
}

/// How the listers list: in batches of at most `batch` names, going no
/// further than `bounds`, a directory's names past `spill`'s limits sorted
/// on disk, `listers` at once.
#[derive(Clone, Copy)]
struct Listing<'a> {
    batch: usize,
    bounds: Bounds<'a>,
    spill: &'a Spill,
    listers: usize,
}

/// Lists the tree under the open root, with the stat threads, and sends it in
/// order.
fn send_tree(
    root: Arc<OwnedFd>,
    stop: &Stop,
    sink: &mut impl Sink,
    stats: usize,
    listing: Listing,
) -> Result<()> {
    // The root's filesystem, to stay on; one the root's cannot be had for
    // is not stayed on.
    #[allow(clippy::unnecessary_cast)]
    let device = listing
        .bounds
        .one_file_system
        .then(|| {
            rustix::fs::fstat(&*root)
                .ok()
                .map(|stat| stat.st_dev as u64)
        })
        .flatten();
    send_tree_on(root, stop, sink, stats, listing, device)
}

/// As [`send_tree`], staying on `device`, if any.
fn send_tree_on(
    root: Arc<OwnedFd>,
    stop: &Stop,
    sink: &mut impl Sink,
    stats: usize,
    listing: Listing,
    device: Option<u64>,
) -> Result<()> {
    let pending = Pending::new(Arc::clone(&root));
    let next = AtomicU64::new(0);
    std::thread::scope(|scope| {
        let stages = Stages::new(scope, stop);
        let profiled = Profiled {
            role: "stat",
            work: State::Stat,
            full: State::ToStat,
        };
        let (batches, found) = pool::ordered(&stages, stats, WAITING, profiled, stat_batch);
        for _ in 0..listing.listers {
            let mut handing = Handing {
                batches: batches.clone(),
                next: &next,
            };
            let (root, pending) = (Arc::clone(&root), &pending);
            stages.spawn("scan", move |stop| {
                lister(&root, pending, &mut handing, listing, device, stop)
            });
        }
        drop(batches);
        send_found(found, sink, listing.spill)
    })
}

/// Hands the stat threads the jobs of one lister of several, each taking
/// the next place among them as it is handed in, room kept for it first,
/// so that one handed in late never finds the pool full of those after it.
struct Handing<'a> {
    batches: pool::Submitter<Job>,
    next: &'a AtomicU64,
}

impl Handing<'_> {
    fn submit(&mut self, job: Job) -> Result<()> {
        self.batches.reserve()?;
        let sequence = self.next.fetch_add(1, Ordering::AcqRel);
        self.batches.submit_reserved(sequence, job)
    }
}

/// The directories of one parent waiting to be listed: their names, by
/// name, held as the parent's listing was, on disk past the bound; and the
/// parent's path, which they share, rather than each a path of its own: a
/// directory may have millions.
struct Waiting {
    parent: Parent,
    names: Sorted,
    parent_path: Arc<Vec<u8>>,
}

/// Where a waiting directory is opened from.
#[derive(Clone)]
enum Parent {
    /// Its parent, held open.
    Open(Arc<OwnedFd>),
    /// The root, one component at a time, never following a symlink: past
    /// [`MAX_OPEN_PARENTS`] held open, its parent is not.
    Root,
}

/// The directories still to list, which the listers share: the root, then
/// the waiting subdirectories of each directory listed, a stack, so that
/// each lister takes the first of the latest directory's.
struct Pending {
    state: Mutex<Stack>,
    changed: Condvar,
}

struct Stack {
    root: Option<Arc<OwnedFd>>,
    waiting: Vec<Waiting>,
    /// Listers listing a directory, which may add to it.
    busy: usize,
    /// How many of those waiting hold their parent open.
    held: usize,
}

/// A directory to list.
enum Next {
    Root(Arc<OwnedFd>),
    Name {
        parent: Parent,
        parent_path: Arc<Vec<u8>>,
        name: Vec<u8>,
    },
}

/// A directory listed whose subdirectories wait to be listed: it, open,
/// its path, and their names, sorted.
struct Subdirectories {
    directory: Arc<OwnedFd>,
    path: Arc<Vec<u8>>,
    names: Sorted,
}

impl Pending {
    fn new(root: Arc<OwnedFd>) -> Self {
        Self {
            state: Mutex::new(Stack {
                root: Some(root),
                waiting: Vec::new(),
                busy: 0,
                held: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// The next directory to list, taken for a lister, waiting for one while
    /// others list; none once there is none, and none listing, who may add
    /// some, or the scan stopped. Its names that cannot be read back from
    /// disk are the scan's failure.
    fn take(&self, stop: &Stop) -> Result<Option<Next>> {
        let mut state = self.state.lock().expect("not poisoned");
        loop {
            if stop.is_stopped() {
                return Ok(None);
            }
            if let Some(root) = state.root.take() {
                state.busy += 1;
                return Ok(Some(Next::Root(root)));
            }
            if let Some(top) = state.waiting.last_mut() {
                match top.names.next() {
                    Some(name) => {
                        let name = name.context("the subdirectories still to list")?.name;
                        let next = Next::Name {
                            parent: top.parent.clone(),
                            parent_path: Arc::clone(&top.parent_path),
                            name,
                        };
                        state.busy += 1;
                        return Ok(Some(next));
                    }
                    None => {
                        let done = state.waiting.pop().expect("there");
                        if matches!(done.parent, Parent::Open(_)) {
                            state.held -= 1;
                        }
                        continue;
                    }
                }
            }
            if state.busy == 0 {
                self.changed.notify_all();
                return Ok(None);
            }
            state = profile::wait(State::Work, || {
                self.changed.wait_timeout(state, Duration::from_millis(50))
            })
            .expect("not poisoned")
            .0;
        }
    }

    /// A lister is done with a directory: what it listed, should it have
    /// subdirectories to list, waits, held open while few are.
    fn done(&self, listed: Option<Subdirectories>) {
        let mut state = self.state.lock().expect("not poisoned");
        if let Some(Subdirectories {
            directory,
            path,
            names,
        }) = listed
        {
            let parent = if state.held < MAX_OPEN_PARENTS {
                state.held += 1;
                Parent::Open(directory)
            } else {
                Parent::Root
            };
            state.waiting.push(Waiting {
                parent,
                names,
                parent_path: path,
            });
        }
        state.busy -= 1;
        self.changed.notify_all();
    }
}

/// A lister: lists the directories it takes, until none is left, handing
/// each one's names to the stat threads in batches, in order, and its
/// subdirectories back, to be listed in turn. A directory beyond `bounds`
/// is not gone into, but said so. Its failure is names that could not be
/// sorted on disk: the scan's own, as sorting them in memory instead would
/// break the bound.
fn lister(
    root: &Arc<OwnedFd>,
    pending: &Pending,
    batches: &mut Handing,
    listing: Listing,
    device: Option<u64>,
    stop: &Stop,
) -> Result<()> {
    loop {
        // Nothing more is gone into once the transfer has stopped: the
        // batches already listed end the scan, which says it was cut short.
        let Some(next) = pending.take(stop)? else {
            return Ok(());
        };
        let listed = match next {
            Next::Root(directory) => list(directory, Vec::new(), batches, listing),
            Next::Name {
                parent,
                parent_path,
                name,
            } => go_into(root, parent, &parent_path, name, batches, listing, device),
        };
        match listed {
            Ok(listed) => pending.done(listed),
            // The stat threads stopped: the scan failed, and says why.
            Err(error) if error.is::<Stopped>() => {
                pending.done(None);
                return Ok(());
            }
            Err(error) => {
                pending.done(None);
                return Err(error);
            }
        }
    }
}

/// Opens the directory `name` in `parent`, at `parent_path`, and lists it,
/// unless it is beyond `bounds`, which the stat threads are told.
fn go_into(
    root: &Arc<OwnedFd>,
    parent: Parent,
    parent_path: &[u8],
    name: Vec<u8>,
    batches: &mut Handing,
    listing: Listing,
    device: Option<u64>,
) -> Result<Option<Subdirectories>> {
    let path = manifest::join(parent_path, &name);
    // Its entry was sent, as a mount point is; what is in it is not, which
    // leaves what the Destination has there as it is: it is unlistable.
    if let (Some(device), Parent::Open(parent)) = (device, &parent)
        && is_elsewhere(parent, &name, device)
    {
        batches.submit(Job::Elsewhere { path })?;
        return Ok(None);
    }
    let on = profile::on(&path);
    let opened = profile::wait(State::List, || match &parent {
        Parent::Open(parent) => fsys::open_directory(parent, &name),
        Parent::Root => fsys::open_path(root, &path),
    });
    drop(on);
    drop(parent);
    match opened {
        Ok(directory) if is_among(&directory, listing.bounds.avoid) => {
            batches.submit(Job::Avoided { path })?;
            Ok(None)
        }
        Ok(directory) if device.is_some_and(|device| is_open_elsewhere(&directory, device)) => {
            batches.submit(Job::Elsewhere { path })?;
            Ok(None)
        }
        Ok(directory) => list(Arc::new(directory), path, batches, listing),
        Err(error) => {
            batches.submit(Job::Unreadable { path, error })?;
            Ok(None)
        }
    }
}

/// Lists the open directory at `path`, hands its names to the stat threads,
/// in order: its subdirectories, to be listed in turn. Names past the bound
/// are sorted on disk, and the subdirectories kept there; one that cannot
/// be is the scan's failure.
fn list(
    directory: Arc<OwnedFd>,
    path: Vec<u8>,
    batches: &mut Handing,
    listing: Listing,
) -> Result<Option<Subdirectories>> {
    let Listing { batch, spill, .. } = listing;
    let _on = profile::on(&path);
    let listing = |path: &[u8]| {
        format!(
            "list {}",
            String::from_utf8_lossy(&crate::source::display(path, true))
        )
    };
    let listed = match profile::wait(State::List, || {
        sorted::list(&*directory, spill.limits, &spill.temp)
    }) {
        Ok(listed) => listed,
        Err(error) if sorted::is_temp_failure(&error) => {
            return Err(error).with_context(|| listing(&path));
        }
        Err(error) => {
            batches.submit(Job::Unreadable { path, error })?;
            return Ok(None);
        }
    };
    let mut subdirectories = Sorter::new(spill.limits, &spill.temp);
    let mut any = false;
    let path = Arc::new(path);
    let failed = |error: io::Error| anyhow::Error::from(error).context(listing(&path));
    // Each batch goes once the next name comes, so that the last is known
    // for the last; an empty directory is one empty batch.
    let mut names = Vec::with_capacity(batch.min(BATCH));
    let mut into = Vec::with_capacity(batch.min(BATCH));
    let mut first = true;
    for listed in listed {
        let listed = listed.map_err(failed)?;
        let is_directory = is_directory(&directory, &listed);
        if is_directory {
            subdirectories
                .push(&listed.name, listed.file_type)
                .map_err(failed)?;
            any = true;
        }
        if names.len() >= batch {
            batches.submit(Job::Batch {
                directory: Arc::clone(&directory),
                path: Arc::clone(&path),
                names: std::mem::replace(&mut names, Vec::with_capacity(batch.min(BATCH))),
                into: std::mem::replace(&mut into, Vec::with_capacity(batch.min(BATCH))),
                first,
                last: false,
            })?;
            first = false;
        }
        names.push(listed.name);
        into.push(is_directory);
    }
    batches.submit(Job::Batch {
        directory: Arc::clone(&directory),
        path: Arc::clone(&path),
        names,
        into,
        first,
        last: true,
    })?;
    if !any {
        return Ok(None);
    }
    let names = subdirectories.finish().map_err(failed)?;
    Ok(Some(Subdirectories {
        directory,
        path,
        names,
    }))
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
            into: listed_into,
            first,
            last,
        } => {
            let mut entries = Vec::with_capacity(names.len());
            let mut into = Vec::with_capacity(names.len());
            let mut problems = Vec::new();
            let mut vanished = Vec::new();
            for (name, goes_into) in names.into_iter().zip(listed_into) {
                match describe(&*directory, name.clone()) {
                    Ok(entry) => entries.push(entry),
                    // Gone since it was listed: left out, as rsync does, so
                    // that `sync` deletes what the Destination has of it.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        vanished.push(manifest::join(&path, &name));
                        continue;
                    }
                    // Listed, so it is there: sent as unreadable, for the
                    // Destination to leave what it has as it is.
                    Err(error) => {
                        problems.push((manifest::join(&path, &name), error));
                        entries.push(unreadable(name));
                    }
                }
                into.push(goes_into);
            }
            Found::Batch {
                path,
                entries,
                into,
                problems,
                vanished,
                first,
                last,
            }
        }
        Job::Unreadable { path, error } => Found::Unreadable { path, error },
        Job::Avoided { path } => Found::Avoided { path },
        Job::Elsewhere { path } => Found::Elsewhere { path },
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

/// Sends what the stat threads found, in order, giving IDs as it goes.
///
/// A directory is sent only if its parent's entry for it, as its stat found,
/// is a directory: the lister went by the listing, and the name may have
/// changed since. The batches of one that is not are left out, and so is
/// everything under it. One whose entry is a directory, and that the lister
/// does not go into, as it was no directory as listed, or as it is on
/// another filesystem, or cannot be read, is unlistable: the first, once
/// its parent's last batch is sent, as the lister says so with its batch;
/// the others as the lister says it went past them.
fn send_found(
    found: impl Iterator<Item = Found>,
    sink: &mut impl Sink,
    spill: &Spill,
) -> Result<()> {
    let mut next_id: FileId = 1;
    let mut sent = Sent::default();
    // The directories whose batches are coming, those sent, and the
    // directories among their entries so far, as the lister goes into
    // them, and not.
    let mut coming: HashMap<Arc<Vec<u8>>, Option<Coming>> = HashMap::new();
    // The directory whose entries were sent last.
    let mut previous = None;
    for found in found {
        match found {
            Found::Unreadable { path, error } => {
                if let Some(dir) = sent.arrive(&path)? {
                    sink.problem(&path, error)?;
                    sink.unlistable(dir)?;
                }
            }
            Found::Elsewhere { path } => {
                if let Some(dir) = sent.arrive(&path)? {
                    sink.unlistable(dir)?;
                }
            }
            // Gone into, it would have the Source read what the Destination
            // writes, and `sync` delete it as it goes.
            Found::Avoided { path } => {
                if sent.arrive(&path)?.is_some() {
                    bail!(
                        "the Destination's root is inside the Source, at {}/",
                        String::from_utf8_lossy(&path)
                    );
                }
            }
            Found::Batch {
                path,
                mut entries,
                into,
                problems,
                vanished,
                first,
                last,
            } => {
                if first {
                    let coming_now = sent.arrive(&path)?.map(|dir| Coming {
                        dir,
                        own: Keyed::new(spill),
                        gone_past: Vec::new(),
                    });
                    coming.insert(Arc::clone(&path), coming_now);
                }
                let Some(Coming {
                    dir,
                    own,
                    gone_past,
                }) = coming.get_mut(&path).and_then(Option::as_mut)
                else {
                    if last {
                        coming.remove(&path);
                    }
                    continue;
                };
                for (problem_path, error) in problems {
                    sink.problem(&problem_path, error)?;
                }
                for gone in vanished {
                    sink.vanished(&gone)?;
                }
                for (entry, &goes_into) in entries.iter_mut().zip(&into) {
                    entry.id = next_id;
                    next_id += 1;
                    if entry.kind == Kind::Directory {
                        if goes_into {
                            own.push(&entry.name, &entry.id.to_be_bytes())
                                .context("keep the directories sent")?;
                        } else {
                            gone_past.push(entry.id);
                        }
                    }
                }
                let dir = *dir;
                sink.entries(Batch {
                    dir,
                    path: &path,
                    entries,
                    first,
                    last,
                    enter: previous != Some(dir),
                })?;
                previous = Some(dir);
                if last {
                    let Some(Some(Coming { own, gone_past, .. })) = coming.remove(&path) else {
                        unreachable!("coming");
                    };
                    for id in gone_past {
                        sink.unlistable(id)?;
                    }
                    sent.listed(path.to_vec(), own);
                    // A directory's last batch ends it: the next is entered.
                    previous = None;
                }
            }
        }
    }
    Ok(())
}

/// A directory whose batches are coming, and are sent: its ID; the
/// directories among its entries so far that the lister goes into, by name,
/// with their IDs, held within the bound, past it on disk; and those it
/// goes past, unlistable once its last batch is sent.
struct Coming {
    dir: FileId,
    own: Keyed,
    gone_past: Vec<FileId>,
}

/// The directories sent as entries, that the lister goes into, and that
/// have not come yet: for each directory sent whole, those among its own,
/// by name, with their IDs, held within the bound, past it on disk, and how
/// many have not come, until none.
#[derive(Default)]
struct Sent {
    /// The root is listed.
    root_listed: bool,
    of: HashMap<Vec<u8>, Own>,
}

struct Own {
    names: Keyed,
    left: usize,
}

/// An ID, as a [`Keyed`]'s value holds it.
fn id_of(value: &[u8]) -> FileId {
    FileId::from_be_bytes(value.try_into().expect("an ID's eight bytes"))
}

impl Sent {
    /// The ID of the directory at `path`, which the lister comes to, if its
    /// parent sent it as one: then it is taken as come. One its parent's
    /// entry did not send as a directory, as one changed since it was
    /// listed, or one under it, was not, and changes nothing.
    fn arrive(&mut self, path: &[u8]) -> Result<Option<FileId>> {
        if path.is_empty() {
            return Ok((!std::mem::replace(&mut self.root_listed, true)).then_some(0));
        }
        let (parent, name) = match path.iter().rposition(|&byte| byte == b'/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => (&b""[..], path),
        };
        let Some(own) = self.of.get_mut(parent) else {
            return Ok(None);
        };
        let Some(id) = own.names.get(name).context("the directories sent")? else {
            return Ok(None);
        };
        own.left -= 1;
        if own.left == 0 {
            self.of.remove(parent);
        }
        Ok(Some(id_of(&id)))
    }

    /// The directory at `path` was sent whole, with `own`, the directories
    /// in it that the lister goes into.
    fn listed(&mut self, path: Vec<u8>, own: Keyed) {
        if !own.is_empty() {
            let left = own.len();
            self.of.insert(path, Own { names: own, left });
        }
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
            1,
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
            1,
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
            1,
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
            &Spill::default(),
            1,
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
                &Spill::default(),
                1,
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

    /// A directory's names past the bound are sorted on disk, and its
    /// subdirectories still to list kept there: the manifest is the same,
    /// and nothing is left in the temporary directory.
    #[test]
    fn a_directory_past_the_bound_sends_the_same_manifest_through_the_disk() {
        let root = tempfile::tempdir().unwrap();
        big_tree(root.path());
        // One directory of many names, half of them directories.
        let many = root.path().join("many");
        fs::create_dir(&many).unwrap();
        for index in 0..400 {
            if index % 2 == 0 {
                fs::create_dir(many.join(format!("dir-{index:04}"))).unwrap();
                fs::write(many.join(format!("dir-{index:04}/in")), "z").unwrap();
            } else {
                fs::write(many.join(format!("file-{index:04}")), "w").unwrap();
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let scanned = |spill: &Spill, batch| {
            let mut sink = Collect::default();
            scan_with(
                root.path().as_os_str(),
                &DirCache::new(root.path().as_os_str(), dirs::KEPT),
                &Stop::new(None),
                &mut sink,
                4,
                batch,
                Bounds::default(),
                spill,
                1,
            )
            .unwrap();
            assert!(sink.problems.is_empty());
            sink.records
        };
        let on_disk = Spill {
            limits: sorted::Limits {
                held: 2_000,
                fan_in: 3,
            },
            temp: temp.path().to_owned(),
        };
        for batch in [BATCH, 7] {
            let in_memory = scanned(&Spill::default(), batch);
            assert_eq!(scanned(&on_disk, batch), in_memory, "batch {batch}");
            passes_the_check(&in_memory);
        }
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    /// Names that cannot be sorted on disk fail the scan, naming the
    /// directory, the temporary one, and TMPDIR, rather than be sorted in
    /// memory, or the directory be taken for one that cannot be listed.
    #[test]
    fn names_that_cannot_go_to_disk_fail_the_scan() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("many")).unwrap();
        for index in 0..50 {
            fs::write(root.path().join(format!("many/file-{index:02}")), "").unwrap();
        }
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut sink = Collect::default();
        let stop = Stop::new(None);
        let stopped = scan_with(
            root.path().as_os_str(),
            &DirCache::new(root.path().as_os_str(), dirs::KEPT),
            &stop,
            &mut sink,
            4,
            BATCH,
            Bounds::default(),
            &Spill {
                limits: sorted::Limits {
                    held: 500,
                    fan_in: 3,
                },
                temp: missing.clone(),
            },
            1,
        )
        .unwrap_err();
        // The scan's stage failed, and stopped this side: its failure is
        // the cause.
        assert!(stopped.is::<Stopped>(), "{stopped:#}");
        let said = format!("{:#}", stop.result().unwrap_err());
        assert!(said.contains("list many/"), "{said}");
        assert!(said.contains(&missing.display().to_string()), "{said}");
        assert!(said.contains("TMPDIR"), "{said}");
        assert!(sink.unlistable.is_empty());
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
            into: vec![false, false],
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
            into: vec![false],
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
            &Spill::default(),
            1,
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
    /// statted as a directory, which the lister does not go into, as it was
    /// no directory as listed, is unlistable, once its parent's last batch
    /// is sent.
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
        // With whether the lister goes into each entry, as listed.
        let listed = |path: &[u8], entries: Vec<(Entry, bool)>| {
            let (entries, into) = entries.into_iter().unzip();
            Found::Batch {
                path: Arc::new(path.to_vec()),
                entries,
                into,
                problems: Vec::new(),
                vanished: Vec::new(),
                first: true,
                last: true,
            }
        };
        let batch = |path: &[u8], entries: Vec<Entry>| {
            listed(
                path,
                entries.into_iter().map(|entry| (entry, true)).collect(),
            )
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
        send_found(found.into_iter(), &mut sink, &Spill::default()).unwrap();
        assert_eq!(paths(&sink), [&b""[..], b"dir"]);
        assert!(sink.problems.is_empty() && sink.unlistable.is_empty());

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
        send_found(found.into_iter(), &mut sink, &Spill::default()).unwrap();
        assert_eq!(paths(&sink), [&b""[..], b"b", b"b/c"]);
        assert!(sink.problems.is_empty() && sink.unlistable.is_empty());

        // Directories as statted, x/ and z/, that the lister does not go
        // into, as they were no directories as listed: unlistable once the
        // root's last batch is sent, before y/ comes. One elsewhere, w/, as
        // the lister comes to it.
        let found = vec![
            listed(
                b"",
                vec![
                    (entry(b"w", Kind::Directory), true),
                    (entry(b"x", Kind::Directory), false),
                    (entry(b"y", Kind::Directory), true),
                    (entry(b"z", Kind::Directory), false),
                ],
            ),
            Found::Elsewhere {
                path: b"w".to_vec(),
            },
            batch(b"y", vec![]),
        ];
        let mut sink = Collect::default();
        sink.root(entry(b"", Kind::Directory)).unwrap();
        send_found(found.into_iter(), &mut sink, &Spill::default()).unwrap();
        sink.end().unwrap();
        assert_eq!(paths(&sink), [&b""[..], b"y"]);
        assert_eq!(sink.unlistable, [2, 4, 1]);
        passes_the_check(&sink.records);
    }

    /// Each entry's path and kind, and the paths of the directories said
    /// unlistable.
    type Held = (Vec<(Vec<u8>, Kind)>, Vec<Vec<u8>>);

    /// What a manifest holds, whatever its order.
    fn held(records: &[Manifest]) -> Held {
        let mut paths: std::collections::HashMap<FileId, Vec<u8>> =
            std::collections::HashMap::from([(0, Vec::new())]);
        let (mut entries, mut unlistable, mut current) = (Vec::new(), Vec::new(), 0);
        for record in records {
            match record {
                Manifest::In { dir } => current = *dir,
                Manifest::Entry(entry) => {
                    let path = manifest::join(&paths[&current], &entry.name);
                    if entry.kind == Kind::Directory {
                        paths.insert(entry.id, path.clone());
                    }
                    entries.push((path, entry.kind.clone()));
                }
                Manifest::Unlistable { dir } => unlistable.push(paths[dir].clone()),
                _ => {}
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        unlistable.sort();
        (entries, unlistable)
    }

    /// Several listers, statting a few names at a time, send what one does,
    /// interleaved, in an order the Destination's check takes, IDs in turn;
    /// what cannot be read is unlistable, and so is what is on another
    /// filesystem, as one does.
    #[test]
    fn several_listers_send_what_one_does() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        big_tree(root.path());
        fs::create_dir_all(root.path().join("wide-007/locked/inner")).unwrap();
        let locked = !rustix::process::geteuid().is_root();
        if locked {
            fs::set_permissions(
                root.path().join("wide-007/locked"),
                fs::Permissions::from_mode(0o000),
            )
            .unwrap();
        }
        let scanned = |listers: usize, bounds: Bounds| {
            let mut sink = Collect::default();
            scan_with(
                root.path().as_os_str(),
                &DirCache::new(root.path().as_os_str(), dirs::KEPT),
                &Stop::new(None),
                &mut sink,
                4,
                7,
                bounds,
                &Spill::default(),
                listers,
            )
            .unwrap();
            passes_the_check(&sink.records);
            let ids: Vec<FileId> = sink
                .records
                .iter()
                .filter_map(|record| match record {
                    Manifest::Entry(entry) => Some(entry.id),
                    _ => None,
                })
                .collect();
            assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 1));
            sink
        };
        let one = scanned(1, Bounds::default());
        let (entries, unlistable) = held(&one.records);
        assert!(entries.len() > 1_000);
        if locked {
            assert_eq!(unlistable, [b"wide-007/locked".to_vec()]);
        }
        for listers in [2, 4, 16] {
            let several = scanned(listers, Bounds::default());
            assert_eq!(
                held(&several.records),
                (entries.clone(), unlistable.clone())
            );
            assert_eq!(several.problems, one.problems);
        }
        // With -x, as a test can make no other filesystem without root, the
        // one to stay on is another, so that every directory but the root
        // is elsewhere: sent, and unlistable.
        let elsewhere = |listers| {
            let mut sink = Collect::default();
            let dirs = DirCache::new(root.path().as_os_str(), dirs::KEPT);
            let directory = dirs.root().unwrap();
            let listing = Listing {
                batch: 7,
                bounds: Bounds::default(),
                spill: &Spill::default(),
                listers,
            };
            sink.root(Entry {
                kind: Kind::Directory,
                ..unreadable(Vec::new())
            })
            .unwrap();
            // A device no directory here is on.
            #[allow(clippy::unnecessary_cast)]
            let device = rustix::fs::fstat(&*directory).unwrap().st_dev as u64 + 1;
            send_tree_on(
                directory,
                &Stop::new(None),
                &mut sink,
                4,
                listing,
                Some(device),
            )
            .unwrap();
            sink.end().unwrap();
            passes_the_check(&sink.records);
            held(&sink.records)
        };
        let (_, one_elsewhere) = elsewhere(1);
        assert!(one_elsewhere.len() > 100, "{}", one_elsewhere.len());
        assert_eq!(elsewhere(4).1, one_elsewhere);
        if locked {
            fs::set_permissions(
                root.path().join("wide-007/locked"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
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
                1,
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
