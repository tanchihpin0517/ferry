//! `ferry test` between two local directories, and to a host through a fake
//! `ssh`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

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
        .args(["--scale", "32M", "--kind", "mixed"])
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

/// What a host's shell prints as it starts, as a startup file might, is not
/// taken for what `ferry fixture` says there, such as a tree's digest.
#[test]
fn at_scale_what_a_hosts_shell_prints_is_not_taken_for_its_digest() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("a"), temp.path().join("b"));
    fs::create_dir(&source).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    // An ssh that runs the command here, after what the shell prints: it
    // cannot forward to a socket, so the streams join with --remote-join.
    // It logs each command it runs.
    let noisy = temp.path().join("noisy-ssh");
    let log = temp.path().join("log");
    fs::write(
        &noisy,
        format!(
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\n\
             printf '%s\\n' \"$1\" >> {log}\n\
             echo other; echo arch=bogus\nexec sh -c \"$1\"\n",
            log = log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&noisy, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("test")
        .arg(&source)
        .arg(format!("host:{}", destination.display()))
        .args(["--scale", "4M", "--kind", "mixed", "--remote-join"])
        .args(["--remote-ferry", env!("CARGO_BIN_EXE_ferry")])
        .env("FERRY_SSH", &noisy)
        .env("FERRY_HOME", temp.path().join("home"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");
    for step in [
        "ok    copy: the destination matches the source",
        "ok    sync: the destination matches the source",
        "8 passed, 0 failed",
    ] {
        assert!(stdout.contains(step), "{step}\n{stdout}");
    }
    // What it printed is said, rather than shown as Ferry's.
    assert!(!stdout.contains("bogus"), "{stdout}");
    assert!(stderr.contains("other / arch=bogus"), "{stderr}");
    // The fixtures ran through the host's launcher: no session ran one.
    let ran = fs::read_to_string(&log).unwrap();
    assert!(
        !ran.lines()
            .map(script_of)
            .any(|script| script.contains("'fixture'")),
        "{ran}"
    );
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
        .args(["--scale", "100G", "--kind", "small"])
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
    // The shell's own kill: a system may have no kill program.
    let interrupted = Command::new("sh")
        .args(["-c", &format!("kill -INT {}", test.id())])
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

/// The script a command line runs with `sh` on a host: its bytes are
/// written as octal escapes for `printf`, for any login shell to pass on.
fn script_of(command: &str) -> String {
    let start = "printf '\\''";
    let Some(at) = command.find(start) else {
        return command.to_owned();
    };
    let escaped = &command[at + start.len()..];
    let escaped = &escaped[..escaped.find('\'').unwrap_or(escaped.len())];
    let bytes: Vec<u8> = escaped
        .split('\\')
        .filter(|octal| !octal.is_empty())
        .filter_map(|octal| u8::from_str_radix(octal, 8).ok())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
