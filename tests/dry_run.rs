//! `ferry copy --dry-run` end to end: the Source walks, the Destination
//! compares, and the client prints what would be transferred.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

/// Runs `ferry copy` with `args`.
fn ferry(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("copy")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn tree(root: &Path) {
    fs::create_dir_all(root.join("dir/empty")).unwrap();
    fs::write(root.join("dir/file"), "content").unwrap();
    fs::write(root.join("top"), "top").unwrap();
    symlink("dir/file", root.join("link")).unwrap();
}

fn same_mtime(from: &Path, to: &Path) {
    let modified = fs::metadata(from).unwrap().modified().unwrap();
    fs::File::options()
        .write(true)
        .open(to)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

#[test]
fn lists_everything_for_a_missing_destination() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    tree(&source);
    let output = ferry(&[
        Path::new("--dry-run"),
        &source,
        &temp.path().join("destination"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        ".\ndir/\nlink\ntop\ndir/empty/\ndir/file\n"
    );
}

#[test]
fn lists_only_what_differs() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    tree(&source);
    tree(&destination);
    same_mtime(&source.join("dir/file"), &destination.join("dir/file"));
    same_mtime(&source.join("top"), &destination.join("top"));

    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");

    fs::write(destination.join("top"), "changed").unwrap();
    fs::remove_dir(destination.join("dir/empty")).unwrap();
    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "top\ndir/empty/\n");
}

#[test]
fn a_single_file() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    fs::write(&file, "content").unwrap();
    let output = ferry(&[Path::new("--dry-run"), &file, &temp.path().join("copy")]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), ".\n");
}

#[test]
fn a_missing_source_fails() {
    let temp = tempfile::tempdir().unwrap();
    let output = ferry(&[
        Path::new("--dry-run"),
        &temp.path().join("missing"),
        &temp.path().join("destination"),
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("missing does not exist"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn mtimes_compare_in_whole_seconds_unless_asked() {
    use std::time::{Duration, SystemTime};
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    for root in [&source, &destination] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("file"), "content").unwrap();
    }
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    for (root, millis) in [(&source, 500), (&destination, 250)] {
        fs::File::options()
            .write(true)
            .open(root.join("file"))
            .unwrap()
            .set_modified(base + Duration::from_millis(millis))
            .unwrap();
    }

    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");

    let output = ferry(&[
        Path::new("--dry-run"),
        Path::new("--modify-window=-1"),
        &source,
        &destination,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "file\n");
}

#[test]
fn a_trailing_slash_changes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    tree(&source);
    let destination = temp.path().join("destination");
    let with_slash = format!("{}/", source.display());
    let plain = ferry(&[Path::new("--dry-run"), &source, &destination]);
    let slashed = ferry(&[Path::new("--dry-run"), Path::new(&with_slash), &destination]);
    assert!(plain.status.success() && slashed.status.success());
    assert_eq!(stdout(&plain), stdout(&slashed));
}

/// A listing read slowly, as by a pager, holds the run back rather than
/// piling up: the run waits, and the listing comes whole, in order.
#[test]
fn a_listing_read_slowly_comes_whole() {
    use std::io::Read;
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    for directory in 0..400 {
        let directory = source.join(format!("d{directory:03}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..50 {
            fs::write(directory.join(format!("f{file:02}")), "").unwrap();
        }
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--dry-run", "--no-ui"])
        .arg(&source)
        .arg(temp.path().join("destination"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Nothing read for a while: the pipe fills, then what feeds it waits.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let mut listing = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut listing)
        .unwrap();
    assert!(child.wait().unwrap().success());
    let lines: Vec<&str> = listing.lines().collect();
    // The root, each directory, and each file in it.
    assert_eq!(lines.len(), 1 + 400 * 51, "{:?}", &lines[..3]);
    assert_eq!(lines[1], "d000/");
    assert_eq!(lines.last(), Some(&"d399/f49"));
}

/// A Destination directory that cannot be read fails its entries alone: the
/// rest is listed, the directory is told once, and the run ends with 1, not
/// with the Destination failing.
#[test]
fn a_destination_directory_that_cannot_be_read_fails_alone() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("locked")).unwrap();
    fs::write(source.join("locked/f"), "a").unwrap();
    fs::write(source.join("zz"), "b").unwrap();
    fs::create_dir_all(destination.join("locked")).unwrap();
    fs::set_permissions(
        destination.join("locked"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    fs::set_permissions(
        destination.join("locked"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(23), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().any(|line| line == "zz"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("cannot compare in locked/"),
        "{}",
        stderr(&output)
    );
    assert!(!stderr(&output).contains("failed:"), "{}", stderr(&output));
}

/// A reader that stops early, as `head` does, ends the listing quietly:
/// nothing said of the pipe, and the run exits with 0.
#[test]
fn a_listing_cut_short_by_its_reader_ends_quietly() {
    use std::io::{BufRead, BufReader};
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    for i in 0..20_000 {
        fs::write(source.join(format!("f{i:05}")), "x").unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--dry-run", "--no-ui"])
        .arg(&source)
        .arg(temp.path().join("destination"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    // The reader goes, as `head -1` does.
    assert!(!first.is_empty());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Broken pipe"), "{stderr}");
    assert_eq!(output.status.code(), Some(0), "{stderr}");
}

/// A dry run's listing of `source` into a new destination, as bytes.
fn listing(source: &Path, extra: &[&str]) -> Vec<u8> {
    let destination = source.with_file_name("destination");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--dry-run", "--no-ui"])
        .args(extra)
        .arg(source)
        .arg(&destination)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    output.stdout
}

/// A name with a newline, or one that starts as a deletion's line does,
/// cannot forge a line of the listing; bytes a terminal acts on are escaped.
#[test]
fn a_name_cannot_forge_a_line() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a\ndeleting important"), "x").unwrap();
    fs::write(source.join("deleting other"), "x").unwrap();
    fs::write(
        source.join(std::ffi::OsStr::from_bytes(b"x\x1b[31mred")),
        "x",
    )
    .unwrap();
    let listed = String::from_utf8(listing(&source, &[])).unwrap();
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(
        lines,
        [
            ".",
            "a\\#012deleting important",
            "\\#144eleting other",
            "x\\#033[31mred"
        ],
        "{listed}"
    );
}

/// `-0` lists each entry's bytes as they are, after `+ `, ending in a NUL.
#[test]
fn a_null_separated_listing_is_exact() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join(std::ffi::OsStr::from_bytes(b"caf\xe9\nx")), "x").unwrap();
    let listed = listing(&source, &["-0"]);
    assert_eq!(listed, b"+ .\0+ caf\xe9\nx\0");
}
