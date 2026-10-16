//! `--remote-sudo`: a host's side run as root, through `sudo`, so that it can
//! give entries any owner, as rsync's `--rsync-path="sudo rsync"` does.
//!
//! Only the server runs through `sudo`; probing and installing stay the
//! login user's, so Ferry is installed in their home as usual.
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
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rustix::termios::{self, LocalModes, OptionalActions, Termios};

use super::shell;
use crate::interrupt;
use crate::transport::Transport;

/// How many times a password typed in is asked for, as `sudo` does.
const TRIES: usize = 3;

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
    /// Gets ready to run programs through `sudo` on the host `transport`
    /// reaches: finds whether it needs a password, and if so, takes it from
    /// `FERRY_SUDO_PASSWORD`, or asks for it on the terminal, and checks it.
    pub fn prepare(transport: &Transport) -> Result<Self> {
        let host = transport.place();
        let shown = crate::status::show(format!("checking sudo on {host}"));
        let free = transport
            .command("exec sudo -n true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("run sudo")?;
        drop(shown);
        if free.status.success() {
            return Ok(Self { password: None });
        }
        if free.status.code() == Some(127) {
            bail!("{host} has no sudo, which --remote-sudo needs");
        }
        if let Some(password) = std::env::var_os("FERRY_SUDO_PASSWORD") {
            let sudo = Self {
                password: Some(password.into_encoded_bytes()),
            };
            sudo.check(transport)?;
            return Ok(sudo);
        }
        let mut tries = 0;
        loop {
            tries += 1;
            let sudo = Self {
                password: Some(ask(&format!("[sudo] password for {host}: "), host)?),
            };
            match sudo.check(transport) {
                Ok(()) => return Ok(sudo),
                Err(error) if tries == TRIES => return Err(error),
                Err(error) => eprintln!("ferry: {error:#}"),
            }
        }
    }

    /// Checks the password, running nothing but `true` with it.
    fn check(&self, transport: &Transport) -> Result<()> {
        let _shown = crate::status::show(format!("checking the password on {}", transport.place()));
        let mut child = transport
            .command(&self.script("true", &[]))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("run sudo")?;
        // Only the line: a wrong password is then asked for again, and finds
        // nothing more.
        let mut stdin = child.stdin.take().expect("piped");
        let _ = stdin.write_all(&self.first());
        drop(stdin);
        let output = child.wait_with_output().context("run sudo")?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            let said = said
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "sudo on {} did not run with that password: {said}",
                transport.place()
            );
        }
        Ok(())
    }

    /// The script that runs `program` with `arguments` through `sudo`.
    pub fn script(&self, program: &str, arguments: &[&str]) -> String {
        let mut line = match self.password {
            None => "exec sudo -n --".to_owned(),
            Some(_) => "exec sudo -k -S -p '' --".to_owned(),
        };
        for word in std::iter::once(program).chain(arguments.iter().copied()) {
            line.push(' ');
            line.push_str(&shell::quote(word));
        }
        line
    }

    /// What to write to the script first: the password's line, if any.
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
    fn the_script_runs_the_program_through_sudo() {
        let free = Sudo { password: None };
        assert_eq!(
            free.script("/home/me/ferry", &["server", "--join", "ab"]),
            "exec sudo -n -- '/home/me/ferry' 'server' '--join' 'ab'"
        );
        assert!(free.first().is_empty());
        let asked = Sudo {
            password: Some(b"pass word".to_vec()),
        };
        assert_eq!(
            asked.script("/home/my files/ferry", &["server"]),
            "exec sudo -k -S -p '' -- '/home/my files/ferry' 'server'"
        );
        assert_eq!(asked.first(), b"pass word\n");
    }
}
