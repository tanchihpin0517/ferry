//! Running commands on a host with the system's `ssh`.
//!
//! Using `ssh` itself, rather than an SSH library, keeps `~/.ssh/config`,
//! `ProxyJump`, agents, Kerberos, hardware keys, OTP, and `known_hosts`
//! working as they do for the user. A password or OTP prompt goes to the
//! terminal, because `ssh` reads it from there rather than from its input.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::SshConfig;
use crate::home;
use crate::remote::shell;

/// The longest Unix socket path macOS accepts, which is shorter than Linux's.
const MAX_SOCKET_PATH: usize = 103;
/// What a control socket path adds to its directory: `/`, the 40 hex digits
/// of `%C`, and the 17 characters OpenSSH appends while it binds a new master.
const CONTROL_SOCKET_SUFFIX: usize = 1 + 40 + 17;

pub struct Ssh {
    host: String,
    options: Vec<String>,
}

impl Ssh {
    pub fn new(host: &str, config: &SshConfig) -> Result<Self> {
        let mut options = vec!["-o".to_owned(), "ConnectTimeout=10".to_owned()];
        if config.manage_control_master {
            options.extend(control_options()?);
        }
        Ok(Self {
            host: host.to_owned(),
            options,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// A command that runs `script` with `sh` on the host. `FERRY_SSH`, if
    /// set, names the program to run instead of `ssh`, as rsync's `RSYNC_RSH`
    /// does; it is given `ssh`'s arguments.
    pub fn script(&self, script: &str) -> Command {
        let program = std::env::var_os("FERRY_SSH").unwrap_or_else(|| "ssh".into());
        let mut command = Command::new(program);
        crate::interrupt::unblock_in_child(&mut command)
            .args(&self.options)
            .arg("-T")
            .arg("--")
            .arg(&self.host)
            .arg(shell::sh_command(script));
        command
    }
}

/// Options that share one authenticated connection per host: the first `ssh`
/// becomes the master and logs in, and later ones reuse it, so a password or
/// OTP is typed once. The master outlives the command by ten minutes.
fn control_options() -> Result<Vec<String>> {
    let runtime = home::runtime_dir();
    private_directory(&runtime)?;
    let sockets = runtime.join("ssh");
    private_directory(&sockets)?;
    let sockets = sockets
        .to_str()
        .context("the socket directory is not UTF-8")?;
    if sockets.len() + CONTROL_SOCKET_SUFFIX > MAX_SOCKET_PATH {
        bail!(
            "SSH control sockets in {sockets} would exceed the {MAX_SOCKET_PATH}-byte limit on socket paths; \
             set manage_control_master = false in Ferry's config.toml"
        );
    }
    Ok([
        "ControlMaster=auto",
        &format!("ControlPath={sockets}/%C"),
        "ControlPersist=600",
    ]
    .into_iter()
    .flat_map(|option| ["-o".to_owned(), option.to_owned()])
    .collect())
}

/// Creates `path` if needed, and refuses it unless this user owns it and no
/// one else may open it: a control socket grants a logged-in connection.
fn private_directory(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "{} must be a directory owned by you with mode 0700",
            path.display()
        );
    }
    Ok(())
}
