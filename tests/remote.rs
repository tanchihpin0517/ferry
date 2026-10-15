//! Push, pull, and remote to remote through the remote code path, with
//! `FERRY_SSH` naming a fake `ssh` that runs the command here, so that no host
//! is needed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fake `ssh`: it skips its options up to `--`, then the host, and runs the
/// command with `sh`, as a login shell would.
fn fake_ssh(directory: &Path) -> PathBuf {
    let path = directory.join("fake-ssh");
    fs::write(
        &path,
        "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn ferry(temp: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(args)
        .args(["--remote-ferry", env!("CARGO_BIN_EXE_ferry")])
        .env("FERRY_SSH", fake_ssh(temp))
        .env("FERRY_HOME", temp.join("home"))
        .output()
        .unwrap()
}

fn tree(root: &Path) {
    fs::create_dir_all(root.join("dir")).unwrap();
    fs::write(root.join("dir/file"), "content").unwrap();
    fs::write(root.join("top"), "top").unwrap();
}

#[test]
fn push_and_pull_through_ssh() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    let source = temp.path().join("source");
    tree(&source);
    let source = source.to_str().unwrap();
    let pushed = format!("host:{}", temp.path().join("pushed").display());
    let pulled = temp.path().join("pulled");

    for args in [
        vec!["copy", source, &pushed],
        vec!["copy", &pushed, pulled.to_str().unwrap()],
    ] {
        let output = ferry(temp.path(), &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
    assert_eq!(fs::read(pulled.join("dir/file")).unwrap(), b"content");
    assert_eq!(fs::read(pulled.join("top")).unwrap(), b"top");

    // A dry run's listing is reported by the Destination's server, and printed
    // by the client.
    fs::write(temp.path().join("pushed/top"), "changed").unwrap();
    let output = ferry(
        temp.path(),
        &["copy", "--dry-run", &pushed, pulled.to_str().unwrap()],
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "top\n");
}

#[test]
fn a_pull_reports_what_the_source_could_not_read() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    let source = temp.path().join("source");
    tree(&source);
    fs::set_permissions(source.join("dir"), fs::Permissions::from_mode(0o000)).unwrap();
    let remote = format!("host:{}", source.display());
    let output = ferry(
        temp.path(),
        &[
            "copy",
            &remote,
            temp.path().join("pulled").to_str().unwrap(),
        ],
    );
    fs::set_permissions(source.join("dir"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot read dir:"), "{stderr}");
}

#[test]
fn remote_to_remote_goes_through_the_client() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let source = format!("host:{}", temp.path().join("source").display());
    let destination = format!("other:{}", temp.path().join("destination").display());
    let output = ferry(temp.path(), &["copy", &source, &destination]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(temp.path().join("destination/dir/file")).unwrap(),
        b"content"
    );

    let output = ferry(temp.path(), &["sync", "--dry-run", &source, &destination]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}
