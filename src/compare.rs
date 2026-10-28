//! The Destination's side of the comparison: what each manifest entry needs.
//!
//! The Destination scans its own tree as the manifest comes, in the same
//! order, so that it compares the two a directory at a time, without either
//! side whole; see [`Tree`]. It opens each directory the manifest names once,
//! by its ID, lists it once, and goes no further into what the Source does
//! not have.
//!
//! For each entry the Destination looks at its own copy, at the same path under
//! its root, without following symlinks. A file's content is unchanged if it is
//! a file of the same size and mtime, as in rsync's quick check; other kinds'
//! are unchanged if they are of the same kind and, for symlinks and devices,
//! point to the same place. An entry whose content is unchanged may still need
//! its permissions, owner, or mtime set.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use rustix::fs::{FileType, Stat};
use rustix::io::Errno;

use crate::fsys::{self, Listed};
use crate::manifest::{Entry, FileId, Kind, Place, Timestamp};
use crate::owners::Ownership;
use crate::profile::{self, State};
use crate::protocol::FileKey;

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

/// Entries the Destination has that the Source does not: names, and whether
/// each is a directory.
pub type Extras = Vec<(Vec<u8>, bool)>;

/// What the Destination's scan knows of a batch of a directory's entries,
/// for them to be compared: the directory, open, and which entries it has.
pub struct Prepared {
    /// The directory, open; `None` if it is missing, so that everything in
    /// it is needed; an error if it could not be opened.
    pub directory: io::Result<Option<Arc<OwnedFd>>>,
    /// For each entry of the batch, whether the directory has it, as far as
    /// its listing says; every entry, if it could not be listed.
    pub present: Vec<bool>,
    /// For `sync`, on a directory's last batch: what it has that the Source
    /// does not.
    pub extras: Option<io::Result<Extras>>,
    /// For `sync`, on a directory's last batch: the names in it, in any
    /// batch, the Source could not read, whose aliases are not deleted.
    pub unreadable: Vec<Vec<u8>>,
    /// On its first batch: its permissions were changed, to work in it, and
    /// are to be set again; what they were, should it not be finished.
    pub let_in: Option<u32>,
    /// The directory is the Source's root: it was not gone into, and the
    /// transfer is to stop.
    pub refused: bool,
}

/// The Destination's tree, scanned as the manifest comes: the directories
/// whose entries came, by their IDs, open, until the manifest says they have
/// gone, with every directory in them. A directory's parent, which announced
/// it, is always among them as its entries come.
pub struct Tree {
    root: Vec<u8>,
    delete: bool,
    /// Whether to let this process into the directories it owns, to work in
    /// them; not to look, as in a dry run.
    let_in: bool,
    /// The Source's root, when on the same kernel: never gone into.
    source_root: Option<FileKey>,
    open: HashMap<FileId, Frame>,
}

/// A directory of the Destination's, as far as the scan has gone into it.
struct Frame {
    directory: Opened,
    /// Its entries as listed, until its last batch; `None` if it could not
    /// be listed, or is missing.
    listed: Option<Vec<Listed>>,
    /// How far into `listed` its batches have come.
    position: usize,
    /// The names that are directories, or may be, as listed: those its
    /// subdirectories may be opened by.
    subdirectories: HashSet<Vec<u8>>,
    /// Why it could not be listed.
    unlisted: Option<(io::ErrorKind, String)>,
    /// For `sync`: what it has that the Source does not, so far.
    extras: Extras,
    /// For `sync`: the names in it the Source could not read, so far.
    unreadable: Vec<Vec<u8>>,
    /// Its permissions were changed to work in it, from these, not yet said
    /// so.
    let_in: Option<u32>,
    /// It is the Source's root, and was not gone into.
    refused: bool,
}

enum Opened {
    Missing,
    Open(Arc<OwnedFd>),
    /// It could not be opened, though it may be there.
    Failed(io::ErrorKind, String),
}

/// An error kept, to be given again for each batch.
fn kept(error: &io::Error) -> (io::ErrorKind, String) {
    (error.kind(), error.to_string())
}

fn again((kind, message): &(io::ErrorKind, String)) -> io::Error {
    io::Error::new(*kind, message.clone())
}

impl Tree {
    /// The tree at `root`, keeping what the Source does not have if
    /// `delete`. With `let_in`, a directory this process owns, but may not
    /// read, write, or go into, is given read, write, and search for its
    /// owner when it is opened, as rsync does, to work in it; its first
    /// batch says so, for its permissions to be set again. A directory that
    /// is `source_root` is not gone into, nor let into: its batches say so.
    pub fn new(root: Vec<u8>, delete: bool, let_in: bool, source_root: Option<FileKey>) -> Self {
        Self {
            root,
            delete,
            let_in,
            source_root,
            open: HashMap::new(),
        }
    }

    /// Prepares a batch of the directory `dir` at `place`, the manifest's
    /// next: on its first, opens and lists the directory; then sets out
    /// which of `entries` it has, and, on its `last`, what it has besides.
    pub fn batch(&mut self, dir: FileId, place: &Place, entries: &[Entry], last: bool) -> Prepared {
        if !self.open.contains_key(&dir) {
            let frame = self.enter(place);
            self.open.insert(dir, frame);
        }
        let delete = self.delete;
        let frame = self.open.get_mut(&dir).expect("entered");
        if delete {
            frame.unreadable.extend(
                entries
                    .iter()
                    .filter(|entry| entry.kind == Kind::Unreadable)
                    .map(|entry| entry.name.clone()),
            );
        }
        let directory = match &frame.directory {
            Opened::Missing => Ok(None),
            Opened::Open(directory) => Ok(Some(Arc::clone(directory))),
            Opened::Failed(kind, message) => Err(io::Error::new(*kind, message.clone())),
        };
        let present = match (&frame.directory, &frame.listed) {
            (Opened::Open(directory), Some(listed)) => {
                let mut present = Vec::with_capacity(entries.len());
                for entry in entries {
                    // What comes before the entry, the Source does not have.
                    while frame.position < listed.len() && listed[frame.position].name < entry.name
                    {
                        if delete {
                            frame.extras.push(extra(directory, &listed[frame.position]));
                        }
                        frame.position += 1;
                    }
                    let has =
                        frame.position < listed.len() && listed[frame.position].name == entry.name;
                    if has {
                        frame.position += 1;
                    }
                    present.push(has);
                }
                present
            }
            (Opened::Open(_), None) => vec![true; entries.len()],
            (Opened::Missing | Opened::Failed(..), _) => vec![false; entries.len()],
        };
        let mut extras = None;
        let mut unreadable = Vec::new();
        if last {
            if let (Opened::Open(directory), Some(listed)) = (&frame.directory, &frame.listed)
                && delete
            {
                for rest in &listed[frame.position..] {
                    frame.extras.push(extra(directory, rest));
                }
            }
            if delete {
                extras = Some(match (&frame.directory, &frame.unlisted) {
                    (Opened::Failed(kind, message), _) => {
                        Err(io::Error::new(*kind, message.clone()))
                    }
                    (_, Some(unlisted)) => Err(again(unlisted)),
                    _ => Ok(std::mem::take(&mut frame.extras)),
                });
                unreadable = std::mem::take(&mut frame.unreadable);
            }
            // Only its subdirectories' names are needed from here on.
            frame.listed = None;
        }
        Prepared {
            directory,
            present,
            extras,
            unreadable,
            let_in: std::mem::take(&mut frame.let_in),
            refused: frame.refused,
        }
    }

    /// The directories `gone`, with every directory in them, are done
    /// with: none is opened from them any more.
    pub fn gone(&mut self, gone: &[FileId]) {
        for dir in gone {
            self.open.remove(dir);
        }
    }

    /// Opens and lists the directory at `place`, from its parent.
    fn enter(&self, place: &Place) -> Frame {
        let path = &place.path[..];
        let _on = profile::on(path);
        let let_in = self.let_in;
        let mut was_let_in = None;
        let mut opening = |opened: io::Result<(OwnedFd, Option<u32>)>| {
            opened.map(|(directory, changed)| {
                was_let_in = changed;
                directory
            })
        };
        // The Source's root, here, is never gone into: looked at before it is
        // opened, so that it is not let into either.
        let source_root = self.source_root;
        let is_source_root = |stat: io::Result<Stat>| {
            source_root.is_some_and(|key| stat.is_ok_and(|stat| fsys::key(&stat) == key))
        };
        let mut refused = false;
        let opened = if path.is_empty() {
            let root = OsStr::from_bytes(&self.root);
            if is_source_root(fsys::stat_root(root)) {
                refused = true;
                Opened::Missing
            } else if let_in {
                open(|| opening(fsys::open_root_to_work_in(root)))
            } else {
                open(|| fsys::open_root(root))
            }
        } else {
            let name = match path.iter().rposition(|&byte| byte == b'/') {
                Some(slash) => &path[slash + 1..],
                None => path,
            };
            match place.parent.and_then(|parent| self.open.get(&parent)) {
                None => Opened::Missing,
                Some(parent) => match &parent.directory {
                    Opened::Missing => Opened::Missing,
                    Opened::Failed(kind, message) => Opened::Failed(*kind, message.clone()),
                    // Listed as something else, or not at all: it is missing.
                    Opened::Open(_)
                        if parent.unlisted.is_none() && !parent.subdirectories.contains(name) =>
                    {
                        Opened::Missing
                    }
                    Opened::Open(directory) if is_source_root(fsys::stat(&**directory, name)) => {
                        refused = true;
                        Opened::Missing
                    }
                    Opened::Open(directory) if let_in => {
                        open(|| opening(fsys::open_directory_to_work_in(&**directory, name)))
                    }
                    Opened::Open(directory) => open(|| fsys::open_directory(&**directory, name)),
                },
            }
        };
        let mut frame = Frame {
            directory: opened,
            listed: None,
            position: 0,
            subdirectories: HashSet::new(),
            unlisted: None,
            extras: Vec::new(),
            unreadable: Vec::new(),
            let_in: was_let_in,
            refused,
        };
        if let Opened::Open(directory) = &frame.directory {
            match profile::wait(State::List, || fsys::read_entries(&**directory)) {
                Ok(listed) => {
                    frame.subdirectories = listed
                        .iter()
                        .filter(|entry| entry.may_be_directory())
                        .map(|entry| entry.name.clone())
                        .collect();
                    frame.listed = Some(listed);
                }
                Err(error) => frame.unlisted = Some(kept(&error)),
            }
        }
        frame
    }
}

/// Opens a directory: missing if it is not there, or is not a directory.
fn open(opening: impl FnOnce() -> io::Result<OwnedFd>) -> Opened {
    match profile::wait(State::List, opening) {
        Ok(directory) => Opened::Open(Arc::new(directory)),
        Err(error) if is_missing(&error) => Opened::Missing,
        Err(error) => Opened::Failed(error.kind(), error.to_string()),
    }
}

/// An entry the Source does not have, and whether it is a directory: as its
/// listing says, or, where that does not say, as a stat of it does.
fn extra(directory: &OwnedFd, listed: &Listed) -> (Vec<u8>, bool) {
    let is_directory = match listed.file_type {
        FileType::Unknown => fsys::stat(directory, &listed.name)
            .is_ok_and(|stat| fsys::file_type(&stat) == FileType::Directory),
        file_type => file_type == FileType::Directory,
    };
    (listed.name.clone(), is_directory)
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
        OsStr::from_bytes(&self.root)
    }

    /// What the root needs. A root that is a directory is reached through a
    /// symlink, if its path is one, as the user named it; any other is the
    /// symlink itself, as rsync takes it, which is then replaced, not
    /// written through.
    pub fn root(&self, entry: &Entry, ownership: &Ownership) -> io::Result<Verdict> {
        let stat = if entry.kind == Kind::Directory {
            fsys::stat_root(self.root_path())
        } else {
            fsys::lstat_root(self.root_path())
        };
        match stat {
            Ok(stat) => self.verdict(entry, &stat, ownership, || {
                std::fs::read_link(self.root_path())
                    .map(|target| target.into_os_string().into_encoded_bytes())
            }),
            Err(error) if is_missing(&error) => Ok(Verdict::Content),
            Err(error) => Err(error),
        }
    }

    /// What each entry of a part needs, in order, as [`Tree::part`] prepared
    /// it: one the Source could not read, nothing, whatever the directory has
    /// under its name; one the directory does not have, everything, unlooked
    /// at; the rest are statted. One that could not be compared fails alone,
    /// as do all of a directory that could not be opened, but the Source's
    /// unreadable ones: each is left as it is.
    pub fn part(
        &self,
        directory: io::Result<Option<Arc<OwnedFd>>>,
        entries: &[Entry],
        present: &[bool],
        ownership: &Ownership,
    ) -> Verdicts {
        let (directory, unopened) = match directory {
            Ok(directory) => (directory, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let mut failures = Vec::new();
        let verdicts = entries
            .iter()
            .zip(present)
            .map(|(entry, &has)| {
                let verdict = match (&directory, &unopened) {
                    _ if entry.kind == Kind::Unreadable => Ok(Verdict::Same),
                    (_, Some(error)) => Err(format!("cannot open its directory: {error}")),
                    (Some(directory), None) if has => self
                        .entry(&**directory, entry, ownership)
                        .map_err(|error| format!("cannot compare it: {error}")),
                    _ => Ok(Verdict::Content),
                };
                verdict.unwrap_or_else(|message| {
                    failures.push((entry.id, message));
                    Verdict::Same
                })
            })
            .collect();
        Verdicts {
            verdicts,
            failures,
            unopened,
        }
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
        let (rdev, owner) = (stat.st_rdev as u64, stat.st_uid as u32);
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
        } else if !same_mtime(fsys::mtime(stat), entry.mtime, self.modify_window)
            // An mtime this process could not set is not a difference.
            && ownership.can_set_mode(owner)
        {
            Verdict::Times
        } else {
            Verdict::Same
        })
    }
}

/// What each entry of a part needs, and which could not be compared.
#[derive(Debug, Default)]
pub struct Verdicts {
    /// In order: one that could not be compared is [`Verdict::Same`], left
    /// as it is.
    pub verdicts: Vec<Verdict>,
    /// Those that could not be compared, by ID, and why.
    pub failures: Vec<(FileId, String)>,
    /// Why the directory could not be opened, if it could not: all its
    /// entries failed for it.
    pub unopened: Option<String>,
}

/// Whether the permissions or an owner differ, that this process could set.
/// What it could not set is not a difference: no run could remove it.
fn attributes_differ(entry: &Entry, stat: &Stat, ownership: &Ownership) -> bool {
    #[allow(clippy::unnecessary_cast)]
    let (mode, uid, gid) = (
        stat.st_mode as u32 & 0o7777,
        stat.st_uid as u32,
        stat.st_gid as u32,
    );
    let mode_differs =
        !entry.kind.is_symlink() && mode != entry.mode && ownership.can_set_mode(uid);
    let wanted_uid = ownership.uid(entry.uid);
    let wanted_gid = ownership.gid(entry.gid);
    let user_differs = wanted_uid != uid && ownership.can_set_user(wanted_uid, uid);
    let group_differs = wanted_gid != gid && ownership.can_set_group(wanted_gid, uid);
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
    use crate::scan::{self, Batch, Sink};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    /// What a scan found, as the Destination takes it: the root, then each
    /// batch of a directory, its ID, where it is, and whether it is its
    /// last.
    enum Scanned {
        Root(Entry),
        Batch(FileId, Place, Vec<Entry>, bool),
    }

    /// Where `path` is, in the directory `parent`.
    fn place(path: &[u8], parent: Option<FileId>) -> Place {
        Place {
            path: path.to_vec(),
            parent,
        }
    }

    /// What a scan finds, and the IDs of the directories by their paths, to
    /// say where each is.
    #[derive(Default)]
    struct Collect(Vec<Scanned>, std::collections::HashMap<Vec<u8>, FileId>);

    impl Sink for Collect {
        fn root(&mut self, entry: Entry) -> anyhow::Result<()> {
            self.0.push(Scanned::Root(entry));
            Ok(())
        }

        fn entries(&mut self, batch: Batch) -> anyhow::Result<()> {
            self.1.insert(batch.path.to_vec(), batch.dir);
            let parent = (!batch.path.is_empty()).then(|| {
                let parent = match batch.path.iter().rposition(|&byte| byte == b'/') {
                    Some(slash) => &batch.path[..slash],
                    None => b"",
                };
                self.1[parent]
            });
            self.0.push(Scanned::Batch(
                batch.dir,
                place(batch.path, parent),
                batch.entries,
                batch.last,
            ));
            Ok(())
        }

        fn unlistable(&mut self, dir: FileId) -> anyhow::Result<()> {
            panic!("{dir} unlistable");
        }

        fn problem(&mut self, path: &[u8], error: io::Error) -> anyhow::Result<()> {
            panic!("{}: {error}", String::from_utf8_lossy(path));
        }

        fn vanished(&mut self, path: &[u8]) -> anyhow::Result<()> {
            panic!("{} vanished", String::from_utf8_lossy(path));
        }

        fn end(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// The IDs the Destination at `destination` needs from the tree at `source`.
    fn needs(source: &Path, destination: &Path) -> Vec<FileId> {
        needs_within(source, destination, 0)
    }

    /// The IDs a dry run lists: those whose content or attributes differ.
    fn needs_within(source: &Path, destination: &Path, modify_window: i64) -> Vec<FileId> {
        compared(source, destination, modify_window, false).0
    }

    /// What `sync` from `source` to `destination` deletes.
    fn extras(source: &Path, destination: &Path) -> Vec<(Vec<u8>, bool)> {
        compared(source, destination, 0, true).1
    }

    /// The IDs listed, and with `delete`, the paths of what the Source does
    /// not have, and whether each is a directory.
    fn compared(
        source: &Path,
        destination: &Path,
        modify_window: i64,
        delete: bool,
    ) -> (Vec<FileId>, Vec<(Vec<u8>, bool)>) {
        let mut manifest = Collect::default();
        scan::scan(source.as_os_str(), &mut manifest).unwrap();
        compare_manifest(manifest.0, destination, modify_window, delete)
    }

    #[test]
    fn the_sources_root_is_never_gone_into_nor_let_into() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("d");
        fs::create_dir_all(destination.join("source/inner")).unwrap();
        fs::write(destination.join("source/kept"), "kept").unwrap();
        let source_root =
            fsys::key(&fsys::stat_root(destination.join("source").as_os_str()).unwrap());
        let root = destination.as_os_str().as_bytes().to_vec();
        let mut tree = Tree::new(root, true, true, Some(source_root));
        let directory = Entry {
            id: 1,
            name: b"source".to_vec(),
            kind: Kind::Directory,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o755,
            uid: 0,
            gid: 0,
        };
        let top = tree.batch(0, &place(b"", None), std::slice::from_ref(&directory), true);
        assert!(!top.refused);
        let inside = tree.batch(1, &place(b"source", Some(0)), &[], true);
        assert!(inside.refused);
        // Not opened, so nothing in it is extra.
        assert!(matches!(inside.directory, Ok(None)));
        assert!(matches!(&inside.extras, Some(Ok(extras)) if extras.is_empty()));
    }

    fn compare_manifest(
        manifest: Vec<Scanned>,
        destination: &Path,
        modify_window: i64,
        delete: bool,
    ) -> (Vec<FileId>, Vec<(Vec<u8>, bool)>) {
        let root = destination.as_os_str().as_bytes().to_vec();
        let comparer = Comparer::new(root.clone(), modify_window);
        let mut tree = Tree::new(root, delete, false, None);
        let ownership = Ownership::here();
        let mut needs = Vec::new();
        let mut deletes = Vec::new();
        for scanned in manifest {
            match scanned {
                Scanned::Root(entry) => {
                    if comparer.root(&entry, &ownership).unwrap().is_listed() {
                        needs.push(entry.id);
                    }
                }
                Scanned::Batch(dir, place, entries, last) => {
                    let path = place.path.clone();
                    let prepared = tree.batch(dir, &place, &entries, last);
                    let verdicts = comparer
                        .part(prepared.directory, &entries, &prepared.present, &ownership)
                        .verdicts;
                    for (entry, verdict) in entries.iter().zip(verdicts) {
                        if verdict.is_listed() {
                            needs.push(entry.id);
                        }
                    }
                    for (name, is_directory) in
                        prepared.extras.map(Result::unwrap).unwrap_or_default()
                    {
                        deletes.push((crate::manifest::join(&path, &name), is_directory));
                    }
                }
            }
        }
        (needs, deletes)
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

    /// What the Source could not read is left as it is: never needed,
    /// whether the Destination has it or not.
    #[test]
    fn what_the_source_could_not_read_is_left_as_it_is() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("there"), "x").unwrap();
        let directory = Arc::new(fsys::open_root(temp.path().as_os_str()).unwrap());
        let unreadable = |name: &[u8]| Entry {
            id: 1,
            name: name.to_vec(),
            kind: Kind::Unreadable,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0,
            uid: 0,
            gid: 0,
        };
        let comparer = Comparer::new(temp.path().as_os_str().as_bytes().to_vec(), 0);
        let entries = [unreadable(b"missing"), unreadable(b"there")];
        let verdicts = comparer
            .part(
                Ok(Some(Arc::clone(&directory))),
                &entries,
                &[false, true],
                &Ownership::here(),
            )
            .verdicts;
        assert_eq!(verdicts, [Verdict::Same, Verdict::Same]);
        // Nor where the directory itself is missing.
        let verdicts = comparer
            .part(Ok(None), &entries, &[false, false], &Ownership::here())
            .verdicts;
        assert_eq!(verdicts, [Verdict::Same, Verdict::Same]);
    }

    /// Permissions or an mtime that this process could not set, on what it
    /// does not own, are no difference: no run could remove them. On what
    /// it owns, they are.
    #[test]
    fn a_mode_that_cannot_be_set_is_not_a_difference() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let ownership = Ownership::here();
        let me = rustix::process::geteuid().as_raw();
        // SAFETY: a `Stat` is plain data, all of whose fields may be zero.
        let mut stat: Stat = unsafe { std::mem::zeroed() };
        stat.st_mode = (libc::S_IFDIR | 0o700) as _;
        stat.st_uid = me.wrapping_add(1) as _;
        let wanted = Entry {
            id: 1,
            name: b"d".to_vec(),
            kind: Kind::Directory,
            size: 0,
            mtime: Timestamp {
                seconds: 1,
                nanoseconds: 0,
            },
            mode: 0o755,
            uid: me.wrapping_add(1),
            gid: 0,
        };
        let comparer = Comparer::new(b"/".to_vec(), 0);
        let verdict = |stat: &Stat| comparer.verdict(&wanted, stat, &ownership, || Ok(Vec::new()));
        assert_eq!(verdict(&stat).unwrap(), Verdict::Same);
        stat.st_uid = me as _;
        assert_eq!(verdict(&stat).unwrap(), Verdict::Attributes);
    }

    /// Each entry of a directory that could not be opened fails alone, but
    /// what the Source could not read, which is left as it is.
    #[test]
    fn a_directory_that_could_not_be_opened_fails_each_entry_alone() {
        let temp = tempfile::tempdir().unwrap();
        let comparer = Comparer::new(temp.path().as_os_str().as_bytes().to_vec(), 0);
        let entry = |id, name: &[u8], kind| Entry {
            id,
            name: name.to_vec(),
            kind,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        let entries = [
            entry(1, b"file", Kind::File),
            entry(2, b"locked", Kind::Unreadable),
        ];
        let denied = io::Error::from_raw_os_error(libc::EACCES);
        let verdicts = comparer.part(Err(denied), &entries, &[true, true], &Ownership::here());
        assert_eq!(verdicts.verdicts, [Verdict::Same, Verdict::Same]);
        assert_eq!(verdicts.failures.len(), 1);
        assert_eq!(verdicts.failures[0].0, 1);
        assert!(
            verdicts.failures[0]
                .1
                .starts_with("cannot open its directory: ")
        );
        assert!(verdicts.unopened.is_some());
    }

    #[test]
    fn what_the_source_does_not_have_is_found_without_going_into_it() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        tree(&source);
        tree(&destination);
        fs::write(destination.join("extra"), "x").unwrap();
        fs::write(destination.join("dir/zz"), "x").unwrap();
        // An extra directory that cannot even be read: never opened.
        fs::create_dir_all(destination.join("aa/locked")).unwrap();
        fs::set_permissions(destination.join("aa"), fs::Permissions::from_mode(0o000)).unwrap();
        let found = extras(&source, &destination);
        fs::set_permissions(destination.join("aa"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            found,
            [
                (b"aa".to_vec(), true),
                (b"extra".to_vec(), false),
                (b"dir/zz".to_vec(), false),
            ]
        );
    }

    #[test]
    fn a_file_where_a_directory_belongs_is_replaced_and_not_gone_into() {
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        tree(&source);
        fs::create_dir(&destination).unwrap();
        // `dir` is a file here, and `link` a directory.
        fs::write(destination.join("dir"), "not a directory").unwrap();
        fs::create_dir(destination.join("link")).unwrap();
        let (needs, deletes) = compared(&source, &destination, 0, true);
        // `dir` and what is in it, and `link`.
        assert_eq!(needs, [1, 2, 3]);
        assert!(deletes.is_empty(), "{deletes:?}");
    }

    /// A directory in batches, with what the Source does not have before,
    /// between, and after them: all of it is found, once its last batch is
    /// here.
    #[test]
    fn extras_between_batches_are_found_at_the_last() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        for name in ["a", "b", "c", "d", "e", "f", "g"] {
            fs::write(destination.join(name), name).unwrap();
        }
        let entry = |id, name: &[u8]| Entry {
            id,
            name: name.to_vec(),
            kind: Kind::File,
            size: 1,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        let manifest = vec![
            Scanned::Batch(
                0,
                place(b"", None),
                vec![entry(1, b"b"), entry(2, b"c")],
                false,
            ),
            Scanned::Batch(
                0,
                place(b"", None),
                vec![entry(3, b"e"), entry(4, b"h")],
                true,
            ),
        ];
        let (needs, deletes) = compare_manifest(manifest, &destination, 0, true);
        // Every file differs in size or mtime; `h` is missing.
        assert_eq!(needs, [1, 2, 3, 4]);
        let names: Vec<&[u8]> = deletes.iter().map(|(name, _)| name.as_slice()).collect();
        assert_eq!(names, [&b"a"[..], b"d", b"f", b"g"]);
    }

    /// A directory that cannot be listed has no extras, but an error, so
    /// that nothing in it is deleted: one this process may not be let into,
    /// as another's. Let into, as its owner's, it is listed, and says so.
    #[test]
    fn a_directory_not_let_into_has_no_extras_and_one_let_into_says_so() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        fs::create_dir_all(destination.join("locked")).unwrap();
        fs::write(destination.join("locked/extra"), "x").unwrap();
        let entry = Entry {
            id: 1,
            name: b"locked".to_vec(),
            kind: Kind::Directory,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o755,
            uid: 0,
            gid: 0,
        };
        let root = destination.as_os_str().as_bytes().to_vec();
        let mode = || {
            fs::metadata(destination.join("locked"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        for let_in in [false, true] {
            fs::set_permissions(
                destination.join("locked"),
                fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            let mut tree = Tree::new(root.clone(), true, let_in, None);
            let top = tree.batch(0, &place(b"", None), std::slice::from_ref(&entry), true);
            assert_eq!(top.let_in, None);
            let locked = tree.batch(1, &place(b"locked", Some(0)), &[], true);
            let changed = mode();
            fs::set_permissions(
                destination.join("locked"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            if let_in {
                assert_eq!(locked.let_in, Some(0o000));
                assert_eq!(changed, 0o700);
                let extras = locked.extras.unwrap().unwrap();
                assert_eq!(extras, [(b"extra".to_vec(), false)]);
            } else {
                assert_eq!(locked.let_in, None);
                assert_eq!(changed, 0o000);
                assert!(locked.directory.is_err());
                assert!(locked.extras.unwrap().is_err());
            }
        }
    }
}
