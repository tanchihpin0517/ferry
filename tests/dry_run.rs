//! `ferry --dry-run` end to end: the client walks, a `ferry server` child
//! compares, and the client prints what would be transferred.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

fn ferry(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
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
fn copying_is_not_implemented_yet() {
    let temp = tempfile::tempdir().unwrap();
    let output = ferry(&[temp.path(), &temp.path().join("destination")]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--dry-run"), "{}", stderr(&output));
    assert!(!temp.path().join("destination").exists());
}
