//! The tree `ferry test` creates, in a few variants, and its removal.
//!
//! Every variant is built from the same base, with fixed contents and mtimes,
//! so the same variant created on two machines compares as identical.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

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
}

/// Creates the tree at `root`, which must not exist yet.
pub fn create(root: &Path, variant: Variant) -> Result<()> {
    fs::create_dir(root).with_context(|| format!("create {}", root.display()))?;
    base(root).with_context(|| format!("create the fixture in {}", root.display()))?;
    match variant {
        Variant::Base => Ok(()),
        Variant::Changed => changed(root),
        Variant::Nanoseconds => set_mtime(&root.join("file.txt"), 1, 987_654_321),
    }
    .with_context(|| format!("change the fixture in {}", root.display()))
}

/// Removes the tree at `root`, if it is a fixture.
pub fn remove(root: &Path) -> Result<()> {
    if !root.join(MARKER).is_file() {
        bail!("{} is not a fixture: it has no {MARKER}", root.display());
    }
    fs::remove_dir_all(root).with_context(|| format!("remove {}", root.display()))
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
    mkfifo(&root.join("fifo"))
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

fn write(path: &Path, content: &[u8], seconds: u64, nanoseconds: u32) -> io::Result<()> {
    fs::write(path, content)?;
    set_mtime(path, seconds, nanoseconds)
}

/// Sets an mtime `seconds` and `nanoseconds` after [`BASE_TIME`].
fn set_mtime(path: &Path, seconds: u64, nanoseconds: u32) -> io::Result<()> {
    let time = SystemTime::UNIX_EPOCH + Duration::new(BASE_TIME + seconds, nanoseconds);
    File::options().write(true).open(path)?.set_modified(time)
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
        for variant in [Variant::Base, Variant::Changed, Variant::Nanoseconds] {
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
}
