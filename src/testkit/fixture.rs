//! The tree `ferry test` creates, in a few variants, and its removal.
//!
//! Every variant is built from the same base, with fixed contents and mtimes,
//! so the same variant created on two machines compares as identical.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::manifest::{self, Entry, FileId, Kind, Timestamp};
use crate::scan::{self, Batch, Sink};

/// A file in every fixture's root. Removal refuses a directory without it, so
/// it can only ever remove a fixture.
pub const MARKER: &str = ".ferry-fixture";
const MARKER_CONTENT: &[u8] = b"ferry test fixture\n";

/// The base of every mtime in the tree, in seconds since the epoch.
const BASE_TIME: u64 = 1_600_000_000;

/// The mode of every file and FIFO in the tree, and of every directory: fixed,
/// not left to the umask, which differs between machines.
const FILE_MODE: u32 = 0o644;
const DIRECTORY_MODE: u32 = 0o755;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Variant {
    /// The tree as the source has it.
    Base,
    /// The base with a file's content, a file's mtime, and a link's target
    /// changed; an empty directory removed; and a directory replaced by a
    /// symlink.
    Changed,
    /// The base with one file's mtime moved within the same second.
    Nanoseconds,
    /// The base with a file's and a directory's permissions changed.
    Modes,
    /// The changed tree, with a directory that is not empty where `file.txt`
    /// belongs.
    Blocked,
    /// The base with that directory where `file.txt` belongs: what a copy
    /// over `Blocked` leaves, since it cannot remove the directory.
    BlockedAfter,
    /// The base with extra entries: a file, and a directory with a file in it.
    Extra,
}

/// Creates the tree at `root`, which must not exist yet.
pub fn create(root: &Path, variant: Variant) -> Result<()> {
    mkdir(root).with_context(|| format!("create {}", root.display()))?;
    base(root).with_context(|| format!("create the fixture in {}", root.display()))?;
    match variant {
        Variant::Base => Ok(()),
        Variant::Changed => changed(root),
        Variant::Nanoseconds => set_mtime(&root.join("file.txt"), 1, 987_654_321),
        Variant::Modes => modes(root),
        Variant::Blocked => changed(root).and_then(|()| block(root)),
        Variant::BlockedAfter => block(root),
        Variant::Extra => extra(root),
    }
    .with_context(|| format!("change the fixture in {}", root.display()))
}

/// Creates `root`, which must not exist yet, with only the marker in it: a
/// fixture that a transfer then fills, and that can be removed whatever it
/// was left with. The marker is the base's, the same everywhere.
pub fn mark(root: &Path) -> Result<()> {
    mkdir(root).with_context(|| format!("create {}", root.display()))?;
    write(&root.join(MARKER), MARKER_CONTENT, 0, 0)?;
    Ok(())
}

/// Removes the tree at `root`, if it is a fixture. A root that does not exist
/// is already removed.
pub fn remove(root: &Path) -> Result<()> {
    if fs::symlink_metadata(root).is_err() {
        return Ok(());
    }
    if !root.join(MARKER).is_file() {
        bail!("{} is not a fixture: it has no {MARKER}", root.display());
    }
    fs::remove_dir_all(root).with_context(|| format!("remove {}", root.display()))
}

/// What [`verify`] compares, beyond each entry's kind and each file's content.
#[derive(Debug, Clone, Copy, Default)]
pub struct Checks {
    /// Compare only regular files and directories.
    pub files_and_directories_only: bool,
    /// Leave directories' mtimes out.
    pub ignore_directory_mtimes: bool,
}

/// Checks that the tree at `root` is `variant`: the same entries, kinds,
/// sizes, permissions, mtimes, symlink targets, and contents. Owners are not
/// compared, since the same user may have other IDs on other machines.
///
/// The expected tree is created beside `root`, compared, and removed.
pub fn verify(root: &Path, variant: Variant, checks: Checks) -> Result<()> {
    let mut reference = root.as_os_str().to_owned();
    reference.push(".reference");
    let reference = Path::new(&reference);
    create(reference, variant)?;
    let compared = compare_trees(reference, root, checks);
    remove(reference)?;
    compared
}

/// A digest of the tree at `root`, of everything [`verify`] compares, with
/// how many entries and file bytes it has: two trees with the same digest are
/// the same, wherever each is.
pub fn digest(root: &Path) -> Result<String> {
    let described = describe(root, Checks::default())?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0;
    for (path, entry) in &described {
        hasher.update(format!("{path}\0{entry:?}\n").as_bytes());
        if entry.kind == Kind::File {
            bytes += entry.size;
        }
    }
    Ok(format!(
        "{} ({}, {})",
        hasher.finalize().to_hex(),
        crate::progress::counted(described.len() as u64, "entry", "entries"),
        crate::progress::size(bytes)
    ))
}

/// Fails once the process has been asked to stop, for work that takes long.
pub(super) fn stopping() -> Result<()> {
    if crate::interrupt::stop_asked() {
        bail!("stopped");
    }
    Ok(())
}

/// An entry as [`verify`] compares it.
#[derive(Debug, PartialEq, Eq)]
struct Described {
    kind: Kind,
    size: u64,
    mode: u32,
    mtime: Option<Timestamp>,
    content: Option<blake3::Hash>,
}

fn compare_trees(expected: &Path, actual: &Path, checks: Checks) -> Result<()> {
    let expected = describe(expected, checks)?;
    let actual = describe(actual, checks)?;
    let mut differences = Vec::new();
    for (path, wanted) in &expected {
        match actual.get(path) {
            None => differences.push(format!("missing: {path}")),
            Some(found) if found != wanted => {
                differences.push(format!(
                    "differs: {path}: expected {wanted:?}, found {found:?}"
                ));
            }
            Some(_) => {}
        }
    }
    for path in actual.keys().filter(|path| !expected.contains_key(*path)) {
        differences.push(format!("unexpected: {path}"));
    }
    if differences.is_empty() {
        return Ok(());
    }
    bail!("the tree is not as expected:\n{}", differences.join("\n"));
}

/// Every entry under `root` but the root, by path.
fn describe(root: &Path, checks: Checks) -> Result<BTreeMap<String, Described>> {
    struct Collect {
        entries: Vec<(Vec<u8>, Entry)>,
        problems: Vec<String>,
    }
    impl Sink for Collect {
        fn root(&mut self, _entry: Entry) -> Result<()> {
            stopping()
        }

        fn entries(&mut self, batch: Batch) -> Result<()> {
            stopping()?;
            for entry in batch.entries {
                self.entries
                    .push((manifest::join(batch.path, &entry.name), entry));
            }
            Ok(())
        }

        fn unlistable(&mut self, _dir: FileId) -> Result<()> {
            Ok(())
        }

        fn end(&mut self) -> Result<()> {
            Ok(())
        }

        fn vanished(&mut self, path: &[u8]) -> Result<()> {
            self.problems
                .push(format!("{}: vanished", String::from_utf8_lossy(path)));
            Ok(())
        }

        fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()> {
            self.problems
                .push(format!("{}: {error}", String::from_utf8_lossy(path)));
            Ok(())
        }
    }

    let mut collect = Collect {
        entries: Vec::new(),
        problems: Vec::new(),
    };
    scan::scan(root.as_os_str(), &mut collect)?;
    if !collect.problems.is_empty() {
        bail!(
            "cannot read {}: {}",
            root.display(),
            collect.problems.join("; ")
        );
    }
    let entries: Vec<(Vec<u8>, Entry)> = collect
        .entries
        .into_iter()
        .filter(|(_, entry)| {
            !checks.files_and_directories_only || matches!(entry.kind, Kind::File | Kind::Directory)
        })
        .collect();
    let mut contents = hash_files(root, &entries)?.into_iter();
    let mut described = BTreeMap::new();
    for (path, entry) in entries {
        let content = contents.next().expect("one for each entry");
        let mtime = match entry.kind {
            Kind::File => Some(entry.mtime),
            Kind::Directory if !checks.ignore_directory_mtimes => Some(entry.mtime),
            _ => None,
        };
        described.insert(
            String::from_utf8_lossy(&path).into_owned(),
            Described {
                size: entry.size,
                mode: if entry.kind.is_symlink() {
                    0
                } else {
                    entry.mode
                },
                kind: entry.kind,
                mtime,
                content,
            },
        );
    }
    Ok(described)
}

/// How many files are hashed at once: a filesystem shared over the network
/// answers each read only after a round trip, so a tree of small files
/// there is read faster by many threads than by one.
const HASHERS: usize = 16;

/// The hash of each file's content among `entries`, in their order; none
/// for anything else.
fn hash_files(root: &Path, entries: &[(Vec<u8>, Entry)]) -> Result<Vec<Option<blake3::Hash>>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let hashed: Vec<std::sync::Mutex<Option<blake3::Hash>>> = entries
        .iter()
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| -> Result<()> {
        let hashers: Vec<_> = (0..crate::pool::threads(HASHERS))
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((path, entry)) = entries.get(index) else {
                            return Ok(());
                        };
                        if entry.kind != Kind::File {
                            continue;
                        }
                        stopping()?;
                        let path = root.join(OsStr::from_bytes(path));
                        let file = File::open(&path)
                            .with_context(|| format!("open {}", path.display()))?;
                        let hash = blake3::Hasher::new()
                            .update_reader(file)
                            .with_context(|| format!("read {}", path.display()))?
                            .finalize();
                        *hashed[index].lock().expect("not poisoned") = Some(hash);
                    }
                })
            })
            .collect();
        for hasher in hashers {
            hasher
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error in a hasher")))?;
        }
        Ok(())
    })?;
    Ok(hashed
        .into_iter()
        .map(|hash| hash.into_inner().expect("not poisoned"))
        .collect())
}

fn base(root: &Path) -> io::Result<()> {
    write(&root.join(MARKER), MARKER_CONTENT, 0, 0)?;
    write(&root.join("file.txt"), b"hello\n", 1, 123_456_789)?;
    mkdir(&root.join("empty-dir"))?;
    mkdir(&root.join("dir"))?;
    mkdir(&root.join("dir/deeper"))?;
    write(&root.join("dir/nested.txt"), b"nested\n", 2, 0)?;
    let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    write(&root.join("dir/deeper/deep.bin"), &pattern, 3, 500)?;
    symlink("file.txt", root.join("link"))?;
    symlink("missing-target", root.join("dangling"))?;
    mkfifo(&root.join("fifo"))?;
    // Last, and deepest first, since writing into a directory changes its
    // mtime.
    set_mtime(&root.join("dir/deeper"), 10, 100)?;
    set_mtime(&root.join("dir"), 11, 0)?;
    set_mtime(&root.join("empty-dir"), 12, 999_999_999)
}

fn changed(root: &Path) -> io::Result<()> {
    // A different size.
    write(&root.join("file.txt"), b"hello, world\n", 1, 123_456_789)?;
    // The same size, a different mtime.
    set_mtime(&root.join("dir/nested.txt"), 100, 0)?;
    // A link to somewhere else.
    fs::remove_file(root.join("link"))?;
    symlink("dir/nested.txt", root.join("link"))?;
    // A missing directory.
    fs::remove_dir(root.join("empty-dir"))?;
    // A symlink where a directory belongs, which must not be followed.
    fs::remove_dir_all(root.join("dir/deeper"))?;
    symlink("..", root.join("dir/deeper"))?;
    // The base's mtime, so that the tree can be checked as it was left.
    set_mtime(&root.join("dir"), 11, 0)
}

/// Adds entries the base does not have, keeping its directories' mtimes.
fn extra(root: &Path) -> io::Result<()> {
    write(&root.join("extra.txt"), b"extra\n", 30, 0)?;
    mkdir(&root.join("dir/extra-dir"))?;
    write(&root.join("dir/extra-dir/inside.txt"), b"inside\n", 31, 0)?;
    set_mtime(&root.join("dir/extra-dir"), 32, 0)?;
    set_mtime(&root.join("dir"), 11, 0)
}

/// Puts a directory with a file in it where `file.txt` belongs.
fn block(root: &Path) -> io::Result<()> {
    fs::remove_file(root.join("file.txt"))?;
    mkdir(&root.join("file.txt"))?;
    write(&root.join("file.txt/inside"), b"inside\n", 20, 0)?;
    set_mtime(&root.join("file.txt"), 21, 0)
}

fn modes(root: &Path) -> io::Result<()> {
    fs::set_permissions(root.join("file.txt"), fs::Permissions::from_mode(0o600))?;
    fs::set_permissions(root.join("dir/deeper"), fs::Permissions::from_mode(0o700))
}

fn write(path: &Path, content: &[u8], seconds: u64, nanoseconds: u32) -> io::Result<()> {
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    set_mtime(path, seconds, nanoseconds)
}

fn mkdir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))
}

/// Sets an mtime `seconds` and `nanoseconds` after [`BASE_TIME`], on a file
/// or a directory.
fn set_mtime(path: &Path, seconds: u64, nanoseconds: u32) -> io::Result<()> {
    let time = SystemTime::UNIX_EPOCH + Duration::new(BASE_TIME + seconds, nanoseconds);
    let file = if path.is_dir() {
        File::open(path)?
    } else {
        File::options().write(true).open(path)?
    };
    file.set_modified(time)
}

fn mkfifo(path: &Path) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `c_path` is a valid, NUL-terminated C string for the whole call.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // `mkfifo` takes the umask out of the mode, so set it again; and its
    // `mode_t` is narrower than `FILE_MODE` on macOS.
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_remove_every_variant() {
        let temp = tempfile::tempdir().unwrap();
        for variant in [
            Variant::Base,
            Variant::Changed,
            Variant::Nanoseconds,
            Variant::Modes,
            Variant::Blocked,
            Variant::BlockedAfter,
            Variant::Extra,
        ] {
            let root = temp.path().join(format!("{variant:?}"));
            create(&root, variant).unwrap();
            assert!(root.join(MARKER).is_file());
            remove(&root).unwrap();
            assert!(!root.exists());
        }
    }

    #[test]
    fn an_existing_root_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        assert!(create(temp.path(), Variant::Base).is_err());
    }

    #[test]
    fn only_a_fixture_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("precious"), "data").unwrap();
        assert!(remove(temp.path()).is_err());
        assert!(temp.path().join("precious").exists());
    }

    #[test]
    fn verify_tells_a_tree_from_another() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        create(&root, Variant::Base).unwrap();
        verify(&root, Variant::Base, Checks::default()).unwrap();
        let error = verify(&root, Variant::Changed, Checks::default()).unwrap_err();
        assert!(error.to_string().contains("file.txt"), "{error}");
        // The reference tree is gone either way.
        assert!(!temp.path().join("tree.reference").exists());
        remove(&root).unwrap();
    }

    #[test]
    fn modes_do_not_follow_the_umask() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        create(&root, Variant::Base).unwrap();
        let mode = |path: &str| {
            fs::symlink_metadata(root.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        for file in [
            MARKER,
            "file.txt",
            "dir/nested.txt",
            "dir/deeper/deep.bin",
            "fifo",
        ] {
            assert_eq!(mode(file), FILE_MODE, "{file}");
        }
        for directory in ["", "empty-dir", "dir", "dir/deeper"] {
            assert_eq!(mode(directory), DIRECTORY_MODE, "{directory}");
        }
        remove(&root).unwrap();
    }

    #[test]
    fn the_digest_tells_a_tree_from_another() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (temp.path().join("a"), temp.path().join("b"));
        create(&a, Variant::Base).unwrap();
        create(&b, Variant::Base).unwrap();
        assert_eq!(digest(&a).unwrap(), digest(&b).unwrap());
        for variant in [Variant::Nanoseconds, Variant::Modes, Variant::Extra] {
            let other = temp.path().join(format!("{variant:?}"));
            create(&other, variant).unwrap();
            assert_ne!(digest(&a).unwrap(), digest(&other).unwrap(), "{variant:?}");
        }
    }

    #[test]
    fn removing_a_missing_root_is_fine() {
        let temp = tempfile::tempdir().unwrap();
        remove(&temp.path().join("missing")).unwrap();
    }
}
