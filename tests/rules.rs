//! `--exclude`, `--include`, their `-from` files, and `--delete-excluded`,
//! end to end, between two local directories: what is Ferry's own about
//! them. What each rule matches is checked against rsync in `rsync.rs`.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn ferry(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(args)
        .arg("--no-ui")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// SOURCE with a trailing slash: its contents.
fn contents(path: &Path) -> String {
    format!("{}/", text(path))
}

/// Every path under `root`, a directory's ending in `/`, sorted.
fn paths(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut waiting = vec![root.to_path_buf()];
    while let Some(directory) = waiting.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let shown = path.strip_prefix(root).unwrap().display().to_string();
            if path.is_dir() {
                waiting.push(path);
                found.push(format!("{shown}/"));
            } else {
                found.push(shown);
            }
        }
    }
    found.sort();
    found
}

fn source(root: &Path) -> std::path::PathBuf {
    let source = root.join("source");
    for path in ["a.o", "a.c", "keep.o", "build/f"] {
        let path = source.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
    source
}

fn refused_with(output: &Output, said: &str) {
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(said), "{stderr}");
}

/// The rules are tried in the order given, across `--exclude` and
/// `--include` and their files: the first that matches says.
#[test]
fn the_rules_are_tried_in_the_order_given() {
    let temp = tempfile::tempdir().unwrap();
    let source = source(temp.path());
    let rules = temp.path().join("rules");
    fs::write(&rules, "*.o\n").unwrap();
    let included_first = temp.path().join("included-first");
    let output = ferry(&[
        "copy",
        "--include=keep.o",
        "--exclude-from",
        text(&rules),
        &contents(&source),
        text(&included_first),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        paths(&included_first),
        ["a.c", "build/", "build/f", "keep.o"]
    );
    let excluded_first = temp.path().join("excluded-first");
    let output = ferry(&[
        "copy",
        "--exclude-from",
        text(&rules),
        "--include=keep.o",
        &contents(&source),
        text(&excluded_first),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(paths(&excluded_first), ["a.c", "build/", "build/f"]);
}

/// `-` reads the rules from standard input.
#[test]
fn rules_are_read_from_standard_input() {
    let temp = tempfile::tempdir().unwrap();
    let source = source(temp.path());
    let destination = temp.path().join("destination");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-ui", "--exclude-from", "-"])
        .args([&contents(&source), text(&destination)])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"# comment\nbuild/\n*.o\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(paths(&destination), ["a.c"]);
}

/// A rule no path can match, `!`, and `copy --delete-excluded` are refused
/// before anything happens, as is a bad line of a file of rules, which is
/// named; a file of rules that cannot be read fails.
#[test]
fn what_cannot_be_done_is_refused_first() {
    let temp = tempfile::tempdir().unwrap();
    let source = source(temp.path());
    let destination = temp.path().join("destination");
    let to = |extra: &[&str]| {
        let mut args = vec!["copy"];
        args.extend_from_slice(extra);
        let from = contents(&source);
        args.extend([from.as_str(), text(&destination)]);
        ferry(&args)
    };
    refused_with(
        &to(&["--exclude=./a.o"]),
        "the rule `./a.o` can match nothing",
    );
    refused_with(&to(&["--exclude=build//"]), "can match nothing");
    refused_with(&to(&["--include=!"]), "is not built yet");
    refused_with(&to(&["--delete-excluded"]), "use sync --delete-excluded");
    let bad = temp.path().join("bad");
    fs::write(&bad, "fine\n\n/../x\n").unwrap();
    refused_with(
        &to(&["--exclude-from", text(&bad)]),
        "line 3: the rule `/../x`",
    );
    let missing = temp.path().join("missing");
    let output = to(&["--exclude-from", text(&missing)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("read the rules in"));
    assert!(!destination.exists());
}

/// `sync` keeps what the rules leave out of what DESTINATION has besides,
/// and `--delete-excluded` deletes it; SOURCE without a trailing slash is
/// matched by its name, which a rule that matches it leaves out whole.
#[test]
fn sync_keeps_what_is_left_out_unless_asked() {
    let temp = tempfile::tempdir().unwrap();
    let source = source(temp.path());
    let destination = temp.path().join("destination");
    fs::create_dir_all(&destination).unwrap();
    for extra in ["old.o", "old.txt"] {
        fs::write(destination.join(extra), "old").unwrap();
    }
    let output = ferry(&[
        "sync",
        "--exclude=*.o",
        &contents(&source),
        text(&destination),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(paths(&destination), ["a.c", "build/", "build/f", "old.o"]);
    let output = ferry(&[
        "sync",
        "--exclude=*.o",
        "--delete-excluded",
        &contents(&source),
        text(&destination),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(paths(&destination), ["a.c", "build/", "build/f"]);
    let output = ferry(&[
        "sync",
        "--exclude=/source",
        text(&source),
        text(&destination),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(!destination.join("source").exists());
}
