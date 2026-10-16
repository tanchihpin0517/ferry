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
    assert!(stdout.contains(" passed, 0 failed, "), "{stdout}");
    // Only what needs a server is skipped: both sides are local.
    assert!(
        stdout
            .lines()
            .filter(|line| line.starts_with("  skip"))
            .all(|line| line
                .ends_with("both sides are on this machine, so no server runs on a host")),
        "{stdout}"
    );
    assert!(
        stdout.contains("phase 1: prepare") && stdout.contains("phase 2: transfer"),
        "{stdout}"
    );
    assert!(
        stdout.contains("ok    copy:") && stdout.contains("ok    sync:"),
        "{stdout}"
    );
    // Nothing is left behind.
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}

#[test]
fn at_scale_copies_changes_syncs_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("a"), temp.path().join("b"));
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("test")
        .arg(&source)
        .arg(&destination)
        .args(["--scale", "32M"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for step in [
        "ok    generate: the source tree, ",
        "ok    copy: the tree is received, at ",
        "ok    copy: the destination matches the source",
        "ok    change: the source, ",
        "ok    sync: the changes are received",
        "ok    sync: the destination matches the source",
        "ok    sync: nothing more is needed",
        "7 passed, 0 failed",
    ] {
        assert!(stdout.contains(step), "{step}\n{stdout}");
    }
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}

#[test]
fn at_scale_a_first_ctrl_c_stops_and_cleans_up() {
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("a"), temp.path().join("b"));
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();

    // Far more than is generated before the Ctrl-C.
    let test = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("test")
        .arg(&source)
        .arg(&destination)
        .args(["--scale", "100G"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while fs::read_dir(&source).unwrap().count() == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "nothing generated"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let interrupted = Command::new("kill")
        .args(["-INT", &test.id().to_string()])
        .status()
        .unwrap();
    assert!(interrupted.success());

    let output = test.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(130), "{stdout}");
    assert!(
        stdout.contains("  stop  generate: the source tree"),
        "{stdout}"
    );
    assert!(stdout.contains("removing the test trees"), "{stdout}");
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}
