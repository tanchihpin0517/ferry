//! The client's settings, in `config.toml` in Ferry's directory.
//!
//! ```toml
//! [ssh]
//! # Share one authenticated connection per host. Turn this off to leave
//! # ControlMaster to your own SSH configuration.
//! manage_control_master = true
//! ```

use std::fs;
use std::io;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::home;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub ssh: SshConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    pub manage_control_master: bool,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            manage_control_master: true,
        }
    }
}

/// Reads the settings; a missing file means every default.
pub fn load() -> Result<Config> {
    let path = home::local_home()?.join("config.toml");
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("read {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.ssh.manage_control_master);
    }

    #[test]
    fn turn_off_the_control_master() {
        let config: Config = toml::from_str("[ssh]\nmanage_control_master = false\n").unwrap();
        assert!(!config.ssh.manage_control_master);
    }

    #[test]
    fn unknown_settings_are_errors() {
        assert!(toml::from_str::<Config>("[ssh]\nmanage = false\n").is_err());
    }
}
