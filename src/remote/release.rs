//! A release's binary for a host, downloaded from GitHub Releases.
//!
//! A release publishes one binary per platform, `ferry-<version>-<target>`,
//! and a `SHA256SUMS` of them all. The client downloads the one for a host's
//! platform with `curl`, checks it against `SHA256SUMS`, and keeps both in
//! `releases/<version>/` in Ferry's directory on this machine, so that a
//! second host of the same platform costs no download. A kept binary is
//! checked again before each use.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::build_info::VERSION;
use crate::home;

/// The list of checksums each release publishes.
const SUMS: &str = "SHA256SUMS";

/// The target a release builds for a host that `uname -s` and `uname -m`
/// describe as `os` and `arch`.
pub fn target(os: &str, arch: &str) -> Result<&'static str> {
    Ok(match (os, arch) {
        ("Linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("Linux", "aarch64" | "arm64") => "aarch64-unknown-linux-musl",
        ("Darwin", "x86_64") => "x86_64-apple-darwin",
        ("Darwin", "arm64" | "aarch64") => "aarch64-apple-darwin",
        _ => bail!("Ferry has no release for {os} on {arch}; use --remote-ferry"),
    })
}

/// The name of a release's binary for `target`.
fn asset(target: &str) -> String {
    format!("ferry-{VERSION}-{target}")
}

/// Where this version's release files are published.
fn base_url() -> String {
    format!(
        "{}/releases/download/v{VERSION}",
        env!("CARGO_PKG_REPOSITORY")
    )
}

/// This version's binary for `target`, checked against `SHA256SUMS`,
/// downloaded first unless it is kept on this machine.
pub fn binary(target: &str) -> Result<PathBuf> {
    let dir = home::local_home()?.join("releases").join(VERSION);
    fetch(&base_url(), &dir, &asset(target))
}

/// `name` in `dir`, downloaded with `SHA256SUMS` from `base_url` unless the
/// kept copy matches its checksum.
fn fetch(base_url: &str, dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    let sums = dir.join(SUMS);
    if path.exists() && sums.exists() && verify(&sums, &path, name).is_ok() {
        return Ok(path);
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    eprintln!("ferry: downloading {name}");
    download(&format!("{base_url}/{SUMS}"), &sums)?;
    download(&format!("{base_url}/{name}"), &path)?;
    verify(&sums, &path, name)?;
    Ok(path)
}

/// Downloads `url` to `path` with `curl`, through a temporary file, so that a
/// download cut short leaves nothing at `path`.
fn download(url: &str, path: &Path) -> Result<()> {
    let partial = path.with_extension(format!("part-{}", std::process::id()));
    let status = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        // GitHub redirects a release download to its CDN; never to plain HTTP.
        .args(["--proto-redir", "=https", "--output"])
        .arg(&partial)
        .arg(url)
        .stdin(Stdio::null())
        .status();
    let status = match status {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("downloading a release needs curl, which is not installed")
        }
        Err(error) => return Err(error).context("run curl"),
    };
    if !status.success() {
        let _ = fs::remove_file(&partial);
        bail!("could not download {url}: curl exited with {status}");
    }
    fs::rename(&partial, path).with_context(|| format!("keep {}", path.display()))
}

/// Checks `path` against the checksum `sums` lists for `name`.
fn verify(sums: &Path, path: &Path, name: &str) -> Result<()> {
    let text = fs::read_to_string(sums).with_context(|| format!("read {}", sums.display()))?;
    let Some(expected) = checksum(&text, name) else {
        bail!("{SUMS} lists no {name}");
    };
    let actual = sha256(path)?;
    if actual != expected {
        let _ = fs::remove_file(path);
        bail!("{name} does not match its checksum in {SUMS}: {actual}, not {expected}");
    }
    Ok(())
}

/// The checksum `sums`, in the format `sha256sum` writes, lists for `name`.
fn checksum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        let file = file.trim_start();
        // `sha256sum --binary` marks each file with `*`.
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name).then(|| sum.to_ascii_lowercase())
    })
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_map_to_targets() {
        assert_eq!(
            target("Linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            target("Linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
        assert_eq!(target("Darwin", "arm64").unwrap(), "aarch64-apple-darwin");
        assert_eq!(target("Darwin", "x86_64").unwrap(), "x86_64-apple-darwin");
        let error = target("FreeBSD", "amd64").unwrap_err().to_string();
        assert!(error.contains("--remote-ferry"), "{error}");
    }

    #[test]
    fn checksums_are_found_by_name() {
        let sums = "AB12  ferry-1-a\ncd34 *ferry-1-b\n";
        assert_eq!(checksum(sums, "ferry-1-a").as_deref(), Some("ab12"));
        assert_eq!(checksum(sums, "ferry-1-b").as_deref(), Some("cd34"));
        assert_eq!(checksum(sums, "ferry-1"), None);
    }

    #[test]
    fn sha256_of_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("abc");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A release served from a directory, through `file://` URLs.
    fn served(binary: &[u8], listed: &[u8]) -> tempfile::TempDir {
        let server = tempfile::tempdir().unwrap();
        fs::write(server.path().join("ferry-1-t"), binary).unwrap();
        let listed_path = server.path().join("listed");
        fs::write(&listed_path, listed).unwrap();
        let sum = sha256(&listed_path).unwrap();
        fs::write(server.path().join(SUMS), format!("{sum}  ferry-1-t\n")).unwrap();
        server
    }

    #[test]
    fn a_release_is_downloaded_checked_and_kept() {
        let server = served(b"binary", b"binary");
        let base = format!("file://{}", server.path().display());
        let cache = tempfile::tempdir().unwrap();
        let path = fetch(&base, cache.path(), "ferry-1-t").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"binary");

        // Kept: found again with the server gone.
        drop(server);
        assert_eq!(fetch(&base, cache.path(), "ferry-1-t").unwrap(), path);
    }

    #[test]
    fn a_binary_that_does_not_match_is_refused() {
        let server = served(b"tampered", b"binary");
        let base = format!("file://{}", server.path().display());
        let cache = tempfile::tempdir().unwrap();
        let error = fetch(&base, cache.path(), "ferry-1-t")
            .unwrap_err()
            .to_string();
        assert!(error.contains("checksum"), "{error}");
        assert!(!cache.path().join("ferry-1-t").exists());
    }

    #[test]
    fn a_missing_release_fails() {
        let server = tempfile::tempdir().unwrap();
        let base = format!("file://{}", server.path().display());
        let cache = tempfile::tempdir().unwrap();
        let error = fetch(&base, cache.path(), "ferry-1-t")
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not download"), "{error}");
    }
}
