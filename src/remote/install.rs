//! Installing a build on a host, and removing old ones.
//!
//! A release build uploads its binary over SSH. A dev build uploads its source
//! as a tar stream, and the host builds it with its own Rust toolchain into a
//! target directory it keeps, so that a rebuild is incremental. Either way the
//! binary is put in place by a rename, so an install that fails leaves no
//! broken binary behind.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::build_info;
use crate::remote::shell::quote;
use crate::transport::Transport;

/// How many builds a host keeps: the most recently used.
const KEEP: usize = 3;

/// Shell that marks `$dir/bin/$build` used, and removes all but the [`KEEP`]
/// most recently used builds. A build's directory's modification time is when
/// it was last used.
pub fn keep_recent() -> String {
    format!(
        r#"touch "$dir/bin/$build"
ls -t "$dir/bin" | tail -n +{first_old} | while IFS= read -r old; do
    if [ "$old" != "$build" ]; then rm -rf "${{dir:?}}/bin/$old"; fi
done
"#,
        first_old = KEEP + 1,
    )
}

/// The script that installs the binary on its input as
/// `<host_dir>/bin/<build>/ferry`.
fn release_script(host_dir: &str, build: &str) -> String {
    format!(
        r#"set -e
dir={dir}
build={build}
mkdir -p "$dir/bin/$build"
cat > "$dir/bin/$build/ferry.new"
chmod 755 "$dir/bin/$build/ferry.new"
mv "$dir/bin/$build/ferry.new" "$dir/bin/$build/ferry"
{keep_recent}"#,
        dir = quote(host_dir),
        build = quote(build),
        keep_recent = keep_recent(),
    )
}

/// Uploads the release binary at `binary` and installs it as `build`.
pub fn release_build(
    transport: &Transport,
    host_dir: &str,
    build: &str,
    binary: &Path,
) -> Result<()> {
    let mut file = File::open(binary).with_context(|| format!("open {}", binary.display()))?;
    let mut child = transport
        .command(&release_script(host_dir, build))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("reach the host")?;
    let upload = (|| -> Result<()> {
        let mut stdin = child.stdin.take().expect("piped");
        std::io::copy(&mut file, &mut stdin).context("upload the binary")?;
        stdin.flush()?;
        Ok(())
    })();
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
/// the binary as `<host_dir>/bin/<build>/ferry`.
fn script(host_dir: &str, build: &str) -> String {
    format!(
        r#"set -e
dir={dir}
build={build}
rm -rf "$dir/src.new"
mkdir -p "$dir/src.new"
tar -x -f - -C "$dir/src.new"
rm -rf "$dir/src"
mv "$dir/src.new" "$dir/src"
cd "$dir/src"
PATH="$HOME/.cargo/bin:$PATH"
export PATH
CARGO_TARGET_DIR="$dir/build" cargo build --locked >&2
mkdir -p "$dir/bin/$build"
cp "$dir/build/debug/ferry" "$dir/bin/$build/ferry.new"
mv "$dir/bin/$build/ferry.new" "$dir/bin/$build/ferry"
{keep_recent}"#,
        dir = quote(host_dir),
        build = quote(build),
        keep_recent = keep_recent(),
    )
}

pub fn dev_build(transport: &Transport, host_dir: &str, build: &str) -> Result<()> {
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

    let mut child = transport
        .command(&script(host_dir, build))
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
    use std::process::Command;

    /// Runs the install script here, with a fake `cargo` that "builds" by
    /// copying a file, to check the unpacking and the installed path.
    #[test]
    fn the_script_unpacks_builds_and_installs() {
        let temp = tempfile::tempdir().unwrap();
        let host_dir = temp.path().join("hosts/abc");
        let fake_bin = temp.path().join("fake-bin");
        fs::create_dir(&fake_bin).unwrap();
        fs::write(
            fake_bin.join("cargo"),
            "#!/bin/sh\nmkdir -p \"$CARGO_TARGET_DIR/debug\"\ncp marker \"$CARGO_TARGET_DIR/debug/ferry\"\n",
        )
        .unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(fake_bin.join("cargo"))
            .status()
            .unwrap();

        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "marker", &b"binary"[..])
            .unwrap();
        let archive = archive.into_inner().unwrap();

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script(host_dir.to_str().unwrap(), "dev-0"))
            // The script puts `$HOME/.cargo/bin` first, so the real cargo
            // must not be found there.
            .env("HOME", temp.path())
            .env(
                "PATH",
                format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap()),
            )
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&archive).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read(host_dir.join("bin/dev-0/ferry")).unwrap(),
            b"binary"
        );
        assert!(host_dir.join("src/marker").exists());
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

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(release_script(host_dir.to_str().unwrap(), "0.1.0"))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"binary").unwrap();
        assert!(child.wait().unwrap().success());

        let installed = host_dir.join("bin/0.1.0/ferry");
        assert_eq!(fs::read(&installed).unwrap(), b"binary");
        assert!(installed.metadata().unwrap().permissions().mode() & 0o111 != 0);
        let mut kept: Vec<String> = fs::read_dir(host_dir.join("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, ["0.0.3", "0.0.4", "0.1.0"]);
    }
}
