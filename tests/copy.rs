//! `ferry copy` end to end, between two local directories.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

fn ferry(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("copy")
        .args(args)
        .output()
        .unwrap()
}

fn succeeds(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A directory of more symlink targets than one message could carry, as
/// 10,000 of 2,000 bytes are: a dry run, a copy, and a sync of it succeed,
/// and every link keeps its target. Where a target may be no longer than
/// 1,023 bytes, as on xfs and macOS, no part of 10,000 comes near a message,
/// and there is nothing to test.
#[test]
fn a_directory_of_long_symlinks_is_copied() {
    const LINKS: usize = 10_000;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    let target = format!("/{}", "t".repeat(1999));
    if let Err(error) = symlink(&target, temp.path().join("probe")) {
        assert_eq!(error.raw_os_error(), Some(libc::ENAMETOOLONG), "{error}");
        eprintln!("this filesystem takes no target of 2,000 bytes: not tested");
        return;
    }
    for i in 0..LINKS {
        symlink(&target, source.join(format!("l{i:05}"))).unwrap();
    }

    succeeds(ferry(&[Path::new("--dry-run"), &source, &destination]));
    succeeds(ferry(&[&source, &destination]));
    let synced = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("sync")
        .args([&source, &destination])
        .output()
        .unwrap();
    succeeds(synced);
    let mut links = 0;
    for entry in fs::read_dir(&destination).unwrap() {
        assert_eq!(
            fs::read_link(entry.unwrap().path()).unwrap(),
            Path::new(&target)
        );
        links += 1;
    }
    assert_eq!(links, LINKS);
}

/// A source named by a symlink to a directory is that directory, as the
/// user named it: its files are read from under it, a directory at a time.
#[test]
fn a_source_named_by_a_symlink_is_copied_as_its_directory() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("release-42");
    fs::create_dir_all(real.join("a/b")).unwrap();
    fs::write(real.join("top"), "top").unwrap();
    fs::write(real.join("a/b/deep"), "deep").unwrap();
    let current = temp.path().join("current");
    symlink(&real, &current).unwrap();
    let destination = temp.path().join("destination");
    succeeds(ferry(&[&current, &destination]));
    assert_eq!(fs::read(destination.join("top")).unwrap(), b"top");
    assert_eq!(fs::read(destination.join("a/b/deep")).unwrap(), b"deep");
    assert!(!fs::symlink_metadata(&destination).unwrap().is_symlink());
}

#[test]
fn copies_files_and_directories() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("dir/empty")).unwrap();
    fs::write(source.join("small"), "small").unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
    fs::write(source.join("dir/big"), &big).unwrap();
    fs::set_permissions(source.join("small"), fs::Permissions::from_mode(0o751)).unwrap();

    succeeds(ferry(&[&source, &destination]));
    assert_eq!(fs::read(destination.join("small")).unwrap(), b"small");
    assert_eq!(fs::read(destination.join("dir/big")).unwrap(), big);
    assert!(destination.join("dir/empty").is_dir());
    let metadata = fs::metadata(destination.join("small")).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o751);
    assert_eq!(
        metadata.modified().unwrap(),
        fs::metadata(source.join("small"))
            .unwrap()
            .modified()
            .unwrap()
    );

    // Nothing is left to transfer, and no temporary file is left behind.
    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let names: Vec<_> = fs::read_dir(&destination)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        names
            .iter()
            .all(|name| !name.to_string_lossy().contains(".ferry-")),
        "{names:?}"
    );
}

#[test]
fn copies_a_single_file() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("file"), "content").unwrap();
    succeeds(ferry(&[
        &temp.path().join("file"),
        &temp.path().join("copy"),
    ]));
    assert_eq!(fs::read(temp.path().join("copy")).unwrap(), b"content");
}

#[test]
fn replaces_a_symlink_where_a_directory_belongs() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let outside = temp.path().join("outside");
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::write(source.join("dir/file"), "content").unwrap();
    fs::create_dir_all(&destination).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, destination.join("dir")).unwrap();

    succeeds(ferry(&[&source, &destination]));
    // The link was replaced by a directory, and nothing was written through it.
    assert!(
        fs::symlink_metadata(destination.join("dir"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(fs::read(destination.join("dir/file")).unwrap(), b"content");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn a_directory_in_the_way_fails_that_file_only() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("name"), "file").unwrap();
    fs::write(source.join("other"), "other").unwrap();
    fs::create_dir_all(destination.join("name/inside")).unwrap();

    let output = ferry(&[&source, &destination]);
    // A partial transfer, as rsync's.
    assert_eq!(output.status.code(), Some(23));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot copy name"), "{stderr}");
    assert!(stderr.contains("1 entry could not be copied"), "{stderr}");
    assert!(destination.join("name/inside").is_dir());
    // The rest was copied.
    assert_eq!(fs::read(destination.join("other")).unwrap(), b"other");
}

/// A file in SOURCE that cannot be read is skipped, and the rest copied:
/// the run exits with 24, as rsync does for what it could not send.
#[test]
fn an_unreadable_file_exits_24() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("locked"), "locked").unwrap();
    fs::set_permissions(source.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(source.join("other"), "other").unwrap();
    let output = ferry(&[&source, &destination]);
    assert_eq!(
        output.status.code(),
        Some(24),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(destination.join("other")).unwrap(), b"other");
}

#[test]
fn copies_symlinks_fifos_modes_and_directory_times() {
    use std::os::unix::fs::FileTypeExt;
    use std::time::{Duration, SystemTime};
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::write(source.join("dir/setuid"), "x").unwrap();
    fs::set_permissions(
        source.join("dir/setuid"),
        fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    symlink("dir/setuid", source.join("link")).unwrap();
    symlink("nowhere", source.join("dangling")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(source.join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    fs::set_permissions(source.join("dir"), fs::Permissions::from_mode(0o750)).unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::open(source.join("dir"))
        .unwrap()
        .set_modified(old)
        .unwrap();

    succeeds(ferry(&[&source, &destination]));
    assert_eq!(
        fs::read_link(destination.join("link")).unwrap(),
        Path::new("dir/setuid")
    );
    assert_eq!(
        fs::read_link(destination.join("dangling")).unwrap(),
        Path::new("nowhere")
    );
    assert!(
        fs::symlink_metadata(destination.join("fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    let setuid = fs::metadata(destination.join("dir/setuid")).unwrap();
    assert_eq!(setuid.permissions().mode() & 0o7777, 0o4755);
    let directory = fs::metadata(destination.join("dir")).unwrap();
    assert_eq!(directory.permissions().mode() & 0o7777, 0o750);
    assert_eq!(directory.modified().unwrap(), old);

    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn permissions_alone_are_listed_and_fixed() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "content").unwrap();
    succeeds(ferry(&[&source, &destination]));
    fs::set_permissions(destination.join("file"), fs::Permissions::from_mode(0o600)).unwrap();

    let output = ferry(&[Path::new("--dry-run"), &source, &destination]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "file\n");
    succeeds(ferry(&[&source, &destination]));
    let mode = fs::metadata(destination.join("file"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        fs::metadata(source.join("file"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    );
}

#[test]
fn an_interrupted_copy_leaves_no_temporary_files() {
    use std::io::Write;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::time::{Duration, Instant};
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    // Large, and sent as it is, so that a fast machine is still writing it
    // when it is interrupted.
    let chunk: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut big = fs::File::create(source.join("big")).unwrap();
    for _ in 0..512 {
        big.write_all(&chunk).unwrap();
    }
    drop(big);

    // A group of its own, so that Ctrl-C can be sent to the client and its
    // server together, as a terminal sends it. Under a temporary name, as
    // where the filesystem makes no file without one: those are what an
    // interruption removes.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-compress"])
        .arg(&source)
        .arg(&destination)
        .env("FERRY_TEST_FAIL", "unnamed")
        .process_group(0)
        .spawn()
        .unwrap();
    let temporary_exists = || {
        fs::read_dir(&destination)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().contains(".ferry-"))
            })
            .unwrap_or(false)
    };
    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < Duration::from_secs(20) {
        if temporary_exists() {
            seen = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        seen,
        "the copy finished before it could be interrupted, or never began writing"
    );
    // SAFETY: sending a signal to the child's own process group.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGINT);
    }
    let status = child.wait().unwrap();
    assert!(
        status.code() == Some(130) || status.signal() == Some(libc::SIGINT),
        "not ended by the interrupt: {status}"
    );
    // The client ends at once; its server removes its temporary files as it
    // ends, which a busy machine or a slow disk may take a while to do.
    let stopped = Instant::now();
    while temporary_exists() {
        assert!(
            stopped.elapsed() < Duration::from_secs(20),
            "a temporary file was left behind"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Run again, it completes the copy.
    succeeds(ferry(&[Path::new("--no-compress"), &source, &destination]));
    assert_eq!(
        fs::metadata(destination.join("big")).unwrap().len(),
        512 * 1024 * 1024
    );
    assert!(fs::read(destination.join("big")).unwrap() == fs::read(source.join("big")).unwrap());
}

/// A temporary directory where a file may be made without a name: beside
/// the build, where the system's may make none, as a container's overlay
/// may not. `None` where it makes none either, for the test to say so.
#[cfg(target_os = "linux")]
fn unnamed_tempdir() -> Option<tempfile::TempDir> {
    use std::os::unix::fs::OpenOptionsExt;
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let made = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_TMPFILE)
        .open(temp.path());
    if made.is_err() {
        eprintln!("this filesystem makes no file without a name: not tested");
        return None;
    }
    Some(temp)
}

/// Names under `path` that a temporary file has, at any depth.
fn temporaries(path: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(path) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(".ferry-") {
            found.push(name);
        } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            found.extend(temporaries(&entry.path()));
        }
    }
    found
}

/// The Destination's server, killed with no chance to clean up while it
/// writes a file, leaves no temporary file behind: on Linux, a file is
/// written without a name, and goes with the process.
#[cfg(target_os = "linux")]
#[test]
fn a_destination_killed_while_writing_leaves_no_temporary_files() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let Some(temp) = unnamed_tempdir() else {
        return;
    };
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("small")).unwrap();
    for i in 0..200 {
        fs::write(source.join(format!("small/f{i:03}")), vec![b'x'; 5000]).unwrap();
    }
    // Large, and sent as it is, so that it is still being written.
    let chunk: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut big = fs::File::create(source.join("big")).unwrap();
    for _ in 0..512 {
        big.write_all(&chunk).unwrap();
    }
    drop(big);

    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-ui", "--no-compress"])
        .arg(&source)
        .arg(&destination)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // The server that holds a file without a name open in the destination:
    // the Destination's, writing it.
    let client = child.id();
    let writing = || -> Option<u32> {
        let children = fs::read_to_string(format!("/proc/{client}/task/{client}/children")).ok()?;
        children.split_whitespace().find_map(|pid| {
            let fds = fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
            fds.filter_map(Result::ok)
                .filter_map(|fd| fs::read_link(fd.path()).ok())
                .any(|target| {
                    target.starts_with(&destination)
                        && target.to_string_lossy().ends_with("(deleted)")
                })
                .then(|| pid.parse().ok())?
        })
    };
    let started = Instant::now();
    let server = loop {
        if let Some(server) = writing() {
            break server;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the Destination never began writing a file without a name"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the copy finished before its server could be killed"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    // SAFETY: sending a signal to the child's own server.
    unsafe {
        libc::kill(server as libc::pid_t, libc::SIGKILL);
    }
    let status = child.wait().unwrap();
    assert!(!status.success(), "the copy succeeded without its server");
    assert_eq!(temporaries(&destination), Vec::<String>::new());
    assert!(!destination.join("big").exists());
}

/// Where no file can be made without a name, as on macOS or NFS, files are
/// made under a temporary name instead: copied, and replaced, as well.
#[test]
fn files_under_a_temporary_name_are_copied_and_replaced() {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::write(source.join("top"), "top").unwrap();
    fs::write(source.join("dir/deep"), "deep").unwrap();
    let (status, stderr) = ferry_with("FERRY_TEST_FAIL", "unnamed", &[&source, &destination]);
    assert!(status.success(), "{stderr}");
    fs::write(source.join("top"), "top, changed").unwrap();
    let (status, stderr) = ferry_with("FERRY_TEST_FAIL", "unnamed", &[&source, &destination]);
    assert!(status.success(), "{stderr}");
    assert_eq!(fs::read(destination.join("top")).unwrap(), b"top, changed");
    assert_eq!(fs::read(destination.join("dir/deep")).unwrap(), b"deep");
    assert_eq!(temporaries(&destination), Vec::<String>::new());
}

/// Files without a name, where the filesystem makes them, take their names
/// where nothing had them, and replace what did.
#[cfg(target_os = "linux")]
#[test]
fn files_without_a_name_are_copied_and_replaced() {
    let Some(temp) = unnamed_tempdir() else {
        return;
    };
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir_all(source.join("dir")).unwrap();
    fs::write(source.join("top"), "top").unwrap();
    fs::write(source.join("dir/deep"), "deep").unwrap();
    succeeds(ferry(&[&source, &destination]));
    fs::write(source.join("top"), "top, changed").unwrap();
    succeeds(ferry(&[&source, &destination]));
    assert_eq!(fs::read(destination.join("top")).unwrap(), b"top, changed");
    assert_eq!(fs::read(destination.join("dir/deep")).unwrap(), b"deep");
    assert_eq!(temporaries(&destination), Vec::<String>::new());
}

#[test]
fn copies_without_fsync() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("file"), "content").unwrap();
    succeeds(ferry(&[
        Path::new("--no-fsync"),
        &temp.path().join("file"),
        &temp.path().join("copy"),
    ]));
    assert_eq!(fs::read(temp.path().join("copy")).unwrap(), b"content");
}

#[test]
fn copies_without_compression() {
    let temp = tempfile::tempdir().unwrap();
    let content: Vec<u8> = (0..5_000_000u32).map(|i| (i % 7) as u8).collect();
    fs::write(temp.path().join("file"), &content).unwrap();
    succeeds(ferry(&[
        Path::new("--no-compress"),
        &temp.path().join("file"),
        &temp.path().join("copy"),
    ]));
    assert_eq!(fs::read(temp.path().join("copy")).unwrap(), content);
}

#[test]
fn a_compression_level_out_of_range_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let output = ferry(&[
        Path::new("--compress-level"),
        Path::new("20"),
        temp.path(),
        &temp.path().join("copy"),
    ]);
    assert!(!output.status.success());
    let output = ferry(&[
        Path::new("--compress-level"),
        Path::new("5"),
        Path::new("--no-compress"),
        temp.path(),
        &temp.path().join("copy"),
    ]);
    assert!(!output.status.success());
}

#[test]
fn progress_is_shown_when_asked_and_not_otherwise() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    for i in 0..3 {
        fs::write(source.join(format!("f{i}")), "content").unwrap();
    }
    // Standard error is not a terminal here, so nothing is shown.
    let quiet = ferry(&[&source, &temp.path().join("quiet")]);
    assert!(quiet.status.success());
    assert!(
        quiet.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&quiet.stderr)
    );

    let shown = ferry(&[Path::new("--progress"), &source, &temp.path().join("shown")]);
    assert!(shown.status.success());
    let stderr = String::from_utf8_lossy(&shown.stderr);
    assert!(
        stderr.contains("ferry: 4 entries compared, 3 files sent, 21 B"),
        "{stderr}"
    );
}

/// Every path under `root`, relative and sorted.
fn paths(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut waiting = vec![root.to_path_buf()];
    while let Some(directory) = waiting.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            found.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                waiting.push(path);
            }
        }
    }
    found.sort();
    found
}

fn fails_with(output: &Output, message: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
}

/// A file onto a directory goes into it, once agreed to: without a terminal
/// to ask on, only with --yes. What the directory had stays.
#[test]
fn a_file_onto_a_directory_goes_into_it_once_agreed() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("file.txt"), temp.path().join("directory"));
    fs::write(&source, "file").unwrap();
    fs::create_dir_all(destination.join("sub")).unwrap();
    fs::write(destination.join("sub/precious"), "precious").unwrap();

    fails_with(&ferry(&[&source, &destination]), "pass --yes to agree");
    assert_eq!(paths(&destination), ["sub", "sub/precious"]);

    succeeds(ferry(&[Path::new("--yes"), &source, &destination]));
    assert_eq!(paths(&destination), ["file.txt", "sub", "sub/precious"]);
    assert_eq!(fs::read(destination.join("file.txt")).unwrap(), b"file");
}

/// Where the file would go inside is a directory itself: the Destination
/// refuses it, rather than replace it.
#[test]
fn a_file_into_a_directory_never_replaces_a_directory_there() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("file.txt"), temp.path().join("directory"));
    fs::write(&source, "file").unwrap();
    fs::create_dir_all(destination.join("file.txt")).unwrap();
    fs::write(destination.join("file.txt/precious"), "precious").unwrap();

    fails_with(
        &ferry(&[Path::new("--yes"), &source, &destination]),
        "is a directory, and the source is not",
    );
    assert_eq!(paths(&destination), ["file.txt", "file.txt/precious"]);
}

/// A directory onto a file is refused, and the file kept.
#[test]
fn a_directory_onto_a_file_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("directory"), temp.path().join("file"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("inside"), "inside").unwrap();
    fs::write(&destination, "precious").unwrap();
    fails_with(
        &ferry(&[&source, &destination]),
        "file is not a directory, and",
    );
    assert_eq!(fs::read(&destination).unwrap(), b"precious");
}

/// Each directory gets its own mtime and permissions, whatever is made in it
/// and under it after its own parts came, and however the parts and the
/// files to send are paced: it is finished once nothing more is made in it.
#[test]
fn each_directory_is_finished_with_its_own_mtime_and_mode() {
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime};
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    let directories = [
        "a", "a/b", "a/b/c", "a/b/c/d", "a/e", "f", "f/many", "g-empty",
    ];
    for (index, directory) in directories.iter().enumerate() {
        fs::create_dir_all(source.join(directory)).unwrap();
        if !directory.ends_with("empty") {
            for file in 0..(if directory.ends_with("many") { 25 } else { 3 }) {
                fs::write(
                    source.join(directory).join(format!("f{file:02}")),
                    format!("{index}"),
                )
                .unwrap();
            }
        }
    }
    // Deepest first, so that setting one does not change its parent's.
    let mode = |index: usize| [0o755, 0o750, 0o705, 0o555][index % 4];
    let mtime = |index: usize| {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + 7_777 * index as u64)
    };
    for (index, directory) in directories.iter().enumerate().rev() {
        let path = source.join(directory);
        fs::set_permissions(&path, fs::Permissions::from_mode(mode(index))).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(mtime(index))
            .unwrap();
    }

    let output = ferry(&[
        Path::new("--part-entries=4"),
        Path::new("--backlog-files=2"),
        &source,
        &destination,
    ]);
    // What each directory got, before its permissions are put back so that
    // the trees can be removed.
    let got: Vec<Option<(u32, SystemTime)>> = directories
        .iter()
        .map(|directory| {
            let metadata = fs::metadata(destination.join(directory)).ok()?;
            Some((metadata.mode() & 0o777, metadata.modified().unwrap()))
        })
        .collect();
    for directory in directories {
        for side in [&source, &destination] {
            let _ = fs::set_permissions(side.join(directory), fs::Permissions::from_mode(0o755));
        }
    }
    succeeds(output);
    for (index, directory) in directories.iter().enumerate() {
        assert_eq!(got[index], Some((mode(index), mtime(index))), "{directory}");
    }
}

#[test]
fn copying_into_itself_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("sub/f2"), "two").unwrap();
    let before = paths(&a);
    fails_with(&ferry(&[&a, &a.join("sub")]), "copy the Source into itself");
    fails_with(
        &ferry(&[&a.join("sub"), &a]),
        "write into the Source as it is read",
    );
    assert_eq!(paths(&a), before);
}

/// Runs `ferry copy` with its soft limit on open files lowered to `soft`,
/// and its hard limit too where `hard` is given.
fn ferry_with_open_files(soft: u64, hard: Option<u64>, args: &[&Path]) -> Output {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command.arg("copy").args(args);
    // SAFETY: only `getrlimit` and `setrlimit`, system calls, run between
    // fork and exec.
    unsafe {
        command.pre_exec(move || {
            let maximum = hard.or(getrlimit(Resource::Nofile).maximum);
            setrlimit(
                Resource::Nofile,
                Rlimit {
                    current: Some(soft),
                    maximum,
                },
            )?;
            Ok(())
        });
    }
    command.output().unwrap()
}

/// Many small files are copied where few may be open at once, as on macOS by
/// default: with the soft limit low, Ferry raises it; with the hard limit low
/// too, it renames them in smaller batches, each holding fewer open.
#[test]
fn many_small_files_copy_under_a_low_open_file_limit() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    for index in 0..2000 {
        fs::write(source.join(format!("f{index:04}")), [b'x'; 100]).unwrap();
    }
    for (variant, hard) in [("soft", None), ("hard", Some(256))] {
        let destination = temp.path().join(variant);
        let output = ferry_with_open_files(256, hard, &[&source, &destination]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{variant}: {stderr}");
        let names: Vec<String> = fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2000, "{variant}");
        assert!(
            names.iter().all(|name| !name.contains(".ferry-")),
            "{variant}"
        );
        // Warned only where it could not be raised.
        let warned = stderr.contains("warning: at most 256 files may be open at once");
        assert_eq!(warned, hard.is_some(), "{variant}: {stderr}");
    }
}

/// A deep tree is scanned whole where few files may be open at once: with
/// the soft limit low, Ferry raises it; with the hard limit low too, the
/// scan holds only so many directories open on the way down.
#[test]
fn a_deep_tree_is_scanned_under_a_low_open_file_limit() {
    let temp = tempfile::tempdir().unwrap();
    for (variant, depth, soft, hard) in [("soft", 300, 128, None), ("hard", 400, 256, Some(256))] {
        let source = temp.path().join(variant);
        // Each level has a sibling, still to list while the one before it is
        // gone into.
        let mut level = source.clone();
        for _ in 0..depth {
            fs::create_dir_all(level.join("z")).unwrap();
            level.push("a");
        }
        fs::create_dir(&level).unwrap();
        let destination = temp.path().join(format!("{variant}-copy"));
        let output =
            ferry_with_open_files(soft, hard, &[Path::new("--dry-run"), &source, &destination]);
        assert!(
            output.status.success(),
            "{variant}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Every directory is listed, the deepest too.
        let listed = String::from_utf8_lossy(&output.stdout);
        let deepest = "a/".repeat(depth - 1) + "a/";
        assert!(
            listed.lines().any(|line| line.ends_with(&deepest)),
            "{variant}: the deepest directory is not listed"
        );
    }
}

/// Runs `ferry copy` with `variable` set to `value`, for 30 s at most, and
/// returns its status and standard error.
fn ferry_with(variable: &str, value: &str, args: &[&Path]) -> (std::process::ExitStatus, String) {
    use std::io::Read;
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("copy")
        .arg("--no-ui")
        .args(args)
        .env(variable, value)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            panic!("{variable}={value}: still running after 30 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status, stderr)
}

/// The names in a tree that are temporary files.
fn temporary_files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut waiting = vec![root.to_path_buf()];
    while let Some(directory) = waiting.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().unwrap().is_dir() {
                waiting.push(entry.path());
            } else if name.contains(".ferry-") {
                found.push(name);
            }
        }
    }
    found
}

/// A tree with files enough for every stage to run.
fn many_files(root: &Path) {
    for directory in 0..20 {
        let directory = root.join(format!("d{directory:02}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..50 {
            fs::write(directory.join(format!("f{file:02}")), vec![b'x'; 5000]).unwrap();
        }
    }
}

/// A panic in any stage, on either side, ends the transfer at once with an
/// internal error, said by its role, and leaves no temporary file: it is a
/// bug, not a failure to go on from, nor a hang.
#[test]
fn a_panic_is_an_internal_error_that_ends_the_transfer() {
    for role in [
        "stat",
        "reader",
        "compress",
        "decompress",
        "writer",
        "parser",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        many_files(&source);
        let destination = temp.path().join("destination");
        let (status, stderr) = ferry_with("FERRY_TEST_PANIC", role, &[&source, &destination]);
        assert_eq!(status.code(), Some(101), "{role}: {stderr}");
        assert!(
            stderr.contains(&format!("ferry: internal error in {role}:")),
            "{role}: {stderr}"
        );
        assert_eq!(
            temporary_files(&destination),
            Vec::<String>::new(),
            "{role}"
        );
    }
}

/// A frame that fails to be compressed fails the transfer, said as it
/// failed, not as what followed from it.
#[test]
fn a_frame_that_fails_to_be_compressed_fails_the_transfer() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    many_files(&source);
    let destination = temp.path().join("destination");
    let (status, stderr) = ferry_with("FERRY_TEST_FAIL", "frame-encode", &[&source, &destination]);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("a failure put in for a test, at frame-encode"),
        "{stderr}"
    );
    assert!(!stderr.contains("stopped"), "{stderr}");
    assert_eq!(temporary_files(&destination), Vec::<String>::new());
}

/// A DESTINATION whose directory does not exist is said so, by name, for a
/// file and for a directory, and nothing is copied.
#[test]
fn a_missing_parent_is_named() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("file"), "x").unwrap();
    fs::create_dir(temp.path().join("dir")).unwrap();
    for (source, name) in [("file", "x"), ("dir", "y")] {
        let destination = temp.path().join("missing/deep").join(name);
        let output = ferry(&[&temp.path().join(source), &destination]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("missing/deep, where"), "{stderr}");
        assert!(stderr.contains("does not exist"), "{stderr}");
        assert!(!stderr.contains("cannot copy ."), "{stderr}");
        assert!(!temp.path().join("missing").exists());
    }
}

/// An option for a host, where both sides are on this machine, is warned
/// of, and the copy goes on.
#[test]
fn an_option_for_a_host_is_warned_of_here() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("source")).unwrap();
    let output = ferry(&[
        Path::new("--remote-join"),
        &temp.path().join("source"),
        &temp.path().join("destination"),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: --remote-join does nothing when both sides are on this machine"),
        "{stderr}"
    );
}

/// A name that cannot be read, with a newline in it, is said in one line of
/// its own: it cannot forge a line of Ferry's.
#[test]
fn a_warning_about_an_odd_name_is_one_line() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let odd = source.join("evil\nferry: all good");
    fs::write(&odd, "x").unwrap();
    fs::set_permissions(&odd, fs::Permissions::from_mode(0o000)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-ui"])
        .arg(&source)
        .arg(temp.path().join("destination"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .lines()
            .all(|line| !line.starts_with("ferry: all good")),
        "{stderr}"
    );
    assert!(stderr.contains("evil\\#012ferry: all good"), "{stderr}");
}

/// A transfer that stands still, its readers held up, says where every
/// thread is once nothing has passed between the servers for a while, though
/// each still reports to the client meanwhile; and then goes on.
#[test]
fn a_transfer_standing_still_says_where_every_thread_is() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "content").unwrap();
    // With the progress shown, each server reports how full its queues are
    // every quarter of a second.
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-ui", "--progress"])
        .arg(&source)
        .arg(temp.path().join("destination"))
        .env("FERRY_TEST_STILL", "2")
        .env("FERRY_TEST_STALL", "reader")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("nothing has passed between the servers for"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(temp.path().join("destination/file")).unwrap(),
        b"content"
    );
}

/// Of `role`, in a profile written by `--profile-file`, the state called
/// `name`: whether it is a wait, and how many times it was in it.
fn state_in(profile: &str, role: &str, name: &str) -> Option<(bool, u64)> {
    let start = profile
        .find(&format!("\"role\":\"{role}\""))
        .unwrap_or_else(|| panic!("no {role} in {profile}"));
    let rest = &profile[start + 1..];
    let section = &rest[..rest.find("\"role\":").unwrap_or(rest.len())];
    let state = &section[section.find(&format!("\"state\":\"{name}\""))?..];
    let waits = state.split("\"wait\":").nth(1)?.starts_with("true");
    let count = state.split("\"count\":").nth(1)?;
    let count = count[..count.find(|c: char| !c.is_ascii_digit())?]
        .parse()
        .ok()?;
    Some((waits, count))
}

/// A thread that waits, as the Destination's finisher does for the next
/// directory done, is shown waiting, not at work: where the time went says
/// what held a transfer back only if each wait is said as one. How much of
/// its time each takes is not asked, which a busy machine changes.
#[test]
fn a_thread_that_waits_is_shown_waiting() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    for i in 0..3 {
        fs::write(source.join(format!("f{i}")), "content").unwrap();
    }
    let profile = temp.path().join("profile.json");
    let output = ferry(&[
        Path::new("--no-fsync"),
        Path::new("--profile-file"),
        &profile,
        &source,
        &temp.path().join("destination"),
    ]);
    assert!(output.status.success());
    let profile = fs::read_to_string(&profile).unwrap();
    // Each directory it is handed, and the end, it waited for.
    let waited = state_in(&profile, "finisher", "wait for work");
    assert!(
        matches!(waited, Some((true, count)) if count >= 2),
        "{waited:?} in {profile}"
    );
}

/// Between two sides on this machine, nothing is compressed, unless a level
/// is asked for: the summary says what compression saved only then.
#[test]
fn a_copy_here_compresses_only_when_asked() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("text"), "words that repeat ".repeat(100_000)).unwrap();
    let summary = |extra: &[&Path], to: &str| {
        let mut arguments = vec![Path::new("--progress")];
        arguments.extend_from_slice(extra);
        let destination = temp.path().join(to);
        arguments.extend([source.as_path(), destination.as_path()]);
        let output = ferry(&arguments);
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let plain = summary(&[], "plain");
    assert!(!plain.contains("saved by compression"), "{plain}");
    let asked = summary(&[Path::new("--compress-level"), Path::new("1")], "asked");
    assert!(asked.contains("saved by compression"), "{asked}");
}

/// A root that is a symlink to a file is replaced, as rsync does, not
/// written through, its mode and all: the file it pointed to is left, and a
/// second run finds nothing to do. One to a directory is gone into.
#[test]
fn a_root_that_is_a_symlink_to_a_file_is_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::write(&source, "content").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    // The same content, and mtime, as the source: only the mode differs.
    let target = temp.path().join("target");
    fs::copy(&source, &target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    let mtime = fs::metadata(&source).unwrap().modified().unwrap();
    fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let link = temp.path().join("link");
    symlink("target", &link).unwrap();

    succeeds(ferry(&[&source, &link]));
    let replaced = fs::symlink_metadata(&link).unwrap();
    assert!(replaced.is_file());
    assert_eq!(replaced.permissions().mode() & 0o7777, 0o600);
    assert_eq!(fs::read(&link).unwrap(), b"content");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    let again = ferry(&[Path::new("--dry-run"), &source, &link]);
    assert_eq!(String::from_utf8_lossy(&again.stdout), "");
    succeeds(again);

    fs::create_dir(temp.path().join("dir")).unwrap();
    let into = temp.path().join("link-to-dir");
    symlink("dir", &into).unwrap();
    succeeds(ferry(&[Path::new("--yes"), &source, &into]));
    assert!(
        fs::symlink_metadata(&into)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(temp.path().join("dir/source")).unwrap(),
        b"content"
    );
}

/// What a copy writes is synced: every file, and every directory it made,
/// as strace sees it done; with `--no-fsync`, nothing is. Where there is no
/// strace, it says it is skipped. Linux's only, where every sync is
/// `fsync`.
#[cfg(target_os = "linux")]
#[test]
fn files_and_directories_are_fsynced() {
    if Command::new("strace").arg("-V").output().is_err() {
        eprintln!("skipped: needs strace");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    for (directory, files) in [("a", 7), ("b", 7), ("b/c", 6)] {
        fs::create_dir_all(source.join(directory)).unwrap();
        for file in 0..files {
            fs::write(source.join(directory).join(format!("f{file}")), "x").unwrap();
        }
    }
    let log = temp.path().join("trace");
    let fsyncs = |arguments: &[&str]| {
        let destination = temp.path().join("destination");
        let _ = fs::remove_dir_all(&destination);
        let traced = Command::new("strace")
            .args(["-f", "-qq", "-e", "trace=fsync,fdatasync", "-o"])
            .arg(&log)
            .arg(env!("CARGO_BIN_EXE_ferry"))
            .arg("copy")
            .args(arguments)
            .args([&source, &destination])
            .output()
            .unwrap();
        succeeds(traced);
        fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|line| line.contains("fsync(") || line.contains("fdatasync("))
            .count()
    };
    // 20 files, and the root, a, b and b/c.
    let synced = fsyncs(&[]);
    assert!(synced >= 20 + 4, "{synced} fsyncs");
    assert_eq!(fsyncs(&["--no-fsync"]), 0);
}

/// A copy stopped by Ctrl-C partway, run again, sends only what is missing:
/// each file at its name is whole, and is not sent again. Files take their
/// names a batch of up to 256 at a time, so there are several batches, and
/// they are compressed hard, though nothing shrinks, so that even a fast
/// machine is stopped between two.
#[test]
fn a_copy_run_again_after_ctrl_c_sends_only_what_is_missing() {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::time::{Duration, Instant};
    const FILES: usize = 1_000;
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    // xorshift: bytes no compressor shrinks.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for file in 0..FILES {
        let bytes: Vec<u8> = (0..64 * 1024 / 8)
            .flat_map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()
            })
            .collect();
        fs::write(source.join(format!("f{file:04}")), bytes).unwrap();
    }
    let whole = || {
        fs::read_dir(&destination)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| !entry.file_name().to_string_lossy().contains(".ferry-"))
                    .count()
            })
            .unwrap_or(0)
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-ui", "--compress-level", "19"])
        .arg(&source)
        .arg(&destination)
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while whole() == 0 {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the copy ended before it could be stopped"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "nothing was copied"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: sending a signal to the child's own process group.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGINT);
    }
    let status = child.wait().unwrap();
    assert!(
        status.code() == Some(130) || status.signal() == Some(libc::SIGINT),
        "not ended by the interrupt: {status}"
    );
    // The server ends once it has removed what it did not finish.
    std::thread::sleep(Duration::from_millis(500));
    let kept = whole();
    eprintln!("{kept} of {FILES} files whole when stopped");
    assert!(kept < FILES, "the copy finished before it was stopped");

    let again = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--no-ui", "--progress"])
        .arg(&source)
        .arg(&destination)
        .output()
        .unwrap();
    let summary = String::from_utf8_lossy(&again.stderr).into_owned();
    succeeds(again);
    let sent: usize = summary
        .split(" sent")
        .next()
        .and_then(|before| before.rsplit(", ").next())
        .and_then(|count| count.split(' ').next())
        .and_then(|count| count.replace(',', "").parse().ok())
        .unwrap_or_else(|| panic!("no count sent in {summary}"));
    assert_eq!(sent, FILES - kept, "{kept} kept: {summary}");
    for file in 0..FILES {
        let name = format!("f{file:04}");
        assert!(
            fs::read(source.join(&name)).unwrap() == fs::read(destination.join(&name)).unwrap()
        );
    }
}
