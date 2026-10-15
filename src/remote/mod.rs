//! The build of Ferry to run at a place: on this machine, the client's own
//! binary; on a host, the build installed there, installed first if it is
//! missing. And where a transfer's sides are.

mod install;
mod location;
mod probe;
pub(crate) mod release;
pub(crate) mod shell;

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};

pub use location::Location;

use crate::build_info::{self, PROFILE, Profile};
use crate::home;
use crate::transport::Transport;

pub struct RemoteOptions<'a> {
    /// A Ferry binary on the host to run instead of the one Ferry installs.
    pub remote_ferry: Option<&'a str>,
    /// Install without asking.
    pub yes: bool,
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

/// The script that runs `program` with `arguments`.
pub fn script(program: &str, arguments: &[&str]) -> String {
    let mut line = format!("exec {}", shell::quote(program));
    for argument in arguments {
        line.push(' ');
        line.push_str(&shell::quote(argument));
    }
    line
}

/// The path of this build on the host, installing it if it is not there.
fn ensure_installed(transport: &Transport, yes: bool) -> Result<String> {
    let build = build_info::identity();
    let probe = probe::run(transport, home::dir_name(), &build)?;
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
            confirm(&format!(
                "Ferry is not installed on {host}. Install {build} there?"
            ))?;
        }
        let binary = release::binary(target)?;
        eprintln!("ferry: installing {build} on {host} ({target})");
        install::release_build(transport, &probe.host_dir, &build, &binary)?;
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
        confirm(&format!(
            "Ferry is not installed on {host}. Build and install {build} there from source?"
        ))?;
    }
    eprintln!(
        "ferry: installing {build} on {host} ({} {})",
        probe.os, probe.arch
    );
    install::dev_build(transport, &probe.host_dir, &build)?;
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
