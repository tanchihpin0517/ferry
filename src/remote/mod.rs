//! Reaching a server on a host: SSH, and the build of Ferry to run there.

mod install;
mod location;
mod probe;
mod shell;
mod ssh;

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::Command;

use anyhow::{Result, bail};

pub use location::Location;

use crate::build_info::{self, PROFILE, Profile};
use crate::config::SshConfig;
use crate::home;
use ssh::Ssh;

pub struct RemoteOptions<'a> {
    /// A Ferry binary on the host to run instead of the one Ferry installs.
    pub remote_ferry: Option<&'a str>,
    /// Install without asking.
    pub yes: bool,
}

/// The command that starts `ferry server` on `host`, installing this build
/// there first if it is missing.
pub fn server_command(host: &str, options: &RemoteOptions, config: &SshConfig) -> Result<Command> {
    let ssh = Ssh::new(host, config)?;
    let program = match options.remote_ferry {
        Some(program) => program.to_owned(),
        None => ensure_installed(&ssh, options.yes)?,
    };
    Ok(ssh.script(&format!("exec {} server", shell::quote(&program))))
}

/// The path of this build on the host, installing it if it is not there.
fn ensure_installed(ssh: &Ssh, yes: bool) -> Result<String> {
    let build = build_info::identity();
    let probe = probe::run(ssh, home::dir_name(), &build)?;
    let program = format!("{}/bin/{build}/ferry", probe.host_dir);
    if probe.installed {
        return Ok(program);
    }
    let host = ssh.host();
    if PROFILE == Profile::Release {
        bail!(
            "{host} does not have Ferry {build}, and installing release builds is not implemented yet; use --remote-ferry"
        );
    }
    if !probe.cargo {
        bail!(
            "{host} has no cargo; installing a dev build builds it there from source, which needs a Rust toolchain"
        );
    }
    // Asking once per host is enough: a host that has any build agreed to Ferry
    // before, and a newer build is installed beside the old ones.
    if !probe.any_build && !yes {
        confirm(&format!(
            "Ferry is not installed on {host}. Build and install {build} there from source?"
        ))?;
    }
    eprintln!(
        "ferry: installing {build} on {host} ({} {})",
        probe.os, probe.arch
    );
    install::dev_build(ssh, &probe.host_dir, &build)?;
    Ok(program)
}

/// Asks on the terminal; without one, refuses and says how to agree.
fn confirm(question: &str) -> Result<()> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        bail!("{question} Not without a terminal to ask on; pass --yes to agree");
    }
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("not installed");
    }
    Ok(())
}
