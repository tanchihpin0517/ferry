//! `ferry update`: replaces this binary with another release, the latest
//! unless told which.
//!
//! The release is downloaded and checked as one for a host is, and kept
//! with them, so that installing it on a host of the same platform later
//! costs no download. The binary is replaced by a rename in its own
//! directory, so an update that fails leaves the old one whole.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::build_info::{PROFILE, Profile, TARGET, VERSION};
use crate::remote::release;

/// What `--check` exits with when a newer release is out: not 2, which a
/// command line that is wrong exits with.
pub const NEWER: u8 = 3;

/// Updates to `version`, or the latest release; with `check`, only says
/// whether a newer one is out.
pub fn run(check: bool, version: Option<&str>) -> Result<ExitCode> {
    if PROFILE != Profile::Release {
        bail!(
            "this is a dev build, built from source; update it from source: pull, and build it again"
        );
    }
    let wanted = match version {
        Some(version) => {
            let version = version.strip_prefix('v').unwrap_or(version);
            if release::parse_version(version).is_none() {
                bail!("{version} is not a version, such as 0.1.0");
            }
            version.to_owned()
        }
        None => release::latest()?,
    };
    let newer = release::parse_version(&wanted) > release::parse_version(VERSION);

    if check {
        if newer {
            println!("Ferry {wanted} is out; this is {VERSION}. `ferry update` installs it.");
            return Ok(ExitCode::from(NEWER));
        }
        println!("Ferry {VERSION} is the latest.");
        return Ok(ExitCode::SUCCESS);
    }
    if wanted == VERSION {
        match version {
            Some(_) => println!("Ferry {VERSION} is already installed."),
            None => println!("Ferry {VERSION} is the latest."),
        }
        return Ok(ExitCode::SUCCESS);
    }
    // The latest that is older: this one is newer than any release.
    if version.is_none() && !newer {
        println!("Ferry {VERSION} is newer than the latest release, {wanted}.");
        return Ok(ExitCode::SUCCESS);
    }

    let binary = release::binary_of(&wanted, TARGET)
        .with_context(|| format!("download Ferry {wanted}; is it released?"))?;
    let installed = std::env::current_exe().context("find this ferry binary")?;
    replace(&installed, &binary)?;
    println!(
        "Updated Ferry {VERSION} to {wanted}, at {}.",
        installed.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Replaces `installed` with a copy of `binary`, by a rename in its
/// directory.
fn replace(installed: &Path, binary: &Path) -> Result<()> {
    let directory = installed
        .parent()
        .with_context(|| format!("{} has no directory", installed.display()))?;
    let new = directory.join(format!(".ferry-update-{}", std::process::id()));
    // Synced before it takes the name, and the name after, so that a power
    // loss leaves the old binary or the new one whole, never one empty, as
    // a filesystem that does not order a rename after the data may.
    let put = (|| -> io::Result<()> {
        fs::copy(binary, &new)?;
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
        fs::File::open(&new)?.sync_all()?;
        fs::rename(&new, installed)?;
        fs::File::open(directory)?.sync_all()
    })();
    if let Err(error) = put {
        let _ = fs::remove_file(&new);
        if error.kind() == io::ErrorKind::PermissionDenied {
            bail!(
                "cannot replace {}: no permission to write in {}; run the update as a user who may, or install again with install.sh",
                installed.display(),
                directory.display()
            );
        }
        return Err(error).with_context(|| format!("replace {}", installed.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_is_replaced_whole() {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("bin/ferry");
        fs::create_dir(temp.path().join("bin")).unwrap();
        fs::write(&installed, b"old").unwrap();
        let binary = temp.path().join("ferry-new");
        fs::write(&binary, b"new").unwrap();

        replace(&installed, &binary).unwrap();
        assert_eq!(fs::read(&installed).unwrap(), b"new");
        let mode = installed.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // Nothing else is left in its directory.
        assert_eq!(fs::read_dir(temp.path().join("bin")).unwrap().count(), 1);
    }

    #[test]
    fn a_directory_without_permission_is_refused_and_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let installed = bin.join("ferry");
        fs::write(&installed, b"old").unwrap();
        let binary = temp.path().join("ferry-new");
        fs::write(&binary, b"new").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).unwrap();

        let result = replace(&installed, &binary);
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        // Root may write anywhere; there is nothing to check then.
        if rustix::process::getuid().is_root() {
            return;
        }
        let error = result.unwrap_err().to_string();
        assert!(error.contains("no permission"), "{error}");
        assert_eq!(fs::read(&installed).unwrap(), b"old");
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 1);
    }
}
