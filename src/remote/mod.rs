//! Reaching a server on a host: SSH, and the build of Ferry to run there.

mod location;
mod shell;
mod ssh;

use std::process::Command;

use anyhow::{Result, bail};

pub use location::Location;

use ssh::Ssh;

pub struct RemoteOptions<'a> {
    /// A Ferry binary on the host to run.
    pub remote_ferry: Option<&'a str>,
}

/// The command that starts `ferry server` on `host`.
pub fn server_command(host: &str, options: &RemoteOptions) -> Result<Command> {
    let ssh = Ssh::new(host);
    let Some(program) = options.remote_ferry else {
        bail!("installing Ferry on a host is not implemented yet; use --remote-ferry");
    };
    Ok(ssh.script(&format!("exec {} server", shell::quote(program))))
}
