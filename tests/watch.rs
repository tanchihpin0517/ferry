//! `--watch`: a transfer round after round, as SOURCE changes, until it is
//! stopped, or a round fails; on this machine, and through a fake `ssh`
//! that runs the command here, as `tests/remote.rs` does.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

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

/// What ferry said, as `log` holds it so far, for a test that fails.
fn ferry_said(log: &Path) -> String {
    fs::read_to_string(log).unwrap_or_default()
}

/// Waits until `done` holds, or fails the test, saying what `log` says.
#[track_caller]
fn until(what: &str, log: &Path, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < PATIENCE,
            "{what}: not within {PATIENCE:?}; ferry said:\n{}",
            ferry_said(log)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for the watch to end with `code`, or fails the test, saying what
/// `log` says: that it did not end, or ended otherwise.
#[track_caller]
fn ended(child: &mut Child, log: &Path, code: i32) {
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!(
                "the watch did not end within {PATIENCE:?}; ferry said:\n{}",
                ferry_said(log)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(code),
        "{status}; ferry said:\n{}",
        ferry_said(log)
    );
}

/// Stops the watch as Ctrl-C does, and waits for it to end with 130, as
/// Ctrl-C's. Sent here, as an image need not have `kill`.
#[track_caller]
fn interrupt(child: &mut Child, log: &Path) {
    let pid = Pid::from_raw(child.id() as i32).unwrap();
    kill_process(pid, Signal::INT).unwrap();
    ended(child, log, 130)
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
    interrupt(&mut child, &log);
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
    assert!(
        destination.join("a").exists(),
        "ferry said:\n{}",
        ferry_said(&log)
    );
    interrupt(&mut child, &log);
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
    interrupt(&mut child, &log);
    fs::set_permissions(source.join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
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
    ended(&mut child, &log, 23);
    // Nothing was deleted for it.
    assert!(
        destination.join("a").exists(),
        "ferry said:\n{}",
        ferry_said(&log)
    );
}

/// Starts `sync --watch SECONDS` of `source`'s contents into `destination`,
/// with `environment`.
fn sync(
    source: &Path,
    destination: &Path,
    seconds: &str,
    log: &Path,
    environment: &[(&str, &Path)],
) -> Child {
    watch(
        &[
            "sync".as_ref(),
            "--watch".as_ref(),
            seconds.as_ref(),
            contents(source).as_os_str(),
            destination.as_os_str(),
        ],
        log,
        environment,
    )
}

#[test]
fn a_watch_has_no_round_until_source_changes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = sync(&source, &destination, "0", &log, &[]);
    until("the first round", &log, || destination.join("a").exists());
    // What changes in DESTINATION is no change in SOURCE: no round comes
    // for it, nor for what each round reads.
    fs::remove_file(destination.join("a")).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !destination.join("a").exists(),
        "ferry said:\n{}",
        ferry_said(&log)
    );
    // A change in SOURCE: a round, which copies all that differs.
    fs::write(source.join("b"), "b").unwrap();
    until("the round for the change", &log, || {
        destination.join("b").exists() && destination.join("a").exists()
    });
    interrupt(&mut child, &log);
    let said = fs::read_to_string(&log).unwrap();
    assert!(said.contains("watching"), "{said}");
}

#[test]
fn a_directory_made_during_the_watch_is_watched_too() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    let mut child = sync(&source, &destination, "0", &log, &[]);
    until("the first round", &log, || destination.exists());
    fs::create_dir_all(source.join("new/deeper")).unwrap();
    until("the new directory", &log, || {
        destination.join("new/deeper").exists()
    });
    // What changes in it later is seen.
    fs::write(source.join("new/deeper/file"), "file").unwrap();
    until("a file in it", &log, || {
        destination.join("new/deeper/file").exists()
    });
    interrupt(&mut child, &log);
}

#[test]
fn a_watch_that_cannot_see_changes_has_a_round_at_each_point() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = sync(
        &source,
        &destination,
        "1",
        &log,
        &[("FERRY_TEST_UNWATCHED", Path::new("1"))],
    );
    until("the first round", &log, || destination.join("a").exists());
    // Each second, a round, whatever changed: one puts back what was taken
    // out of DESTINATION.
    fs::remove_file(destination.join("a")).unwrap();
    until("a round on time", &log, || destination.join("a").exists());
    interrupt(&mut child, &log);
    let said = fs::read_to_string(&log).unwrap();
    assert!(said.contains("cannot be watched for changes"), "{said}");
    assert!(said.contains("a round every 1 s"), "{said}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_watch_past_the_limit_on_watches_has_a_round_at_each_point() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::create_dir_all(source.join("one")).unwrap();
    fs::create_dir_all(source.join("two")).unwrap();
    fs::write(source.join("a"), "a").unwrap();
    let mut child = sync(
        &source,
        &destination,
        "1",
        &log,
        &[("FERRY_TEST_WATCHES", Path::new("2"))],
    );
    until("the first round", &log, || destination.join("a").exists());
    fs::remove_file(destination.join("a")).unwrap();
    until("a round on time", &log, || destination.join("a").exists());
    interrupt(&mut child, &log);
    let said = fs::read_to_string(&log).unwrap();
    assert!(said.contains("max_user_watches"), "{said}");
}

#[test]
fn a_watch_of_a_file_sees_it_change() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("file");
    let destination = temp.path().join("destination");
    let log = temp.path().join("log");
    fs::write(&source, "first").unwrap();
    fs::create_dir(&destination).unwrap();
    let mut child = watch(
        &[
            "copy".as_ref(),
            "--watch".as_ref(),
            "0".as_ref(),
            source.as_os_str(),
            destination.as_os_str(),
        ],
        &log,
        &[],
    );
    until("the first round", &log, || {
        read(&destination.join("file")).as_deref() == Some("first")
    });
    // Replaced, as an editor saves it: a new file renamed over it.
    fs::write(temp.path().join("file.new"), "second").unwrap();
    fs::rename(temp.path().join("file.new"), &source).unwrap();
    until("the change", &log, || {
        read(&destination.join("file")).as_deref() == Some("second")
    });
    interrupt(&mut child, &log);
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
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{said}");
    assert!(said.contains("--watch with --dry-run"), "{said}");
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
        assert_eq!(
            output.status.code(),
            Some(2),
            "{seconds}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

/// Watches SOURCE into a host, through the fake `ssh`, each round that
/// changed something summed up as it ends.
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
            "--progress".as_ref(),
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
    interrupt(&mut child, &log);
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
    // Between rounds, the first over, as its summary says: cut during a
    // round, a stream may break first, which the watch says instead. Its
    // mark, not the word alone: a shell may say `done`, as macOS's does
    // printing the loops it ends as the first session becomes the monitor.
    until("the first round over", &log, || {
        read(&log).is_some_and(|said| said.contains("✓ done"))
    });
    // Every `ssh` to the host goes, with all it runs, each its own process
    // group, as with a connection that dropped: its first session, which
    // became its monitor, among them.
    let pids = fs::read_to_string(temp.path().join("pids")).unwrap();
    for pid in pids.lines() {
        let group = Pid::from_raw(pid.parse().unwrap()).unwrap();
        // One gone already, as a probe's, is let be.
        let _ = kill_process_group(group, Signal::KILL);
    }
    ended(&mut child, &log, 1);
    let said = fs::read_to_string(&log).unwrap();
    assert!(said.contains("host: the connection to it ended"), "{said}");
}
