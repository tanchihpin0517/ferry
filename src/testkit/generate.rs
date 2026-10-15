//! Trees for benchmarks, made the same on every machine: the same names,
//! sizes, and bytes, from a fixed seed.
//!
//! Content is of two kinds: text, words that compress about as prose does,
//! and random bytes, which do not compress at all.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;

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
                if index % 50 == 0 {
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
}
