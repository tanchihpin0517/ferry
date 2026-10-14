//! Where Ferry keeps its files, on the client and on hosts.
//!
//! A release build and a dev build never share any of them: a release uses
//! `~/.ferry/` and `/tmp/ferry-<uid>/`, a dev build `~/.ferry-dev/` and
//! `/tmp/ferry-dev-<uid>/`.

use std::env;
use std::path::PathBuf;

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

/// The directory for this user's sockets. It is under `/tmp` rather than the
/// home directory, because a Unix socket path is limited to about 100 bytes.
pub fn runtime_dir() -> PathBuf {
    let uid = rustix::process::getuid().as_raw();
    let prefix = match PROFILE {
        Profile::Release => "ferry",
        Profile::Dev => "ferry-dev",
    };
    PathBuf::from(format!("/tmp/{prefix}-{uid}"))
}
