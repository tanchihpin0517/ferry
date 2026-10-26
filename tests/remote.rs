//! Push, pull, and remote to remote through the remote code path, with
//! `FERRY_SSH` naming a fake `ssh` that runs the command here, so that no host
//! is needed. The fake cannot forward to a socket, as `ssh -W` does, so the
//! streams join the server instead, with `--remote-join`; a stream that
//! reaches the socket straight is tested on this machine, in the transport.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fake `ssh`: it skips its options up to `--`, then the host, and runs the
/// command with `sh`, as a login shell would. Asked to forward, with no
/// command, it runs nothing. With `FAKE_SSH_ENV` set, it adds its
/// environment to that file first.
fn fake_ssh(directory: &Path) -> PathBuf {
    let path = directory.join("fake-ssh");
    fs::write(
        &path,
        "#!/bin/sh\nif [ -n \"$FAKE_SSH_ENV\" ]; then env >> \"$FAKE_SSH_ENV\"; fi\n\
         while [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake `ssh` that, as sshd does, gives the command pipes for its input
/// and output, rather than its own: through `cat`, both ways.
fn piping_ssh(directory: &Path) -> PathBuf {
    let path = directory.join("piping-ssh");
    fs::write(
        &path,
        "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\n\
         cat | sh -c \"$1\" | cat\n",
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

/// A master already up, as the fake's always is, is used as it is: no
/// `ssh -N` runs, which would ask it for a session, a login shell on the
/// host, for nothing.
#[test]
fn a_master_already_up_is_used_with_no_ssh_dash_n() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    // An ssh that logs how it is run, then runs the command here; asked
    // whether its master is up, it runs nothing, and says yes.
    let log = temp.path().join("log");
    let logging = temp.path().join("logging-every-ssh");
    fs::write(
        &logging,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" | head -n 1 >> '{}'\n\
             while [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&logging, fs::Permissions::from_mode(0o755)).unwrap();
    let destination = temp.path().join("destination");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--remote-join", "--remote-ferry"])
        .arg(env!("CARGO_BIN_EXE_ferry"))
        .arg(temp.path().join("source"))
        .arg(format!("host:{}", destination.display()))
        .env("FERRY_SSH", &logging)
        .env("FERRY_HOME", temp.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(fs::read(destination.join("dir/file")).unwrap(), b"content");
    let log = fs::read_to_string(&log).unwrap();
    // Each run's options, before the host.
    let options: Vec<&str> = log
        .lines()
        .map(|line| line.split(" -- ").next().unwrap())
        .collect();
    assert!(
        options.iter().any(|options| options.ends_with("-O check")),
        "{log}"
    );
    assert!(
        !options
            .iter()
            .any(|options| options.split(' ').any(|word| word == "-N")),
        "{log}"
    );
}

#[test]
fn a_side_slow_to_join_is_waited_for() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    // An ssh whose streams, and the control connection's channel, join the
    // server 5 s late, as where the host's shell startup files are slow. The
    // Source, on this machine, has its end of the stream by then, on which
    // nothing comes until the other end joins: its session waits, as it
    // takes a stream for broken only once nothing came on it for 20 s.
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
    // An ssh that refuses to log in, and has no master up, as a host would
    // whose login needs a terminal there is not.
    let refusing = temp.path().join("refusing-ssh");
    fs::write(
        &refusing,
        "#!/bin/sh\ncase \"$*\" in *' -N '*|*' check '*) exit 255;; esac\n\
         while [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&refusing, fs::Permissions::from_mode(0o755)).unwrap();
    let destination = temp.path().join("destination");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--remote-ferry"])
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
    assert!(stderr.contains("host: the SSH connection"), "{stderr}");
    assert!(!destination.exists());
}

/// A fake `sudo`, in a directory of its own to put first on the `PATH`. With
/// `-n`, it runs only if `FAKE_SUDO_FREE` is set, or the program's name
/// contains `FAKE_SUDO_FREE_FOR`, as sudoers may ask no password for one
/// program alone; with `-S`, it reads a line first, which must be `secret`,
/// but for such a program, as `sudo` then reads none. It logs each command
/// it runs, then runs it, with `SUDO_UID` set as `sudo` sets it, though not
/// as root.
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
free=$FAKE_SUDO_FREE
case "$1" in *"${FAKE_SUDO_FREE_FOR:-/nothing/}"*) free=1 ;; esac
if [ -n "$never" ] && [ -z "$free" ]; then
    echo "sudo: a password is required" >&2
    exit 1
fi
if [ -n "$asks" ] && [ -z "$free" ]; then
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
    // The check, of Ferry itself, and the server through sudo; its streams
    // join as this user.
    assert!(ran.lines().any(|line| line.ends_with(" build-id")), "{ran}");
    assert!(
        ran.lines()
            .any(|line| line.contains(" server --sudo --lifeline")),
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
        ran.lines()
            .any(|line| line.contains(" server --sudo --lifeline")),
        "{ran}"
    );
}

/// Where sudoers asks no password for Ferry alone, none is asked for, nor
/// given to the server as its first input.
#[test]
fn remote_sudo_needs_no_password_where_ferry_alone_needs_none() {
    let temp = tempfile::tempdir().unwrap();
    let (output, ran) = push_with_sudo(temp.path(), &[("FAKE_SUDO_FREE_FOR", "ferry")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        ran.lines()
            .any(|line| line.contains(" server --sudo --lifeline")),
        "{ran}"
    );
}

/// The password `FERRY_SUDO_PASSWORD` gives is read once, and no child sees
/// it, not `ssh`, nor what `ssh` runs.
#[test]
fn the_sudo_password_reaches_no_child() {
    let temp = tempfile::tempdir().unwrap();
    let seen = temp.path().join("ssh-env");
    let (output, _) = push_with_sudo(
        temp.path(),
        &[
            ("FERRY_SUDO_PASSWORD", "secret"),
            ("FAKE_SSH_ENV", seen.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen = fs::read_to_string(&seen).unwrap();
    assert!(seen.contains("FAKE_SSH_ENV="), "{seen}");
    assert!(!seen.contains("FERRY_SUDO_PASSWORD"), "{seen}");
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

/// This machine's ID, as the probe reads it, or `None` where it would need
/// `ioreg`, or there is none, as in some containers.
fn machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|file| fs::read_to_string(file).ok())
        .map(|id| id.split_whitespace().collect::<String>())
        .filter(|id| !id.is_empty())
}

/// A fake `ssh` that logs each command it runs, on a line with its host;
/// for the host `elsewhere`, Ferry's directory is `elsewhere-home`, where
/// nothing is installed.
fn logging_ssh(temp: &Path) -> PathBuf {
    let path = temp.join("logging-ssh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nhost=$1\nshift\n\
             printf '%s\\t%s\\n' \"$host\" \"$(printf %s \"$1\" | tr '\\n' ' ')\" >> {log}\n\
             if [ \"$host\" = elsewhere ]; then FERRY_HOME={elsewhere}; export FERRY_HOME; fi\n\
             exec sh -c \"$1\"\n",
            log = temp.join("log").display(),
            elsewhere = temp.join("elsewhere-home").display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Installs this build in `home`, as the probe finds an installed build:
/// this test binary's `ferry`, under its build's name.
fn installed_in(home: &Path, machine_id: &str) {
    let build = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("build-id")
        .output()
        .unwrap();
    let build = String::from_utf8(build.stdout).unwrap();
    let dir = home
        .join("hosts")
        .join(machine_id)
        .join("bin")
        .join(build.trim());
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ferry"), dir.join("ferry")).unwrap();
}

/// Runs `ferry copy` from `from` to `to` through the logging ssh, with this
/// build installed, and returns how it went and what the ssh ran, each
/// command with its host.
fn copy_logged(temp: &Path, from: &str, to: &str) -> (Output, Vec<(String, String)>) {
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--remote-join", from, to])
        .env("FERRY_SSH", logging_ssh(temp))
        .env("FERRY_HOME", temp.join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let log = fs::read_to_string(temp.join("log")).unwrap_or_default();
    let ran = log
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(host, command)| (host.to_owned(), script_of(command)))
        .collect();
    (output, ran)
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
        .map(|octal| u8::from_str_radix(octal, 8).unwrap())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn is_probe(command: &str) -> bool {
    command.contains("machine_id=")
}

/// A session that joins Ferry on the host: with `--remote-join`, how the
/// client reaches the monitor there, and the servers it starts.
fn is_join(command: &str) -> bool {
    command.contains("'--join'")
}

#[test]
fn every_host_is_probed_before_any_server_starts() {
    let Some(id) = machine_id() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    installed_in(&temp.path().join("home"), &id);
    tree(&temp.path().join("source"));
    // Two names for this machine: two places, each probed.
    let from = format!("one:{}", temp.path().join("source").display());
    let to = format!("other:{}", temp.path().join("destination").display());
    let (output, ran) = copy_logged(temp.path(), &from, &to);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Each probe opens its host's first session, which becomes the monitor
    // there; nothing joins either before both are probed.
    let probes: Vec<usize> = (0..ran.len()).filter(|&i| is_probe(&ran[i].1)).collect();
    let first_join = (0..ran.len()).find(|&i| is_join(&ran[i].1)).unwrap();
    let probed: Vec<&str> = probes.iter().map(|&i| ran[i].0.as_str()).collect();
    assert_eq!(probed, ["one", "other"], "{ran:#?}");
    assert!(probes.iter().all(|&probe| probe < first_join), "{ran:#?}");
    assert_eq!(
        fs::read(temp.path().join("destination/dir/file")).unwrap(),
        b"content"
    );
}

#[test]
fn a_host_on_both_sides_is_probed_once() {
    let Some(id) = machine_id() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    installed_in(&temp.path().join("home"), &id);
    tree(&temp.path().join("source"));
    let from = format!("host:{}", temp.path().join("source").display());
    let to = format!("host:{}", temp.path().join("destination").display());
    let (output, ran) = copy_logged(temp.path(), &from, &to);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let probes = ran.iter().filter(|(_, command)| is_probe(command)).count();
    assert_eq!(probes, 1, "{ran:#?}");
    assert_eq!(
        fs::read(temp.path().join("destination/dir/file")).unwrap(),
        b"content"
    );
}

/// The Destination's host has no Ferry, and there is no terminal to ask on:
/// the run stops before any server starts, the Source's too.
#[test]
fn an_install_not_agreed_to_starts_no_server() {
    let Some(id) = machine_id() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    installed_in(&temp.path().join("home"), &id);
    tree(&temp.path().join("source"));
    let from = format!("one:{}", temp.path().join("source").display());
    let to = format!("elsewhere:{}", temp.path().join("destination").display());
    let (output, ran) = copy_logged(temp.path(), &from, &to);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Asked about, or, where the machine has no cargo, refused at once.
    assert!(
        stderr.contains("Not without a terminal") || stderr.contains("has no cargo"),
        "{stderr}"
    );
    // No monitor was reached, so no server started.
    assert!(!ran.iter().any(|(_, command)| is_join(command)), "{ran:#?}");
    assert!(!temp.path().join("destination").exists());
}

/// A fake `ssh` in `directory`, named `name`, running `body` as `sh`.
fn script_ssh(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Copies a tree to `host:destination` with `FERRY_SSH` set to `ssh`, in a
/// session of its own, so that there is no terminal to ask in, and with
/// `environment` set.
fn copy_without_terminal(temp: &Path, ssh: &str, environment: &[(&str, &str)]) -> Output {
    use std::os::unix::process::CommandExt;
    fs::create_dir_all(temp.join("home")).unwrap();
    tree(&temp.join("source"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .args(["copy", "--no-ui", "--remote-ferry"])
        .arg(env!("CARGO_BIN_EXE_ferry"))
        .arg("--remote-join")
        .arg(temp.join("source"))
        .arg(format!("host:{}", temp.join("destination").display()))
        .env("FERRY_SSH", ssh)
        .env("FERRY_HOME", temp.join("home"))
        .envs(environment.iter().copied())
        .stdin(std::process::Stdio::null());
    // SAFETY: only `setsid`, a system call, runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.output().unwrap()
}

/// A host whose first session says nothing, not even that it is alive, as
/// when its connection dropped as it opened, is given up on once nothing
/// came for the silence, rather than waited for forever.
#[test]
fn a_silent_first_session_is_given_up_on() {
    let temp = tempfile::tempdir().unwrap();
    // The master is up; a session opens, and says nothing.
    let ssh = script_ssh(
        temp.path(),
        "silent-ssh",
        "case \"$*\" in\n-G*) exit 0;;\n*'-O check'*) exit 0;;\n*' -N '*) exit 0;;\nesac\nexec sleep 1000\n",
    );
    let started = std::time::Instant::now();
    let output = copy_without_terminal(
        temp.path(),
        ssh.to_str().unwrap(),
        &[("FERRY_TEST_SILENCE", "2")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("nothing came from host for 2 s"),
        "{stderr}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

/// Without a terminal, a host that cannot be reached is said to be so, as
/// `ssh` says it, not to need a login.
#[test]
fn an_unreachable_host_says_why() {
    let temp = tempfile::tempdir().unwrap();
    let ssh = script_ssh(
        temp.path(),
        "unreachable-ssh",
        "echo 'ssh: Could not resolve hostname host: Name or service not known' >&2\nexit 255\n",
    );
    let output = copy_without_terminal(temp.path(), ssh.to_str().unwrap(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("Could not resolve hostname"), "{stderr}");
    assert!(!stderr.contains("needs a login"), "{stderr}");
}

/// A `FERRY_SSH` that cannot be run is said to be so, by name.
#[test]
fn a_missing_ssh_program_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("no-such-ssh");
    let output = copy_without_terminal(temp.path(), missing.to_str().unwrap(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains(&format!("cannot run {} (FERRY_SSH)", missing.display())),
        "{stderr}"
    );
}

/// Without a terminal, a host that asks for a password still needs a login,
/// said with why.
#[test]
fn a_login_that_needs_a_password_still_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let ssh = script_ssh(
        temp.path(),
        "password-ssh",
        "echo 'me@host: Permission denied (publickey,password).' >&2\nexit 255\n",
    );
    let output = copy_without_terminal(temp.path(), ssh.to_str().unwrap(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("needs a login"), "{stderr}");
    assert!(stderr.contains("Permission denied"), "{stderr}");
    assert!(!temp.path().join("destination").exists());
}

/// `FERRY_SSH` may carry options of its own, as rsync's `RSYNC_RSH` does:
/// they come first, before `ssh`'s arguments.
#[test]
fn ferry_ssh_may_carry_options() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("ssh.log");
    let ssh = script_ssh(
        temp.path(),
        "logging ssh",
        &format!(
            "printf '%.8s\\n' \"$*\" >> '{}'\n\
             while [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
            log.display()
        ),
    );
    let command = format!("'{}' -p 2222", ssh.display());
    let output = copy_without_terminal(temp.path(), &command, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logged = fs::read_to_string(&log).unwrap();
    assert!(logged.lines().all(|line| line == "-p 2222 "), "{logged}");
    assert_eq!(
        fs::read(temp.path().join("destination/top")).unwrap(),
        b"top"
    );
}

/// An `ssh` that logs in and lasts, but whose master never answers, as one
/// that ignores `ControlPath` would: the login gives up, and says why.
#[test]
fn a_master_that_never_answers_is_given_up_on() {
    let temp = tempfile::tempdir().unwrap();
    let ssh = script_ssh(
        temp.path(),
        "masterless-ssh",
        "case \"$*\" in *' -O check '*) exit 255;; *' -N '*) exec sleep 1000;; esac\n\
         while [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
    );
    let started = std::time::Instant::now();
    let output = copy_without_terminal(
        temp.path(),
        ssh.to_str().unwrap(),
        &[("FERRY_TEST_MASTER_WAIT", "2")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("no ControlMaster answered"), "{stderr}");
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

/// Whether the process `pid` is gone, or no more than a zombie waiting for
/// whoever adopted it to reap it, or dead, as it is for a moment while it is
/// reaped: `Z` then `X`.
#[cfg(target_os = "linux")]
fn is_gone(pid: &str) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with(['Z', 'X'])),
    }
}

/// Ferry, ended by SIGTERM or SIGHUP sent to it alone, as `kill` or a job
/// manager sends them, while it prepares, ends the `ssh` holding its
/// connection too: nothing else would, and it would keep the master up.
#[cfg(target_os = "linux")]
#[test]
fn ferry_ended_by_a_signal_ends_the_ssh_holding_its_connection() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let temp = tempfile::tempdir().unwrap();
        let holder = temp.path().join("holder.pid");
        let probes = temp.path().join("probes.pid");
        // Its `ssh -N` is the master, as one without `ControlPersist` is:
        // the master answers once that has started, and said who it is.
        let ssh = script_ssh(
            temp.path(),
            "holding-ssh",
            &format!(
                "case \"$*\" in\n\
                 *' -N '*) echo $$ > '{holder}'; exec sleep 1000;;\n\
                 *'-O check'*) [ -s '{holder}' ] || exit 255; exit 0;;\n\
                 *' -T '*) echo $$ >> '{probes}'; exec sleep 1000;;\n\
                 esac\nexit 0\n",
                holder = holder.display(),
                probes = probes.display(),
            ),
        );
        fs::create_dir_all(temp.path().join("home")).unwrap();
        tree(&temp.path().join("source"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
        command
            .args(["copy", "--no-ui", "--remote-join"])
            .arg(temp.path().join("source"))
            .arg(format!(
                "host:{}",
                temp.path().join("destination").display()
            ))
            .env("FERRY_SSH", &ssh)
            .env("FERRY_HOME", temp.path().join("home"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: only `setsid`, a system call, runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut ferry = command.spawn().unwrap();
        let started = Instant::now();
        // Logged in, and probing, which hangs.
        let said = |path: &Path| fs::read_to_string(path).is_ok_and(|said| said.ends_with('\n'));
        while !said(&probes) || !said(&holder) {
            assert!(started.elapsed() < Duration::from_secs(20), "never probed");
            std::thread::sleep(Duration::from_millis(20));
        }
        let holding = fs::read_to_string(&holder).unwrap().trim().to_owned();
        // SAFETY: a signal to the child this test started.
        unsafe {
            libc::kill(ferry.id() as libc::pid_t, signal);
        }
        ferry.wait().unwrap();
        let ended = Instant::now();
        while !is_gone(&holding) && ended.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let gone = is_gone(&holding);
        // Whatever came of it, nothing is left running.
        for pid in std::iter::once(holding.clone()).chain(
            fs::read_to_string(&probes)
                .unwrap()
                .lines()
                .map(str::to_owned),
        ) {
            // SAFETY: a process this test's fake `ssh` started.
            unsafe {
                libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL);
            }
        }
        assert!(
            gone,
            "the ssh holding the connection outlived Ferry ({signal})"
        );
    }
}

/// Pushed and pulled over channels that are pipes, as sshd gives them: the
/// stream ends with its end, though a joining process still holds its pipe
/// open, waiting for the server to let it go.
#[test]
fn a_transfer_over_pipes_ends_with_its_end() {
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
        let started = std::time::Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args(&args)
            .args([
                "--remote-ferry",
                env!("CARGO_BIN_EXE_ferry"),
                "--remote-join",
            ])
            .env("FERRY_SSH", piping_ssh(temp.path()))
            .env("FERRY_HOME", temp.path().join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
    }
    assert_eq!(fs::read(pulled.join("dir/file")).unwrap(), b"content");
}

/// A fake `ssh` that runs each command here, as `fake_ssh` does, and gives
/// `-W` a channel to the socket it names, as `ssh` forwards one, through a
/// debug build's `ferry relay`. It logs each run, on a line: `session`, for
/// one that runs a command, which a host's login shell would, `forward`, or
/// `other`, for one that runs nothing; and its process ID. A session runs
/// `before` first, as a host's startup files would.
fn forwarding_ssh(temp: &Path, before: &str) -> PathBuf {
    let path = temp.join("forwarding-ssh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nlog={log}\nw=\nkind=session\n\
             for a in \"$@\"; do case \"$a\" in -O|-G|-N) kind=other ;; --) break ;; esac; done\n\
             while [ \"$1\" != -- ]; do case \"$1\" in -W) w=$2; kind=forward; shift ;; esac; shift; done\n\
             shift 2\necho \"$kind $$\" >> \"$log\"\n\
             if [ -n \"$w\" ]; then exec {ferry} relay \"$w\"; fi\n\
             if [ \"$kind\" = session ]; then {before}\nfi\nexec sh -c \"$1\"\n",
            log = temp.join("log").display(),
            ferry = env!("CARGO_BIN_EXE_ferry"),
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Runs `ferry copy` from `from` to `to` through `ssh`, with `environment`
/// set, and returns how it went, and what the ssh logged: each run's kind
/// and process ID. Given `FAKE_SUDO_LOG`, with `--remote-sudo`.
fn copy_through(
    temp: &Path,
    ssh: &Path,
    from: &str,
    to: &str,
    environment: &[(&str, &str)],
) -> (Output, Vec<(String, u32)>) {
    // With the fake `sudo` logging, through it.
    let sudo = environment.iter().any(|(name, _)| *name == "FAKE_SUDO_LOG");
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args([
            "copy",
            "--remote-ferry",
            env!("CARGO_BIN_EXE_ferry"),
            from,
            to,
        ])
        .args(sudo.then_some("--remote-sudo"))
        .env("FERRY_SSH", ssh)
        .env("FERRY_HOME", temp.join("home"))
        .envs(environment.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let log = fs::read_to_string(temp.join("log")).unwrap_or_default();
    let ran = log
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(kind, id)| (kind.to_owned(), id.parse().unwrap()))
        .collect();
    (output, ran)
}

/// Through a host, over several streams, each a channel SSH forwards: a
/// push and a pull are whole, and so is a copy between two hosts, its
/// streams joined back to back; one of them cut fails the transfer.
#[test]
fn transfers_through_a_host_over_several_streams() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    let source = temp.path().join("source");
    tree(&source);
    for index in 0..300 {
        fs::write(
            source.join(format!("dir/f{index:03}")),
            vec![index as u8; 2000 + index * 50],
        )
        .unwrap();
    }
    fs::write(source.join("large"), vec![7u8; 5 * 2 * 1024 * 1024 + 99]).unwrap();
    let ssh = forwarding_ssh(temp.path(), ":");
    let host = |path: &str| format!("host:{}", temp.path().join(path).display());
    let local = |path: &str| temp.path().join(path).display().to_string();
    for (from, to, copied, cut) in [
        (source.display().to_string(), host("pushed"), "pushed", None),
        (host("source"), local("pulled"), "pulled", None),
        (host("source"), host("between"), "between", None),
        (source.display().to_string(), host("cut"), "cut", Some("1")),
    ] {
        let mut environment = vec![("FERRY_TEST_STREAMS", "8")];
        environment.extend(cut.map(|cut| ("FERRY_TEST_CUT_STREAM", cut)));
        let (output, ran) = copy_through(temp.path(), &ssh, &from, &to, &environment);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if copied == "pushed" {
            // The link to the host's monitor, the server's control
            // connection, and the eight streams.
            let forwards = ran.iter().filter(|(kind, _)| kind == "forward").count();
            assert_eq!(forwards, 10, "{ran:?}");
        }
        if cut.is_some() {
            assert!(!output.status.success(), "{to}: {stderr}");
            assert!(stderr.contains("stream"), "{to}: {stderr}");
            continue;
        }
        assert!(output.status.success(), "{to}: {stderr}");
        let copied = temp.path().join(copied);
        assert!(
            fs::read(copied.join("large")).unwrap() == fs::read(source.join("large")).unwrap(),
            "{to}"
        );
        for index in [0, 151, 299] {
            let name = format!("dir/f{index:03}");
            assert_eq!(
                fs::read(copied.join(&name)).unwrap(),
                fs::read(source.join(&name)).unwrap(),
                "{to} {name}"
            );
        }
    }
}

/// A host runs one session, however many sides are there: the first, which
/// probes it and becomes Ferry's monitor; all else reaches the monitor, or
/// a server, over channels SSH forwards, which run no shell.
#[test]
fn a_host_runs_one_session() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = forwarding_ssh(temp.path(), ":");
    let source = temp.path().join("source");
    for (from, to) in [
        (
            source.display().to_string(),
            format!("host:{}", temp.path().join("pushed").display()),
        ),
        (
            format!("host:{}", source.display()),
            format!("host:{}", temp.path().join("within").display()),
        ),
    ] {
        let _ = fs::remove_file(temp.path().join("log"));
        let (output, ran) = copy_through(temp.path(), &ssh, &from, &to, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let sessions = ran.iter().filter(|(kind, _)| kind == "session").count();
        assert_eq!(sessions, 1, "{from} to {to}: {ran:?}");
        assert!(ran.iter().any(|(kind, _)| kind == "forward"), "{ran:?}");
    }
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
    assert_eq!(
        fs::read(temp.path().join("within/dir/file")).unwrap(),
        b"content"
    );
}

/// What a host's startup files print, and what they leave running prints
/// later, comes amid the first session's lines, and is no line of Ferry's:
/// the transfer goes as it would.
#[test]
fn what_the_hosts_shell_leaves_running_does_not_cut_into_ferrys_lines() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = forwarding_ssh(
        temp.path(),
        "echo starting; (for i in 1 2 3 4 5 6 7 8; do sleep 0.05; echo still here; done) &",
    );
    let (output, _) = copy_through(
        temp.path(),
        &ssh,
        &temp.path().join("source").display().to_string(),
        &format!("host:{}", temp.path().join("pushed").display()),
        &[],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("starting"), "{stderr}");
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
}

/// The monitor ends with the transfer, as the connection it is followed by
/// closes: the first session's process, which became it, is gone.
#[test]
fn the_monitor_ends_with_the_transfer() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = forwarding_ssh(temp.path(), ":");
    let (output, ran) = copy_through(
        temp.path(),
        &ssh,
        &temp.path().join("source").display().to_string(),
        &format!("host:{}", temp.path().join("pushed").display()),
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (_, monitor) = ran.iter().find(|(kind, _)| kind == "session").unwrap();
    // The shell's own `kill`: some systems have no program of that name.
    let alive = Command::new("sh")
        .args(["-c", &format!("kill -0 {monitor}")])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the monitor, {monitor}, is still running");
}

/// A server the monitor started that panics is an internal error, as one
/// on this machine is: the monitor says how it ended.
#[test]
fn a_hosts_server_that_panics_is_an_internal_error() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = forwarding_ssh(temp.path(), ":");
    let (output, _) = copy_through(
        temp.path(),
        &ssh,
        &format!("host:{}", temp.path().join("source").display()),
        &temp.path().join("pulled").display().to_string(),
        &[("FERRY_TEST_PANIC", "scan")],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "{stderr}");
    assert!(stderr.contains("internal error in scan"), "{stderr}");
}

/// Installing on a host happens in its first session too: the source goes
/// up as the last the session reads, is built there, by a fake `cargo` that
/// copies this build, and the session becomes the monitor: one session.
#[test]
fn an_install_happens_in_the_first_session() {
    // The probe refuses a host with no machine ID, as some containers are.
    if machine_id().is_none() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    fs::write(
        fake_bin.join("cargo"),
        format!(
            "#!/bin/sh\nmkdir -p \"$CARGO_TARGET_DIR/debug\"\ncp {} \"$CARGO_TARGET_DIR/debug/ferry\"\n",
            env!("CARGO_BIN_EXE_ferry")
        ),
    )
    .unwrap();
    fs::set_permissions(fake_bin.join("cargo"), fs::Permissions::from_mode(0o755)).unwrap();
    // The install puts `$HOME/.cargo/bin` first, so the real cargo must not
    // be found there.
    let ssh = forwarding_ssh(
        temp.path(),
        &format!(
            "HOME={}; PATH={}:$PATH; export HOME PATH",
            temp.path().display(),
            fake_bin.display()
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["copy", "--yes"])
        .arg(temp.path().join("source"))
        .arg(format!("host:{}", temp.path().join("pushed").display()))
        .env("FERRY_SSH", &ssh)
        .env("FERRY_HOME", temp.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("building it there from source"), "{stderr}");
    let log = fs::read_to_string(temp.path().join("log")).unwrap();
    let sessions = log
        .lines()
        .filter(|line| line.starts_with("session "))
        .count();
    assert_eq!(sessions, 1, "{log}");
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
}

/// With `--remote-sudo`, sudo's checks and the server it runs go through the
/// monitor too: one session.
#[test]
fn remote_sudo_runs_in_the_one_session() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let log = temp.path().join("sudo.log");
    let path = format!(
        "{}:{}",
        fake_sudo(temp.path()).display(),
        std::env::var("PATH").unwrap()
    );
    let ssh = forwarding_ssh(temp.path(), ":");
    let (output, ran) = copy_through(
        temp.path(),
        &ssh,
        &temp.path().join("source").display().to_string(),
        &format!("host:{}", temp.path().join("pushed").display()),
        &[
            ("PATH", &path),
            ("FAKE_SUDO_LOG", log.to_str().unwrap()),
            ("FERRY_SUDO_PASSWORD", "secret"),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let sessions = ran.iter().filter(|(kind, _)| kind == "session").count();
    assert_eq!(sessions, 1, "{ran:?}");
    let sudo = fs::read_to_string(&log).unwrap();
    assert!(
        sudo.lines().any(|line| line.ends_with(" build-id")),
        "{sudo}"
    );
    assert!(
        sudo.lines()
            .any(|line| line.contains(" server --sudo --lifeline")),
        "{sudo}"
    );
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
}

/// A fake `ssh` as `forwarding_ssh`, but one whose `sshd` forwards to no
/// socket, as with `AllowStreamLocalForwarding no`: `-W` to a path ends at
/// once, as one refused; to a port, it is carried. It logs each run, on a
/// line: `session`, `refused` or `forward`, and what it was to.
fn port_only_ssh(temp: &Path) -> PathBuf {
    let path = temp.join("port-only-ssh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nlog={log}\nw=\nkind=session\n\
             for a in \"$@\"; do case \"$a\" in -O|-G|-N) kind=other ;; --) break ;; esac; done\n\
             while [ \"$1\" != -- ]; do case \"$1\" in -W) w=$2; kind=forward; shift ;; esac; shift; done\n\
             shift 2\n\
             case \"$w\" in */*) echo \"refused $w\" >> \"$log\"; exit 255 ;; esac\n\
             echo \"$kind $w\" >> \"$log\"\n\
             if [ -n \"$w\" ]; then exec {ferry} relay \"$w\"; fi\n\
             exec sh -c \"$1\"\n",
            log = temp.join("log").display(),
            ferry = env!("CARGO_BIN_EXE_ferry"),
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Where SSH forwards to no socket, but to a port, the monitor and the
/// servers are reached at their ports on loopback, by themselves: no
/// `--remote-join`, and still one session.
#[test]
fn where_ssh_forwards_to_no_socket_the_port_is_taken() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = port_only_ssh(temp.path());
    let source = temp.path().join("source").display().to_string();
    for (from, to) in [
        (
            source.clone(),
            format!("host:{}", temp.path().join("pushed").display()),
        ),
        (
            format!("host:{source}"),
            temp.path().join("pulled").display().to_string(),
        ),
    ] {
        let _ = fs::remove_file(temp.path().join("log"));
        let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args([
                "copy",
                "--remote-ferry",
                env!("CARGO_BIN_EXE_ferry"),
                &from,
                &to,
            ])
            .env("FERRY_SSH", &ssh)
            .env("FERRY_HOME", temp.path().join("home"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{from} to {to}: {stderr}");
        let log = fs::read_to_string(temp.path().join("log")).unwrap();
        let count = |kind: &str| log.lines().filter(|line| line.starts_with(kind)).count();
        assert_eq!(count("session "), 1, "{log}");
        // The monitor's socket, tried once; then its port, and the server's,
        // for the server, and the stream.
        assert_eq!(count("refused "), 1, "{log}");
        assert!(count("forward 127.0.0.1:") >= 3, "{log}");
    }
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
    assert_eq!(
        fs::read(temp.path().join("pulled/dir/file")).unwrap(),
        b"content"
    );
}

/// A fake `ssh` that gives the command its input and output through pipes
/// of their own, as `sshd` does, and, as `sshd` does, closes its input once
/// the command ends, whoever else still reads it, though it carries its
/// output until no one writes it: through two FIFOs, fed and drained by
/// `cat`. The feeding `cat` reads the input by a descriptor of its own: a
/// shell gives what it runs in the background `/dev/null` for its input.
fn closing_ssh(temp: &Path) -> PathBuf {
    script_ssh(
        temp,
        "closing-ssh",
        "while [ \"$1\" != -- ]; do shift; done\nshift 2\n\
         dir=$(mktemp -d)\nmkfifo \"$dir/in\" \"$dir/out\"\n\
         exec 3<&0\ncat <&3 > \"$dir/in\" &\nfeeder=$!\nexec 3<&-\n\
         cat < \"$dir/out\" &\ndrainer=$!\n\
         sh -c \"$1\" < \"$dir/in\" > \"$dir/out\"\nstatus=$?\n\
         kill $feeder 2>/dev/null\nwait $drainer\nrm -rf \"$dir\"\nexit $status\n",
    )
}

/// Reached by joining processes, the monitor stays as long as it is
/// followed: the joining process that carries that connection waits for it,
/// though `sshd` closes its session's input should it end.
#[test]
fn a_monitor_reached_by_joining_stays_while_followed() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args([
            "copy",
            "--remote-join",
            "--remote-ferry",
            env!("CARGO_BIN_EXE_ferry"),
        ])
        .arg(temp.path().join("source"))
        .arg(format!("host:{}", temp.path().join("pushed").display()))
        .env("FERRY_SSH", closing_ssh(temp.path()))
        .env("FERRY_HOME", temp.path().join("home"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        fs::read(temp.path().join("pushed/dir/file")).unwrap(),
        b"content"
    );
}

/// Runs `ferry monitor` on this machine, nothing allowed to be silent for
/// longer than `silence` seconds: it, what it says on standard output, held
/// open, as the client holds it, and its socket.
fn monitor_here(
    silence: &str,
) -> (
    std::process::Child,
    std::io::BufReader<std::process::ChildStdout>,
    String,
) {
    monitor_with(silence, &[])
}

/// [`monitor_here`], with `environment` set too.
fn monitor_with(
    silence: &str,
    environment: &[(&str, &str)],
) -> (
    std::process::Child,
    std::io::BufReader<std::process::ChildStdout>,
    String,
) {
    let mut monitor = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["monitor", "--tag", "t"])
        .env("FERRY_TEST_SILENCE", silence)
        .envs(environment.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = std::io::BufReader::new(monitor.stdout.take().unwrap());
    let lines = ferry::transport::start::read_part(&mut said, "t", &mut Vec::new())
        .unwrap()
        .unwrap();
    let socket = lines
        .iter()
        .find_map(|line| line.content.strip_prefix(b"socket="))
        .map(|socket| String::from_utf8(socket.to_vec()).unwrap())
        .unwrap();
    (monitor, said, socket)
}

/// The process IDs of `parent`'s children whose command line holds
/// `named`, as `/proc` says, which every Linux has, as not every one has
/// `pgrep`.
#[cfg(target_os = "linux")]
fn children_of(parent: &str, named: &str) -> Vec<String> {
    let Ok(tasks) = fs::read_dir(format!("/proc/{parent}/task")) else {
        return Vec::new();
    };
    tasks
        .filter_map(Result::ok)
        .filter_map(|task| fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|child| {
            fs::read(format!("/proc/{child}/cmdline")).is_ok_and(|cmdline| {
                let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
                cmdline.contains(named)
            })
        })
        .collect()
}

/// Whether the process `pid` takes SIGTERM itself, as Ferry's processes do
/// once they have started: blocked, for the thread that takes signals.
#[cfg(target_os = "linux")]
fn taking_signals(pid: &str) -> bool {
    let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & (1 << (libc::SIGTERM - 1)) != 0)
}

/// Whether the monitor answered `connection` with the start mark.
fn started(connection: &std::os::unix::net::UnixStream) -> bool {
    ferry::transport::start::skip(&mut std::io::BufReader::new(
        connection.try_clone().unwrap(),
    ))
    .unwrap()
    .found
}

/// The monitor stays while the connection it is followed by is kept alive,
/// though nothing else comes on it, and ends once nothing comes for the
/// silence it is allowed, as when its SSH connection dropped without a word,
/// rather than waiting for `sshd` to find that.
#[test]
fn a_monitor_no_longer_heard_from_ends() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use ferry::protocol::{self, Monitor};
    use ferry::transport::STRAIGHT;

    let (mut monitor, _said, socket) = monitor_here("2");
    let mut connection = UnixStream::connect(&socket).unwrap();
    connection.write_all(&[STRAIGHT]).unwrap();
    assert!(started(&connection));
    protocol::send(&mut connection, &Monitor::Follow).unwrap();
    // Kept alive, as the client's heartbeat keeps it.
    let alive = Arc::new(AtomicBool::new(true));
    let keeping = {
        let (alive, mut connection) = (Arc::clone(&alive), connection.try_clone().unwrap());
        std::thread::spawn(move || {
            while alive.load(Ordering::SeqCst) {
                protocol::send(&mut connection, &Monitor::Beat).unwrap();
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        monitor.try_wait().unwrap().is_none(),
        "the monitor ended while kept alive"
    );
    // Nothing more comes, though the connection stays open.
    alive.store(false, Ordering::SeqCst);
    keeping.join().unwrap();
    let silent = Instant::now();
    while monitor.try_wait().unwrap().is_none() {
        if silent.elapsed() > Duration::from_secs(30) {
            let _ = monitor.kill();
            panic!("the monitor stayed though nothing came");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        silent.elapsed() >= Duration::from_secs(1),
        "{:?}",
        silent.elapsed()
    );
    drop(connection);
}

/// A server the monitor started stops once the monitor is gone, however it
/// went, as when it is killed: its lifeline, its standard error, breaks.
/// Here it stops while waiting for its hello, which nothing else would end.
#[cfg(target_os = "linux")]
#[test]
fn a_server_stops_once_its_monitor_is_killed() {
    use std::time::{Duration, Instant};
    let (mut monitor, _said, socket) = monitor_here("30");
    let (following, launched, server) = follow_and_launch(&monitor, &socket);
    monitor.kill().unwrap();
    monitor.wait().unwrap();
    let killed = Instant::now();
    while !is_gone(&server) {
        assert!(
            killed.elapsed() < Duration::from_secs(10),
            "the server, {server}, stayed once its monitor was killed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop((following, launched));
}

/// A connection to the monitor at `socket`, reached straight.
#[cfg(target_os = "linux")]
fn reach(socket: &str) -> std::os::unix::net::UnixStream {
    use std::io::Write;
    let mut connection = std::os::unix::net::UnixStream::connect(socket).unwrap();
    connection.write_all(&[ferry::transport::STRAIGHT]).unwrap();
    assert!(started(&connection));
    connection
}

/// Follows the monitor at `socket`, and has it start a server, as `id`:
/// the link, the server's connection, and the server's process ID, once the
/// monitor has started it.
#[cfg(target_os = "linux")]
fn follow_and_launch(
    monitor: &std::process::Child,
    socket: &str,
) -> (
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
    String,
) {
    use ferry::protocol::{self, Launch, Monitor};
    use std::time::{Duration, Instant};
    let mut following = reach(socket);
    protocol::send(&mut following, &Monitor::Follow).unwrap();
    let mut launched = reach(socket);
    protocol::send(
        &mut launched,
        &Monitor::Launch {
            id: 0,
            what: Launch::Server {
                sudo: None,
                port: false,
            },
        },
    )
    .unwrap();
    let waiting = Instant::now();
    let server = loop {
        // Once it runs the server, and takes its signals: before, a SIGTERM
        // would end it as the system does, not as the server does.
        if let Some(server) = children_of(&monitor.id().to_string(), " server ")
            .into_iter()
            .find(|server| taking_signals(server))
        {
            break server;
        }
        assert!(
            waiting.elapsed() < Duration::from_secs(30),
            "the server never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    (following, launched, server)
}

/// The next message on the link, but for heartbeats.
#[cfg(target_os = "linux")]
fn next_said(following: &mut std::os::unix::net::UnixStream) -> ferry::protocol::Monitor {
    use ferry::protocol::{self, Monitor};
    following
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    loop {
        match protocol::expect::<Monitor>(following).unwrap() {
            Monitor::Beat => {}
            said => return said,
        }
    }
}

/// Told to stop, the monitor sends what it started SIGTERM, says how it
/// ended, and stays, still linked.
#[cfg(target_os = "linux")]
#[test]
fn a_monitor_told_to_stop_ends_what_it_started_and_stays() {
    use ferry::protocol::{self, Monitor};
    let (mut monitor, _said, socket) = monitor_here("30");
    let (mut following, launched, server) = follow_and_launch(&monitor, &socket);
    protocol::send(&mut following, &Monitor::Stop).unwrap();
    let ended = next_said(&mut following);
    assert_eq!(
        ended,
        Monitor::Ended {
            id: 0,
            code: Some(128 + libc::SIGTERM),
            signal: None,
            said: Vec::new(),
        }
    );
    assert!(is_gone(&server));
    assert!(monitor.try_wait().unwrap().is_none(), "the monitor ended");
    let _ = monitor.kill();
    let _ = monitor.wait();
    drop(launched);
}

/// A server the monitor started that is paused, then goes on, is said so on
/// the link: the monitor watches it. Asked, it says how the server looks
/// from outside, though the server can answer nothing: stopped.
#[cfg(target_os = "linux")]
#[test]
fn a_monitor_says_when_what_it_started_is_paused_and_goes_on() {
    use ferry::monitor::look::State;
    use ferry::protocol::{self, Monitor};
    let (mut monitor, _said, socket) = monitor_here("30");
    let (mut following, launched, server) = follow_and_launch(&monitor, &socket);
    let signal = |signal: &str| {
        Command::new("kill")
            .args([signal, &server])
            .status()
            .unwrap()
    };
    signal("-STOP");
    assert_eq!(next_said(&mut following), Monitor::Paused { id: 0 });
    protocol::send(&mut following, &Monitor::Look).unwrap();
    let Monitor::Looked { id: 0, outside } = next_said(&mut following) else {
        panic!("not how the server looks");
    };
    assert!(outside.alive && outside.stopped, "{outside:?}");
    assert!(
        !outside.threads.is_empty()
            && outside
                .threads
                .iter()
                .all(|thread| thread.state == State::Stopped),
        "{outside:?}"
    );
    signal("-CONT");
    assert_eq!(next_said(&mut following), Monitor::Resumed { id: 0 });
    let _ = monitor.kill();
    let _ = monitor.wait();
    drop(launched);
}

/// Pinged on the link, the monitor answers at once, with the same ID: the
/// client times the round trip so, to choose how many streams to make.
#[cfg(target_os = "linux")]
#[test]
fn a_monitor_answers_a_ping_at_once() {
    use ferry::protocol::{self, Monitor};
    let (mut monitor, _said, socket) = monitor_here("30");
    let (mut following, launched, _server) = follow_and_launch(&monitor, &socket);
    for id in [7, 8] {
        protocol::send(&mut following, &Monitor::Ping { id }).unwrap();
        loop {
            match next_said(&mut following) {
                Monitor::Pong { id: answered } => {
                    assert_eq!(answered, id);
                    break;
                }
                // What else it says of its server meanwhile.
                Monitor::Paused { .. } | Monitor::Resumed { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }
    let _ = monitor.kill();
    let _ = monitor.wait();
    drop(launched);
}

/// Once its link is lost, here as it ends, the monitor sends what it
/// started SIGTERM, and ends once it has; one that does not end within the
/// grace, here as its signals wait 5 s, is killed then, and the monitor
/// ends anyway.
#[cfg(target_os = "linux")]
#[test]
fn a_monitor_that_lost_its_link_ends_what_it_started_within_the_grace() {
    use std::time::{Duration, Instant};
    for (stall, at_least, at_most) in [("", 0, 3), ("signals", 2, 5)] {
        let (mut monitor, _said, socket) = monitor_with("2", &[("FERRY_TEST_STALL", stall)]);
        let (following, launched, server) = follow_and_launch(&monitor, &socket);
        let lost = Instant::now();
        drop(following);
        while monitor.try_wait().unwrap().is_none() {
            assert!(
                lost.elapsed() < Duration::from_secs(30),
                "the monitor stayed once its link was lost"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let took = lost.elapsed();
        assert!(
            took >= Duration::from_secs(at_least) && took < Duration::from_secs(at_most),
            "{stall:?}: {took:?}"
        );
        let killed = Instant::now();
        while !is_gone(&server) {
            assert!(
                killed.elapsed() < Duration::from_secs(10),
                "the server, {server}, stayed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(launched);
    }
}

/// A copy to a host through a forwarding ssh, its writers waiting as they
/// start, once its server there has started: the copy, the host's monitor,
/// and its server.
#[cfg(target_os = "linux")]
#[allow(
    clippy::zombie_processes,
    reason = "the caller waits for the copy it is given"
)]
fn a_copy_to_a_host_under_way(temp: &Path) -> (std::process::Child, String, String) {
    use std::time::{Duration, Instant};
    fs::create_dir(temp.join("home")).unwrap();
    tree(&temp.join("source"));
    let ssh = forwarding_ssh(temp, ":");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args([
            "copy",
            "--no-ui",
            "--remote-ferry",
            env!("CARGO_BIN_EXE_ferry"),
        ])
        .arg(temp.join("source"))
        .arg(format!("host:{}", temp.join("pushed").display()))
        .env("FERRY_SSH", &ssh)
        .env("FERRY_HOME", temp.join("home"))
        .env("FERRY_TEST_STALL", "writer")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        let log = fs::read_to_string(temp.join("log")).unwrap_or_default();
        // The session's shell runs the monitor, which runs the server.
        let child_of = |parent: &str, named: &str| children_of(parent, named).into_iter().next();
        let session = log.lines().find_map(|line| line.strip_prefix("session "));
        if let Some(session) = session
            && let Some(monitor) = child_of(session, "ferry monitor")
            && let Some(server) = child_of(&monitor, "ferry server --lifeline")
            && taking_signals(&server)
        {
            return (child, monitor, server);
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the server on the host never started: {log}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until the processes `pids` are gone.
#[cfg(target_os = "linux")]
fn all_gone(pids: &[&str], what: &str) {
    use std::time::{Duration, Instant};
    let started = Instant::now();
    while !pids.iter().all(|pid| is_gone(pid)) {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{what}: one of {pids:?} stayed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A Ctrl-C tells the host's monitor to stop, which stops its server, and
/// says how it ended; the copy then ends, as interrupted, the host's monitor
/// with it, nothing of it left on the host.
#[cfg(target_os = "linux")]
#[test]
fn a_ctrl_c_stops_what_runs_on_a_host() {
    let temp = tempfile::tempdir().unwrap();
    let (child, monitor, server) = a_copy_to_a_host_under_way(temp.path());
    // SAFETY: a signal to the copy.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    all_gone(&[&monitor, &server], "after a Ctrl-C");
}

/// A server on a host that is stopped, as by SIGSTOP, is said to be, as the
/// host's monitor tells, and how it looks from outside, as that monitor sees
/// it: stopped. Once it goes on, so does the copy, which completes.
#[cfg(target_os = "linux")]
#[test]
fn a_server_stopped_on_a_host_is_said_so_and_seen_from_outside() {
    let temp = tempfile::tempdir().unwrap();
    let (child, _monitor, server) = a_copy_to_a_host_under_way(temp.path());
    let signal = |signal: &str| {
        Command::new("kill")
            .args([signal, &server])
            .status()
            .unwrap()
    };
    signal("-STOP");
    std::thread::sleep(std::time::Duration::from_secs(2));
    signal("-CONT");
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("the Destination on host was stopped"),
        "{stderr}"
    );
    assert!(
        stderr.lines().any(
            |line| line.contains("the Destination on host, from outside: ")
                && line.ends_with(" stopped")
        ),
        "{stderr}"
    );
}

/// The client killed, as by `kill -9`, the host's monitor finds its link
/// gone, ends its server, and ends.
#[cfg(target_os = "linux")]
#[test]
fn a_host_cleans_up_after_a_client_killed() {
    let temp = tempfile::tempdir().unwrap();
    let (mut child, monitor, server) = a_copy_to_a_host_under_way(temp.path());
    child.kill().unwrap();
    child.wait().unwrap();
    all_gone(&[&monitor, &server], "after the client was killed");
}

/// A fake `ssh` that forwards channels, as [`forwarding_ssh`] does, but
/// stalls the `stopped`th, counted from 1, once the third opens: neither side
/// hears from the other on it any more, and it does not close, as when an
/// SSH connection drops without a word. Copying to a host, the first is the
/// one the monitor is followed by, and the second the server's control
/// connection; the third is the stream. Its relay stays, rather than being
/// stopped by a signal: a stopped process in a process group left orphaned
/// would have the system hang up on the whole group, the test among it.
/// With `way` "down", it stalls only what comes from the host. `stopped`
/// may name several, as "1 2 3", which then stall together, as every channel
/// of one SSH connection does once it drops.
fn stalling_ssh(temp: &Path, stopped: &str, way: &str) -> PathBuf {
    let path = temp.join("stalling-ssh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nlog={log}\nw=\nkind=session\n\
             for a in \"$@\"; do case \"$a\" in -O|-G|-N) kind=other ;; --) break ;; esac; done\n\
             while [ \"$1\" != -- ]; do case \"$1\" in -W) w=$2; kind=forward; shift ;; esac; shift; done\n\
             shift 2\necho \"$kind $$\" >> \"$log\"\n\
             if [ -n \"$w\" ]; then\n\
             n=$(grep -c '^forward' \"$log\")\n\
             if [ \"$n\" = 3 ]; then for s in {stopped}; do : > {stall}-$s; done; fi\n\
             FERRY_TEST_RELAY_STALL={stall}-$n; export FERRY_TEST_RELAY_STALL\n\
             FERRY_TEST_RELAY_STALL_WAY={way}; export FERRY_TEST_RELAY_STALL_WAY\n\
             exec {ferry} relay \"$w\"\nfi\nexec sh -c \"$1\"\n",
            log = temp.join("log").display(),
            stall = temp.join("stall").display(),
            ferry = env!("CARGO_BIN_EXE_ferry"),
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A fake `ssh` that forwards channels, as [`stalling_ssh`] does, but
/// carries the third, the stream, at `pace` bytes a second each way, and,
/// once it opens, holds up those `stalled` names, `way` as there, while
/// their files are there, until the test removes them: as a slow connection
/// on which the heartbeat waits behind the data.
#[cfg(target_os = "linux")]
fn slow_ssh(temp: &Path, stalled: &str, way: &str, pace: u64) -> PathBuf {
    let path = temp.join("slow-ssh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nlog={log}\nw=\nkind=session\n\
             for a in \"$@\"; do case \"$a\" in -O|-G|-N) kind=other ;; --) break ;; esac; done\n\
             while [ \"$1\" != -- ]; do case \"$1\" in -W) w=$2; kind=forward; shift ;; esac; shift; done\n\
             shift 2\necho \"$kind $$\" >> \"$log\"\n\
             if [ -n \"$w\" ]; then\n\
             n=$(grep -c '^forward' \"$log\")\n\
             if [ \"$n\" = 3 ]; then for s in {stalled}; do : > {stall}-$s; done\n\
             FERRY_TEST_RELAY_PACE={pace}; export FERRY_TEST_RELAY_PACE; fi\n\
             FERRY_TEST_RELAY_STALL={stall}-$n; export FERRY_TEST_RELAY_STALL\n\
             FERRY_TEST_RELAY_STALL_WAY={way}; export FERRY_TEST_RELAY_STALL_WAY\n\
             exec {ferry} relay \"$w\"\nfi\nexec sh -c \"$1\"\n",
            log = temp.join("log").display(),
            stall = temp.join("stall").display(),
            ferry = env!("CARGO_BIN_EXE_ferry"),
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A host whose heartbeat waits behind the data, as on a slow connection,
/// here as the link to its monitor stalls the way the data goes for longer
/// than the silence, is not taken for lost while the data still comes: what
/// a server's stream carried from the far end counts as word from there.
/// Pushing, the host's monitor hears it from its server, on its lifeline;
/// pulling, the client from the server on this machine, as the control
/// connection of the host's server stalls too. Each copy completes.
#[cfg(target_os = "linux")]
#[test]
fn a_heartbeat_behind_the_data_takes_nothing_for_lost() {
    use std::time::{Duration, Instant};
    for (push, stalled, way) in [(true, "1", "up"), (false, "1 2", "down")] {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("home")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        // As good as random, so that compressing it saves nothing: 8 s at
        // the pace.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let data: Vec<u8> = (0..2 << 20)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        fs::write(source.join("file"), &data).unwrap();
        let (from, to, copied) = if push {
            let remote = temp.path().join("remote");
            (
                source.display().to_string(),
                format!("host:{}", remote.display()),
                remote,
            )
        } else {
            let local = temp.path().join("local");
            (
                format!("host:{}", source.display()),
                local.display().to_string(),
                local,
            )
        };
        let child = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args([
                "copy",
                "--no-ui",
                "--remote-ferry",
                env!("CARGO_BIN_EXE_ferry"),
                &from,
                &to,
            ])
            .env("FERRY_SSH", slow_ssh(temp.path(), stalled, way, 256 << 10))
            .env("FERRY_HOME", temp.path().join("home"))
            .env("FERRY_TEST_SILENCE", "3")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The link stalls as the stream opens, for longer than the silence.
        let started = Instant::now();
        while !temp.path().join("stall-1").exists() {
            assert!(started.elapsed() < Duration::from_secs(60), "no stream");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_secs(5));
        for stall in stalled.split(' ') {
            fs::remove_file(temp.path().join(format!("stall-{stall}"))).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "push {push}: {stderr}");
        assert!(
            fs::read(copied.join("file")).unwrap() == data,
            "push {push}"
        );
    }
}

/// Once nothing comes on the connection the monitor is followed by, the
/// client takes the monitor for gone, and says so, rather than waiting for
/// it to say how its server ended; and the monitor has ended. Only what
/// comes from the host stalls, so that the monitor, still hearing the
/// client, does not stop its server first, which the client would hear of.
#[test]
fn a_monitor_no_longer_heard_from_is_said_to_be_gone() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = stalling_ssh(temp.path(), "1", "down");
    let started = std::time::Instant::now();
    // The writers wait 5 s as they start, for the transfer to last past the
    // silence allowed.
    let (output, ran) = copy_through(
        temp.path(),
        &ssh,
        &temp.path().join("source").display().to_string(),
        &format!("host:{}", temp.path().join("pushed").display()),
        &[("FERRY_TEST_SILENCE", "3"), ("FERRY_TEST_STALL", "writer")],
    );
    let elapsed = started.elapsed();
    let alive = |id: u32| {
        Command::new("sh")
            .args(["-c", &format!("kill -0 {id}")])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    let (_, monitor) = ran.iter().find(|(kind, _)| kind == "session").unwrap();
    let (_, stopped) = ran.iter().find(|(kind, _)| kind == "forward").unwrap();
    let monitor_alive = alive(*monitor);
    let _ = Command::new("sh")
        .args(["-c", &format!("kill -KILL {stopped}")])
        .status();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("nothing came from it for 3 s"), "{stderr}");
    assert!(!monitor_alive, "the monitor, {monitor}, is still running");
    assert!(elapsed < std::time::Duration::from_secs(40), "{elapsed:?}");
}

/// Once the monitor is followed, a connection to it on which nothing comes,
/// neither its first byte nor, after it, its first packet, is let go once
/// it has been silent as long as a stream may be: it holds up no other, as
/// the monitor takes them in turn, and the monitor goes on, the connection
/// it is followed by kept alive.
#[test]
fn a_connection_that_says_nothing_holds_up_no_other() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use ferry::protocol::{self, Monitor};
    use ferry::transport::STRAIGHT;

    let (mut monitor, _said, socket) = monitor_here("2");
    let mut following = UnixStream::connect(&socket).unwrap();
    following.write_all(&[STRAIGHT]).unwrap();
    assert!(started(&following));
    protocol::send(&mut following, &Monitor::Follow).unwrap();
    let alive = Arc::new(AtomicBool::new(true));
    let keeping = {
        let (alive, mut following) = (Arc::clone(&alive), following.try_clone().unwrap());
        std::thread::spawn(move || {
            while alive.load(Ordering::SeqCst) {
                protocol::send(&mut following, &Monitor::Beat).unwrap();
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };
    // Nothing at all, then its first byte alone.
    let silent = UnixStream::connect(&socket).unwrap();
    let mut first_only = UnixStream::connect(&socket).unwrap();
    first_only.write_all(&[STRAIGHT]).unwrap();
    // One after them is taken once both are let go.
    let began = Instant::now();
    let after = UnixStream::connect(&socket).unwrap();
    after
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    (&after).write_all(&[STRAIGHT]).unwrap();
    assert!(started(&after), "the monitor never took it");
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "{:?}",
        began.elapsed()
    );
    assert!(monitor.try_wait().unwrap().is_none(), "the monitor ended");
    alive.store(false, Ordering::SeqCst);
    keeping.join().unwrap();
    let _ = monitor.kill();
    let _ = monitor.wait();
    drop((silent, first_only, after));
}

/// Until the monitor is followed, nothing says whether the client is there:
/// a first connection on which nothing comes, or only its first byte, ends
/// the monitor, as the client's start failed, rather than leaving it
/// waiting for one that will not come.
#[test]
fn a_monitor_whose_first_connection_says_nothing_ends() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use ferry::transport::STRAIGHT;

    for first_byte in [false, true] {
        let (mut monitor, _said, socket) = monitor_here("2");
        let mut first = UnixStream::connect(&socket).unwrap();
        if first_byte {
            first.write_all(&[STRAIGHT]).unwrap();
        }
        let began = Instant::now();
        while monitor.try_wait().unwrap().is_none() {
            if began.elapsed() > Duration::from_secs(30) {
                let _ = monitor.kill();
                panic!("the monitor stayed, its first connection silent ({first_byte})");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(first);
    }
}

/// Once every channel of a host's SSH connection stalls, as when it drops
/// without a word, though none closes, the client takes the host for lost
/// once its heartbeat is silent, says so, and ends, rather than waiting.
#[test]
fn a_host_whose_connection_stalls_is_taken_for_lost() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("home")).unwrap();
    tree(&temp.path().join("source"));
    let ssh = stalling_ssh(temp.path(), "1 2 3", "both");
    let started = std::time::Instant::now();
    // The writers wait 5 s as they start, for the transfer to last past the
    // silence allowed.
    let (output, ran) = copy_through(
        temp.path(),
        &ssh,
        &temp.path().join("source").display().to_string(),
        &format!("host:{}", temp.path().join("pushed").display()),
        &[("FERRY_TEST_SILENCE", "3"), ("FERRY_TEST_STALL", "writer")],
    );
    let elapsed = started.elapsed();
    for (_, stopped) in ran.iter().filter(|(kind, _)| kind == "forward") {
        let _ = Command::new("sh")
            .args(["-c", &format!("kill -KILL {stopped} 2>/dev/null")])
            .status();
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("nothing came from it for 3 s"), "{stderr}");
    assert!(elapsed < std::time::Duration::from_secs(40), "{elapsed:?}");
}
