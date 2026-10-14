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
        stderr(&output).contains("cannot read"),
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
