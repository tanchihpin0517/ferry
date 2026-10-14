//! The tree `ferry test` creates, in a few variants, and its removal.
//!
//! Every variant is built from the same base, with fixed contents and mtimes,
//! so the same variant created on two machines compares as identical.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::manifest::{self, Entry, Kind, Manifest, Timestamp};
use crate::walk::{self, Sink};

/// A file in every fixture's root. Removal refuses a directory without it, so
/// it can only ever remove a fixture.
pub const MARKER: &str = ".ferry-fixture";

/// The base of every mtime in the tree, in seconds since the epoch.
const BASE_TIME: u64 = 1_600_000_000;

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
}

/// Creates the tree at `root`, which must not exist yet.
pub fn create(root: &Path, variant: Variant) -> Result<()> {
    fs::create_dir(root).with_context(|| format!("create {}", root.display()))?;
    base(root).with_context(|| format!("create the fixture in {}", root.display()))?;
    match variant {
        Variant::Base => Ok(()),
        Variant::Changed => changed(root),
        Variant::Nanoseconds => set_mtime(&root.join("file.txt"), 1, 987_654_321),
        Variant::Modes => modes(root),
        Variant::Blocked => changed(root).and_then(|()| block(root)),
        Variant::BlockedAfter => block(root),
    }
    .with_context(|| format!("change the fixture in {}", root.display()))
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
        fn manifest(&mut self, message: Manifest) -> Result<()> {
            if let Manifest::Directory { path, entries } = message {
                for entry in entries {
                    self.entries
                        .push((manifest::join(&path, &entry.name), entry));
                }
            }
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
    walk::walk(root.as_os_str(), &mut collect)?;
    if !collect.problems.is_empty() {
        bail!(
            "cannot read {}: {}",
            root.display(),
            collect.problems.join("; ")
        );
    }
    let mut described = BTreeMap::new();
    for (path, entry) in collect.entries {
        let is_file = entry.kind == Kind::File;
        let is_directory = entry.kind == Kind::Directory;
        if checks.files_and_directories_only && !is_file && !is_directory {
            continue;
        }
        let content = if is_file {
            let bytes = fs::read(root.join(OsStr::from_bytes(&path)))?;
            Some(blake3::hash(&bytes))
        } else {
            None
        };
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

fn base(root: &Path) -> io::Result<()> {
    write(&root.join(MARKER), b"ferry test fixture\n", 0, 0)?;
    write(&root.join("file.txt"), b"hello\n", 1, 123_456_789)?;
    fs::create_dir(root.join("empty-dir"))?;
    fs::create_dir_all(root.join("dir/deeper"))?;
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
    symlink("..", root.join("dir/deeper"))
}

/// Puts a directory with a file in it where `file.txt` belongs.
fn block(root: &Path) -> io::Result<()> {
    fs::remove_file(root.join("file.txt"))?;
    fs::create_dir(root.join("file.txt"))?;
    write(&root.join("file.txt/inside"), b"inside\n", 20, 0)?;
    set_mtime(&root.join("file.txt"), 21, 0)
}

fn modes(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root.join("file.txt"), fs::Permissions::from_mode(0o600))?;
    fs::set_permissions(root.join("dir/deeper"), fs::Permissions::from_mode(0o700))
}

fn write(path: &Path, content: &[u8], seconds: u64, nanoseconds: u32) -> io::Result<()> {
    fs::write(path, content)?;
    set_mtime(path, seconds, nanoseconds)
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
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `path` is a valid, NUL-terminated C string for the whole call.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o644) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    fn removing_a_missing_root_is_fine() {
        let temp = tempfile::tempdir().unwrap();
        remove(&temp.path().join("missing")).unwrap();
    }
}
