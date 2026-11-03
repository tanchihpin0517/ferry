//! `--watch`: a transfer round after round, as SOURCE changes, until it is
//! stopped, or a round fails; on this machine, and through a fake `ssh`
//! that runs the command here, as `tests/remote.rs` does.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a change may take to reach DESTINATION, or the watch to end.
const PATIENCE: Duration = Duration::from_secs(20);

/// SOURCE with a trailing slash: its contents.
fn contents(path: &Path) -> PathBuf {
    path.join("")
}

/// Starts `ferry` with `args`, what it says on standard error going to
/// `log`.
fn watch(args: &[&std::ffi::OsStr], log: &Path, environment: &[(&str, &Path)]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(log).unwrap());
    for (name, value) in environment {
        command.env(name, value);
    }
    command.spawn().unwrap()
}

/// Waits until `done` holds, or fails the test, saying what `log` says.
fn until(what: &str, log: &Path, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < PATIENCE,
            "{what}: not within {PATIENCE:?}; ferry said:\n{}",
            fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for the watch to end, or fails the test.
fn ended(child: &mut Child, log: &Path) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!(
                "the watch did not end within {PATIENCE:?}; ferry said:\n{}",
                fs::read_to_string(log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stops the watch as Ctrl-C does, and waits for it to end.
fn interrupt(child: &mut Child, log: &Path) -> ExitStatus {
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    ended(child, log)
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

#[test]
fn a_watch_carries_each_change_over_until_ctrl_c() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::write(source.join("a"), "first").unwrap();
    let mut child = watch(
        &[
            "sync".as_ref(),
            "--watch".as_ref(),
            "0".as_ref(),
            contents(&source).as_os_str(),
            destination.as_os_str(),
        ],
        &log,
        &[],
    );
    until("the first round", &log, || {
        read(&destination.join("a")).as_deref() == Some("first")
    });
    fs::write(source.join("a"), "second, longer").unwrap();
    fs::write(source.join("dir/b"), "new").unwrap();
    until("a change and a new file", &log, || {
        read(&destination.join("a")).as_deref() == Some("second, longer")
            && read(&destination.join("dir/b")).as_deref() == Some("new")
    });
    fs::remove_file(source.join("a")).unwrap();
    until("a deletion", &log, || !destination.join("a").exists());
    let status = interrupt(&mut child, &log);
    assert_eq!(status.code(), Some(130), "{status}");
}

#[test]
fn copy_with_watch_deletes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = watch(
        &[
            "copy".as_ref(),
            "--watch".as_ref(),
            "0".as_ref(),
            contents(&source).as_os_str(),
            destination.as_os_str(),
        ],
        &log,
        &[],
    );
    until("the first round", &log, || destination.join("a").exists());
    fs::remove_file(source.join("a")).unwrap();
    fs::write(source.join("b"), "b").unwrap();
    until("the round after", &log, || destination.join("b").exists());
    assert!(destination.join("a").exists());
    let status = interrupt(&mut child, &log);
    assert_eq!(status.code(), Some(130), "{status}");
}

#[test]
fn a_round_that_cannot_read_a_path_goes_on() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("secret"), "secret").unwrap();
    fs::set_permissions(source.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = watch(
        &[
            "copy".as_ref(),
            "--watch".as_ref(),
            "0".as_ref(),
            contents(&source).as_os_str(),
            destination.as_os_str(),
        ],
        &log,
        &[],
    );
    until("the first round", &log, || destination.join("a").exists());
    fs::write(source.join("b"), "b").unwrap();
    until("the round after", &log, || destination.join("b").exists());
    let status = interrupt(&mut child, &log);
    fs::set_permissions(source.join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(status.code(), Some(130), "{status}");
    let said = fs::read_to_string(&log).unwrap();
    assert!(said.contains("could not be read"), "{said}");
}

#[test]
fn a_watch_whose_source_cannot_be_read_any_more_ends_with_23() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = watch(
        &[
            "sync".as_ref(),
            "--watch".as_ref(),
            "0".as_ref(),
            contents(&source).as_os_str(),
            destination.as_os_str(),
        ],
        &log,
        &[],
    );
    until("the first round", &log, || destination.join("a").exists());
    fs::rename(&source, temp.path().join("gone")).unwrap();
    let status = ended(&mut child, &log);
    assert_eq!(status.code(), Some(23), "{status}");
    // Nothing was deleted for it.
    assert!(destination.join("a").exists());
}

#[test]
fn watch_with_dry_run_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--watch", "1", "--dry-run"])
        .arg(temp.path().join(""))
        .arg(temp.path().join("destination"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--watch with --dry-run"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn watch_takes_whole_seconds() {
    let temp = tempfile::tempdir().unwrap();
    for seconds in ["1.5", "-1", "soon"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args(["copy", "--watch", seconds])
            .arg(temp.path().join(""))
            .arg(temp.path().join("destination"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{seconds}");
    }
}

/// A fake `ssh`, as `tests/remote.rs`'s, that writes its process ID to
/// `pids` first: each runs the command it is given in its place.
fn fake_ssh(directory: &Path) -> PathBuf {
    let path = directory.join("fake-ssh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho $$ >> {}\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
            directory.join("pids").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Watches SOURCE into a host, through the fake `ssh`.
fn watch_to_host(temp: &Path, seconds: &str, source: &Path, destination: &Path) -> Child {
    fs::create_dir_all(temp.join("home")).unwrap();
    let ssh = fake_ssh(temp);
    watch(
        &[
            "sync".as_ref(),
            "--watch".as_ref(),
            seconds.as_ref(),
            "--remote-ferry".as_ref(),
            env!("CARGO_BIN_EXE_ferry").as_ref(),
            "--remote-join".as_ref(),
            contents(source).as_os_str(),
            format!("host:{}", destination.display()).as_ref(),
        ],
        &temp.join("log"),
        &[("FERRY_SSH", &ssh), ("FERRY_HOME", &temp.join("home"))],
    )
}

#[test]
fn a_watch_to_a_host_carries_each_change_over() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = watch_to_host(temp.path(), "0", &source, &destination);
    until("the first round", &log, || destination.join("a").exists());
    fs::write(source.join("b"), "b").unwrap();
    fs::remove_file(source.join("a")).unwrap();
    until("the round after", &log, || {
        destination.join("b").exists() && !destination.join("a").exists()
    });
    let status = interrupt(&mut child, &log);
    assert_eq!(status.code(), Some(130), "{status}");
}

#[test]
fn a_watch_whose_connection_drops_ends() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = watch_to_host(temp.path(), "1", &source, &destination);
    until("the first round", &log, || destination.join("a").exists());
    // Every `ssh` to the host goes, with all it runs, each its own process
    // group, as with a connection that dropped: its first session, which
    // became its monitor, among them.
    let pids = fs::read_to_string(temp.path().join("pids")).unwrap();
    for pid in pids.lines() {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status();
    }
    let status = ended(&mut child, &log);
    let said = fs::read_to_string(&log).unwrap();
    assert_eq!(status.code(), Some(1), "{status}: {said}");
    assert!(said.contains("host: the connection to it ended"), "{said}");
}
