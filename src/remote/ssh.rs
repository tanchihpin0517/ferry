//! Running commands on a host with the system's `ssh`.
//!
//! Using `ssh` itself, rather than an SSH library, keeps `~/.ssh/config`,
//! `ProxyJump`, agents, Kerberos, hardware keys, OTP, and `known_hosts`
//! working as they do for the user. A password or OTP prompt goes to the
//! terminal, because `ssh` reads it from there rather than from its input.

use std::process::Command;

use crate::remote::shell;

pub struct Ssh {
    host: String,
    options: Vec<String>,
}

impl Ssh {
    pub fn new(host: &str) -> Self {
        let options = vec!["-o".to_owned(), "ConnectTimeout=10".to_owned()];
        Self {
            host: host.to_owned(),
            options,
        }
    }

    /// A command that runs `script` with `sh` on the host.
    pub fn script(&self, script: &str) -> Command {
        let mut command = Command::new("ssh");
        command
            .args(&self.options)
            .arg("-T")
            .arg("--")
            .arg(&self.host)
            .arg(shell::sh_command(script));
        command
    }
}
