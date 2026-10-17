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

/// A directory that cannot be listed has nothing listed, so nothing in it is
/// deleted; the rest is synced as usual.
#[test]
fn deletes_nothing_in_a_directory_the_source_could_not_list() {
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
    assert!(!destination.join("extra").exists());
    assert!(destination.join("dir/kept").exists());
    assert!(destination.join("dir/extra-dir").exists());
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

/// Every path under `root`, relative, sorted, with `/` after directories:
/// what a run is checked to leave, or not to touch.
fn paths(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                found.push(format!("{relative}/"));
                walk(root, &path, found);
            } else {
                found.push(relative);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found.sort();
    found
}

fn fails_with(output: &Output, message: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
}

/// `sync` is only from a directory into a directory, or where there is none
/// yet; anything else is refused before anything changes, a file with a
/// pointer to `copy`.
#[test]
fn sync_is_only_between_directories() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    let directory = temp.path().join("directory");
    let other_file = temp.path().join("other-file");
    fs::write(&file, "file").unwrap();
    fs::create_dir_all(directory.join("sub")).unwrap();
    fs::write(directory.join("sub/precious"), "precious").unwrap();
    fs::write(&other_file, "precious").unwrap();
    let before = paths(temp.path());
    for (source, destination, message) in [
        (&file, &directory, "use `ferry copy`"),
        (&file, &temp.path().join("missing"), "use `ferry copy`"),
        (&file, &other_file, "use `ferry copy`"),
        (&directory, &other_file, "other-file is not a directory"),
    ] {
        for dry in [&[][..], &[Path::new("--dry-run")]] {
            let mut arguments = dry.to_vec();
            arguments.extend([source.as_path(), destination.as_path()]);
            fails_with(&ferry("sync", &arguments), message);
            assert_eq!(paths(temp.path()), before);
        }
    }
}

/// A dry run says what a run deletes, a directory a file replaces among it,
/// and changes nothing.
#[test]
fn a_dry_run_lists_a_directory_a_file_replaces_and_changes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("name"), "file").unwrap();
    fs::create_dir_all(destination.join("name/inside")).unwrap();
    fs::write(destination.join("name/inside/x"), "x").unwrap();
    fs::write(destination.join("extra"), "extra").unwrap();
    let before = paths(&destination);

    let dry = ferry("sync", &[Path::new("--dry-run"), &source, &destination]);
    succeeds(&dry);
    assert_eq!(
        String::from_utf8_lossy(&dry.stdout),
        "deleting name/\nname\ndeleting extra\n"
    );
    assert_eq!(paths(&destination), before);
    // `copy` deletes nothing, so lists no deleting.
    let dry = ferry("copy", &[Path::new("--dry-run"), &source, &destination]);
    succeeds(&dry);
    assert_eq!(String::from_utf8_lossy(&dry.stdout), "name\n");

    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(paths(&destination), ["name"]);
}

/// Deleting a directory removes the links in it, never what they point to;
/// so does replacing one.
#[test]
fn deleting_never_follows_a_symlink_inside() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("precious"), "precious").unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("replaced"), "file").unwrap();
    for name in ["extra", "replaced"] {
        fs::create_dir_all(destination.join(name)).unwrap();
        symlink(&outside, destination.join(name).join("link")).unwrap();
        symlink(
            outside.join("precious"),
            destination.join(name).join("file-link"),
        )
        .unwrap();
    }

    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(paths(&destination), ["replaced"]);
    assert_eq!(paths(&outside), ["precious"]);
    assert_eq!(fs::read(outside.join("precious")).unwrap(), b"precious");
}

/// Deep down, a directory the Source cannot list keeps all under it on the
/// Destination, extras and all; beside it, extras are deleted.
#[test]
fn a_directory_the_source_cannot_list_keeps_all_under_it_at_any_depth() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("a/b/locked")).unwrap();
    fs::write(source.join("a/b/locked/kept"), "kept").unwrap();
    fs::write(source.join("a/b/beside"), "beside").unwrap();
    succeeds(&ferry("copy", &[&source, &destination]));
    fs::create_dir_all(destination.join("a/b/locked/only-here/deeper")).unwrap();
    fs::write(destination.join("a/b/locked/only-here/deeper/file"), "x").unwrap();
    fs::write(destination.join("a/b/locked/also-here"), "x").unwrap();
    fs::write(destination.join("a/b/extra"), "x").unwrap();
    fs::write(destination.join("a/extra"), "x").unwrap();
    fs::set_permissions(source.join("a/b/locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let output = ferry("sync", &[&source, &destination]);
    fs::set_permissions(source.join("a/b/locked"), fs::Permissions::from_mode(0o755)).unwrap();
    fails_with(&output, "cannot read");
    fs::set_permissions(
        destination.join("a/b/locked"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        paths(&destination),
        [
            "a/",
            "a/b/",
            "a/b/beside",
            "a/b/locked/",
            "a/b/locked/also-here",
            "a/b/locked/kept",
            "a/b/locked/only-here/",
            "a/b/locked/only-here/deeper/",
            "a/b/locked/only-here/deeper/file",
        ]
    );
}

/// A directory the Destination cannot open stops the transfer, and nothing
/// in it is deleted.
#[test]
fn a_directory_the_destination_cannot_list_has_nothing_deleted() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = trees(temp.path());
    fs::set_permissions(destination.join("dir"), fs::Permissions::from_mode(0o000)).unwrap();

    let output = ferry("sync", &[&source, &destination]);
    fs::set_permissions(destination.join("dir"), fs::Permissions::from_mode(0o755)).unwrap();
    fails_with(&output, "compare dir/: Permission denied");
    assert!(destination.join("dir/extra-dir/deeper/file").exists());
    assert!(destination.join("dir/kept").exists());
}

/// An extra that cannot be deleted is reported, and the rest still is.
#[test]
fn an_extra_that_cannot_be_deleted_is_reported_and_the_rest_deleted() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = trees(temp.path());
    // Its file cannot be removed from it, so neither can it.
    fs::set_permissions(
        destination.join("dir/extra-dir/deeper"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();

    let output = ferry("sync", &[&source, &destination]);
    fs::set_permissions(
        destination.join("dir/extra-dir/deeper"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fails_with(&output, "cannot delete dir/extra-dir");
    assert!(!destination.join("extra").exists());
    assert!(destination.join("dir/kept").exists());
}

/// Names are bytes, not text: one that is not UTF-8 is matched, and kept,
/// and an extra one deleted.
#[test]
fn names_that_are_not_utf8_are_matched_by_their_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let latin = OsStr::from_bytes(b"caf\xe9");
    let other = OsStr::from_bytes(b"caf\xc3\xa9");
    fs::create_dir(&source).unwrap();
    fs::write(source.join(latin), "latin").unwrap();
    succeeds(&ferry("copy", &[&source, &destination]));
    // The same name in UTF-8 is another name.
    fs::write(destination.join(other), "other").unwrap();

    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(fs::read(destination.join(latin)).unwrap(), b"latin");
    assert!(!destination.join(other).exists());
}
