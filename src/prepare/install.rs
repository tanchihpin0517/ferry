//! The build of Ferry to run at a place: on this machine, the client's own
//! binary; on a host, the build installed there, installed first if it is
//! missing.
//!
//! A release build uploads its binary over SSH. A dev build uploads its source
//! as a tar stream, and the host builds it with its own Rust toolchain into a
//! target directory it keeps, so that a rebuild is incremental. Either way the
//! binary is put in place by a rename, only once the host has checked it: an
//! upload must be all there, as its size and checksum say, and either binary
//! must run there and say it is the build it is installed as. So an install
//! cut short, or that fails, leaves no broken binary behind, and installs
//! that run at once each write a file of their own.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use super::ask::confirm;
use super::probe;
use crate::build_info::{self, PROFILE, Profile};
use crate::home;
use crate::progress::size;
use crate::remote::release;
use crate::remote::shell::quote;
use crate::status;
use crate::transport::Transport;

pub struct RemoteOptions<'a> {
    /// A Ferry binary on the host to run instead of the one Ferry installs.
    pub remote_ferry: Option<&'a str>,
    /// Install without asking.
    pub yes: bool,
    /// Have paths join the server through the shell on the host, rather than
    /// reach its socket through SSH.
    pub remote_join: bool,
}

/// The Ferry program to run through `transport`: this binary on this machine,
/// and on a host, the one `--remote-ferry` names, or else this build,
/// installed first if it is missing.
pub fn program(transport: &Transport, options: &RemoteOptions) -> Result<String> {
    match transport {
        Transport::Local => {
            let program = std::env::current_exe().context("find the ferry binary")?;
            program
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("the ferry binary's path is not UTF-8"))
        }
        Transport::Ssh { .. } => match options.remote_ferry {
            Some(program) => Ok(program.to_owned()),
            None => ensure_installed(transport, options.yes),
        },
    }
}

/// The path of this build on the host, installing it if it is not there.
fn ensure_installed(transport: &Transport, yes: bool) -> Result<String> {
    let build = build_info::identity();
    let probe = {
        let _shown = status::show(format!("checking Ferry on {}", transport.place()));
        probe::run(transport, home::dir_name(), &build)?
    };
    let program = format!("{}/bin/{build}/ferry", probe.host_dir);
    if probe.installed {
        return Ok(program);
    }
    let host = transport.place();
    if PROFILE == Profile::Release {
        let target = release::target(&probe.os, &probe.arch)
            .with_context(|| format!("install Ferry on {host}"))?;
        // Asking once per host is enough, as for a dev build below.
        if !probe.any_build && !yes {
            confirm(
                &format!("Ferry is not installed on {host}. Install {build} there?"),
                "not installed",
            )?;
        }
        let binary = release::binary(target)?;
        status::say(format!("installing {build} on {host} ({target})"));
        release_build(transport, &probe.host_dir, &build, &binary)?;
        return Ok(program);
    }
    if !probe.cargo {
        bail!(
            "{host} has no cargo; installing a dev build builds it there from source, which needs a Rust toolchain"
        );
    }
    // Asking once per host is enough: a host that has any build agreed to Ferry
    // before, and a newer build is installed beside the old ones.
    if !probe.any_build && !yes {
        confirm(
            &format!(
                "Ferry is not installed on {host}. Build and install {build} there from source?"
            ),
            "not installed",
        )?;
    }
    status::say(format!(
        "installing {build} on {host} ({} {}), building it there from source, which takes a few minutes",
        probe.os, probe.arch
    ));
    dev_build(transport, &probe.host_dir, &build)?;
    Ok(program)
}

/// How many builds a host keeps: the most recently used.
const KEEP: usize = 3;

/// Shell that marks `$dir/bin/$build` used, and removes all but the [`KEEP`]
/// most recently used builds, but for any used in the last hour: another
/// client may have just probed it, and be about to start it. A build's
/// directory's modification time is when it was last used. Where `find` has
/// no `-mmin`, nothing is removed.
pub fn keep_recent() -> String {
    format!(
        r#"touch "$dir/bin/$build"
ls -t "$dir/bin" | tail -n +{first_old} | while IFS= read -r old; do
    if [ "$old" != "$build" ] && [ -n "$(find "$dir/bin/$old" -prune -mmin +60 2>/dev/null)" ]; then
        rm -rf "${{dir:?}}/bin/$old"
    fi
done
"#,
        first_old = KEEP + 1,
    )
}

/// Shell that removes `$tmp` however the script ends, as when its upload is
/// cut short and the client gone.
const REMOVED_AT_EXIT: &str = r#"trap 'rm -f "$tmp"' EXIT
trap 'exit 1' HUP INT TERM PIPE
"#;

/// Shell that installs `$tmp` as `$dir/bin/$build/ferry` once it runs here
/// and says it is `$build`, or else says what it is, after `which`, and
/// `why` if it is another build; a build installed at once by another client
/// is replaced whole.
fn put_in_place(which: &str, why: &str) -> String {
    format!(
        r#"chmod 755 "$tmp"
ran=$("$tmp" build-id </dev/null 2>/dev/null) || ran=
if [ -z "$ran" ]; then
    echo "ferry: {which} does not run here" >&2
    exit 1
elif [ "$ran" != "$build" ]; then
    echo "ferry: {which} is build $ran, not $build{why}" >&2
    exit 1
fi
mv -f "$tmp" "$dir/bin/$build/ferry"
"#
    )
}

/// The script that installs the binary on its input as
/// `<host_dir>/bin/<build>/ferry`, once it has all `size` bytes of it, whose
/// SHA-256 is `sha256`. Without `sha256sum` or `shasum` on the host, only the
/// size is checked, which finds an upload cut short.
fn release_script(host_dir: &str, build: &str, size: u64, sha256: &str) -> String {
    format!(
        r#"set -e
dir={dir}
build={build}
mkdir -p "$dir/bin/$build"
tmp="$dir/bin/$build/ferry.new.$$"
{removed_at_exit}cat > "$tmp"
got=$(wc -c < "$tmp" | tr -d ' ')
if [ "$got" != {size} ]; then
    echo "ferry: the upload ended after $got of {size} bytes" >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    sum=$(sha256sum "$tmp" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
    sum=$(shasum -a 256 "$tmp" | cut -d ' ' -f 1)
else
    sum={sha256}
fi
if [ "$sum" != {sha256} ]; then
    echo "ferry: the upload does not match its checksum" >&2
    exit 1
fi
{put_in_place}{keep_recent}"#,
        dir = quote(host_dir),
        build = quote(build),
        sha256 = quote(sha256),
        removed_at_exit = REMOVED_AT_EXIT,
        put_in_place = put_in_place("the uploaded binary", ""),
        keep_recent = keep_recent(),
    )
}

/// Uploads the release binary at `binary` and installs it as `build`.
fn release_build(transport: &Transport, host_dir: &str, build: &str, binary: &Path) -> Result<()> {
    let mut file = File::open(binary).with_context(|| format!("open {}", binary.display()))?;
    let total = file
        .metadata()
        .with_context(|| format!("inspect {}", binary.display()))?
        .len();
    let sha256 = release::sha256(binary)?;
    let sent = Arc::new(AtomicU64::new(0));
    let _shown = {
        let (sent, place) = (Arc::clone(&sent), transport.place().to_owned());
        crate::status::show_with(move || {
            format!(
                "uploading Ferry to {place}: {} of {}",
                size(sent.load(Ordering::Relaxed)),
                size(total)
            )
        })
    };
    let mut child = transport
        .command(&release_script(host_dir, build, total, &sha256))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("reach the host")?;
    let upload = (|| -> Result<()> {
        let mut stdin = child.stdin.take().expect("piped");
        let mut buffer = vec![0; 64 << 10];
        loop {
            let read = file.read(&mut buffer).context("read the binary")?;
            if read == 0 {
                break;
            }
            stdin
                .write_all(&buffer[..read])
                .context("upload the binary")?;
            sent.fetch_add(read as u64, Ordering::Relaxed);
        }
        stdin.flush()?;
        Ok(())
    })();
    // The host would only find the upload short; it is told at once.
    if upload.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("wait for the install")?;
    upload?;
    if !status.success() {
        bail!(
            "installing Ferry on {} failed: it exited with {status}",
            transport.place()
        );
    }
    Ok(())
}

/// The script that unpacks the source from its input, builds it, and installs
/// the binary as `<host_dir>/bin/<build>/ferry`, once it says it is `build`:
/// a source upload cut short builds something else, or nothing. With
/// `width`, the width of the terminal here, cargo shows its progress bar,
/// though its standard error is not a terminal there.
///
/// One dev install at a time builds on a host, holding `install.lock`, for
/// they share the source and the target directory; a lock whose holder is
/// gone, or an hour old with none named, is taken over.
fn script(host_dir: &str, build: &str, width: Option<u16>) -> String {
    let progress = width.map_or_else(String::new, |width| {
        format!("CARGO_TERM_PROGRESS_WHEN=always CARGO_TERM_PROGRESS_WIDTH={width} ")
    });
    format!(
        r#"set -e
dir={dir}
build={build}
mkdir -p "$dir/bin/$build"
lock="$dir/install.lock"
src="$dir/src.new.$$"
tmp="$dir/bin/$build/ferry.new.$$"
trap 'rm -f "$tmp"; rm -rf "$src"; if [ "$(cat "$lock/pid" 2>/dev/null)" = $$ ]; then rm -rf "$lock"; fi' EXIT
trap 'exit 1' HUP INT TERM PIPE
said=
until mkdir "$lock" 2>/dev/null; do
    holder=$(cat "$lock/pid" 2>/dev/null) || holder=
    if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
        rm -rf "$lock"
    elif [ -z "$holder" ] && [ -n "$(find "$lock" -prune -mmin +60 2>/dev/null)" ]; then
        rm -rf "$lock"
    else
        if [ -z "$said" ]; then
            echo "ferry: waiting for another install of Ferry here to finish" >&2
            said=yes
        fi
        sleep 1
    fi
done
echo $$ > "$lock/pid"
mkdir "$src"
tar -x -f - -C "$src"
rm -rf "$dir/src"
mv "$src" "$dir/src"
cd "$dir/src"
PATH="$HOME/.cargo/bin:$PATH"
export PATH
{progress}CARGO_TARGET_DIR="$dir/build" cargo build --locked >&2
cp "$dir/build/debug/ferry" "$tmp"
{put_in_place}{keep_recent}"#,
        dir = quote(host_dir),
        build = quote(build),
        put_in_place = put_in_place(
            "what was built here",
            ", as from source that did not all arrive",
        ),
        keep_recent = keep_recent(),
    )
}

fn dev_build(transport: &Transport, host_dir: &str, build: &str) -> Result<()> {
    let root = build_info::source_dir();
    let files = build_info::source_files(root)
        .with_context(|| format!("list the source in {}", root.display()))?;
    let hash = build_info::source_hash(root, &files)?;
    if hash != build_info::SOURCE_HASH {
        bail!(
            "the source in {} has changed since this binary was built; rebuild it and run again",
            root.display()
        );
    }

    // Cargo shows its own progress, on the same line.
    let _aside = crate::status::aside();
    let width = std::io::stderr()
        .is_terminal()
        .then(|| ratatui::crossterm::terminal::size().map_or(80, |(columns, _)| columns.max(20)));
    let mut child = transport
        .command(&script(host_dir, build, width))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("reach the host")?;
    let upload = (|| -> Result<()> {
        let mut archive = tar::Builder::new(child.stdin.take().expect("piped"));
        for file in &files {
            archive
                .append_path_with_name(root.join(file), file)
                .with_context(|| format!("upload {}", file.display()))?;
        }
        archive.into_inner()?.flush()?;
        Ok(())
    })();
    if upload.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("wait for the build")?;
    upload?;
    if !status.success() {
        bail!(
            "building Ferry on {} failed: it exited with {status}",
            transport.place()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Output};

    /// A binary that runs anywhere `sh` does, and says it is `build`.
    fn says(build: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho {build}\n").into_bytes()
    }

    /// Runs the release install script here, for `build`, declaring the size
    /// and checksum of `declared`, and giving it `given` on its input.
    fn install_release(host_dir: &Path, build: &str, declared: &[u8], given: &[u8]) -> Output {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("declared");
        fs::write(&file, declared).unwrap();
        let sha256 = release::sha256(&file).unwrap();
        let script = release_script(
            host_dir.to_str().unwrap(),
            build,
            declared.len() as u64,
            &sha256,
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(given).unwrap();
        child.wait_with_output().unwrap()
    }

    /// What is in a build's directory, but the binary.
    fn left_beside(host_dir: &Path, build: &str) -> Vec<String> {
        fs::read_dir(host_dir.join("bin").join(build))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name != "ferry")
            .collect()
    }

    /// Whether this machine can check a SHA-256 as a host does.
    fn hashes() -> bool {
        ["sha256sum", "shasum"].iter().any(|tool| {
            Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {tool}"))
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    /// Runs the dev install script here, for `build`, with a fake `cargo`
    /// that "builds" by copying `marker`, the archive's one file.
    fn install_dev(temp: &Path, host_dir: &Path, build: &str, marker: &[u8]) -> Output {
        let fake_bin = temp.join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::write(
            fake_bin.join("cargo"),
            "#!/bin/sh\nmkdir -p \"$CARGO_TARGET_DIR/debug\"\ncp marker \"$CARGO_TARGET_DIR/debug/ferry\"\n",
        )
        .unwrap();
        fs::set_permissions(fake_bin.join("cargo"), fs::Permissions::from_mode(0o755)).unwrap();

        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(marker.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive.append_data(&mut header, "marker", marker).unwrap();
        let archive = archive.into_inner().unwrap();

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script(host_dir.to_str().unwrap(), build, None))
            // The script puts `$HOME/.cargo/bin` first, so the real cargo
            // must not be found there.
            .env("HOME", temp)
            .env(
                "PATH",
                format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap()),
            )
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&archive).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn the_script_unpacks_builds_and_installs() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let output = install_dev(temp.path(), &host_dir, "dev-0", &says("dev-0"));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(host_dir.join("bin/dev-0/ferry")).unwrap(),
            says("dev-0")
        );
        assert!(host_dir.join("src/marker").exists());
        assert!(left_beside(&host_dir, "dev-0").is_empty());
        // Its lock is let go of, and its unpacked source moved into place.
        let mut kept: Vec<String> = fs::read_dir(&host_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, ["bin", "build", "src"]);
    }

    /// Source that did not all arrive builds another build, which is not
    /// installed as this one.
    #[test]
    fn a_dev_build_that_is_another_build_is_not_installed() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let output = install_dev(temp.path(), &host_dir, "dev-0", &says("dev-1"));
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("is build dev-1, not dev-0"), "{stderr}");
        assert!(!host_dir.join("bin/dev-0/ferry").exists());
        assert!(left_beside(&host_dir, "dev-0").is_empty());
        assert!(!host_dir.join("install.lock").exists());
    }

    /// A lock left by an install that is gone does not hold up the next.
    #[test]
    fn a_dev_install_takes_over_a_lock_whose_holder_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let mut gone = Command::new("true").spawn().unwrap();
        gone.wait().unwrap();
        fs::create_dir_all(host_dir.join("install.lock")).unwrap();
        fs::write(
            host_dir.join("install.lock/pid"),
            format!("{}\n", gone.id()),
        )
        .unwrap();
        crate::test_support::within(
            "a dev install takes over a lock whose holder is gone",
            crate::test_support::PATIENCE,
            move || {
                let output = install_dev(temp.path(), &host_dir, "dev-0", &says("dev-0"));
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(host_dir.join("bin/dev-0/ferry").exists());
                assert!(!host_dir.join("install.lock").exists());
            },
        );
    }

    /// Runs the release install script here, on a host that already has four
    /// other builds, used at different times.
    #[test]
    fn a_release_is_installed_and_old_builds_removed() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        for (build, time) in [
            ("0.0.1", "202001010000"),
            ("0.0.2", "202101010000"),
            ("0.0.3", "202201010000"),
            ("0.0.4", "202301010000"),
        ] {
            let dir = host_dir.join("bin").join(build);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("ferry"), b"old").unwrap();
            assert!(
                Command::new("touch")
                    .args(["-t", time])
                    .arg(&dir)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let binary = says("0.1.0");
        let output = install_release(&host_dir, "0.1.0", &binary, &binary);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let installed = host_dir.join("bin/0.1.0/ferry");
        assert_eq!(fs::read(&installed).unwrap(), binary);
        assert!(installed.metadata().unwrap().permissions().mode() & 0o111 != 0);
        assert!(left_beside(&host_dir, "0.1.0").is_empty());
        let mut kept: Vec<String> = fs::read_dir(host_dir.join("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, ["0.0.3", "0.0.4", "0.1.0"]);
    }

    /// A build beyond the three most recent, used in the last hour, as one
    /// another client just probed and is about to start, is kept.
    #[test]
    fn a_build_used_in_the_last_hour_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let now = std::time::SystemTime::now();
        for (build, ago) in [
            ("0.0.1", 3 * 365 * 86_400),
            ("0.0.2", 10 * 60),
            ("0.0.3", 60),
            ("0.0.4", 0),
        ] {
            let dir = host_dir.join("bin").join(build);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("ferry"), b"old").unwrap();
            File::open(&dir)
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(ago))
                .unwrap();
        }
        let binary = says("0.1.0");
        let output = install_release(&host_dir, "0.1.0", &binary, &binary);
        assert!(output.status.success());
        let mut kept: Vec<String> = fs::read_dir(host_dir.join("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, ["0.0.2", "0.0.3", "0.0.4", "0.1.0"]);
    }

    /// An upload cut short, as by Ctrl-C or a dropped connection, which the
    /// host sees only as its input ending, is not installed, and leaves
    /// nothing behind.
    #[test]
    fn an_upload_cut_short_is_not_installed() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let binary = says("0.1.0");
        let output = install_release(&host_dir, "0.1.0", &binary, &binary[..10]);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("ended after 10 of {} bytes", binary.len())),
            "{stderr}"
        );
        assert!(!host_dir.join("bin/0.1.0/ferry").exists());
        assert!(left_beside(&host_dir, "0.1.0").is_empty());

        // Run again, it installs.
        let output = install_release(&host_dir, "0.1.0", &binary, &binary);
        assert!(output.status.success());
        assert_eq!(fs::read(host_dir.join("bin/0.1.0/ferry")).unwrap(), binary);
    }

    #[test]
    fn an_upload_that_does_not_match_its_checksum_is_not_installed() {
        if !hashes() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let binary = says("0.1.0");
        let other = says("0.1.1");
        assert_eq!(binary.len(), other.len());
        let output = install_release(&host_dir, "0.1.0", &binary, &other);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("does not match its checksum"), "{stderr}");
        assert!(!host_dir.join("bin/0.1.0/ferry").exists());
        assert!(left_beside(&host_dir, "0.1.0").is_empty());
    }

    /// A binary that cannot run on the host, as one for another platform,
    /// or where the home directory is mounted noexec, is found at install.
    #[test]
    fn a_binary_that_does_not_run_on_the_host_is_not_installed() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let binary = b"\x7fELF not for here".to_vec();
        let output = install_release(&host_dir, "0.1.0", &binary, &binary);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("the uploaded binary does not run here"),
            "{stderr}"
        );
        assert!(!host_dir.join("bin/0.1.0/ferry").exists());
        assert!(left_beside(&host_dir, "0.1.0").is_empty());
    }

    /// Two clients installing the same build at once each write their own
    /// file, and what is installed is whole.
    #[test]
    fn two_installs_at_once_leave_one_whole_binary() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let binary = says("0.1.0");
        let installs: Vec<_> = (0..2)
            .map(|_| {
                let (host_dir, binary) = (host_dir.clone(), binary.clone());
                std::thread::spawn(move || install_release(&host_dir, "0.1.0", &binary, &binary))
            })
            .collect();
        for install in installs {
            let output = install.join().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let installed = host_dir.join("bin/0.1.0/ferry");
        assert_eq!(fs::read(&installed).unwrap(), binary);
        assert!(left_beside(&host_dir, "0.1.0").is_empty());
        let ran = Command::new(&installed).output().unwrap();
        assert_eq!(ran.stdout, b"0.1.0\n");
    }
}
