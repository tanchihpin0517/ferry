//! A directory past the bound on the names held, end to end: its names,
//! sorted on disk as `FERRY_TEST_NAMES_HELD` brings the bound down to a few
//! dozen, give the same transfer as sorted in memory, and leave nothing in
//! `TMPDIR`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Runs `ferry` with `args`, the names held past `held` bytes sorted in
/// `temp`, if given.
fn ferry(args: &[&Path], held: Option<(usize, &Path)>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command.args(args);
    if let Some((held, temp)) = held {
        command
            .env("FERRY_TEST_NAMES_HELD", held.to_string())
            .env("TMPDIR", temp);
    }
    command.output().unwrap()
}

fn succeeds(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A tree with a directory of 400 names, half of them directories with a
/// file each, and a few more beside it.
fn tree(root: &Path) {
    let many = root.join("many");
    fs::create_dir_all(&many).unwrap();
    for index in 0..400 {
        if index % 2 == 0 {
            let directory = many.join(format!("dir-{index:04}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("in"), format!("in {index}")).unwrap();
        } else {
            fs::write(many.join(format!("file-{index:04}")), format!("{index}")).unwrap();
        }
    }
    fs::create_dir_all(root.join("other/deeper")).unwrap();
    fs::write(root.join("other/deeper/leaf"), "leaf").unwrap();
    fs::write(root.join("top"), "top").unwrap();
}

/// Every path under `root`, with a file's content.
fn walk(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(directory) = todo.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            if path.is_dir() {
                found.push((relative, None));
                todo.push(path);
            } else {
                found.push((relative, Some(fs::read(&path).unwrap())));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn a_directory_past_the_bound_is_listed_and_copied_as_in_memory() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    tree(&source);
    let sorting = temp.path().join("sorting");
    fs::create_dir(&sorting).unwrap();
    let held = Some((2_000, sorting.as_path()));

    let dry = |held| {
        ferry(
            &[
                Path::new("copy"),
                Path::new("--dry-run"),
                &source,
                &destination,
            ],
            held,
        )
    };
    let in_memory = dry(None);
    succeeds(&in_memory);
    let on_disk = dry(held);
    succeeds(&on_disk);
    assert!(in_memory.stdout.len() > 400 * 10);
    assert_eq!(
        String::from_utf8_lossy(&on_disk.stdout),
        String::from_utf8_lossy(&in_memory.stdout)
    );

    succeeds(&ferry(&[Path::new("copy"), &source, &destination], held));
    assert_eq!(walk(&destination), walk(&source));
    assert_eq!(fs::read_dir(&sorting).unwrap().count(), 0);
}

#[test]
fn names_that_cannot_be_sorted_on_disk_fail_the_transfer_naming_tmpdir() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    tree(&source);
    let missing = temp.path().join("missing");
    let output = ferry(
        &[Path::new("copy"), &source, &destination],
        Some((2_000, &missing)),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("list many/"), "{stderr}");
    assert!(stderr.contains(&missing.display().to_string()), "{stderr}");
    assert!(stderr.contains("TMPDIR"), "{stderr}");
}

#[test]
fn a_large_directory_synced_over_one_with_extras_ends_as_in_memory() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    tree(&source);
    let sorting = temp.path().join("sorting");
    fs::create_dir(&sorting).unwrap();
    let held = Some((2_000, sorting.as_path()));
    // A destination with the tree, but for some of it, and with extras
    // among it: files and directories with something in them.
    let destination_like = |destination: &Path| {
        succeeds(&ferry(&[Path::new("copy"), &source, destination], None));
        for index in (0..400).step_by(7) {
            let name = destination.join(format!("many/extra-{index:04}"));
            if index % 2 == 0 {
                fs::create_dir(&name).unwrap();
                fs::write(name.join("old"), "old").unwrap();
            } else {
                fs::write(&name, "old").unwrap();
            }
        }
        for index in (1..400).step_by(10) {
            fs::remove_file(destination.join(format!("many/file-{index:04}"))).unwrap();
        }
    };
    let (first, second) = (temp.path().join("first"), temp.path().join("second"));
    destination_like(&first);
    destination_like(&second);

    let dry = |destination: &Path, held| {
        ferry(
            &[
                Path::new("sync"),
                Path::new("--dry-run"),
                &source,
                destination,
            ],
            held,
        )
    };
    let in_memory = dry(&first, None);
    succeeds(&in_memory);
    let on_disk = dry(&second, held);
    succeeds(&on_disk);
    let listed = String::from_utf8_lossy(&in_memory.stdout);
    assert!(listed.contains("deleting many/extra-0000/"), "{listed}");
    assert_eq!(String::from_utf8_lossy(&on_disk.stdout), listed);
    // Compared four directories at once, the same.
    let at_once = ferry(
        &[
            Path::new("sync"),
            Path::new("--dry-run"),
            Path::new("--listers"),
            Path::new("4"),
            &source,
            &second,
        ],
        held,
    );
    succeeds(&at_once);
    assert_eq!(String::from_utf8_lossy(&at_once.stdout), listed);

    succeeds(&ferry(&[Path::new("sync"), &source, &first], None));
    succeeds(&ferry(&[Path::new("sync"), &source, &second], held));
    assert_eq!(walk(&first), walk(&source));
    assert_eq!(walk(&second), walk(&source));
    assert_eq!(fs::read_dir(&sorting).unwrap().count(), 0);
}
