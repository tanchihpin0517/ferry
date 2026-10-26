//! `--remote-sudo`: a host's side run as root, through `sudo`, so that it can
//! give entries any owner, as rsync's `--rsync-path="sudo rsync"` does.
//!
//! Only the server runs through `sudo`; probing and installing stay the
//! login user's, so Ferry is installed in their home as usual. Ferry's
//! monitor on the host runs `sudo`, for the checks below and for the
//! server, on connections no shell can write to.
//!
//! A server's standard input is its control connection or its path, which no
//! terminal may sit on, so `sudo` cannot ask for a password itself. If it
//! needs one, Ferry asks for it here, once a host, and has `sudo` read it
//! from standard input, first, with `-S`: it reads up to the line's end, and
//! the program it runs reads the rest. `-k` has `sudo` read it every time,
//! even where it would remember it, so that the line is never left for the
//! program to read. The password is checked before anything runs with it.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use rustix::termios::{self, LocalModes, OptionalActions, Termios};

use crate::interrupt;
use crate::protocol::{Launch, SudoAsks};
use crate::transport::monitor::{Monitor, Ran};

/// How many times a password typed in is asked for, as `sudo` does.
const TRIES: usize = 3;

/// `FERRY_SUDO_PASSWORD`, as it was when the process started.
static FROM_ENVIRONMENT: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

/// Takes `FERRY_SUDO_PASSWORD` out of the environment, keeping it for
/// [`Sudo::prepare`], so that no child sees it: `ssh` and what it runs, which
/// `SendEnv` might pass on to a host, `curl`, or a server. Must run before
/// any other thread starts.
pub fn take_password_from_environment() {
    let password =
        std::env::var_os("FERRY_SUDO_PASSWORD").map(|password| password.into_encoded_bytes());
    // SAFETY: no other thread runs yet, to read the environment meanwhile.
    unsafe {
        std::env::remove_var("FERRY_SUDO_PASSWORD");
    }
    let _ = FROM_ENVIRONMENT.set(password);
}

/// How a host's server runs through `sudo`.
#[derive(Clone)]
pub struct Sudo {
    /// The password to give it, or none if it needs none.
    password: Option<Vec<u8>>,
}

impl Drop for Sudo {
    fn drop(&mut self) {
        if let Some(password) = &mut self.password {
            password.fill(0);
        }
    }
}

impl Sudo {
    /// Gets ready to run Ferry through `sudo` on `host`, whose `monitor`
    /// runs it: finds whether it needs a password, and if so, takes it from
    /// `FERRY_SUDO_PASSWORD`, or asks for it on the terminal, and checks it.
    /// Both run Ferry itself, harmlessly, to say its build: sudoers may ask
    /// no password for it alone, and a password `sudo` did not read would be
    /// the server's first input.
    pub fn prepare(monitor: &Arc<Monitor>, host: &str) -> Result<Self> {
        let shown = crate::status::show(format!("checking sudo on {host}"));
        let free = monitor.run(
            Launch::SudoCheck {
                asks: SudoAsks::Nothing,
            },
            &[],
        )?;
        drop(shown);
        match free {
            Ran::Ended { status, .. } if status.success() => return Ok(Self { password: None }),
            Ran::NotStarted(reason) => {
                bail!("{host} has no sudo to run, which --remote-sudo needs: {reason}")
            }
            Ran::Ended { .. } => {}
        }
        if let Some(password) = FROM_ENVIRONMENT.get().cloned().flatten() {
            let sudo = Self {
                password: Some(password),
            };
            sudo.check(monitor, host)?;
            return Ok(sudo);
        }
        let mut tries = 0;
        loop {
            tries += 1;
            let sudo = Self {
                password: Some(ask(&format!("[sudo] password for {host}: "), host)?),
            };
            match sudo.check(monitor, host) {
                Ok(()) => return Ok(sudo),
                Err(error) if tries == TRIES => return Err(error),
                Err(error) => eprintln!("ferry: {error:#}"),
            }
        }
    }

    /// Checks the password, running nothing with it but Ferry, to say its
    /// build. Only its line goes: a wrong password is then asked for again,
    /// and finds nothing more.
    fn check(&self, monitor: &Arc<Monitor>, host: &str) -> Result<()> {
        let _shown = crate::status::show(format!("checking the password on {host}"));
        let ran = monitor.run(
            Launch::SudoCheck {
                asks: SudoAsks::Password,
            },
            &self.first(),
        )?;
        let said = match ran {
            Ran::Ended { status, .. } if status.success() => return Ok(()),
            Ran::Ended { said, .. } => said,
            Ran::NotStarted(reason) => reason.into_bytes(),
        };
        let said = String::from_utf8_lossy(&said);
        let said = said
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        bail!("sudo on {host} did not run with that password: {said}")
    }

    /// Whether `sudo` reads a password, first on its input.
    pub fn asks_password(&self) -> bool {
        self.password.is_some()
    }

    /// What to write to `sudo` first: the password's line, if any.
    pub fn first(&self) -> Vec<u8> {
        match &self.password {
            None => Vec::new(),
            Some(password) => {
                let mut line = password.clone();
                line.push(b'\n');
                line
            }
        }
    }
}

/// The terminal's settings while a password is typed with its echo off, to
/// put back if the process is interrupted then.
static ECHOING: Mutex<Option<Termios>> = Mutex::new(None);

fn put_echo_back() {
    if let Some(settings) = ECHOING.lock().expect("not poisoned").take()
        && let Ok(terminal) = File::options().read(true).write(true).open("/dev/tty")
    {
        let _ = termios::tcsetattr(&terminal, OptionalActions::Now, &settings);
    }
}

/// Asks for a password on the terminal, without showing what is typed.
fn ask(prompt: &str, host: &str) -> Result<Vec<u8>> {
    let _aside = crate::status::aside();
    let Ok(mut terminal) = File::options().read(true).write(true).open("/dev/tty") else {
        bail!(
            "sudo on {host} needs a password, and there is no terminal to type it in; set FERRY_SUDO_PASSWORD"
        );
    };
    let settings = termios::tcgetattr(&terminal).context("read the terminal's settings")?;
    let mut quiet = settings.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    *ECHOING.lock().expect("not poisoned") = Some(settings);
    interrupt::on_exit(Some(put_echo_back));
    terminal.write_all(prompt.as_bytes())?;
    termios::tcsetattr(&terminal, OptionalActions::Now, &quiet)
        .context("turn the terminal's echo off")?;
    let mut line = Vec::new();
    let read = BufReader::new(&terminal).read_until(b'\n', &mut line);
    put_echo_back();
    interrupt::on_exit(None);
    let _ = terminal.write_all(b"\n");
    read.context("read the password")?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_goes_first_on_a_line_of_its_own() {
        let free = Sudo { password: None };
        assert!(free.first().is_empty() && !free.asks_password());
        let asked = Sudo {
            password: Some(b"pass word".to_vec()),
        };
        assert!(asked.asks_password());
        assert_eq!(asked.first(), b"pass word\n");
    }
}
