//! The files a dev build is made from, and the hash that identifies it.
//!
//! `build.rs` includes this file too, so the hash a binary is compiled with and
//! the hash the client computes before uploading its source come from the same
//! code and cannot drift apart.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Files at the crate root that a build depends on. `rust-toolchain.toml` is
/// among them so that a host builds with the same toolchain.
pub const ROOT_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain.toml",
];

/// The directory whose every file is part of the source.
pub const SOURCE_DIR: &str = "src";

/// The source files under `root`, relative to it, in a fixed order.
pub fn source_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = ROOT_FILES
        .iter()
        .map(PathBuf::from)
        .filter(|file| root.join(file).is_file())
        .collect();
    collect(root, Path::new(SOURCE_DIR), &mut files)?;
    files.sort();
    Ok(files)
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(dir))? {
        let entry = entry?;
        let path = dir.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect(root, &path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

/// A short hex hash over each file's path, length, and content, in order.
pub fn source_hash(root: &Path, files: &[PathBuf]) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    for file in files {
        let path = file.as_os_str().as_bytes();
        let content = fs::read(root.join(file))?;
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path);
        hasher.update(&(content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    Ok(hasher.finalize().to_hex()[..20].to_owned())
}
