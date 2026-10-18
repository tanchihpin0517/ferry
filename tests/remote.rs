//! Push, pull, and remote to remote through the remote code path, with
//! `FERRY_SSH` naming a fake `ssh` that runs the command here, so that no host
//! is needed. The fake cannot forward to a socket, as `ssh -W` does, so the
//! paths join the server instead, with `--remote-join`; a path that reaches
//! the socket straight is tested on this machine, in the transport.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fake `ssh`: it skips its options up to `--`, then the host, and runs the
/// command with `sh`, as a login shell would. Asked to forward, with no
/// command, it runs nothing.
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
        .args([
            "--remote-ferry",
            env!("CARGO_BIN_EXE_ferry"),
            "--remote-join",
        ])
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
fn a_host_that_will_not_forward_needs_remote_join() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    let source = temp.path().join("source");
    tree(&source);
    let destination = temp.path().join("pushed");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--remote-ferry", env!("CARGO_BIN_EXE_ferry")])
        .arg(&source)
        .arg(format!("host:{}", destination.display()))
        .env("FERRY_SSH", fake_ssh(temp.path()))
        .env("FERRY_HOME", temp.path().join("home"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--remote-join"), "{stderr}");
    assert!(!destination.exists());
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

#[test]
fn several_ssh_connections_carry_one_transfer() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let destination = format!("host:{}", temp.path().join("destination").display());
    let source = temp.path().join("source");
    let output = ferry(
        temp.path(),
        &[
            "copy",
            "--ssh-connections",
            "3",
            source.to_str().unwrap(),
            &destination,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(temp.path().join("destination/dir/file")).unwrap(),
        b"content"
    );
}

#[test]
fn what_the_hosts_shell_prints_is_not_taken_for_ferrys() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    // An ssh whose shell prints, as a startup file might, before every
    // command it runs: here, what the client reads of a root, or a probe.
    let noisy = temp.path().join("noisy-ssh");
    fs::write(
        &noisy,
        "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\n\
         echo other; echo arch=bogus\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&noisy, fs::Permissions::from_mode(0o755)).unwrap();
    let destination = temp.path().join("destination");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--remote-join", "--remote-ferry"])
        .arg(env!("CARGO_BIN_EXE_ferry"))
        .arg(temp.path().join("source"))
        .arg(format!("host:{}", destination.display()))
        .env("FERRY_SSH", &noisy)
        .env("FERRY_HOME", temp.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(fs::read(destination.join("dir/file")).unwrap(), b"content");
    // What it printed is said, once.
    assert_eq!(stderr.matches("other / arch=bogus").count(), 1, "{stderr}");
}

#[test]
fn a_side_slow_to_join_does_not_starve_the_other_of_heartbeats() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    // An ssh whose paths and heartbeat channels join the server 5 s late, as
    // where the host's shell startup files are slow. The Source, on this
    // machine, has joined its server by then, which watches for heartbeats,
    // here for a silence of under 3 s.
    let slow = temp.path().join("slow-ssh");
    fs::write(
        &slow,
        "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\n\
         case \"$1\" in *--join*) sleep 5;; esac\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).unwrap();
    let destination = temp.path().join("destination");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--remote-join", "--remote-ferry"])
        .arg(env!("CARGO_BIN_EXE_ferry"))
        .arg(temp.path().join("source"))
        .arg(format!("host:{}", destination.display()))
        .env("FERRY_SSH", &slow)
        .env("FERRY_HOME", temp.path().join("home"))
        .env("FERRY_TEST_HEARTBEAT", "100,2000")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(destination.join("dir/file")).unwrap(), b"content");
}

#[test]
fn an_ssh_connection_that_cannot_log_in_fails_before_anything_changes() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    // An ssh that refuses the second SSH connection, whose control path ends
    // in -1, as a host would whose login needs a terminal there is not.
    let refusing = temp.path().join("refusing-ssh");
    fs::write(
        &refusing,
        "#!/bin/sh\ncase \"$*\" in *%C-1*) exit 255;; esac\n\
         while [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&refusing, fs::Permissions::from_mode(0o755)).unwrap();
    let destination = temp.path().join("destination");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--ssh-connections", "2", "--remote-ferry"])
        .arg(env!("CARGO_BIN_EXE_ferry"))
        .arg(temp.path().join("source"))
        .arg(format!("host:{}", destination.display()))
        .env("FERRY_SSH", &refusing)
        .env("FERRY_HOME", temp.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SSH connection 2 of 2"), "{stderr}");
    assert!(!destination.exists());
}

/// A fake `sudo`, in a directory of its own to put first on the `PATH`. With
/// `-n`, it runs only if `FAKE_SUDO_FREE` is set; with `-S`, it reads a line
/// first, which must be `secret`. It logs each command it runs, then runs it,
/// with `SUDO_UID` set as `sudo` sets it, though not as root.
fn fake_sudo(directory: &Path) -> PathBuf {
    let bin = directory.join("sudo-bin");
    fs::create_dir(&bin).unwrap();
    let path = bin.join("sudo");
    fs::write(
        &path,
        r#"#!/bin/sh
asks=; never=
while [ $# -gt 0 ]; do
    case "$1" in
        -n) never=1 ;;
        -S) asks=1 ;;
        -k) ;;
        -p) shift ;;
        --) shift; break ;;
        *) break ;;
    esac
    shift
done
if [ -n "$never" ] && [ -z "$FAKE_SUDO_FREE" ]; then
    echo "sudo: a password is required" >&2
    exit 1
fi
if [ -n "$asks" ]; then
    IFS= read -r password || exit 1
    if [ "$password" != secret ]; then
        echo "sudo: Authentication failed" >&2
        exit 1
    fi
fi
echo "$*" >> "$FAKE_SUDO_LOG"
SUDO_UID=$(id -u) exec "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Pushes `tree` with `--remote-sudo`, the fake `sudo` first on the `PATH`,
/// and `environment` set; returns the output and what `sudo` ran.
fn push_with_sudo(temp: &Path, environment: &[(&str, &str)]) -> (Output, String) {
    fs::create_dir_all(temp.join("home")).unwrap();
    let source = temp.join("source");
    tree(&source);
    let log = temp.join("sudo.log");
    let path = format!(
        "{}:{}",
        fake_sudo(temp).display(),
        std::env::var("PATH").unwrap()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("copy")
        .arg("--remote-sudo")
        .arg("--remote-join")
        .arg(&source)
        .arg(format!("host:{}", temp.join("pushed").display()))
        .args(["--remote-ferry", env!("CARGO_BIN_EXE_ferry")])
        .env("FERRY_SSH", fake_ssh(temp))
        .env("FERRY_HOME", temp.join("home"))
        .env("PATH", path)
        .env("FAKE_SUDO_LOG", &log)
        .env_remove("FERRY_SUDO_PASSWORD")
        .envs(environment.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    (output, fs::read_to_string(log).unwrap_or_default())
}

#[test]
fn remote_sudo_runs_the_server_through_sudo_with_its_password() {
    let temp = tempfile::tempdir().unwrap();
    let (output, ran) = push_with_sudo(temp.path(), &[("FERRY_SUDO_PASSWORD", "secret")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
    // The check and the server through sudo; its paths join as this user.
    assert!(ran.lines().any(|line| line == "true"), "{ran}");
    assert!(
        ran.lines().any(|line| line.ends_with(" server --sudo")),
        "{ran}"
    );
    assert!(!ran.contains("--join"), "{ran}");
}

#[test]
fn remote_sudo_needs_no_password_where_sudo_asks_for_none() {
    let temp = tempfile::tempdir().unwrap();
    let (output, ran) = push_with_sudo(temp.path(), &[("FAKE_SUDO_FREE", "1")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        ran.lines().any(|line| line.ends_with(" server --sudo")),
        "{ran}"
    );
}

#[test]
fn remote_sudo_with_a_wrong_password_runs_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let (output, ran) = push_with_sudo(temp.path(), &[("FERRY_SUDO_PASSWORD", "wrong")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("did not run with that password")
            && stderr.contains("Authentication failed"),
        "{stderr}"
    );
    assert!(ran.is_empty(), "{ran}");
    assert!(!temp.path().join("pushed").exists());
}

/// Runs `ferry` with `args` through the fake `ssh`, as from a host whose home
/// directory is `home`: the fake runs its commands where `ferry` runs, which
/// is `home`, and `HOME` names it, as a login there would. Without a
/// terminal: standard input is not one.
fn ferry_at_home(temp: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(args)
        .args([
            "--remote-ferry",
            env!("CARGO_BIN_EXE_ferry"),
            "--remote-join",
        ])
        .current_dir(home)
        .env("HOME", home)
        .env("FERRY_SSH", fake_ssh(temp))
        .env("FERRY_HOME", temp.join("ferry-home"))
        .output()
        .unwrap()
}

/// A home directory with something precious in it, and a source to sync.
fn home_and_source(temp: &Path) -> (PathBuf, String) {
    let home = temp.join("home-on-host");
    fs::create_dir_all(home.join("important")).unwrap();
    fs::write(home.join("important/thesis.tex"), "precious").unwrap();
    fs::create_dir(temp.join("ferry-home")).unwrap();
    let source = temp.join("source");
    tree(&source);
    (home, source.to_str().unwrap().to_owned())
}

#[test]
fn syncing_into_a_hosts_home_is_refused_without_a_terminal() {
    let temp = tempfile::tempdir().unwrap();
    let (home, source) = home_and_source(temp.path());
    for destination in ["host:", "host:~"] {
        for agreed in [&[][..], &["--yes"][..]] {
            let mut args = vec!["sync", &source, destination];
            args.extend_from_slice(agreed);
            let output = ferry_at_home(temp.path(), &home, &args);
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("host: is the home directory on host")
                    && stderr.contains("--allow-dangerous-destination"),
                "{stderr}"
            );
            assert_eq!(
                fs::read(home.join("important/thesis.tex")).unwrap(),
                b"precious"
            );
        }
    }
}

#[test]
fn a_dry_run_only_warns_of_a_hosts_home() {
    let temp = tempfile::tempdir().unwrap();
    let (home, source) = home_and_source(temp.path());
    let output = ferry_at_home(temp.path(), &home, &["sync", "--dry-run", &source, "host:"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: host: is the home directory on host")
            && stderr.contains("would ask first"),
        "{stderr}"
    );
    assert!(home.join("important/thesis.tex").exists());
}

#[test]
fn allow_dangerous_destination_lets_it_through() {
    let temp = tempfile::tempdir().unwrap();
    let (home, source) = home_and_source(temp.path());
    let output = ferry_at_home(
        temp.path(),
        &home,
        &["sync", "--allow-dangerous-destination", &source, "host:"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.join("important").exists());
    assert_eq!(fs::read(home.join("top")).unwrap(), b"top");
}

#[test]
fn copying_into_a_hosts_home_is_not_asked_about() {
    let temp = tempfile::tempdir().unwrap();
    let (home, source) = home_and_source(temp.path());
    let output = ferry_at_home(temp.path(), &home, &["copy", &source, "host:"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.join("important/thesis.tex").exists());
    assert!(home.join("top").exists());
}

#[test]
fn a_tilde_is_the_home_directory() {
    let temp = tempfile::tempdir().unwrap();
    let (home, source) = home_and_source(temp.path());
    let output = ferry_at_home(temp.path(), &home, &["copy", &source, "host:~/backup"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(home.join("backup/dir/file")).unwrap(), b"content");
    assert!(!home.join("~").exists());

    let output = ferry_at_home(temp.path(), &home, &["copy", &source, "host:~root/backup"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("another user's home"), "{stderr}");
}

#[test]
fn syncing_from_a_host_into_itself_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let (home, _) = home_and_source(temp.path());
    let a = temp.path().join("a");
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("sub/f2"), "two").unwrap();
    let from = format!("host:{}", a.display());
    let into = format!("host:{}", a.join("sub").display());
    let output = ferry_at_home(temp.path(), &home, &["sync", &from, &into]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is inside"), "{stderr}");
    assert_eq!(fs::read(a.join("sub/f2")).unwrap(), b"two");
}
