//! Directories listed, and compared, several at once, give what one at a
//! time does: a dry run says the same, in another order, and a transfer
//! leaves the same tree.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn ferry(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
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

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A tree of many small directories, three levels deep, with files in each,
/// and a larger one beside them.
fn tree(root: &Path) {
    for top in 0..6 {
        for middle in 0..5 {
            let directory = root.join(format!("t{top}/m{middle}"));
            fs::create_dir_all(directory.join("leaf")).unwrap();
            for file in 0..3 {
                fs::write(
                    directory.join(format!("f{file}")),
                    format!("{top}{middle}{file}"),
                )
                .unwrap();
            }
            fs::write(directory.join("leaf/x"), "x").unwrap();
        }
    }
    fs::create_dir_all(root.join("wide")).unwrap();
    for file in 0..40 {
        fs::write(root.join(format!("wide/w{file:02}")), format!("{file}")).unwrap();
    }
}

/// A Destination with the tree, but for some of it, and something besides:
/// files changed, a directory missing, extras, a file where the Source has
/// a directory.
fn destination_like(source: &Path, destination: &Path) {
    succeeds(&ferry(&["copy", text(source), text(destination)]));
    fs::write(destination.join("t1/m2/f0"), "changed, and longer").unwrap();
    fs::remove_dir_all(destination.join("t3/m1")).unwrap();
    fs::create_dir_all(destination.join("t2/extra/deeper")).unwrap();
    fs::write(destination.join("t2/extra/deeper/old"), "old").unwrap();
    fs::write(destination.join("wide/extra"), "old").unwrap();
    fs::remove_dir_all(destination.join("t4/m0/leaf")).unwrap();
    fs::write(destination.join("t4/m0/leaf"), "a file").unwrap();
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
fn directories_listed_at_once_give_what_one_at_a_time_does() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    tree(&source);
    let prepared = temp.path().join("prepared");
    destination_like(&source, &prepared);

    // A dry run says the same, whether a directory is one batch or several.
    for batch in ["0", "2"] {
        let dry = |listers: &str| {
            let output = ferry(&[
                "sync",
                "--dry-run",
                "--listers",
                listers,
                "--batch-entries",
                batch,
                text(&source),
                text(&prepared),
            ]);
            succeeds(&output);
            String::from_utf8(output.stdout).unwrap()
        };
        let sorted = |said: String| {
            let mut lines: Vec<String> = said.lines().map(str::to_owned).collect();
            lines.sort();
            lines
        };
        let one = dry("1");
        assert!(one.contains("deleting t2/extra/"), "{one}");
        assert!(one.contains("t3/m1/"), "{one}");
        let one = sorted(one);
        for listers in ["4", "16"] {
            let several = sorted(dry(listers));
            assert_eq!(several, one, "{listers} at once, batches of {batch}");
        }
    }

    // A sync, and a first copy, leave the same tree.
    for listers in ["1", "4"] {
        let destination = temp.path().join(format!("synced-{listers}"));
        destination_like(&source, &destination);
        succeeds(&ferry(&[
            "sync",
            "--listers",
            listers,
            "--batch-entries",
            "2",
            text(&source),
            text(&destination),
        ]));
        assert_eq!(walk(&destination), walk(&source), "{listers} at once");
        let copied = temp.path().join(format!("copied-{listers}"));
        succeeds(&ferry(&[
            "copy",
            "--listers",
            listers,
            text(&source),
            text(&copied),
        ]));
        assert_eq!(walk(&copied), walk(&source), "{listers} at once");
    }
}
