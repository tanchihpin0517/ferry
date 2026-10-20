//! Where Ferry keeps its files, on the client and on hosts.
//!
//! A release build and a dev build never share any of them: a release uses
//! `~/.ferry/` and `/tmp/ferry-<uid>/`, a dev build `~/.ferry-dev/` and
//! `/tmp/ferry-dev-<uid>/`.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::build_info::{PROFILE, Profile};

/// The name of Ferry's directory in a home directory.
pub fn dir_name() -> &'static str {
    match PROFILE {
        Profile::Release => ".ferry",
        Profile::Dev => ".ferry-dev",
    }
}

/// Ferry's directory on this machine: `FERRY_HOME` if set, which must be
/// absolute, or the directory named by [`dir_name`] in the home directory.
pub fn local_home() -> Result<PathBuf> {
    if let Some(home) = env::var_os("FERRY_HOME") {
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            bail!(
                "FERRY_HOME must be an absolute path, not {}",
                home.display()
            );
        }
        return Ok(home);
    }
    match env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(dir_name())),
        None => bail!("HOME is not set"),
    }
}

/// How many other names a runtime directory may take, after its own, where
/// another user made a directory of that name first, to keep this one out.
const RUNTIME_SPARES: usize = 9;

/// This user's directory for its sockets, private to it, made if need be. It
/// is under `/tmp` rather than the home directory, because a Unix socket
/// path is limited to about 100 bytes. Its name is fixed, so that a later
/// run finds the same, as a ControlMaster kept alive is found by its socket;
/// where another user made a directory of that name first, the first of a
/// few spare names that is this user's alone, the same each time.
pub fn runtime_dir() -> Result<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    first_private(Path::new("/tmp"), &format!("{}-{uid}", runtime_prefix()))
}

/// The first directory in `base` named `name`, or `name` and one of a few
/// spare suffixes, that is this user's alone, made if need be; see
/// [`runtime_dir`].
pub fn first_private(base: &Path, name: &str) -> Result<PathBuf> {
    let mut refused = None;
    for spare in 0..=RUNTIME_SPARES {
        let path = match spare {
            0 => base.join(name),
            _ => base.join(format!("{name}-{spare}")),
        };
        match crate::transport::ssh::private_directory(&path) {
            Ok(()) => return Ok(path),
            Err(error) => {
                refused.get_or_insert(error);
            }
        }
    }
    Err(refused.expect("tried").context(format!(
        "no directory {}/{name} or with a spare name is this user's alone",
        base.display()
    )))
}

/// The name of the directory where a server started through `sudo` by the
/// user `uid` waits for that user's paths, in `/tmp`, before any spare
/// suffix: root's, for that user to reach.
pub fn sudo_runtime_name(uid: u32) -> String {
    format!("{}-sudo-{uid}", runtime_prefix())
}

pub fn runtime_prefix() -> &'static str {
    match PROFILE {
        Profile::Release => "ferry",
        Profile::Dev => "ferry-dev",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A directory of the name that is not this user's alone, as one another
    /// user made first would not be, is passed over for a spare name, the
    /// same each time; one never is used.
    #[test]
    fn a_runtime_directory_taken_is_passed_over_for_a_spare() {
        let temp = tempfile::tempdir().unwrap();
        let taken = temp.path().join("ferry-7");
        std::fs::create_dir(&taken).unwrap();
        std::fs::set_permissions(&taken, std::fs::Permissions::from_mode(0o755)).unwrap();
        let chosen = first_private(temp.path(), "ferry-7").unwrap();
        assert_eq!(chosen, temp.path().join("ferry-7-1"));
        assert_eq!(first_private(temp.path(), "ferry-7").unwrap(), chosen);
        let mode = std::fs::metadata(&chosen).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // The taken one is left as it was.
        let mode = std::fs::metadata(&taken).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}
