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

/// A file the Source lists but cannot open is not copied, and fails the
/// sync; its directory, and those above it, are finished all the same, with
/// their own mtime and permissions, and what is beside it is synced.
#[test]
fn a_file_the_source_cannot_open_leaves_its_directories_finished() {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime};
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("a/b")).unwrap();
    fs::create_dir_all(source.join("a/c")).unwrap();
    fs::write(source.join("a/b/locked"), "locked").unwrap();
    fs::write(source.join("a/b/readable"), "readable").unwrap();
    fs::write(source.join("a/c/after"), "after").unwrap();
    fs::set_permissions(source.join("a/b/locked"), fs::Permissions::from_mode(0o000)).unwrap();
    // Deepest first, so that setting one does not change its parent's.
    let directories = [("a", 0o750), ("a/b", 0o705)];
    let mtime =
        |index: usize| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + index as u64);
    for (index, (directory, mode)) in directories.iter().enumerate().rev() {
        let path = source.join(directory);
        fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(mtime(index))
            .unwrap();
    }

    let output = ferry("sync", &[&source, &destination]);
    fs::set_permissions(source.join("a/b/locked"), fs::Permissions::from_mode(0o644)).unwrap();
    fails_with(&output, "cannot read a/b/locked");
    for (index, (directory, mode)) in directories.iter().enumerate() {
        let metadata = fs::metadata(destination.join(directory)).unwrap();
        assert_eq!(
            (metadata.mode() & 0o777, metadata.modified().unwrap()),
            (*mode, mtime(index)),
            "{directory}"
        );
    }
    assert_eq!(
        paths(&destination),
        ["a/", "a/b/", "a/b/readable", "a/c/", "a/c/after"]
    );
}

/// Gives every directory under `root`, and it, its owner's permissions back,
/// so that the tree can be looked at and removed.
fn open_up(root: &Path) {
    fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            open_up(&path);
        }
    }
}

/// A directory the Destination has, but may not read, write, or go into,
/// is worked in all the same, as rsync does, when it is this process's:
/// what it has extra is deleted, what it lacks written, in one run, and it
/// ends with the Source's permissions, whatever those are.
#[test]
fn a_directory_its_owner_may_not_work_in_is_synced_and_ends_with_the_sources_mode() {
    use std::os::unix::fs::MetadataExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    // The Source's permissions, then the Destination's.
    for (source_mode, destination_mode) in [
        (0o755, 0o555),
        (0o555, 0o555),
        (0o555, 0o755),
        (0o755, 0o000),
        (0o500, 0o100),
        (0o700, 0o300),
    ] {
        let case = format!("{source_mode:o} over {destination_mode:o}");
        let temp = tempfile::tempdir().unwrap();
        let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
        fs::create_dir_all(source.join("a/deeper")).unwrap();
        fs::write(source.join("a/new"), "new").unwrap();
        fs::write(source.join("a/deeper/new"), "new").unwrap();
        fs::create_dir_all(destination.join("a/deeper")).unwrap();
        fs::write(destination.join("a/extra"), "extra").unwrap();
        fs::write(destination.join("a/deeper/extra"), "extra").unwrap();
        for (side, mode) in [(&source, source_mode), (&destination, destination_mode)] {
            fs::set_permissions(side.join("a/deeper"), fs::Permissions::from_mode(mode)).unwrap();
            fs::set_permissions(side.join("a"), fs::Permissions::from_mode(mode)).unwrap();
        }

        let output = ferry("sync", &[&source, &destination]);
        let modes: Vec<u32> = ["a", "a/deeper"]
            .iter()
            .map(|directory| {
                fs::symlink_metadata(destination.join(directory))
                    .map(|metadata| metadata.mode() & 0o7777)
                    .unwrap_or(0o7777)
            })
            .collect();
        open_up(&source);
        open_up(&destination);
        succeeds(&output);
        assert_eq!(modes, [source_mode, source_mode], "{case}");
        assert_eq!(
            paths(&destination),
            ["a/", "a/deeper/", "a/deeper/new", "a/new"],
            "{case}"
        );
    }
}

/// So is the root, and one the run makes no change in.
#[test]
fn a_root_its_owner_may_not_work_in_is_synced_and_ends_with_the_sources_mode() {
    use std::os::unix::fs::MetadataExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("same")).unwrap();
    fs::write(source.join("new"), "new").unwrap();
    fs::write(source.join("same/file"), "same").unwrap();
    succeeds(&ferry("copy", &[&source, &destination]));
    fs::remove_file(destination.join("new")).unwrap();
    fs::write(destination.join("extra"), "extra").unwrap();
    for side in [&source, &destination] {
        fs::set_permissions(side.join("same"), fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(side, fs::Permissions::from_mode(0o500)).unwrap();
    }

    let output = ferry("sync", &[&source, &destination]);
    let modes: Vec<u32> = [destination.clone(), destination.join("same")]
        .iter()
        .map(|directory| fs::symlink_metadata(directory).unwrap().mode() & 0o7777)
        .collect();
    open_up(&source);
    open_up(&destination);
    succeeds(&output);
    assert_eq!(modes, [0o500, 0o500]);
    assert_eq!(paths(&destination), ["new", "same/", "same/file"]);
}

/// An extra directory the run may not go into, or empty, is deleted all the
/// same, as rsync does, when it is this process's, at any depth.
#[test]
fn an_extra_directory_its_owner_may_not_work_in_is_deleted() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(destination.join("gone/unreadable/locked")).unwrap();
    fs::create_dir_all(destination.join("gone/unwritable")).unwrap();
    fs::write(destination.join("gone/unreadable/locked/file"), "x").unwrap();
    fs::write(destination.join("gone/unwritable/file"), "x").unwrap();
    for (directory, mode) in [
        ("gone/unreadable/locked", 0o000),
        ("gone/unreadable", 0o300),
        ("gone/unwritable", 0o500),
        ("gone", 0o100),
    ] {
        fs::set_permissions(
            destination.join(directory),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
    }

    let output = ferry("sync", &[&source, &destination]);
    if destination.join("gone").exists() {
        open_up(&destination);
    }
    succeeds(&output);
    assert_eq!(paths(&destination), Vec::<String>::new());
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
/// Destination, only extras are; and the next sync makes them the same. The
/// sync may have ended before it could be cut: then it is checked as it
/// ended, the same as the Source.
fn interrupted_sync_keeps_what_the_source_has(interrupt: impl Fn(&Path, &Path) -> Output) {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let wanted = busy_trees(&source, &destination);

    let output = interrupt(&source, &destination);
    if output.status.success() {
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

/// A stream cut short fails the sync, before anything changes: there is no
/// other stream to go on, and none is made again.
#[test]
fn a_sync_whose_stream_is_cut_short_fails_changing_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    busy_trees(&source, &destination);
    let before = paths(&destination);
    let output = ferry(
        "sync",
        &[Path::new("--cut-a-stream"), &source, &destination],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("the stream"), "{stderr}");
    assert_eq!(paths(&destination), before);
}

/// Ctrl-C partway, once deleting has begun, and maybe before it is over:
/// wherever it lands, the same holds. On a machine fast enough, the sync may
/// end before the Ctrl-C comes.
#[test]
fn a_sync_stopped_partway_keeps_what_the_source_has() {
    interrupted_sync_keeps_what_the_source_has(|source, destination| {
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
            // Not spinning: on a runner of two cores, that would take the
            // time the transfer needs.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // It may have ended already, and been waited for: the outcome is
        // checked either way.
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_child(&child),
            rustix::process::Signal::INT,
        );
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

/// With the scan waiting while more than a few needed files wait to be
/// sent, and the manifest little ahead of the connection, so that the wait
/// comes, as on a tree of millions: a copy and then a sync, of many
/// directories and one in parts, go through, and leave the Destination as
/// the Source. The wait never holds the transfer up for good. The scan goes
/// at most 200 entries ahead of the comparison, which says how far it is
/// only up to the files it asked for; and, for the copy, the readers are
/// held up as they start: so the scan cannot send everything before any
/// file is asked for, and the files asked for pile up while it waits,
/// however fast they are read after. Without both, it sent everything first
/// in a run now and then, and never waited.
#[test]
fn a_scan_that_waits_for_files_to_be_sent_goes_through() {
    const DIRECTORIES: usize = 2000;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    for directory in 0..DIRECTORIES {
        let directory = source.join(format!("d{directory:04}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..20 {
            fs::write(directory.join(format!("f{file:02}")), "").unwrap();
        }
    }
    fs::create_dir_all(source.join("many")).unwrap();
    for file in 0..130 {
        fs::write(source.join(format!("many/f{file:03}")), "x").unwrap();
    }
    let profile = temp.path().join("profile.json");
    let run = |command: &str, stalled: Option<&str>| {
        let mut ferry = Command::new(env!("CARGO_BIN_EXE_ferry"));
        ferry.arg(command).args([
            Path::new("--backlog-files=3"),
            Path::new("--paced-bytes=4096"),
            Path::new("--part-entries=50"),
            Path::new("--in-flight-entries=200"),
            Path::new("--profile-file"),
            profile.as_path(),
            source.as_path(),
            destination.as_path(),
        ]);
        if let Some(role) = stalled {
            ferry.env("FERRY_TEST_STALL", role);
        }
        succeeds(&ferry.output().unwrap());
    };
    run("copy", Some("reader"));
    assert_eq!(paths(&destination), paths(&source));
    // The scan did wait for files to be sent: where its time went says so.
    let profiled = fs::read_to_string(&profile).unwrap();
    assert!(
        profiled.contains("wait for needed files to be sent"),
        "the scan never waited"
    );

    // Changed, and with extras, for a sync with as much to send again.
    for directory in 0..DIRECTORIES {
        let name = format!("d{directory:04}");
        fs::write(source.join(&name).join("f00"), "changed").unwrap();
        fs::write(destination.join(&name).join("extra"), "extra").unwrap();
    }
    fs::write(destination.join("many/extra"), "extra").unwrap();
    run("sync", None);
    assert_eq!(paths(&destination), paths(&source));
    assert_eq!(fs::read(destination.join("d0700/f00")).unwrap(), b"changed");
}

/// A tree `a`, with a file and a directory `sub` holding another: two roots
/// of it overlap.
fn overlapping(root: &Path) -> std::path::PathBuf {
    let a = root.join("a");
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("f1"), "one").unwrap();
    fs::write(a.join("sub/f2"), "two").unwrap();
    a
}

/// What a tree holds, every file's content, and every entry's mtime, the
/// root's included: what a refused run must leave as it was.
fn contents(root: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
    let mtime = |path: &Path| fs::symlink_metadata(path).unwrap().modified().unwrap();
    let mut contents = vec![(String::from("."), Vec::new(), mtime(root))];
    contents.extend(paths(root).into_iter().map(|path| {
        let content = if path.ends_with('/') {
            Vec::new()
        } else {
            fs::read(root.join(&path)).unwrap()
        };
        let modified = mtime(&root.join(&path));
        (path, content, modified)
    }));
    contents
}

#[test]
fn a_destination_inside_the_source_is_refused_and_nothing_changes() {
    let temp = tempfile::tempdir().unwrap();
    let a = overlapping(temp.path());
    let before = contents(&a);
    // There, or not there yet.
    for destination in [a.join("sub"), a.join("new")] {
        fails_with(&ferry("sync", &[&a, &destination]), "is inside");
        assert_eq!(contents(&a), before);
    }
    assert!(!a.join("new").exists());
}

#[test]
fn a_source_inside_the_destination_is_refused_and_nothing_changes() {
    let temp = tempfile::tempdir().unwrap();
    let a = overlapping(temp.path());
    let before = contents(&a);
    fails_with(
        &ferry("sync", &[&a.join("sub"), &a]),
        "the Source, is inside",
    );
    assert_eq!(contents(&a), before);
}

#[test]
fn the_same_directory_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let a = overlapping(temp.path());
    let before = contents(&a);
    symlink(&a, temp.path().join("link-to-a")).unwrap();
    fails_with(&ferry("sync", &[&a, &a]), "nothing to sync");
    fails_with(
        &ferry("sync", &[&a, &temp.path().join("link-to-a")]),
        "nothing to sync",
    );
    assert_eq!(contents(&a), before);
}

#[test]
fn a_dry_run_refuses_overlapping_roots_too() {
    let temp = tempfile::tempdir().unwrap();
    let a = overlapping(temp.path());
    let output = ferry("sync", &[Path::new("--dry-run"), &a, &a.join("sub")]);
    fails_with(&output, "is inside");
    assert!(output.stdout.is_empty());
}

/// More extras in one directory than one message could name: a dry run
/// lists them all, and the sync deletes them all, and both succeed. With a
/// packet of at most 2 MiB, as `FERRY_TEST_MAX_PAYLOAD` makes it, 9,000
/// names of 250 bytes are more than one could hold, rather than the 70,000
/// that 16 MiB takes. How long each step took is said as it ends.
#[test]
fn deletes_more_extras_in_one_directory_than_a_message_could_name() {
    const EXTRAS: usize = 9_000;
    const MAX_PAYLOAD: &str = "2097152";
    let started = std::time::Instant::now();
    let step = |what: &str| eprintln!("{what} at {:.1?}", started.elapsed());
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("d")).unwrap();
    fs::create_dir_all(destination.join("d")).unwrap();
    for i in 0..EXTRAS {
        let name = format!("{i:06}{}", "x".repeat(244));
        fs::File::create(destination.join("d").join(name)).unwrap();
    }
    step("made the extras");
    let sync = |args: &[&Path]| {
        Command::new(env!("CARGO_BIN_EXE_ferry"))
            .arg("sync")
            .args(args)
            .env("FERRY_TEST_MAX_PAYLOAD", MAX_PAYLOAD)
            .output()
            .unwrap()
    };

    let dry = sync(&[Path::new("--dry-run"), &source, &destination]);
    step("dry run done");
    succeeds(&dry);
    let listed = String::from_utf8_lossy(&dry.stdout);
    assert_eq!(listed.lines().count(), EXTRAS);
    assert!(listed.lines().all(|line| line.starts_with("deleting d/")));
    assert_eq!(fs::read_dir(destination.join("d")).unwrap().count(), EXTRAS);

    succeeds(&sync(&[&source, &destination]));
    step("sync done");
    assert_eq!(fs::read_dir(destination.join("d")).unwrap().count(), 0);
}

/// Whether `directory` takes `A` for `a`, as a filesystem that ignores case
/// does.
fn ignores_case(directory: &Path) -> bool {
    fs::write(directory.join("probe"), "").unwrap();
    let ignores = directory.join("PROBE").exists();
    fs::remove_file(directory.join("probe")).unwrap();
    ignores
}

/// Two of the Source's names that a Destination ignoring case takes for one:
/// the first made keeps it, and the other fails, saying so; neither is
/// deleted, written over, nor written into. Again the same. Runs where the
/// tests' own directory ignores case, and the build's does not, as on CI's
/// ext4-casefold.
#[test]
fn names_a_destination_takes_for_one_fail_and_delete_nothing() {
    let source_side = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let destination_side = tempfile::tempdir().unwrap();
    if ignores_case(source_side.path()) || !ignores_case(destination_side.path()) {
        return;
    }
    // A file and a directory, then two directories.
    let trees: [&[(&str, Option<&str>)]; 2] = [
        &[("A", Some("file")), ("a", None), ("a/f", Some("in a"))],
        &[
            ("A", None),
            ("A/x", Some("x")),
            ("a", None),
            ("a/y", Some("y")),
        ],
    ];
    for command in ["copy", "sync"] {
        for (index, tree) in trees.iter().enumerate() {
            let source = source_side.path().join(format!("{command}-{index}"));
            let destination = destination_side.path().join(format!("{command}-{index}"));
            fs::create_dir(&source).unwrap();
            for (path, content) in tree.iter() {
                match content {
                    Some(content) => fs::write(source.join(path), content).unwrap(),
                    None => fs::create_dir_all(source.join(path)).unwrap(),
                }
            }
            for run in 0..2 {
                let output = ferry(command, &[&source, &destination]);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert_eq!(output.status.code(), Some(23), "{command} {run}: {stderr}");
                assert!(
                    stderr.contains("the filesystem here takes its name for that of"),
                    "{stderr}"
                );
                // What is there is one of the Source's, whole, and only it.
                let kept = fs::read_dir(&destination)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(kept.len(), 1, "{kept:?}");
                let (paths, held): (Vec<_>, Vec<_>) = tree
                    .iter()
                    .filter(|(path, _)| path.split('/').next() == Some(kept[0].as_str()))
                    .map(|(path, content)| {
                        let at = destination.join(path);
                        (
                            path.to_string(),
                            content.map(|content| fs::read_to_string(&at).unwrap() == content),
                        )
                    })
                    .unzip();
                assert!(held.iter().all(|same| *same != Some(false)), "{paths:?}");
                let inside = |path: &Path| match fs::read_dir(path) {
                    Ok(names) => names.count(),
                    Err(_) => 0,
                };
                let expected = paths.iter().filter(|path| path.contains('/')).count();
                assert_eq!(inside(&destination.join(&kept[0])), expected, "{paths:?}");
            }
        }
    }
}

/// However deep an extra tree, `sync` deletes it, or not, without running
/// out of stack: up to the deepest it deletes, here 64, it goes; deeper, it
/// stays, said so, and the rest is done. Once taken down halfway, the rest
/// goes.
#[test]
fn a_deep_extra_tree_is_deleted_or_said_too_deep() {
    const DEEPEST: usize = 64;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("file"), "file").unwrap();
    let chain = |top: &str, depth: usize| {
        let mut path = destination.join(top);
        for _ in 1..depth {
            path.push("d");
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("f"), "f").unwrap();
    };
    chain("deep", DEEPEST);
    chain("deeper", DEEPEST + 36);
    let sync = || {
        Command::new(env!("CARGO_BIN_EXE_ferry"))
            .arg("sync")
            .args([&source, &destination])
            .env("FERRY_TEST_REMOVE_DEPTH", DEEPEST.to_string())
            .output()
            .unwrap()
    };

    let output = sync();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(23), "{stderr}");
    assert!(
        stderr.contains("deeper/") && stderr.contains("more than 64 directories deep"),
        "{stderr}"
    );
    assert!(!destination.join("deep").exists());
    assert!(destination.join("deeper").is_dir());
    assert_eq!(fs::read(destination.join("file")).unwrap(), b"file");

    let halfway: std::path::PathBuf = ["deeper"].into_iter().chain(["d"; DEEPEST / 2]).collect();
    fs::remove_dir_all(destination.join(halfway)).unwrap();
    succeeds(&sync());
    assert!(!destination.join("deeper").exists());
}

/// A tree is deleted without running out of stack, however deep: 200
/// levels, in a thread of 64 KiB. Deleting by recursion took over 1 KiB a
/// level, and ran out of it before 50 (DST-12); a level at a time takes
/// under 16 KiB, at 2,000 levels too. Not so deep as the deepest deleted,
/// which would hold thousands of directories open.
#[test]
fn a_deep_tree_is_deleted_in_little_stack() {
    const DEPTH: usize = 200;
    let root = tempfile::tempdir().unwrap();
    let mut level = root.path().join("deep");
    fs::create_dir(&level).unwrap();
    for _ in 1..DEPTH {
        level.push("d");
        fs::create_dir(&level).unwrap();
    }
    fs::write(level.join("file"), "f").unwrap();
    let opened = fs::File::open(root.path()).unwrap();
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let keep = ferry::fsys::Keep::protecting(Vec::new());
            ferry::fsys::remove_tree(&opened, b"deep", &keep)
        })
        .unwrap()
        .join()
        .unwrap()
        .unwrap();
    assert!(!root.path().join("deep").exists());
}

/// With the scan waiting, before a directory, while more than a few entries
/// sent are not compared yet: a copy, a dry run and a sync go through, the
/// wait comes, and, wherever the profile took a sample, no more files wait
/// to be sent than its bounds and a directory's own allow.
#[test]
fn a_scan_that_waits_for_the_destination_to_compare_goes_through() {
    const DIRECTORIES: usize = 40;
    const FILES: usize = 200;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    for directory in 0..DIRECTORIES {
        let directory = source.join(format!("d{directory:02}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..FILES {
            fs::write(directory.join(format!("f{file:03}")), "").unwrap();
        }
    }
    let profile = temp.path().join("profile.json");
    let run = |command: &str, dry_run: bool| {
        let mut arguments = vec![
            Path::new("--in-flight-entries=200"),
            Path::new("--backlog-files=3"),
            Path::new("--part-entries=50"),
            Path::new("--profile-file"),
            profile.as_path(),
        ];
        if dry_run {
            arguments.push(Path::new("--dry-run"));
        }
        arguments.extend([source.as_path(), destination.as_path()]);
        succeeds(&ferry(command, &arguments));
        let profiled = fs::read_to_string(&profile).unwrap();
        assert!(
            profiled.contains("wait for the Destination to compare"),
            "{command}: the scan never waited"
        );
        // Each sample's peak of needed files: its bound, the entries on the
        // way, and the largest directory's, whose needs come at once.
        for peak in profiled
            .split("\"name\":\"needed files\"")
            .skip(1)
            .map(|rest| {
                let at = rest.find("\"peak\":").unwrap() + 7;
                let digits: String = rest[at..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse::<usize>().unwrap()
            })
        {
            assert!(peak <= 3 + 200 + FILES + 50, "{command}: a peak of {peak}");
        }
    };
    run("copy", true);
    assert!(!destination.exists());
    run("copy", false);
    assert_eq!(paths(&destination), paths(&source));
    for directory in 0..DIRECTORIES {
        let name = format!("d{directory:02}");
        fs::write(source.join(&name).join("f000"), "changed").unwrap();
        fs::write(destination.join(&name).join("extra"), "extra").unwrap();
    }
    run("sync", false);
    assert_eq!(paths(&destination), paths(&source));
    assert_eq!(fs::read(destination.join("d07/f000")).unwrap(), b"changed");
}
