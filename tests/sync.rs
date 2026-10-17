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
        "deleting extra\ndeleting name/\nname\n"
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
    if fs::write(source.join(latin), "latin").is_err() {
        // A filesystem that takes only UTF-8 names, as a Mac's does.
        return;
    }
    succeeds(&ferry("copy", &[&source, &destination]));
    // The same name in UTF-8 is another name.
    fs::write(destination.join(other), "other").unwrap();

    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(fs::read(destination.join(latin)).unwrap(), b"latin");
    assert!(!destination.join(other).exists());
}

/// Where the Destination has a symlink to a directory and the Source a
/// directory, the link is replaced, never gone into: what it points to, out
/// of the tree, keeps all it has, though the Source has none of it.
#[test]
fn a_symlink_to_a_directory_is_replaced_never_gone_into() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let outside = temp.path().join("outside");
    fs::create_dir_all(source.join("x")).unwrap();
    fs::write(source.join("x/in"), "in").unwrap();
    fs::create_dir_all(outside.join("deeper")).unwrap();
    fs::write(outside.join("precious"), "precious").unwrap();
    fs::write(outside.join("deeper/file"), "file").unwrap();
    fs::create_dir(&destination).unwrap();
    symlink(&outside, destination.join("x")).unwrap();
    let before = paths(&outside);

    let dry = ferry("sync", &[Path::new("--dry-run"), &source, &destination]);
    succeeds(&dry);
    assert_eq!(String::from_utf8_lossy(&dry.stdout), "x/\nx/in\n");
    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(paths(&destination), ["x/", "x/in"]);
    assert_eq!(paths(&outside), before);
}

/// A Source whose root cannot be listed has nothing listed: nothing is
/// deleted.
#[test]
fn a_source_root_that_cannot_be_listed_deletes_nothing() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = trees(temp.path());
    let before = paths(&destination);
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();

    let output = ferry("sync", &[&source, &destination]);
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    // The root's mode is copied as it is: let the check in.
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    fails_with(&output, "cannot read");
    assert_eq!(paths(&destination), before);
}

/// At most how many entries of a directory a test's Source sends in a part:
/// a directory of several parts then needs few files.
const PART: &str = "--part-entries=100";

/// A directory whose entries come in several parts, extras before, between,
/// and after them: only the extras are deleted, found once its last part
/// is here.
#[test]
fn a_directory_in_parts_deletes_its_extras_only() {
    // More than two parts.
    const FILES: usize = 201;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("many")).unwrap();
    for index in 0..FILES {
        fs::write(source.join(format!("many/f{index:05}")), "").unwrap();
    }
    succeeds(&ferry("copy", &[&source, &destination]));
    let extras = [
        "a-first", "f00000-x", "f00099-x", "f00100-x", "f00199-x", "z-last",
    ];
    for extra in extras {
        fs::write(destination.join("many").join(extra), "extra").unwrap();
    }

    let dry = ferry(
        "sync",
        &[
            Path::new(PART),
            Path::new("--dry-run"),
            &source,
            &destination,
        ],
    );
    succeeds(&dry);
    let listed: Vec<String> = extras
        .iter()
        .map(|extra| format!("deleting many/{extra}\n"))
        .collect();
    assert_eq!(String::from_utf8_lossy(&dry.stdout), listed.concat());

    succeeds(&ferry("sync", &[Path::new(PART), &source, &destination]));
    let left = paths(&destination);
    assert_eq!(left.len(), 1 + FILES);
    assert!(
        extras
            .iter()
            .all(|extra| !left.contains(&format!("many/{extra}"))),
        "{:?}",
        &left[..3]
    );
}

/// How many directories [`busy_trees`] makes, each with an extra.
const BUSY: usize = 300;

/// A tree to sync with something to do in many directories: files changed,
/// and an extra in each. Returns the paths the Source has.
fn busy_trees(source: &Path, destination: &Path) -> Vec<String> {
    for directory in 0..BUSY {
        let directory = source.join(format!("d{directory:03}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..10 {
            fs::write(directory.join(format!("f{file:02}")), vec![7; 4096]).unwrap();
        }
    }
    succeeds(&ferry("copy", &[source, destination]));
    for directory in 0..BUSY {
        let name = format!("d{directory:03}");
        fs::write(destination.join(&name).join("extra"), "extra").unwrap();
        // Changed, so that there is content to send.
        fs::write(source.join(&name).join("f00"), vec![8; 200_000]).unwrap();
    }
    paths(source)
}

/// However a sync is cut short, nothing the Source has is gone from the
/// Destination, only extras are; and the next sync makes them the same.
/// Unless `surely_cut`, the sync may have ended before it could be cut: then
/// it is checked as it ended, the same as the Source.
fn interrupted_sync_keeps_what_the_source_has(
    surely_cut: bool,
    interrupt: impl Fn(&Path, &Path) -> Output,
) {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let wanted = busy_trees(&source, &destination);

    let output = interrupt(&source, &destination);
    if output.status.success() {
        assert!(!surely_cut, "it was not cut short");
        eprintln!("it ended before it was cut: only how it ended is checked");
        assert_eq!(paths(&destination), wanted);
        return;
    }
    let left = paths(&destination);
    let missing: Vec<&String> = wanted.iter().filter(|path| !left.contains(path)).collect();
    assert!(missing.is_empty(), "gone: {missing:?}");
    assert!(
        left.iter()
            .filter(|path| !wanted.contains(path))
            .all(|path| path.ends_with("/extra") || path.contains(".ferry-")),
        "{left:?}"
    );

    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(paths(&destination), wanted);
}

/// A path cut short fails the sync early.
#[test]
fn a_sync_whose_connection_fails_keeps_what_the_source_has() {
    interrupted_sync_keeps_what_the_source_has(true, |source, destination| {
        ferry("sync", &[Path::new("--cut-a-path"), source, destination])
    });
}

/// Ctrl-C partway, once deleting has begun, and maybe before it is over:
/// wherever it lands, the same holds. On a machine fast enough, the sync may
/// end before the Ctrl-C comes.
#[test]
fn a_sync_stopped_partway_keeps_what_the_source_has() {
    interrupted_sync_keeps_what_the_source_has(false, |source, destination| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .arg("sync")
            .args([source, destination])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let extras_left = || {
            (0..BUSY)
                .filter(|directory| destination.join(format!("d{directory:03}/extra")).exists())
                .count()
        };
        let started = std::time::Instant::now();
        while extras_left() == BUSY
            && child.try_wait().unwrap().is_none()
            && started.elapsed() < std::time::Duration::from_secs(30)
        {
            std::thread::yield_now();
        }
        rustix::process::kill_process(
            rustix::process::Pid::from_child(&child),
            rustix::process::Signal::INT,
        )
        .unwrap();
        child.wait_with_output().unwrap()
    });
}

/// A Destination named through a symlink, as the user named it, is the
/// directory it points to: synced there, the link itself kept, and nothing
/// beside the directory touched.
#[test]
fn a_destination_named_through_a_symlink_is_synced_where_it_points() {
    let temp = tempfile::tempdir().unwrap();
    let (source, real) = trees(temp.path());
    fs::write(temp.path().join("beside"), "beside").unwrap();
    let link = temp.path().join("link");
    symlink(&real, &link).unwrap();

    succeeds(&ferry("sync", &[&source, &link]));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(paths(&real), ["dir/", "dir/kept"]);
    assert_eq!(fs::read(temp.path().join("beside")).unwrap(), b"beside");
    assert_eq!(fs::read_link(&link).unwrap(), real);
}

/// Two trees for a sync whose names change only in case: the Destination a
/// copy of `before`, then synced from `after`. Each is a list of paths and
/// what each file holds; a path ending in `/` is a directory.
fn renamed_by_case(
    before: &[(&str, &str)],
    after: &[(&str, &str)],
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let make = |root: &Path, tree: &[(&str, &str)]| {
        fs::create_dir_all(root).unwrap();
        for (path, content) in tree {
            if let Some(directory) = path.strip_suffix('/') {
                fs::create_dir_all(root.join(directory)).unwrap();
            } else {
                fs::write(root.join(path), content).unwrap();
            }
        }
    };
    let (old, new, destination) = (
        temp.path().join("old"),
        temp.path().join("new"),
        temp.path().join("destination"),
    );
    make(&old, before);
    make(&new, after);
    succeeds(&ferry("copy", &[&old, &destination]));
    (temp, new, destination)
}

/// Where the Source renamed a directory and a file only in case, sync makes
/// the Destination the same, whether its filesystem tells the two names
/// apart or not: where it does not, as a Mac's does, the old name is the
/// new, and must not be deleted after the new one is made, nor what is in
/// it.
#[test]
fn a_rename_by_case_is_synced() {
    let (_temp, source, destination) = renamed_by_case(
        &[
            ("photos/", ""),
            ("photos/a", "a"),
            ("photos/sub/", ""),
            ("photos/sub/c", "c"),
            ("File.txt", "file"),
        ],
        &[
            ("Photos/", ""),
            ("Photos/a", "a"),
            ("Photos/sub/", ""),
            ("Photos/sub/c", "c"),
            ("file.txt", "file"),
        ],
    );
    succeeds(&ferry("sync", &[&source, &destination]));
    assert_eq!(paths(&destination), paths(&source));
    assert_eq!(fs::read(destination.join("Photos/sub/c")).unwrap(), b"c");
    let again = ferry("sync", &[Path::new("--dry-run"), &source, &destination]);
    succeeds(&again);
    assert_eq!(String::from_utf8_lossy(&again.stdout), "");
}

/// The same in a directory of several parts, where a file is written for an
/// early part, and its old name, the same where case is ignored, is extra
/// only once the last part is here.
#[test]
fn a_rename_by_case_in_a_directory_of_parts_is_synced() {
    const FILES: usize = 201;
    let names: Vec<String> = (0..FILES)
        .map(|index| format!("many/f{index:05}"))
        .collect();
    let mut before: Vec<(&str, &str)> = vec![("many/", ""), ("many/b", "old")];
    before.extend(names.iter().map(|name| (name.as_str(), "")));
    let mut after: Vec<(&str, &str)> = vec![("many/", ""), ("many/B", "new")];
    // Changed, so that files of the first part are written early.
    after.extend(
        names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.as_str(), if index < 60 { "changed" } else { "" })),
    );
    let (_temp, source, destination) = renamed_by_case(&before, &after);
    succeeds(&ferry("sync", &[Path::new(PART), &source, &destination]));
    assert_eq!(fs::read(destination.join("many/B")).unwrap(), b"new");
    assert_eq!(paths(&destination), paths(&source));
}
