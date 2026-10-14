//! `ferry sync` end to end, between two local directories.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

fn ferry(command: &str, args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg(command)
        .args(args)
        .output()
        .unwrap()
}

fn succeeds(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A source with a file and a directory, and a copy of it with extras.
fn trees(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let (source, destination) = (root.join("source"), root.join("destination"));
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::write(source.join("dir/kept"), "kept").unwrap();
    succeeds(&ferry("copy", &[&source, &destination]));
    fs::write(destination.join("extra"), "extra").unwrap();
    fs::create_dir_all(destination.join("dir/extra-dir/deeper")).unwrap();
    fs::write(destination.join("dir/extra-dir/deeper/file"), "x").unwrap();
    (source, destination)
}

#[test]
fn deletes_what_the_source_does_not_have() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = trees(temp.path());
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious"), "precious").unwrap();
    symlink(&outside, destination.join("link-out")).unwrap();

    let dry = ferry("sync", &[Path::new("--dry-run"), &source, &destination]);
    succeeds(&dry);
    assert_eq!(
        String::from_utf8_lossy(&dry.stdout),
        "deleting extra\ndeleting link-out\ndeleting dir/extra-dir/\n"
    );
    assert!(destination.join("extra").exists());

    succeeds(&ferry("sync", &[&source, &destination]));
    let mut names: Vec<_> = fs::read_dir(&destination)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["dir"]);
    assert!(!destination.join("dir/extra-dir").exists());
    assert_eq!(fs::read(destination.join("dir/kept")).unwrap(), b"kept");
    // The link was removed, not followed.
    assert_eq!(fs::read(outside.join("precious")).unwrap(), b"precious");
}

#[test]
fn copy_deletes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = trees(temp.path());
    succeeds(&ferry("copy", &[&source, &destination]));
    assert!(destination.join("extra").exists());
    assert!(destination.join("dir/extra-dir/deeper/file").exists());
}

#[test]
fn deletes_nothing_when_the_source_could_not_be_read() {
    if rustix::process::geteuid().is_root() {
        // Root reads everything, so nothing is unreadable.
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = trees(temp.path());
    fs::set_permissions(source.join("dir"), fs::Permissions::from_mode(0o000)).unwrap();

    let output = ferry("sync", &[&source, &destination]);
    // The copy is faithful: the destination's directory got the same mode.
    for side in [&source, &destination] {
        fs::set_permissions(side.join("dir"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("deleted nothing"), "{stderr}");
    assert!(destination.join("extra").exists());
    assert!(destination.join("dir/kept").exists());
}

#[test]
fn replaces_a_directory_where_a_file_belongs() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("name"), "file").unwrap();
    fs::create_dir_all(destination.join("name/inside")).unwrap();

    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(fs::read(destination.join("name")).unwrap(), b"file");
}
