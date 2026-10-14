//! `ferry copy` end to end, to a local destination through a `ferry server`
//! child.

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
fn a_directory_in_the_way_of_a_file_is_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("name"), "file").unwrap();
    fs::create_dir_all(destination.join("name/inside")).unwrap();

    let output = ferry(&[&source, &destination]);
    assert!(!output.status.success());
    assert!(destination.join("name/inside").is_dir());
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
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    let temp = tempfile::tempdir().unwrap();
    let (source, destination) = (temp.path().join("source"), temp.path().join("destination"));
    fs::create_dir(&source).unwrap();
    let big: Vec<u8> = (0..128 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    fs::write(source.join("big"), &big).unwrap();

    // A group of its own, so that Ctrl-C can be sent to the client and its
    // server together, as a terminal sends it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("copy")
        .arg(&source)
        .arg(&destination)
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
    while !temporary_exists() && started.elapsed() < Duration::from_secs(20) {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: sending a signal to the child's own process group.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGINT);
    }
    child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(!temporary_exists(), "a temporary file was left behind");
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
