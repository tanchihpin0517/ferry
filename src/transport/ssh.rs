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

/// What your own SSH configuration sets for a host, as `ssh -G` reads it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Configured {
    /// A ControlMaster: then Ferry shares it, rather than starting its own.
    pub yours: bool,
    /// A `ConnectTimeout` of your own, which Ferry keeps.
    pub connect_timeout: bool,
}

/// Options for every `ssh` Ferry runs, whatever the configuration says:
/// Ferry's sessions run their own command, and need no forwarding, which a
/// `RemoteCommand` would refuse, and a forward would bind again for each of
/// Ferry's masters, and hold for as long as they last.
const SESSION_OPTIONS: [&str; 4] = [
    "RemoteCommand=none",
    "ClearAllForwardings=yes",
    "PermitLocalCommand=no",
    "ForwardX11=no",
];

impl Ssh {
    /// The `index`th SSH connection to `host`, counting from 0. The first
    /// shares your own ControlMaster, if you have one; every other one, and
    /// the first without yours, has a ControlMaster of Ferry's own, so that
    /// each is its own TCP connection. A login gives up after 10 s, unless
    /// your configuration says otherwise.
    pub fn new(host: &str, index: usize, configured: Configured) -> Result<Self> {
        let mut options: Vec<String> = SESSION_OPTIONS
            .iter()
            .flat_map(|option| ["-o".to_owned(), (*option).to_owned()])
            .collect();
        if !configured.connect_timeout {
            options.extend(["-o".to_owned(), "ConnectTimeout=10".to_owned()]);
        }
        if !(index == 0 && configured.yours) {
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
    /// set, names the program to run instead of `ssh`, with options of its
    /// own, as rsync's `RSYNC_RSH` does; it is given `ssh`'s arguments after
    /// them.
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

/// `ssh`, or the command `FERRY_SSH` names instead, as rsync's `RSYNC_RSH`:
/// a program and its options, split as a shell would at spaces, but for
/// those in quotes. It is given `ssh`'s arguments after them.
fn program() -> Command {
    let words = std::env::var("FERRY_SSH")
        .ok()
        .and_then(|value| words(&value))
        .filter(|words| !words.is_empty())
        .unwrap_or_else(|| vec!["ssh".to_owned()]);
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]);
    crate::limits::restore_in_child(&mut command);
    command
}

/// The program run instead of `ssh`, as an error says it could not be run.
pub fn program_name() -> String {
    match std::env::var("FERRY_SSH") {
        Ok(value) if !value.trim().is_empty() => format!("{value} (FERRY_SSH)"),
        _ => "ssh".to_owned(),
    }
}

/// `value` split into words at spaces and tabs, as a shell splits a command,
/// a word in single or double quotes kept whole, without them; `None` if a
/// quote is not closed. Nothing else is special: no escapes, no variables.
fn words(value: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    for character in value.chars() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some(_), _) => word.get_or_insert_default().push(character),
            (None, '\'' | '"') => {
                quote = Some(character);
                word.get_or_insert_default();
            }
            (None, ' ' | '\t') => words.extend(word.take()),
            (None, _) => word.get_or_insert_default().push(character),
        }
    }
    if quote.is_some() {
        return None;
    }
    words.extend(word);
    Some(words)
}

/// What your own SSH configuration sets for `host`, as `ssh -G` reads it;
/// nothing, where it cannot be read.
pub fn configured(host: &str) -> Configured {
    let output = program()
        .args(["-G", "--", host])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return Configured::default();
    };
    if !output.status.success() {
        return Configured::default();
    }
    let options = String::from_utf8_lossy(&output.stdout);
    let option = |name: &str| {
        options.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == name).then(|| value.trim().to_owned())
        })
    };
    Configured {
        yours: option("controlmaster").is_some_and(|value| value != "false" && value != "no")
            && option("controlpath").is_some_and(|value| value != "none"),
        connect_timeout: option("connecttimeout").is_some_and(|value| value != "none"),
    }
}

/// Options for a ControlMaster of Ferry's own, for the `index`th SSH
/// connection: the first `ssh` becomes the master and logs in, and later ones
/// reuse it, so a password or OTP is typed once. The master outlives the
/// command by ten minutes. It ends once the host has not answered for about a
/// minute, and every session on it with it, so that nothing done before the
/// heartbeats start, such as an upload, waits on a connection that is gone
/// until TCP gives up, for hours.
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
        "ServerAliveInterval=15",
        "ServerAliveCountMax=4",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Ferry's own master gives up on a host that stopped answering; yours
    /// is left as you set it.
    #[test]
    fn only_ferrys_own_master_gives_up_on_a_silent_host() {
        let has = |ssh: &Ssh, option: &str| ssh.options.iter().any(|given| given == option);
        let own = Ssh::new("host", 1, Configured::default()).unwrap();
        assert!(has(&own, "ServerAliveInterval=15"));
        assert!(has(&own, "ServerAliveCountMax=4"));
        let yours = Ssh::new(
            "host",
            0,
            Configured {
                yours: true,
                ..Configured::default()
            },
        )
        .unwrap();
        assert!(
            !yours
                .options
                .iter()
                .any(|given| given.starts_with("ServerAlive"))
        );
    }

    /// Every `ssh` Ferry runs, with your master or its own, neither runs your
    /// `RemoteCommand` nor sets up your forwards, and says so before the
    /// host, where `ssh` reads its options.
    #[test]
    fn no_session_runs_a_remote_command_or_forwards() {
        for (index, yours) in [(0, true), (0, false), (1, false)] {
            let configured = Configured {
                yours,
                ..Configured::default()
            };
            let ssh = Ssh::new("h", index, configured).unwrap();
            for command in [
                ssh.script("true"),
                ssh.forward("/s"),
                ssh.hold(),
                ssh.check(),
            ] {
                let arguments: Vec<String> = command
                    .get_args()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect();
                let end = arguments
                    .iter()
                    .position(|argument| argument == "--")
                    .unwrap();
                for option in ["RemoteCommand=none", "ClearAllForwardings=yes"] {
                    let at = arguments.iter().position(|argument| argument == option);
                    assert!(at.is_some_and(|at| at < end), "{option} in {arguments:?}");
                }
            }
        }
    }

    /// Your own `ConnectTimeout` is kept; without one, a login gives up
    /// after 10 s.
    #[test]
    fn your_connect_timeout_is_kept() {
        let has = |ssh: &Ssh| ssh.options.iter().any(|given| given == "ConnectTimeout=10");
        assert!(has(&Ssh::new("h", 1, Configured::default()).unwrap()));
        let configured = Configured {
            connect_timeout: true,
            ..Configured::default()
        };
        assert!(!has(&Ssh::new("h", 1, configured).unwrap()));
    }

    /// With the options Ferry gives, `ssh` itself reads no `RemoteCommand`
    /// and no forward from a configuration that sets them; where there is
    /// an `ssh` to ask.
    #[test]
    fn ssh_itself_takes_the_options_over_the_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        fs::write(
            &config,
            "Host *\n  RemoteCommand tmux new -A\n  LocalForward 127.0.0.1:65001 127.0.0.1:22\n",
        )
        .unwrap();
        let mut arguments = vec!["-F".to_owned(), config.to_string_lossy().into_owned()];
        for option in SESSION_OPTIONS {
            arguments.extend(["-o".to_owned(), option.to_owned()]);
        }
        arguments.extend(["-G".to_owned(), "nohost.invalid".to_owned()]);
        let Ok(output) = Command::new("ssh").args(&arguments).output() else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let read = String::from_utf8_lossy(&output.stdout).to_lowercase();
        assert!(
            !read.lines().any(|line| line.starts_with("remotecommand ")),
            "{read}"
        );
        assert!(
            read.lines().any(|line| line == "clearallforwardings yes"),
            "{read}"
        );
    }

    /// `FERRY_SSH` is split into words as a shell would, quotes keeping a
    /// word whole; an unclosed quote is no command at all.
    #[test]
    fn a_command_is_split_into_words() {
        assert_eq!(words("ssh -p 2222").unwrap(), ["ssh", "-p", "2222"]);
        assert_eq!(
            words("  '/my tools/ssh'\t-i \"a key\" ").unwrap(),
            ["/my tools/ssh", "-i", "a key"]
        );
        assert_eq!(words("ssh -o ''").unwrap(), ["ssh", "-o", ""]);
        assert!(words("").unwrap().is_empty());
        assert!(words("ssh 'open").is_none());
    }
}
