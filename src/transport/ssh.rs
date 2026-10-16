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

use crate::home;
use crate::remote::shell;

/// The longest Unix socket path macOS accepts, which is shorter than Linux's.
const MAX_SOCKET_PATH: usize = 103;
/// What a control socket path adds to its directory: `/`, the 40 hex digits
/// of `%C`, a `-` and the SSH connection's number, and the 17 characters
/// OpenSSH appends while it binds a new master.
const CONTROL_SOCKET_SUFFIX: usize = 1 + 40 + 4 + 17;

/// One SSH connection to a host, through its own ControlMaster.
pub struct Ssh {
    host: String,
    options: Vec<String>,
}

impl Ssh {
    /// The `index`th SSH connection to `host`, counting from 0. The first
    /// shares your own ControlMaster, if `yours`; every other one, and the
    /// first without yours, has a ControlMaster of Ferry's own, so that each
    /// is its own TCP connection.
    pub fn new(host: &str, index: usize, yours: bool) -> Result<Self> {
        let mut options = vec!["-o".to_owned(), "ConnectTimeout=10".to_owned()];
        if !(index == 0 && yours) {
            options.extend(control_options(index)?);
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
        let mut command = self.ssh(&["-T"]);
        command.arg(shell::sh_command(script));
        command
    }

    /// A command whose input and output are a channel to the Unix socket at
    /// `socket` on the host, which SSH forwards to without a shell.
    pub fn forward(&self, socket: &str) -> Command {
        self.ssh(&["-W", socket])
    }

    /// A command that logs in and holds the connection open, running nothing:
    /// with a ControlMaster, it starts the master.
    pub fn hold(&self) -> Command {
        self.ssh(&["-N"])
    }

    /// A command that succeeds once the ControlMaster is up.
    pub fn check(&self) -> Command {
        self.ssh(&["-O", "check"])
    }

    /// The same as `hold`, but never asking: it fails where a login would
    /// need someone to type it.
    pub fn hold_quietly(&self) -> Command {
        self.ssh(&["-o", "BatchMode=yes", "-N"])
    }

    fn ssh(&self, arguments: &[&str]) -> Command {
        let mut command = program();
        crate::interrupt::unblock_in_child(&mut command)
            .args(&self.options)
            .args(arguments)
            .arg("--")
            .arg(&self.host);
        command
    }
}

/// `ssh`, or the program `FERRY_SSH` names instead, as rsync's `RSYNC_RSH`; it
/// is given `ssh`'s arguments.
fn program() -> Command {
    Command::new(std::env::var_os("FERRY_SSH").unwrap_or_else(|| "ssh".into()))
}

/// Whether your own SSH configuration sets up a ControlMaster for `host`, as
/// `ssh -G` reads it: then Ferry shares it, rather than starting its own.
pub fn yours(host: &str) -> bool {
    let output = program()
        .args(["-G", "--", host])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    let options = String::from_utf8_lossy(&output.stdout);
    let option = |name: &str| {
        options.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == name).then(|| value.trim().to_owned())
        })
    };
    output.status.success()
        && option("controlmaster").is_some_and(|value| value != "false" && value != "no")
        && option("controlpath").is_some_and(|value| value != "none")
}

/// Options for a ControlMaster of Ferry's own, for the `index`th SSH
/// connection: the first `ssh` becomes the master and logs in, and later ones
/// reuse it, so a password or OTP is typed once. The master outlives the
/// command by ten minutes.
fn control_options(index: usize) -> Result<Vec<String>> {
    let runtime = home::runtime_dir();
    private_directory(&runtime)?;
    let sockets = runtime.join("ssh");
    private_directory(&sockets)?;
    let sockets = sockets
        .to_str()
        .context("the socket directory is not UTF-8")?;
    if sockets.len() + CONTROL_SOCKET_SUFFIX > MAX_SOCKET_PATH {
        bail!(
            "SSH control sockets in {sockets} would exceed the {MAX_SOCKET_PATH}-byte limit on socket paths"
        );
    }
    let name = match index {
        0 => "%C".to_owned(),
        _ => format!("%C-{index}"),
    };
    Ok([
        "ControlMaster=auto",
        &format!("ControlPath={sockets}/{name}"),
        "ControlPersist=600",
    ]
    .into_iter()
    .flat_map(|option| ["-o".to_owned(), option.to_owned()])
    .collect())
}

/// Creates `path` if needed, and refuses it unless this user owns it and no
/// one else may open it: a control socket grants a logged-in connection.
pub(crate) fn private_directory(path: &Path) -> Result<()> {
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
