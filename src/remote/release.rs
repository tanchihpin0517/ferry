//! A release's binary for a host, downloaded from GitHub Releases.
//!
//! A release publishes one binary per platform, `ferry-<version>-<target>`,
//! and a `SHA256SUMS` of them all. The client downloads the one for a host's
//! platform with `curl`, checks it against `SHA256SUMS`, and keeps both in
//! `releases/<version>/` in Ferry's directory on this machine, so that a
//! second host of the same platform costs no download. A kept binary is
//! checked again before each use. `ferry update` downloads its new version
//! the same way.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::build_info::VERSION;
use crate::home;
use crate::interrupt;

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
fn asset(version: &str, target: &str) -> String {
    format!("ferry-{version}-{target}")
}

/// Where a version's release files are published.
fn base_url(version: &str) -> String {
    format!(
        "{}/releases/download/v{version}",
        env!("CARGO_PKG_REPOSITORY")
    )
}

/// This version's binary for `target`, checked against `SHA256SUMS`,
/// downloaded first unless it is kept on this machine.
pub fn binary(target: &str) -> Result<PathBuf> {
    binary_of(VERSION, target)
}

/// `version`'s binary for `target`, as [`binary`].
pub fn binary_of(version: &str, target: &str) -> Result<PathBuf> {
    let dir = home::local_home()?.join("releases").join(version);
    fetch(&base_url(version), &dir, &asset(version, target))
}

/// Where a host downloads this version's binary for `target` itself, and
/// the SHA-256 this version's `SHA256SUMS` lists for it, the list downloaded
/// first unless it is kept on this machine: the same list a binary the
/// client downloads is checked against.
pub fn for_host(target: &str) -> Result<(String, String)> {
    let dir = home::local_home()?.join("releases").join(VERSION);
    let name = asset(VERSION, target);
    let sums = dir.join(SUMS);
    let base = base_url(VERSION);
    if !sums.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        download(&format!("{base}/{SUMS}"), &sums)?;
    }
    let text = fs::read_to_string(&sums).with_context(|| format!("read {}", sums.display()))?;
    let sum = checksum(&text, &name).with_context(|| format!("{SUMS} lists no {name}"))?;
    Ok((format!("{base}/{name}"), sum))
}

/// The latest release's version, from where GitHub's `releases/latest`
/// redirects: the page of its tag, `…/releases/tag/v<version>`.
pub fn latest() -> Result<String> {
    let url = format!("{}/releases/latest", env!("CARGO_PKG_REPOSITORY"));
    let output = curl()
        .args(["--head", "--output", "/dev/null"])
        .args(["--write-out", "%{url_effective}"])
        .arg(&url)
        .stderr(Stdio::inherit())
        .output();
    let output = ran(output)?;
    if !output.status.success() {
        bail!(
            "could not find the latest release at {url}: curl exited with {}",
            output.status
        );
    }
    let landed = String::from_utf8_lossy(&output.stdout);
    version_in(&landed).with_context(|| format!("{url} led to {landed}, not a release"))
}

/// The version in the URL of a release's tag.
fn version_in(url: &str) -> Option<String> {
    let (_, version) = url.trim().rsplit_once("/tag/v")?;
    parse_version(version).map(|_| version.to_owned())
}

/// A version's major, minor, and patch numbers, for comparing versions.
pub fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut numbers = version.split('.').map(|number| number.parse().ok());
    let parsed = (numbers.next()??, numbers.next()??, numbers.next()??);
    numbers.next().is_none().then_some(parsed)
}

/// `curl` as Ferry runs it: failing on an HTTP error, quiet but for errors,
/// following redirects, though never to plain HTTP. GitHub redirects a
/// release download to its CDN. A Ctrl-C reaches it, as it does Ferry. As
/// the network it waits on is not one the monitor can see, it gives up on a
/// connection not made within 10 s, or a download slower than 1 KiB/s for
/// 30 s, as a host downloading a release does.
fn curl() -> Command {
    let mut curl = Command::new("curl");
    crate::limits::restore_in_child(&mut curl);
    interrupt::unblock_in_child(&mut curl)
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--proto-redir", "=https"])
        .args(["--connect-timeout", "10"])
        .args(["--speed-limit", "1024", "--speed-time", "30"])
        .stdin(Stdio::null());
    curl
}

/// What running `curl` came to, saying so when it is not installed.
fn ran<T>(result: std::io::Result<T>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("downloading a release needs curl, which is not installed")
        }
        Err(error) => Err(error).context("run curl"),
    }
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
    download(&format!("{base_url}/{SUMS}"), &sums)?;
    download(&format!("{base_url}/{name}"), &path)?;
    verify(&sums, &path, name)?;
    Ok(path)
}

/// Where `path` is downloaded to, until it is all there: its name, all of
/// it, after which process downloads it.
fn partial(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".part-{}", std::process::id()));
    path.with_file_name(name)
}

/// Downloads `url` to `path` with `curl`, through a temporary file, so that a
/// download cut short leaves nothing at `path`. The temporary file is
/// registered, as a transfer's are, so that a Ctrl-C removes it too.
fn download(url: &str, path: &Path) -> Result<()> {
    let partial = partial(path);
    let directory = path.parent().context("a download has a directory")?;
    let opened = Arc::new(
        crate::fsys::open_root(directory.as_os_str())
            .with_context(|| format!("open {}", directory.display()))?,
    );
    let ((), _, mut registered) = interrupt::create(&opened, || {
        File::create(&partial)?;
        let name = partial.file_name().unwrap_or_default();
        Ok(((), name.as_encoded_bytes().to_vec()))
    })
    .with_context(|| format!("create {}", partial.display()))?;
    let shown = {
        let (partial, name) = (
            partial.clone(),
            path.file_name().unwrap_or_default().to_owned(),
        );
        crate::status::show_with(move || {
            let got = fs::metadata(&partial).map_or(0, |metadata| metadata.len());
            format!(
                "downloading {}: {}",
                name.to_string_lossy(),
                crate::progress::size(got)
            )
        })
    };
    let status = ran(curl().arg("--output").arg(&partial).arg(url).status())?;
    drop(shown);
    if !status.success() {
        bail!("could not download {url}: curl exited with {status}");
    }
    fs::rename(&partial, path).with_context(|| format!("keep {}", path.display()))?;
    registered.disarm();
    Ok(())
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

/// The SHA-256 of the file at `path`, in hex.
pub(crate) fn sha256(path: &Path) -> Result<String> {
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

    /// A download that stalls gives up, as the host's does.
    #[test]
    fn curl_gives_up_on_a_download_that_does_not_move() {
        let command = curl();
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        for pair in [
            ["--connect-timeout", "10"],
            ["--speed-limit", "1024"],
            ["--speed-time", "30"],
        ] {
            assert!(
                arguments.windows(2).any(|window| window == pair),
                "{pair:?} in {arguments:?}"
            );
        }
    }

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
    fn versions_are_read_and_compared() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert!(parse_version("0.10.0") > parse_version("0.9.3"));
        assert_eq!(parse_version("0.1"), None);
        assert_eq!(parse_version("0.1.0.1"), None);
        assert_eq!(parse_version("0.1.0-rc1"), None);
        assert_eq!(
            version_in("https://github.com/o/ferry/releases/tag/v0.2.1\n").as_deref(),
            Some("0.2.1")
        );
        assert_eq!(version_in("https://github.com/o/ferry/releases"), None);
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
        // Nothing is left of the download.
        let left: Vec<_> = fs::read_dir(cache.path()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A download's temporary file has the whole of the name it is for.
    #[test]
    fn a_download_is_named_for_the_whole_of_its_file() {
        let path = Path::new("/r/0.14.4/ferry-0.14.4-x86_64-unknown-linux-musl");
        assert_eq!(
            partial(path),
            Path::new(&format!(
                "/r/0.14.4/ferry-0.14.4-x86_64-unknown-linux-musl.part-{}",
                std::process::id()
            ))
        );
    }
}
