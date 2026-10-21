//! What only root can do, end to end: owners mapped by name, or kept by
//! number, setuid bits kept through the change of owner, devices made, and
//! a Source reached through a bind mount never written into. Each test says
//! it is skipped, and passes, unless run as root, as CI's root job does.
//!
//! Names are looked up with `getent`, on `PATH`: a fake one, put first,
//! answers by ID as one machine and by name as another, so that a copy on
//! one machine maps owners as between two. On macOS, names are looked up
//! through the C library, which no fake reaches, so the tests of names are
//! Linux's alone.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Whether this test can run: as root. Says it is skipped if not.
fn root() -> bool {
    let root = rustix::process::geteuid().is_root();
    if !root {
        eprintln!("skipped: needs root");
    }
    root
}

/// By ID, the Source's names: 4001 is alice, and staff. By name, the
/// Destination's IDs: alice is 4002, staff 4002, and root 5, which must
/// never be taken.
const GETENT: &str = r#"#!/bin/sh
db=$1; shift
for key; do
  case "$db:$key" in
    passwd:4001) echo "alice:x:4001:4001::/nonexistent:/bin/sh" ;;
    passwd:alice) echo "alice:x:4002:4002::/nonexistent:/bin/sh" ;;
    group:4001) echo "staff:x:4001:" ;;
    group:staff) echo "staff:x:4002:" ;;
    passwd:root|passwd:0) echo "root:x:5:5::/root:/bin/sh" ;;
    group:root|group:0) echo "root:x:5:" ;;
  esac
done
exit 0
"#;

/// A tree for a test, and the fake `getent` its runs find first.
struct Trees {
    _temp: tempfile::TempDir,
    bin: PathBuf,
    source: PathBuf,
    destination: PathBuf,
}

fn trees() -> Trees {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("getent"), GETENT).unwrap();
    fs::set_permissions(bin.join("getent"), fs::Permissions::from_mode(0o755)).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let destination = temp.path().join("destination");
    Trees {
        bin,
        source,
        destination,
        _temp: temp,
    }
}

impl Trees {
    /// Runs `ferry <command> <arguments> SOURCE DESTINATION`, with the fake
    /// `getent` first on `PATH`.
    fn ferry(&self, command: &str, arguments: &[&str]) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&path));
        Command::new(env!("CARGO_BIN_EXE_ferry"))
            .arg(command)
            .args(arguments)
            .args([&self.source, &self.destination])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap()
    }

    fn succeeds(&self, command: &str, arguments: &[&str]) -> Output {
        let output = self.ferry(command, arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn owner(path: &Path) -> (u32, u32) {
    let metadata = fs::symlink_metadata(path).unwrap();
    (metadata.uid(), metadata.gid())
}

/// A file of alice's here is the Destination's alice's, by name.
#[cfg(target_os = "linux")]
#[test]
fn owners_are_mapped_by_name() {
    if !root() {
        return;
    }
    let trees = trees();
    let file = trees.source.join("file");
    fs::write(&file, "alice's").unwrap();
    std::os::unix::fs::chown(&file, Some(4001), Some(4001)).unwrap();
    trees.succeeds("copy", &[]);
    assert_eq!(owner(&trees.destination.join("file")), (4002, 4002));
}

/// With `--numeric-ids`, the numbers are kept, whatever the names.
#[test]
fn numeric_ids_keeps_the_numbers() {
    if !root() {
        return;
    }
    let trees = trees();
    let file = trees.source.join("file");
    fs::write(&file, "alice's").unwrap();
    std::os::unix::fs::chown(&file, Some(4001), Some(4001)).unwrap();
    trees.succeeds("copy", &["--numeric-ids"]);
    assert_eq!(owner(&trees.destination.join("file")), (4001, 4001));
}

/// Root's files stay root's, though the Destination names another root.
#[test]
fn root_is_never_mapped() {
    if !root() {
        return;
    }
    let trees = trees();
    fs::write(trees.source.join("file"), "root's").unwrap();
    trees.succeeds("copy", &[]);
    assert_eq!(owner(&trees.destination.join("file")), (0, 0));
}

/// A setuid file whose owner changes keeps its bits: the owner is set
/// before the mode, which a change of owner would clear.
#[test]
fn setuid_survives_the_owner_change() {
    if !root() {
        return;
    }
    let trees = trees();
    let file = trees.source.join("tool");
    fs::write(&file, "#!/bin/sh\n").unwrap();
    std::os::unix::fs::chown(&file, Some(4001), Some(4001)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o6755)).unwrap();
    trees.succeeds("copy", &[]);
    let copied = trees.destination.join("tool");
    assert_eq!(owner(&copied), (4002, 4002));
    assert_eq!(
        fs::metadata(&copied).unwrap().permissions().mode() & 0o7777,
        0o6755
    );
}

/// A character device and a socket are made as they are, with no warning.
#[test]
fn devices_and_sockets_are_made() {
    if !root() {
        return;
    }
    let trees = trees();
    let device = trees.source.join("null");
    let made = rustix::fs::mknodat(
        rustix::fs::CWD,
        &device,
        rustix::fs::FileType::CharacterDevice,
        rustix::fs::Mode::from_raw_mode(0o666),
        rustix::fs::makedev(1, 3),
    );
    made.unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(trees.source.join("socket")).unwrap();
    let output = trees.succeeds("copy", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("warning"), "{stderr}");
    let copied = fs::symlink_metadata(trees.destination.join("null")).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_char_device(
        &copied.file_type()
    ));
    assert_eq!(copied.rdev(), fs::symlink_metadata(&device).unwrap().rdev());
    assert!(std::os::unix::fs::FileTypeExt::is_socket(
        &fs::symlink_metadata(trees.destination.join("socket"))
            .unwrap()
            .file_type()
    ));
}

/// A file whose owner alone differs is listed by a dry run, set right by a
/// copy, and listed no more.
#[cfg(target_os = "linux")]
#[test]
fn an_owner_change_alone_is_listed_and_fixed() {
    if !root() {
        return;
    }
    let trees = trees();
    let file = trees.source.join("file");
    fs::write(&file, "the same").unwrap();
    std::os::unix::fs::chown(&file, Some(4001), Some(4001)).unwrap();
    trees.succeeds("copy", &[]);
    let copied = trees.destination.join("file");
    std::os::unix::fs::chown(&copied, Some(4003), Some(4003)).unwrap();
    let listed = trees.succeeds("copy", &["--dry-run"]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "file\n");
    trees.succeeds("copy", &[]);
    assert_eq!(owner(&copied), (4002, 4002));
    let listed = trees.succeeds("copy", &["--dry-run"]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "");
}

/// Unmounts a bind mount however the test ends.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}

/// The Destination bind-mounted inside the Source, where no check before
/// the transfer can see it: the scan stops where it reaches it, and the
/// Destination grows no copy of itself.
#[cfg(target_os = "linux")]
#[test]
fn a_destination_mounted_inside_the_source_is_never_gone_into() {
    if !root() {
        return;
    }
    let trees = trees();
    let backup = trees.destination.clone();
    fs::create_dir(&backup).unwrap();
    fs::write(trees.source.join("file"), "data").unwrap();
    let inside = trees.source.join("b");
    fs::create_dir(&inside).unwrap();
    let mounted = Command::new("mount")
        .args(["--bind"])
        .args([&backup, &inside])
        .status()
        .unwrap();
    assert!(mounted.success(), "mount --bind");
    let _mounted = Mounted(inside);
    let output = trees.ferry("sync", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("inside the Source"), "{stderr}");
    // What the Destination made of the Source's root, its `b` at most, is
    // there; nothing under it, as of the Source read through the mount.
    let grown: Vec<_> = fs::read_dir(backup.join("b"))
        .map(|names| names.map(|name| name.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(grown.is_empty(), "{grown:?}");
}
