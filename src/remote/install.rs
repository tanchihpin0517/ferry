//! Installing a dev build on a host, from the source it was compiled from.
//!
//! The client uploads its source as a tar stream over SSH, and the host builds
//! it with its own Rust toolchain into a target directory it keeps, so that a
//! rebuild is incremental. The binary is put in place by a rename, so a build
//! that fails leaves no broken binary behind.

use std::io::Write;
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::build_info;
use crate::remote::shell::quote;
use crate::remote::ssh::Ssh;

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
"#,
        dir = quote(host_dir),
        build = quote(build),
    )
}

pub fn dev_build(ssh: &Ssh, host_dir: &str, build: &str) -> Result<()> {
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

    let mut child = ssh
        .script(&script(host_dir, build))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start ssh")?;
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
            "building Ferry on {} failed: ssh exited with {status}",
            ssh.host()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
}
