//! Ferry's launcher: what a host's first session becomes once Ferry is
//! there, so that nothing else Ferry runs on the host needs a session of its
//! own, each of which runs the user's login shell and its startup files.
//! See the [draft](../../docs/drafts/one-session.md).
//!
//! It listens on a Unix socket, which the client reaches as it reaches a
//! server's: straight, over a channel SSH forwards to it, or by a joining
//! process, where SSH will not forward to a socket. It says where, and its
//! build, on its standard output, in lines that a run's tag marks among what
//! the host's shell prints there. The first connection the client makes is
//! the one it follows the launcher by: the launcher lives as long as it, and
//! as its first session, whose output the client holds until it is done:
//! should the client be gone before it ever follows the launcher, that output
//! closes, and the launcher ends. It says on the connection it is followed
//! by how each thing it started ended. That connection goes over the stream
//! layer once the client has said what it is for: the launcher ends once
//! nothing came on it for [`SILENCE`](crate::transport::stream::SILENCE), as
//! when its SSH connection dropped without a word, rather than waiting for
//! `sshd` to find that, which may take hours. On each other connection,
//! the client asks it to start one thing, a server, which then has the
//! connection as its input and output: no shell, nor what one left running,
//! can write to it. What it starts does not depend on it once started.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::build_info;
use crate::interrupt;
use crate::protocol::{self, Launch, Launcher, SudoAsks};
use crate::transport::join::Listening;
use crate::transport::stream::{self, Ends, Writing};
use crate::transport::{Joined, start};

/// The kind of the lines the launcher says where it is in.
pub const LINE: &str = "launcher";

/// The most of what something run to its end says on standard error that
/// goes to the client.
const SAID: u64 = 64 << 10;

/// Runs the launcher, its lines marked with `tag`, until the connection the
/// client follows it by closes.
pub fn run(tag: &str) -> Result<()> {
    // Both, until the client's first connection says which it reaches.
    let listening = Listening::open(false, true, true)?;
    let mut output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let socket = listening.socket().expect("listening on a socket");
    let (port, secret) = listening.port().expect("listening on a port");
    let lines = [
        format!("build={}", build_info::identity()).into_bytes(),
        [b"socket=".as_slice(), socket.as_os_str().as_bytes()].concat(),
        format!("port={port}").into_bytes(),
        format!("secret={secret}").into_bytes(),
    ];
    // Each line in a write of its own, as a pipe keeps it whole.
    for line in &lines {
        output.write_all(&start::frame(tag, LINE, line))?;
    }
    output.write_all(&start::frame(
        tag,
        start::DONE,
        lines.len().to_string().as_bytes(),
    ))?;
    // The client holds the other end of this output for as long as it keeps
    // the first session: once that closes, it is gone, or done.
    std::thread::spawn(move || {
        use rustix::event::{PollFd, PollFlags, poll};
        let mut polled = [PollFd::new(&output, PollFlags::empty())];
        loop {
            match poll(&mut polled, None) {
                Ok(_)
                    if polled[0]
                        .revents()
                        .intersects(PollFlags::ERR | PollFlags::HUP) =>
                {
                    interrupt::stop(0)
                }
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => interrupt::stop(0),
            }
        }
    });
    // What the client follows the launcher by, once it has come.
    let following: Arc<Mutex<Option<Writing>>> = Arc::new(Mutex::new(None));
    let mut kept = false;
    loop {
        let (via, joined) = listening.accept()?;
        // The first says which way the client reaches the launcher: the
        // other closes. One by the port gave its secret.
        if !kept {
            listening.keep(via);
            kept = true;
        }
        // Taken in turn, so that the client's first connection, the one it
        // follows the launcher by, is taken before any it opens after it;
        // the client writes what each is for at once. One gone wrong is only
        // that connection's matter, and is closed.
        let _ = take(joined, &following);
    }
}

/// Takes a connection: the one to follow the launcher by, first, then each
/// asking it to start something.
fn take(joined: Joined, following: &Arc<Mutex<Option<Writing>>>) -> Result<()> {
    let Joined {
        input,
        output,
        held,
    } = joined;
    let mut input = File::from(input);
    let output = File::from(output);
    let packet = protocol::receive(&mut input)?.context("a connection closed at once")?;
    match packet.decode::<Launcher>()? {
        Launcher::Follow => {
            let mut follower = following.lock().expect("not poisoned");
            if follower.is_some() {
                bail!("the launcher is followed already");
            }
            // What a joining process handed over is held as long as the
            // launcher is followed by it: the joining process waits for it,
            // and its session, which `sshd` ends with it, closes the input
            // it handed over.
            let (mut reading, writing, _) = stream::start(Ends {
                input,
                output,
                held,
                marked: false,
                before: Vec::new(),
            })?;
            *follower = Some(writing);
            drop(follower);
            std::thread::spawn(move || {
                // Nothing comes on it but keepalives, and its end: then the
                // client is done, or, once nothing came for a while, gone,
                // and so is the launcher. What it started goes on.
                let _ = io::copy(&mut reading, &mut io::sink());
                interrupt::stop(0);
            });
            Ok(())
        }
        Launcher::Launch { id, what } => {
            if following.lock().expect("not poisoned").is_none() {
                bail!("asked to start something before being followed");
            }
            let mut command = command(&what)?;
            // What is run to its end says on standard error why it failed, as
            // `sudo` of a password, for the client to show; a server's own
            // goes where the launcher's does.
            let ran_to_its_end = !matches!(what, Launch::Server { .. });
            command
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(output))
                .stderr(if ran_to_its_end {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                });
            let spawned = command.spawn();
            // Its copies of the connection go: the child's are the only ones.
            drop(command);
            let mut child = match spawned {
                Ok(child) => child,
                Err(error) => {
                    tell(
                        following,
                        &Launcher::NotStarted {
                            id,
                            reason: error.to_string(),
                        },
                    );
                    return Ok(());
                }
            };
            let following = Arc::clone(following);
            std::thread::spawn(move || {
                // What it says, read as it says it, up to a size, so that it
                // never waits for room to say more.
                let mut said = Vec::new();
                if let Some(stderr) = child.stderr.take() {
                    let _ = stderr.take(SAID).read_to_end(&mut said);
                }
                let status = child.wait();
                // What a joining process handed over is held until the child
                // is done with it: the joining process waits for it.
                drop(held);
                let (code, signal) = match status {
                    Ok(status) => (status.code(), status.signal()),
                    Err(_) => (None, None),
                };
                tell(
                    &following,
                    &Launcher::Ended {
                        id,
                        code,
                        signal,
                        said,
                    },
                );
            });
            Ok(())
        }
        other => bail!("expected to be followed, or asked to start something, got {other:?}"),
    }
}

/// Tells the client `message` on the connection it follows the launcher by,
/// should it still be there.
fn tell(following: &Mutex<Option<Writing>>, message: &Launcher) {
    let mut follower = following.lock().expect("not poisoned");
    if let Some(out) = follower.as_mut() {
        let _ = protocol::send(out, message).and_then(|()| Ok(out.flush()?));
    }
}

/// The command that starts `what`: this binary, through `sudo` should it ask
/// to be.
fn command(what: &Launch) -> Result<Command> {
    let program = std::env::current_exe().context("find the ferry binary")?;
    let through_sudo = |asks: &SudoAsks, arguments: &[&str]| {
        let mut command = Command::new("sudo");
        match asks {
            SudoAsks::Nothing => command.args(["-n", "--"]),
            SudoAsks::Password => command.args(["-k", "-S", "-p", "", "--"]),
        };
        command.arg(&program).args(arguments);
        command
    };
    let mut command = match what {
        Launch::Server { sudo: None, port } => {
            let mut command = Command::new(&program);
            command.arg("server");
            if *port {
                command.arg("--port");
            }
            command
        }
        Launch::Server {
            sudo: Some(asks),
            port: false,
        } => through_sudo(asks, &["server", "--sudo"]),
        Launch::Server {
            sudo: Some(asks),
            port: true,
        } => through_sudo(asks, &["server", "--sudo", "--port"]),
        Launch::SudoCheck { asks } => through_sudo(asks, &["build-id"]),
        Launch::Fixture { arguments } => {
            let mut command = Command::new(&program);
            command
                .args(["fixture", "--until-stdin-closes"])
                .args(arguments);
            command
        }
    };
    crate::limits::restore_in_child(&mut command);
    interrupt::unblock_in_child(&mut command);
    Ok(command)
}
