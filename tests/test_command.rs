//! `ferry test` between two local directories.

use std::fs;
use std::process::Command;

#[test]
fn runs_every_scenario_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("a"), temp.path().join("b"));
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("test")
        .arg(&source)
        .arg(&destination)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(" passed, 0 failed, 0 skipped"), "{stdout}");
    assert!(
        stdout.contains("ok    copy:") && stdout.contains("ok    sync:"),
        "{stdout}"
    );
    // Nothing is left behind.
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}
