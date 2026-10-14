//! `ferry --dry-run` end to end, with the client starting a `ferry server`
//! child for a local destination.

use std::path::Path;
use std::process::{Command, Output};

fn ferry(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(args)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn connects_to_a_local_server() {
    let temp = tempfile::tempdir().unwrap();
    let output = ferry(&[
        Path::new("--dry-run"),
        temp.path(),
        &temp.path().join("destination"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
}

#[test]
fn copying_is_not_implemented_yet() {
    let temp = tempfile::tempdir().unwrap();
    let output = ferry(&[temp.path(), &temp.path().join("destination")]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--dry-run"), "{}", stderr(&output));
    assert!(!temp.path().join("destination").exists());
}
