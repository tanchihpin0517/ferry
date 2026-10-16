//! Trees for `ferry test --scale` and benchmarks, made the same on every
//! machine: the same names, sizes, and bytes, from a fixed seed.
//!
//! A tree is of a kind, and of a size: the bytes of its files in all. Each
//! kind is shaped as such data tends to be. File sizes are spread as they are
//! on real filesystems, on a log-normal curve with a long tail, with some
//! files empty; directories hold more files or fewer, and nest more or less
//! deeply, most of them holding few and a few many; some files have the same
//! bytes as another, and some are symlinks.
//!
//! Content is of three classes: text, words that compress about as prose
//! does; random bytes, which do not compress at all; and blocks of the two
//! in turn, which compress by about half, as binaries and checkpoints do.
//!
//! A tree is a fixture: it has the marker, so that it can be removed, and
//! [`change`] changes it as a user would between two syncs.

use std::f64::consts::TAU;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::ValueEnum;

use super::fixture::{self, MARKER, stopping};

/// The mtime [`change`] gives what it changes: long past, so that it differs
/// from whatever a sync just left, even within the same second.
const CHANGED_TIME: u64 = 1_500_000_000;

/// Which data a tree is shaped as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    /// Small files, as source code, packages and environments, or a dataset
    /// of small images are: a few KiB each, some empty, in deep trees of
    /// directories, mostly text, a tenth with the same bytes as another.
    Small,
    /// Large files, as checkpoints, videos, and archives are: 64 to 256 MiB
    /// each, mostly random bytes, with a few small text files beside them,
    /// in a shallow tree. Larger would take longer, and show nothing more: a
    /// file this large is sent and written at a steady pace.
    Large,
    /// A project or a home directory: small files in `code/`, a tenth of the
    /// bytes; files of all sizes in `docs/`, three tenths; and large files in
    /// `data/`, the rest.
    Mixed,
}

/// What a tree holds.
#[derive(Default)]
pub struct Generated {
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub bytes: u64,
}

impl fmt::Display for Generated {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use crate::progress::{counted, size};
        write!(
            f,
            "{} in {}, ",
            counted(self.files, "file", "files"),
            counted(self.directories, "directory", "directories"),
        )?;
        if self.symlinks > 0 {
            write!(f, "{}, ", counted(self.symlinks, "symlink", "symlinks"))?;
        }
        write!(f, "{}", size(self.bytes))
    }
}

/// Makes a tree of `kind` at `root`, which must not exist yet, with `bytes`
/// in its files in all.
pub fn generate(root: &Path, kind: Kind, bytes: u64) -> Result<Generated> {
    fixture::mark(root)?;
    let mut generated = Generated::default();
    match kind {
        Kind::Small => fill(root, &SMALL, bytes, 1, &mut generated)?,
        Kind::Large => fill(root, &LARGE, bytes, 2, &mut generated)?,
        Kind::Mixed => {
            let code = bytes / 10;
            let docs = bytes / 10 * 3;
            for (name, profile, share, seed) in [
                ("code", &SMALL, code, 1),
                ("docs", &MEDIUM, docs, 3),
                ("data", &LARGE, bytes - code - docs, 2),
            ] {
                let directory = root.join(name);
                fs::create_dir(&directory)?;
                generated.directories += 1;
                fill(&directory, profile, share, seed, &mut generated)?;
            }
        }
    }
    Ok(generated)
}

/// How a kind of data is shaped.
struct Profile {
    /// The parts files are drawn from.
    parts: &'static [Part],
    /// Of files, per thousand: empty ones, ones with another's bytes, and
    /// symlinks to another in the same directory.
    empty: u64,
    duplicate: u64,
    symlink: u64,
    /// Files in a directory: their median, and how widely they spread.
    files_median: f64,
    files_spread: f64,
    /// Subdirectories of a directory at the top, on average; each level down
    /// has 0.7 times as many.
    subdirectories: f64,
    /// How deep directories nest, below the top.
    deepest: u32,
}

/// Files of one sort: their sizes, log-normal around a median, and their
/// content.
struct Part {
    /// Of files, per hundred.
    share: u64,
    median: f64,
    /// The standard deviation of the size's natural logarithm.
    spread: f64,
    smallest: u64,
    largest: u64,
    /// Of files, per hundred: text, and blocks of text and random bytes in
    /// turn. The rest are random bytes.
    text: u64,
    blocks: u64,
}

const KIB: f64 = 1024.0;
const MIB: u64 = 1 << 20;

const SMALL: Profile = Profile {
    parts: &[Part {
        share: 100,
        median: 4.0 * KIB,
        spread: 1.5,
        smallest: 1,
        largest: 8 * MIB,
        text: 70,
        blocks: 20,
    }],
    empty: 30,
    duplicate: 100,
    symlink: 5,
    files_median: 8.0,
    files_spread: 1.2,
    subdirectories: 4.0,
    deepest: 10,
};

const MEDIUM: Profile = Profile {
    parts: &[Part {
        share: 100,
        median: 64.0 * KIB,
        spread: 2.0,
        smallest: 1,
        largest: 256 * MIB,
        text: 40,
        blocks: 20,
    }],
    empty: 10,
    duplicate: 20,
    symlink: 5,
    files_median: 12.0,
    files_spread: 1.0,
    subdirectories: 3.0,
    deepest: 6,
};

const LARGE: Profile = Profile {
    parts: &[
        // Configurations, notes, and checksums beside the large files.
        Part {
            share: 60,
            median: 2.0 * KIB,
            spread: 1.0,
            smallest: 1,
            largest: MIB,
            text: 100,
            blocks: 0,
        },
        Part {
            share: 40,
            median: 128.0 * KIB * KIB,
            spread: 0.5,
            smallest: 64 * MIB,
            largest: 256 * MIB,
            text: 0,
            blocks: 10,
        },
    ],
    empty: 0,
    duplicate: 0,
    symlink: 0,
    files_median: 4.0,
    files_spread: 0.5,
    subdirectories: 3.0,
    deepest: 2,
};

/// What a file's bytes are.
#[derive(Clone, Copy)]
enum Content {
    Text,
    Blocks,
    Random,
}

impl Content {
    /// An extension for a file of this content, as such files tend to have.
    fn extension(self, random: &mut Random) -> &'static str {
        let choices: &[&str] = match self {
            Content::Text => &["py", "txt", "json", "md", "c", "yaml", "csv", "log"],
            Content::Blocks => &["bin", "so", "pyc", "pt", "o"],
            Content::Random => &["jpg", "png", "gz", "zip", "mp4", "npz"],
        };
        choices[(random.next() % choices.len() as u64) as usize]
    }
}

/// A directory being filled, depth first.
struct Directory {
    path: PathBuf,
    depth: u32,
    files_left: u64,
    subdirectories_left: u64,
    subdirectories: u64,
    /// The last file made in it, for a symlink to point to.
    last_file: Option<String>,
}

/// Fills `root` with files of `profile`, `bytes` in all, adding what it made
/// to `generated`. Directories are made depth first, each with a number of
/// files and of subdirectories drawn as it is made; once the top one has
/// made all its own, it makes another subdirectory, until the bytes are
/// reached.
fn fill(
    root: &Path,
    profile: &Profile,
    bytes: u64,
    seed: u64,
    generated: &mut Generated,
) -> Result<()> {
    let mut random = Random(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
    let mut written = 0;
    let mut stack = vec![directory(root.to_path_buf(), 0, profile, &mut random)];
    // Recent files, for a duplicate to copy: their paths and sizes.
    let mut recent: Vec<(PathBuf, u64)> = Vec::new();
    let mut index = 0u64;
    while written < bytes {
        stopping()?;
        let nested = stack.len() > 1;
        let top = stack.last_mut().expect("the root stays");
        if top.files_left == 0 {
            if top.subdirectories_left == 0 {
                if nested {
                    stack.pop();
                    continue;
                }
                top.subdirectories_left = 1;
            }
            top.subdirectories_left -= 1;
            top.subdirectories += 1;
            let path = top.path.join(format!("d{}", top.subdirectories));
            let depth = top.depth + 1;
            fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
            generated.directories += 1;
            stack.push(directory(path, depth, profile, &mut random));
            continue;
        }
        top.files_left -= 1;
        index += 1;

        let draw = random.next() % 1000;
        if draw < profile.symlink
            && let Some(target) = &top.last_file
        {
            symlink(target, top.path.join(format!("link{index}")))?;
            generated.symlinks += 1;
            continue;
        }
        if draw < profile.symlink + profile.empty {
            let name = format!("f{index}.py");
            File::create(top.path.join(&name))?;
            top.last_file = Some(name);
            generated.files += 1;
            continue;
        }
        let left = bytes - written;
        if draw < profile.symlink + profile.empty + profile.duplicate
            && let Some((source, size)) =
                recent.iter().rev().find(|(_, size)| *size <= left).cloned()
        {
            let extension = source.extension().unwrap_or_default().to_string_lossy();
            let name = format!("f{index}.{extension}");
            fs::copy(&source, top.path.join(&name))
                .with_context(|| format!("copy {}", source.display()))?;
            top.last_file = Some(name);
            generated.files += 1;
            generated.bytes += size;
            written += size;
            continue;
        }

        let part = choose(profile.parts, &mut random);
        let size = part.size(&mut random).min(left);
        let content = match random.next() % 100 {
            draw if draw < part.text => Content::Text,
            draw if draw < part.text + part.blocks => Content::Blocks,
            _ => Content::Random,
        };
        let name = format!("f{index}.{}", content.extension(&mut random));
        let path = top.path.join(&name);
        write(&path, size, content, &mut random)?;
        top.last_file = Some(name);
        if recent.len() == 64 {
            recent.remove(0);
        }
        recent.push((path, size));
        generated.files += 1;
        generated.bytes += size;
        written += size;
    }
    Ok(())
}

/// A directory of `profile` at `depth`, with its numbers drawn.
fn directory(path: PathBuf, depth: u32, profile: &Profile, random: &mut Random) -> Directory {
    let files = profile.files_median * (profile.files_spread * random.normal()).exp();
    let subdirectories = if depth < profile.deepest {
        // Exponential, with the mean for this depth.
        let mean = profile.subdirectories * 0.7f64.powi(depth as i32);
        -(1.0 - random.unit()).ln() * mean
    } else {
        0.0
    };
    Directory {
        path,
        depth,
        files_left: files.round() as u64,
        subdirectories_left: subdirectories as u64,
        subdirectories: 0,
        last_file: None,
    }
}

/// A part, each as likely as its share.
fn choose<'a>(parts: &'a [Part], random: &mut Random) -> &'a Part {
    let mut draw = random.next() % parts.iter().map(|part| part.share).sum::<u64>();
    for part in parts {
        if draw < part.share {
            return part;
        }
        draw -= part.share;
    }
    unreachable!("the draw is below the shares' sum")
}

impl Part {
    fn size(&self, random: &mut Random) -> u64 {
        let size = self.median * (self.spread * random.normal()).exp();
        (size as u64).clamp(self.smallest, self.largest)
    }
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
        use crate::progress::{count, counted, size};
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
        write!(
            f,
            "{}, {} added",
            counted(self.added.files, "file", "files"),
            size(self.added.bytes)
        )
    }
}

/// Changes a tree [`generate`] made, as a user would between two syncs:
/// removes the last directory below the root with files in it and no
/// directories, unless it has all the files; then, of the
/// files left in order, rewrites one in a hundred with other bytes of the
/// same size, gives another only an older mtime, and deletes another; and
/// adds about 1% more in `added/`, small files and large.
pub fn change(root: &Path) -> Result<Changes> {
    let mut files = Vec::new();
    let mut leaves = Vec::new();
    list(root, &mut files, &mut leaves)?;
    files.retain(|file| file != &root.join(MARKER));
    files.sort();
    let mut bytes = 0;
    for file in &files {
        bytes += fs::metadata(file)?.len();
    }

    leaves.sort();
    let last_leaf = leaves
        .into_iter()
        .rev()
        .find(|leaf| leaf != root && files.iter().any(|file| file.starts_with(leaf)));
    let removed = match last_leaf {
        // Only when files are left elsewhere, to sync something else too.
        Some(directory) if files.iter().any(|file| !file.starts_with(&directory)) => {
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
                let size = fs::metadata(file)?.len();
                write(file, size, Content::Random, &mut random)?;
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
    let added_part = Part {
        share: 100,
        median: 16.0 * KIB,
        spread: 1.5,
        smallest: 1,
        largest: 16 * MIB,
        text: 50,
        blocks: 0,
    };
    let wanted = (bytes / 100).max(1);
    let mut added = Generated {
        directories: 1,
        ..Generated::default()
    };
    while added.bytes < wanted {
        stopping()?;
        let size = added_part.size(&mut random).min(wanted - added.bytes);
        let content = if added.files.is_multiple_of(2) {
            Content::Text
        } else {
            Content::Random
        };
        write(
            &added_root.join(format!("f{}", added.files)),
            size,
            content,
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

/// Every regular file under `directory`, at any depth, and every directory
/// with no directories in it, `directory` too if so.
fn list(directory: &Path, files: &mut Vec<PathBuf>, leaves: &mut Vec<PathBuf>) -> Result<()> {
    let mut leaf = true;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            leaf = false;
            list(&entry.path(), files, leaves)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    if leaf {
        leaves.push(directory.to_path_buf());
    }
    Ok(())
}

/// Writes `size` bytes of `content`.
fn write(path: &Path, size: u64, content: Content, random: &mut Random) -> Result<()> {
    const WORDS: [&str; 32] = [
        "the", "of", "and", "a", "to", "in", "is", "you", "that", "it", "he", "was", "for", "on",
        "are", "as", "with", "his", "they", "at", "be", "this", "have", "from", "or", "one", "had",
        "by", "word", "but", "not", "what",
    ];
    /// How long a block of one class is, in blocks of text and random bytes.
    const BLOCK: u64 = 4096;
    let mut file = BufWriter::with_capacity(
        1 << 20,
        File::create(path).with_context(|| format!("create {}", path.display()))?,
    );
    let mut written = 0;
    let mut chunk = Vec::with_capacity(BLOCK as usize);
    while written < size {
        chunk.clear();
        let want = (size - written).min(BLOCK) as usize;
        let text = match content {
            Content::Text => true,
            Content::Random => false,
            Content::Blocks => (written / BLOCK).is_multiple_of(2),
        };
        if text {
            while chunk.len() < want {
                let next = random.next();
                chunk.extend_from_slice(WORDS[(next % 32) as usize].as_bytes());
                chunk.push(if next >> 60 == 0 { b'\n' } else { b' ' });
            }
        } else {
            while chunk.len() < want {
                chunk.extend_from_slice(&random.next().to_le_bytes());
            }
        }
        chunk.truncate(want);
        file.write_all(&chunk)?;
        written += want as u64;
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

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal, by the Box-Muller transform.
    fn normal(&mut self) -> f64 {
        let (u, v) = (1.0 - self.unit(), self.unit());
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// Every entry under `root` but the marker: its path, and for a file
    /// its bytes, for a symlink its target.
    fn entries(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(root: &Path, directory: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                let kind = fs::symlink_metadata(&path).unwrap().file_type();
                if kind.is_symlink() {
                    let target = fs::read_link(&path).unwrap();
                    entries.push((relative, target.into_os_string().into_encoded_bytes()));
                } else if kind.is_dir() {
                    entries.push((relative, Vec::new()));
                    walk(root, &path, entries);
                } else if relative != Path::new(MARKER) {
                    entries.push((relative, fs::read(&path).unwrap()));
                }
            }
        }
        let mut entries = Vec::new();
        walk(root, root, &mut entries);
        entries.sort();
        entries
    }

    #[test]
    fn the_same_tree_every_time_of_the_size_asked_for() {
        let temp = tempfile::tempdir().unwrap();
        for (kind, bytes) in [
            (Kind::Small, 4 * MIB),
            (Kind::Large, 70 * MIB),
            (Kind::Mixed, 90 * MIB),
        ] {
            let (a, b) = (
                temp.path().join(format!("{kind:?}-a")),
                temp.path().join(format!("{kind:?}-b")),
            );
            let generated = generate(&a, kind, bytes).unwrap();
            generate(&b, kind, bytes).unwrap();
            assert_eq!(generated.bytes, bytes, "{kind:?}");
            let made = entries(&a);
            assert_eq!(made, entries(&b), "{kind:?}");
            let files: u64 = made
                .iter()
                .filter(|(path, _)| fs::symlink_metadata(a.join(path)).unwrap().is_file())
                .map(|(_, content)| content.len() as u64)
                .sum();
            assert_eq!(files, bytes, "{kind:?}");
            fixture::remove(&a).unwrap();
            fixture::remove(&b).unwrap();
        }
    }

    #[test]
    fn small_files_are_small_nested_and_some_empty_or_alike() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        let generated = generate(&root, Kind::Small, 32 * MIB).unwrap();
        let mut sizes = Vec::new();
        let mut deepest = 0;
        let mut files = Vec::new();
        list(&root, &mut files, &mut Vec::new()).unwrap();
        for file in &files {
            sizes.push(fs::metadata(file).unwrap().size());
            deepest = deepest.max(file.strip_prefix(&root).unwrap().components().count());
        }
        sizes.sort();
        let median = sizes[sizes.len() / 2];
        assert!((2 << 10..8 << 10).contains(&median), "median {median}");
        assert!(sizes[0] == 0, "no empty file");
        assert!(deepest >= 4, "only {deepest} deep");
        assert!(generated.symlinks > 0);
        let mut contents: Vec<Vec<u8>> = files
            .iter()
            .filter(|file| fs::metadata(file).unwrap().size() > 0)
            .map(|file| fs::read(file).unwrap())
            .collect();
        let all = contents.len();
        contents.sort();
        contents.dedup();
        assert!(contents.len() < all, "no file has another's bytes");
    }

    #[test]
    fn large_files_are_large_and_mixed_has_all() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        generate(&root, Kind::Mixed, 300 * MIB).unwrap();
        for part in ["code", "docs", "data"] {
            assert!(root.join(part).is_dir(), "{part}");
        }
        let mut files = Vec::new();
        list(&root.join("data"), &mut files, &mut Vec::new()).unwrap();
        let largest = files
            .iter()
            .map(|file| fs::metadata(file).unwrap().size())
            .max()
            .unwrap();
        assert!(largest >= 64 * MIB, "largest {largest}");
    }

    #[test]
    fn a_change_rewrites_touches_deletes_removes_and_adds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        // Enough for files in more than one directory, and more than three
        // left.
        generate(&root, Kind::Small, 4 * MIB).unwrap();
        let changes = change(&root).unwrap();
        assert!(changes.rewritten > 0 && changes.touched > 0 && changes.deleted > 0);
        assert!(changes.removed.is_some());
        // In a mixed tree, a directory deep down, not a part of it.
        let mixed = temp.path().join("mixed");
        generate(&mixed, Kind::Mixed, 90 * MIB).unwrap();
        let removed = change(&mixed).unwrap().removed.unwrap();
        assert!(Path::new(&removed).components().count() > 1, "{removed}");
        for part in ["code", "docs", "data"] {
            assert!(mixed.join(part).is_dir(), "{part}");
        }
        assert!(changes.added.files > 0 && root.join("added/f0").is_file());
    }
}
