//! Trees for benchmarks, made the same on every machine: the same names,
//! sizes, and bytes, from a fixed seed.
//!
//! Content is of two kinds: text, words that compress about as prose does,
//! and random bytes, which do not compress at all.
//!
//! A tree of any size, for `ferry test --scale`, is a fixture: it has the
//! marker, so that it can be removed, and [`change`] changes it as a user
//! would between two syncs.

use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::ValueEnum;

use super::fixture::{self, MARKER, stopping};

/// The mtime [`change`] gives what it changes: long past, so that it differs
/// from whatever a sync just left, even within the same second.
const CHANGED_TIME: u64 = 1_500_000_000;

/// Which tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    /// 100,000 files of 1 KiB of text, 100 to a directory.
    Small,
    /// 8 files of 128 MiB: 4 of text, 4 of random bytes.
    Large,
    /// 10,000 files, mostly small, some up to 16 MiB, about 1 GiB in all,
    /// half text and half random, in nested directories.
    Mixed,
}

/// Makes the tree at `root`, which must not exist yet.
pub fn generate(root: &Path, kind: Kind) -> Result<()> {
    fs::create_dir(root).with_context(|| format!("create {}", root.display()))?;
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    match kind {
        Kind::Small => {
            for index in 0..100_000u32 {
                let directory = root.join(format!("d{:02}/e{}", index / 10_000, index / 100 % 100));
                if index % 100 == 0 {
                    fs::create_dir_all(&directory)?;
                }
                write(
                    &directory.join(format!("f{index}.txt")),
                    1024,
                    true,
                    &mut random,
                )?;
            }
        }
        Kind::Large => {
            for index in 0..8u32 {
                let text = index % 2 == 0;
                let name = format!("{}{index}", if text { "text" } else { "random" });
                write(&root.join(name), 128 << 20, text, &mut random)?;
            }
        }
        Kind::Mixed => {
            for index in 0..10_000u32 {
                let directory = root.join(format!(
                    "a{}/b{}/c{}",
                    index / 2_500,
                    index / 250 % 10,
                    index / 50 % 5
                ));
                if index.is_multiple_of(50) {
                    fs::create_dir_all(&directory)?;
                }
                let size = mixed_size(&mut random);
                let text = index % 2 == 0;
                write(
                    &directory.join(format!("f{index}")),
                    size,
                    text,
                    &mut random,
                )?;
            }
        }
    }
    Ok(())
}

/// What [`scaled`] made.
pub struct Generated {
    pub files: u64,
    pub bytes: u64,
}

impl fmt::Display for Generated {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}, {}",
            crate::progress::counted(self.files, "file", "files"),
            crate::progress::size(self.bytes)
        )
    }
}

/// Makes a tree of `bytes` in all at `root`, which must not exist yet: files
/// sized as in the mixed tree, half text and half random, 50 to a directory,
/// nested three deep.
pub fn scaled(root: &Path, bytes: u64) -> Result<Generated> {
    fixture::mark(root)?;
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut generated = Generated { files: 0, bytes: 0 };
    let mut index = 0u64;
    while generated.bytes < bytes {
        stopping()?;
        let directory = root.join(format!(
            "a{}/b{}/c{}",
            index / 2_500,
            index / 250 % 10,
            index / 50 % 5
        ));
        if index.is_multiple_of(50) {
            fs::create_dir_all(&directory)?;
        }
        let size = (mixed_size(&mut random) as u64).min(bytes - generated.bytes);
        write(
            &directory.join(format!("f{index}")),
            size as usize,
            index.is_multiple_of(2),
            &mut random,
        )?;
        generated.files += 1;
        generated.bytes += size;
        index += 1;
    }
    Ok(generated)
}

/// What [`change`] did.
pub struct Changes {
    pub rewritten: u64,
    pub touched: u64,
    pub deleted: u64,
    pub removed: Option<String>,
    pub added: Generated,
}

impl fmt::Display for Changes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use crate::progress::count;
        write!(
            f,
            "{} rewritten, {} touched, {} deleted, ",
            count(self.rewritten),
            count(self.touched),
            count(self.deleted)
        )?;
        if let Some(directory) = &self.removed {
            write!(f, "{directory}/ removed, ")?;
        }
        write!(f, "{} added", self.added)
    }
}

/// Changes a tree [`scaled`] made, as a user would between two syncs: removes
/// the last directory of files, unless it has them all; then, of the files left in order, rewrites
/// one in a hundred with other bytes of the same size, gives another only an
/// older mtime, and deletes another; and adds about 1% more in `added/`.
pub fn change(root: &Path) -> Result<Changes> {
    let mut files = Vec::new();
    list_files(root, &mut files)?;
    files.retain(|file| file != &root.join(MARKER));
    files.sort();
    let mut bytes = 0;
    for file in &files {
        bytes += fs::metadata(file)?.len();
    }

    let last_directory = files
        .last()
        .and_then(|file| file.parent())
        .map(Path::to_path_buf);
    let removed = match last_directory {
        // Only when files are left elsewhere, to sync something else too.
        Some(directory)
            if directory != root && files.iter().any(|file| !file.starts_with(&directory)) =>
        {
            fs::remove_dir_all(&directory)
                .with_context(|| format!("remove {}", directory.display()))?;
            files.retain(|file| !file.starts_with(&directory));
            Some(
                directory
                    .strip_prefix(root)
                    .expect("inside the root")
                    .display()
                    .to_string(),
            )
        }
        _ => None,
    };

    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(CHANGED_TIME);
    let mut random = Random(0x0123_4567_89ab_cdef);
    let (mut rewritten, mut touched, mut deleted) = (0, 0, 0);
    for (index, file) in files.iter().enumerate() {
        stopping()?;
        match index % 100 {
            0 => {
                let size = fs::metadata(file)?.len() as usize;
                write(file, size, false, &mut random)?;
                File::options().write(true).open(file)?.set_modified(old)?;
                rewritten += 1;
            }
            1 => {
                File::options().write(true).open(file)?.set_modified(old)?;
                touched += 1;
            }
            2 => {
                fs::remove_file(file).with_context(|| format!("remove {}", file.display()))?;
                deleted += 1;
            }
            _ => {}
        }
    }

    let added_root = root.join("added");
    fs::create_dir(&added_root)?;
    let wanted = (bytes / 100).max(1);
    let mut added = Generated { files: 0, bytes: 0 };
    while added.bytes < wanted {
        stopping()?;
        let size = (mixed_size(&mut random) as u64).min(wanted - added.bytes);
        write(
            &added_root.join(format!("f{}", added.files)),
            size as usize,
            added.files.is_multiple_of(2),
            &mut random,
        )?;
        added.files += 1;
        added.bytes += size;
    }
    Ok(Changes {
        rewritten,
        touched,
        deleted,
        removed,
        added,
    })
}

/// Every file under `directory`, at any depth.
fn list_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            list_files(&entry.path(), files)?;
        } else {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// A size for the mixed tree: most files small, a fifth between 16 KiB and
/// 1 MiB, and one in a hundred between 1 and 16 MiB, each spread evenly on a
/// logarithmic scale.
fn mixed_size(random: &mut Random) -> usize {
    let (low, high) = match random.next() % 100 {
        0 => (1 << 20, 16 << 20),
        1..=20 => (16 << 10, 1 << 20),
        _ => (1 << 10, 16 << 10),
    };
    let fraction = (random.next() >> 11) as f64 / (1u64 << 53) as f64;
    let (low, high) = (f64::from(low).ln(), f64::from(high).ln());
    (low + fraction * (high - low)).exp() as usize
}

/// Writes `size` bytes of text or of random bytes.
fn write(path: &Path, size: usize, text: bool, random: &mut Random) -> Result<()> {
    const WORDS: [&str; 32] = [
        "the", "of", "and", "a", "to", "in", "is", "you", "that", "it", "he", "was", "for", "on",
        "are", "as", "with", "his", "they", "at", "be", "this", "have", "from", "or", "one", "had",
        "by", "word", "but", "not", "what",
    ];
    let mut file = BufWriter::with_capacity(
        1 << 20,
        File::create(path).with_context(|| format!("create {}", path.display()))?,
    );
    let mut written = 0;
    let mut chunk = Vec::with_capacity(1 << 16);
    while written < size {
        chunk.clear();
        let want = (size - written).min(1 << 16);
        if text {
            while chunk.len() < want {
                let next = random.next();
                chunk.extend_from_slice(WORDS[(next % 32) as usize].as_bytes());
                chunk.push(if next >> 60 == 0 { b'\n' } else { b' ' });
            }
            chunk.truncate(want);
        } else {
            while chunk.len() < want {
                chunk.extend_from_slice(&random.next().to_le_bytes());
            }
            chunk.truncate(want);
        }
        file.write_all(&chunk)?;
        written += want;
    }
    file.flush()?;
    Ok(())
}

/// xorshift64*: fast, and the same everywhere.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_bytes_every_time_and_the_sizes_asked_for() {
        let temp = tempfile::tempdir().unwrap();
        for (name, text) in [("a", true), ("b", true), ("c", false)] {
            write(&temp.path().join(name), 70_001, text, &mut Random(1)).unwrap();
            assert_eq!(fs::metadata(temp.path().join(name)).unwrap().len(), 70_001);
        }
        let read = |name: &str| fs::read(temp.path().join(name)).unwrap();
        assert_eq!(read("a"), read("b"));
        assert_ne!(read("a"), read("c"));
        let mut random = Random(7);
        let sizes: Vec<usize> = (0..1_000).map(|_| mixed_size(&mut random)).collect();
        assert!(sizes.iter().all(|size| (1 << 10..=16 << 20).contains(size)));
    }

    #[test]
    fn a_scaled_tree_is_as_large_as_asked_and_the_same_every_time() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (temp.path().join("a"), temp.path().join("b"));
        let generated = scaled(&a, 3 << 20).unwrap();
        assert_eq!(generated.bytes, 3 << 20);
        scaled(&b, 3 << 20).unwrap();
        let mut files = Vec::new();
        list_files(&a, &mut files).unwrap();
        assert_eq!(files.len() as u64, generated.files + 1);
        let total: u64 = files
            .iter()
            .map(|file| fs::metadata(file).unwrap().len())
            .sum();
        assert_eq!(total, (3 << 20) + MARKER_SIZE);
        for file in &files {
            let other = b.join(file.strip_prefix(&a).unwrap());
            assert_eq!(fs::read(file).unwrap(), fs::read(other).unwrap());
        }
        fixture::remove(&a).unwrap();
    }

    #[test]
    fn a_change_rewrites_touches_deletes_removes_and_adds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        // Enough for files in more than one directory, and more than three
        // left.
        scaled(&root, 32 << 20).unwrap();
        let changes = change(&root).unwrap();
        assert!(changes.rewritten > 0 && changes.touched > 0 && changes.deleted > 0);
        assert!(changes.removed.is_some());
        assert!(changes.added.files > 0 && root.join("added/f0").is_file());
    }

    const MARKER_SIZE: u64 = b"ferry test fixture\n".len() as u64;
}
