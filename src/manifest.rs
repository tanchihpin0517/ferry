//! What the Source tells the Destination about its files.
//!
//! The manifest is a stream of records, a directory's entries after an `In`
//! naming it, sorted by name, and its `Listed` after them. Paths are bytes
//! with `/` between components, relative to the transfer's root; the root
//! itself is the empty path. Names are not required to be UTF-8.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::sorted::{Keyed, Spill};

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

/// The manifest's records, each on its own, as the Source writes them on
/// the manifest's pipe: a directory's entries follow an `In` naming it, so
/// that the entries of directories listed at once may interleave, and its
/// `Listed` ends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Manifest {
    /// The root of the transfer: always the first record, with ID 0. A root
    /// that is a directory is announced, as one among a directory's entries
    /// is.
    Root(Entry),
    /// The entries after it are of the directory `dir`, by its entry's ID,
    /// the root's 0: one announced, and not ended.
    In { dir: FileId },
    /// An entry of the directory the last `In` named: names strictly
    /// increasing within a directory, IDs one after another, in the order
    /// written. A directory among them is announced.
    Entry(Entry),
    /// Every entry of the directory `dir` is sent: it has ended.
    Listed { dir: FileId },
    /// The directory `dir` is not listed, and none of its entries sent: the
    /// Source could not list it, or, with `-x`, it is on another
    /// filesystem. What the Destination has in it stays as it is. It has
    /// ended.
    Unlistable { dir: FileId },
    /// The name of a user ID the entries after it use, sent once, before
    /// its first use. ID 0 is never named.
    User { id: u32, name: String },
    /// The name of a group ID, as [`Manifest::User`].
    Group { id: u32, name: String },
    /// Something at `path` could not be read, as the scan listed it: it is
    /// in the manifest as unreadable.
    Problem { path: Vec<u8> },
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

/// Checks the manifest as it comes, a record at a time, against what a
/// Source sends, so that the Destination acts on nothing else: whatever a
/// Source sends, by a bug or ill will, it can then make the Destination
/// touch only what is under its root, by names it has listed. A root comes
/// first, and once; each name is one, not `.` or `..`, nor with a `/` or a
/// NUL in it; an `In` names a directory announced, by its parent's entries
/// or as the root, and not ended; entries come only after one, names in
/// its directory in order, strictly; each directory announced ends once,
/// listed, or unlistable before any of its entries came; the end comes once
/// every one has; IDs follow manifest order, from 0 for the root; an
/// mtime's nanoseconds are fewer than a second's. Any order of directories
/// that keeps to that passes, interleaved or not.
///
/// What it keeps of a directory announced and not begun yet is held by the
/// one it is in, within the bound, past it on disk, so that a directory of
/// however many takes no more.
pub struct Check {
    /// A root came.
    root: bool,
    ended: bool,
    /// The ID the next entry has.
    next: FileId,
    /// The root, and the directories begun, until each has ended and every
    /// one announced in it has gone: see [`Directory`].
    directories: HashMap<FileId, Directory>,
    /// Where the directories announced in each begun one are among the IDs:
    /// the first ID of each run of them, with the one they are in. One not
    /// begun is in the one its run names.
    runs: BTreeMap<FileId, FileId>,
    /// The directory the last directory announced is in.
    announcing: Option<FileId>,
    /// The directory the entries go to, as the last `In` named, until it
    /// ends.
    current: Option<FileId>,
    /// When, and where, what is held of a directory goes to disk.
    spill: Spill,
}

/// The root, or a directory begun, as the check keeps it.
struct Directory {
    /// The directory it is in; none for the root.
    parent: Option<FileId>,
    /// Its path: what the paths of its own are made of.
    place: Vec<u8>,
    /// Its entries began; only the root is kept before.
    begun: bool,
    ended: bool,
    /// The last name among its entries.
    last: Option<Vec<u8>>,
    /// The directories announced in it that have not gone.
    announced: usize,
    /// The directories announced in it, by ID, with their names.
    own: Keyed,
    /// The ranges of those that began, by their numbers in `own`.
    began: BTreeMap<usize, usize>,
    /// Where its runs start, in [`Check::runs`].
    runs: Vec<FileId>,
}

impl Directory {
    fn new(parent: Option<FileId>, place: Vec<u8>, begun: bool, spill: &Spill) -> Self {
        Self {
            parent,
            place,
            begun,
            ended: false,
            last: None,
            announced: 0,
            own: Keyed::new(spill),
            began: BTreeMap::new(),
            runs: Vec::new(),
        }
    }

    /// Takes the one of its own numbered `number` as begun: whether it had
    /// not.
    fn begin_own(&mut self, number: usize) -> bool {
        let (mut first, mut last) = (number, number);
        if let Some((&start, &end)) = self.began.range(..=number).next_back() {
            if end >= number {
                return false;
            }
            if end + 1 == number {
                self.began.remove(&start);
                first = start;
            }
        }
        if let Some(end) = self.began.remove(&(number + 1)) {
            last = end;
        }
        self.began.insert(first, last);
        true
    }

    /// The first of its own not begun, if any: its ID and name.
    fn not_begun(&mut self) -> Result<Option<(FileId, Vec<u8>)>> {
        let began = &self.began;
        let is_begun = |number: usize| {
            began
                .range(..=number)
                .next_back()
                .is_some_and(|(_, &end)| end >= number)
        };
        let mut number = 0;
        while let Some((id, name)) = self.own.next_record().context(KEEP)? {
            if !is_begun(number) {
                return Ok(Some((id_of(&id), name)));
            }
            number += 1;
        }
        Ok(None)
    }
}

/// What the check says, failing to keep what it holds on disk.
const KEEP: &str = "keep the directories announced";

/// An ID, as a [`Keyed`]'s key holds it.
fn id_of(key: &[u8]) -> FileId {
    FileId::from_be_bytes(key.try_into().expect("an ID's eight bytes"))
}

/// Where a directory is: its path, and the directory it is in, none for the
/// root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub path: Vec<u8>,
    pub parent: Option<FileId>,
}

/// A directory that ended: where it is, and the directories gone with it,
/// each ended and with none announced in it left, deepest first: nothing
/// more of them comes.
#[derive(Debug)]
pub struct Ending {
    pub place: Place,
    pub gone: Vec<FileId>,
}

/// Whether `entry`, in the directory at `path`, has an mtime that is one: of
/// fewer nanoseconds than a second, which setting it takes.
fn has_a_time(entry: &Entry, path: &[u8]) -> Result<()> {
    ensure!(
        entry.mtime.nanoseconds < 1_000_000_000,
        "an mtime of {} nanoseconds past a second, for {:?} in {:?}",
        entry.mtime.nanoseconds,
        String::from_utf8_lossy(&entry.name),
        String::from_utf8_lossy(path)
    );
    Ok(())
}

impl Default for Check {
    fn default() -> Self {
        Self::new(Spill::default())
    }
}

impl Check {
    pub fn new(spill: Spill) -> Self {
        Self {
            root: false,
            ended: false,
            next: 0,
            directories: HashMap::new(),
            runs: BTreeMap::new(),
            announcing: None,
            current: None,
            spill,
        }
    }

    pub fn root(&mut self, entry: &Entry) -> Result<()> {
        ensure!(!self.ended, "a root after the end");
        ensure!(!self.root, "a second root");
        ensure!(
            entry.id == 0 && entry.name.is_empty(),
            "a root with ID {} named {}",
            entry.id,
            String::from_utf8_lossy(&entry.name)
        );
        has_a_time(entry, b"")?;
        self.root = true;
        self.next = 1;
        if entry.kind == Kind::Directory {
            let root = Directory::new(None, Vec::new(), false, &self.spill);
            self.directories.insert(0, root);
        }
        Ok(())
    }

    /// The directory `dir`, named `name`, announced by the entries of
    /// `parent`, which has begun.
    fn announce(&mut self, dir: FileId, parent: FileId, name: &[u8]) -> Result<()> {
        let directory = self.directories.get_mut(&parent).expect("begun");
        if self.announcing != Some(parent) {
            self.announcing = Some(parent);
            self.runs.insert(dir, parent);
            directory.runs.push(dir);
        }
        directory.announced += 1;
        directory.own.push(&dir.to_be_bytes(), name).context(KEEP)
    }

    /// Begins the directory `dir`, if it is one announced and not begun:
    /// whether it was.
    fn take(&mut self, dir: FileId) -> Result<bool> {
        let Some((_, &parent)) = self.runs.range(..=dir).next_back() else {
            return Ok(false);
        };
        // A run goes with its directory.
        let directory = self.directories.get_mut(&parent).expect("there");
        let Some((number, name)) = directory.own.find(&dir.to_be_bytes()).context(KEEP)? else {
            return Ok(false);
        };
        if !directory.begin_own(number) {
            return Ok(false);
        }
        let path = join(&directory.place, &name);
        let begun = Directory::new(Some(parent), path, true, &self.spill);
        self.directories.insert(dir, begun);
        Ok(true)
    }

    /// The directory `dir`, announced and not ended, begun if it was not:
    /// where it is.
    fn begin(&mut self, dir: FileId) -> Result<Place> {
        if !self.directories.contains_key(&dir) {
            self.take(dir)?;
        }
        let Some(directory) = self
            .directories
            .get_mut(&dir)
            .filter(|directory| !directory.ended)
        else {
            anyhow::bail!("directory {dir}, which is not one announced, or has ended");
        };
        directory.begun = true;
        Ok(Place {
            path: directory.place.clone(),
            parent: directory.parent,
        })
    }

    /// An `In`: the entries after it are of `dir`. Where it is.
    pub fn enter(&mut self, dir: FileId) -> Result<Place> {
        ensure!(!self.ended, "an In after the end");
        let place = self.begin(dir)?;
        self.current = Some(dir);
        Ok(place)
    }

    /// An entry, of the directory the last `In` named, which it says.
    pub fn entry(&mut self, entry: &Entry) -> Result<FileId> {
        ensure!(!self.ended, "an entry after the end");
        let Some(dir) = self.current else {
            anyhow::bail!(
                "the entry {:?} in no directory",
                String::from_utf8_lossy(&entry.name)
            );
        };
        let directory = self
            .directories
            .get_mut(&dir)
            .expect("the current is there");
        let shown = || String::from_utf8_lossy(&directory.place).into_owned();
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
        if let Some(last) = &directory.last {
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
        has_a_time(entry, &directory.place)?;
        directory.last = Some(name.clone());
        self.next += 1;
        if entry.kind == Kind::Directory {
            self.announce(entry.id, dir, &entry.name)?;
        }
        Ok(dir)
    }

    /// A `Listed`: `dir` ended, all its entries come, if any did.
    pub fn listed(&mut self, dir: FileId) -> Result<Ending> {
        ensure!(!self.ended, "a Listed after the end");
        let place = self.begin(dir)?;
        Ok(self.close(dir, place))
    }

    /// An `Unlistable`: `dir` ended, none of its entries come.
    pub fn unlistable(&mut self, dir: FileId) -> Result<Ending> {
        ensure!(!self.ended, "an Unlistable after the end");
        let not_begun = match self.directories.get(&dir) {
            Some(directory) => !directory.begun && !directory.ended,
            None => self.take(dir)?,
        };
        ensure!(
            not_begun,
            "directory {dir} unlistable, which is not one announced, or began, or has ended"
        );
        let place = self.begin(dir)?;
        Ok(self.close(dir, place))
    }

    /// Ends `dir`, at `place`, and lets go of it and those above it, as
    /// each has gone.
    fn close(&mut self, dir: FileId, place: Place) -> Ending {
        self.directories.get_mut(&dir).expect("there").ended = true;
        if self.current == Some(dir) {
            self.current = None;
        }
        let mut gone = Vec::new();
        let mut at = Some(dir);
        while let Some(id) = at {
            let directory = &self.directories[&id];
            if !directory.ended || directory.announced > 0 {
                break;
            }
            at = directory.parent;
            let directory = self.directories.remove(&id).expect("there");
            for run in directory.runs {
                self.runs.remove(&run);
            }
            if self.announcing == Some(id) {
                self.announcing = None;
            }
            gone.push(id);
            if let Some(parent) = at {
                self.directories
                    .get_mut(&parent)
                    .expect("a parent stays while what it announced does")
                    .announced -= 1;
            }
        }
        Ending { place, gone }
    }

    /// Whether the manifest has come to its end.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    pub fn end(&mut self) -> Result<()> {
        ensure!(!self.ended, "a second end");
        if let Some((&dir, directory)) = self
            .directories
            .iter()
            .find(|(_, directory)| !directory.ended)
        {
            anyhow::bail!(
                "the end before directory {dir}, {:?}, ended",
                String::from_utf8_lossy(&directory.place)
            );
        }
        for directory in self.directories.values_mut() {
            if let Some((dir, name)) = directory.not_begun()? {
                anyhow::bail!(
                    "the end before directory {dir}, {:?}, ended",
                    String::from_utf8_lossy(&join(&directory.place, &name))
                );
            }
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

    /// Takes `records`, failing at the first that does not pass.
    fn take(check: &mut Check, records: &[Manifest]) -> Result<()> {
        for record in records {
            match record {
                Manifest::Root(entry) => check.root(entry)?,
                Manifest::In { dir } => drop(check.enter(*dir)?),
                Manifest::Entry(entry) => drop(check.entry(entry)?),
                Manifest::Listed { dir } => drop(check.listed(*dir)?),
                Manifest::Unlistable { dir } => drop(check.unlistable(*dir)?),
                Manifest::End => check.end()?,
                Manifest::User { .. } | Manifest::Group { .. } | Manifest::Problem { .. } => {}
            }
        }
        Ok(())
    }

    /// Whether `records` pass, as the same do when every directory
    /// announced and not begun is kept on disk.
    fn passes(records: &[Manifest]) -> bool {
        let passed = take(&mut Check::default(), records).is_ok();
        let temp = tempfile::tempdir().unwrap();
        let on_disk = take(&mut Check::new(on_disk(temp.path())), records);
        assert_eq!(on_disk.is_ok(), passed, "{on_disk:?}");
        passed
    }

    /// Each record kept goes to disk, in `temp`.
    fn on_disk(temp: &std::path::Path) -> Spill {
        Spill {
            limits: crate::sorted::Limits { held: 1, fan_in: 4 },
            temp: temp.to_path_buf(),
        }
    }

    fn root() -> Manifest {
        Manifest::Root(named(0, b"", Kind::Directory))
    }

    fn entry(id: FileId, name: &[u8], kind: Kind) -> Manifest {
        Manifest::Entry(named(id, name, kind))
    }

    use Manifest::{End, In, Listed, Unlistable};

    /// A Source's manifest, as a scan sends it, passes, its places said as
    /// they come, and each directory let go of once it and all announced in
    /// it have ended.
    #[test]
    fn a_manifest_as_a_scan_sends_it_passes() {
        let mut check = Check::default();
        take(
            &mut check,
            &[
                root(),
                In { dir: 0 },
                entry(1, b"a", Kind::Directory),
                entry(2, b"b", Kind::File),
                entry(3, b"c", Kind::Unreadable),
                entry(4, b"d", Kind::Directory),
            ],
        )
        .unwrap();
        let ending = check.listed(0).unwrap();
        assert_eq!(ending.place.path, b"");
        assert!(ending.gone.is_empty());
        let place = check.enter(1).unwrap();
        assert_eq!(
            place,
            Place {
                path: b"a".to_vec(),
                parent: Some(0)
            }
        );
        assert_eq!(check.entry(&named(5, b"x", Kind::Directory)).unwrap(), 1);
        assert!(check.listed(1).unwrap().gone.is_empty());
        let ending = check.unlistable(5).unwrap();
        assert_eq!(ending.place.path, b"a/x");
        assert_eq!(ending.gone, [5, 1]);
        let ending = check.listed(4).unwrap();
        assert_eq!(ending.gone, [4, 0]);
        check.end().unwrap();
        assert!(check.is_ended());
    }

    /// Directories may come in any order, their entries interleaved, so
    /// long as each is announced before it comes, and ends once.
    #[test]
    fn any_order_that_keeps_the_rules_passes() {
        let tree = [
            root(),
            In { dir: 0 },
            entry(1, b"a", Kind::Directory),
            entry(2, b"b", Kind::Directory),
            Listed { dir: 0 },
        ];
        let with = |rest: &[Manifest]| [&tree[..], rest].concat();
        // The second before the first, and their entries interleaved.
        assert!(passes(&with(&[
            In { dir: 2 },
            entry(3, b"x", Kind::File),
            In { dir: 1 },
            entry(4, b"x", Kind::File),
            In { dir: 2 },
            entry(5, b"y", Kind::File),
            Listed { dir: 1 },
            Listed { dir: 2 },
            End,
        ])));
        // A directory with no entries needs no In.
        assert!(passes(&with(&[
            Listed { dir: 2 },
            Unlistable { dir: 1 },
            End
        ])));
        // A directory begun by one In, which more of its entries follow.
        assert!(passes(&with(&[
            In { dir: 1 },
            entry(3, b"x", Kind::Directory),
            entry(4, b"y", Kind::File),
            Listed { dir: 1 },
            In { dir: 3 },
            Listed { dir: 3 },
            Listed { dir: 2 },
            End,
        ])));
    }

    /// Whatever else, it does not.
    #[test]
    fn anything_else_does_not() {
        let tree = [
            root(),
            In { dir: 0 },
            entry(1, b"a", Kind::Directory),
            entry(2, b"b", Kind::File),
        ];
        let with = |rest: &[Manifest]| [&tree[..], rest].concat();
        // A file root announces no directory, not even itself.
        assert!(!passes(&[
            Manifest::Root(named(0, b"", Kind::File)),
            In { dir: 0 }
        ]));
        assert!(passes(&[Manifest::Root(named(0, b"", Kind::File)), End]));
        // Nothing before the root, nor after the end; one root; the end
        // once.
        assert!(!passes(&[In { dir: 0 }]));
        assert!(!passes(&[entry(1, b"a", Kind::File)]));
        let ended = [root(), Listed { dir: 0 }, End];
        assert!(passes(&ended));
        for after in [root(), In { dir: 0 }, Listed { dir: 0 }, End] {
            assert!(!passes(&[&ended[..], &[after]].concat()));
        }
        assert!(!passes(&[root(), root()]));
        assert!(!passes(&[Manifest::Root(named(
            0,
            b"named",
            Kind::Directory
        ))]));
        assert!(!passes(&[Manifest::Root(named(5, b"", Kind::Directory))]));
        // No end before every directory ended.
        assert!(!passes(&with(&[Listed { dir: 0 }, End])));
        assert!(!passes(&[root(), End]));
        // An entry in no directory: before any In, or after its directory
        // ended.
        assert!(!passes(&[root(), entry(1, b"a", Kind::File)]));
        assert!(!passes(&with(&[
            Listed { dir: 0 },
            entry(3, b"c", Kind::File)
        ])));
        // A directory not announced: a file, an unread one, or none.
        assert!(!passes(&with(&[In { dir: 2 }])));
        assert!(!passes(&with(&[Listed { dir: 2 }])));
        assert!(!passes(&with(&[In { dir: 9 }])));
        assert!(!passes(&with(&[
            entry(3, b"c", Kind::Unreadable),
            In { dir: 3 }
        ])));
        // A directory ended twice, or begun once ended.
        assert!(!passes(&with(&[Listed { dir: 1 }, Listed { dir: 1 }])));
        assert!(!passes(&with(&[Listed { dir: 1 }, In { dir: 1 }])));
        assert!(!passes(&with(&[Unlistable { dir: 1 }, Listed { dir: 1 }])));
        // Unlistable once begun: some of its entries came.
        assert!(!passes(&with(&[In { dir: 1 }, Unlistable { dir: 1 }])));
        assert!(!passes(&[root(), In { dir: 0 }, Unlistable { dir: 0 }]));
        // Names out of order in a directory, or the same twice, across Ins.
        assert!(!passes(&with(&[entry(3, b"a", Kind::File)])));
        assert!(!passes(&with(&[
            In { dir: 1 },
            entry(3, b"x", Kind::File),
            In { dir: 0 },
            entry(4, b"b", Kind::File),
        ])));
        // Bad names.
        for name in [&b""[..], b".", b"..", b"a/b", b"x\0y"] {
            assert!(!passes(&[
                root(),
                In { dir: 0 },
                entry(1, name, Kind::File)
            ]));
        }
        // IDs out of turn.
        assert!(!passes(&[
            root(),
            In { dir: 0 },
            entry(2, b"a", Kind::File)
        ]));
        assert!(!passes(&with(&[entry(2, b"c", Kind::File)])));
        // An mtime of a second's nanoseconds or more, of the root or not.
        let mut late = named(0, b"", Kind::Directory);
        late.mtime.nanoseconds = 1_000_000_000;
        assert!(!passes(&[Manifest::Root(late.clone())]));
        late.id = 1;
        late.name = b"late".to_vec();
        assert!(!passes(&[root(), In { dir: 0 }, Manifest::Entry(late)]));
        let mut last = named(1, b"last", Kind::File);
        last.mtime.nanoseconds = 999_999_999;
        assert!(passes(&[root(), In { dir: 0 }, Manifest::Entry(last)]));
    }

    /// Many directories in one, kept on disk as announced, begin where
    /// they were announced, in any order, once each; the end before all of
    /// them have says where the first left is.
    #[test]
    fn directories_announced_kept_on_disk_begin_in_any_order() {
        let temp = tempfile::tempdir().unwrap();
        let mut check = Check::new(on_disk(temp.path()));
        let mut records = vec![root(), In { dir: 0 }];
        for id in 1..=300 {
            let kind = if id % 3 == 0 {
                Kind::File
            } else {
                Kind::Directory
            };
            records.push(entry(id, format!("d{id:03}").as_bytes(), kind));
        }
        records.push(Listed { dir: 0 });
        take(&mut check, &records).unwrap();
        // From the last, and one in each a directory of its own.
        for id in (2..=300).rev().filter(|id| id % 3 != 0) {
            assert_eq!(
                check.enter(id).unwrap().path,
                format!("d{id:03}").as_bytes()
            );
            if id == 200 {
                check.entry(&named(301, b"in", Kind::Directory)).unwrap();
                check.listed(id).unwrap();
                assert_eq!(check.enter(301).unwrap().path, b"d200/in");
                assert!(check.listed(301).unwrap().gone.contains(&200));
            } else {
                check.listed(id).unwrap();
            }
            assert!(check.enter(id).is_err());
        }
        assert!(check.enter(3).is_err());
        let error = check.end().unwrap_err().to_string();
        assert!(error.contains("directory 1, \"d001\""), "{error}");
        let mut check = Check::new(on_disk(temp.path()));
        take(&mut check, &records).unwrap();
        for id in (1..=300).filter(|id| id % 3 != 0) {
            check.unlistable(id).unwrap();
        }
        check.end().unwrap();
    }

    #[test]
    fn join_paths() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a", b"b"), b"a/b");
        assert_eq!(join(b"a/b", b"c"), b"a/b/c");
    }
}
