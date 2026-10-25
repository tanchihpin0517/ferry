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
//! by how each thing it started ended. On each other connection,
//! the client asks it to start one thing, a server, which then has the
//! connection as its input and output: no shell, nor what one left running,
//! can write to it. What it starts does not depend on it once started.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::build_info;
use crate::interrupt;
use crate::protocol::{self, Launch, Launcher, SudoAsks};
use crate::transport::{self, Joined, start};

/// The kind of the lines the launcher says where it is in.
pub const LINE: &str = "launcher";

/// Runs the launcher, its lines marked with `tag`, until the connection the
/// client follows it by closes.
pub fn run(tag: &str) -> Result<()> {
    let joining = transport::listen(false)?;
    let mut output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let lines = [
        format!("build={}", build_info::identity()).into_bytes(),
        [
            b"socket=".as_slice(),
            joining.socket().as_os_str().as_bytes(),
        ]
        .concat(),
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
    let following: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    loop {
        let joined = joining.accept()?;
        // Taken in turn, so that the client's first connection, the one it
        // follows the launcher by, is taken before any it opens after it;
        // the client writes what each is for at once. One gone wrong is only
        // that connection's matter, and is closed.
        let _ = take(joined, &following);
    }
}

/// Takes a connection: the one to follow the launcher by, first, then each
/// asking it to start something.
fn take(joined: Joined, following: &Arc<Mutex<Option<File>>>) -> Result<()> {
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
            *follower = Some(output);
            drop(follower);
            std::thread::spawn(move || {
                // Nothing comes on it but its end: then the client is gone,
                // or done, and so is the launcher. What it started goes on.
                let _ = held;
                let _ = io::copy(&mut input, &mut io::sink());
                interrupt::stop(0);
            });
            Ok(())
        }
        Launcher::Launch { id, what } => {
            if following.lock().expect("not poisoned").is_none() {
                bail!("asked to start something before being followed");
            }
            let mut command = command(&what)?;
            command
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(output))
                .stderr(Stdio::inherit());
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
                let status = child.wait();
                // What a joining process handed over is held until the child
                // is done with it: the joining process waits for it.
                drop(held);
                let (code, signal) = match status {
                    Ok(status) => (status.code(), status.signal()),
                    Err(_) => (None, None),
                };
                tell(&following, &Launcher::Ended { id, code, signal });
            });
            Ok(())
        }
        other => bail!("expected to be followed, or asked to start something, got {other:?}"),
    }
}

/// Tells the client `message` on the connection it follows the launcher by,
/// should it still be there.
fn tell(following: &Mutex<Option<File>>, message: &Launcher) {
    let mut follower = following.lock().expect("not poisoned");
    if let Some(out) = follower.as_mut() {
        let _ = protocol::send(out, message).and_then(|()| Ok(out.flush()?));
    }
}

/// The command that starts `what`: this binary, through `sudo` should it ask
/// to be.
fn command(what: &Launch) -> Result<Command> {
    let program = std::env::current_exe().context("find the ferry binary")?;
    let mut command = match what {
        Launch::Server { sudo: None } => {
            let mut command = Command::new(&program);
            command.arg("server");
            command
        }
        Launch::Server { sudo: Some(asks) } => {
            let mut command = Command::new("sudo");
            match asks {
                SudoAsks::Nothing => command.args(["-n", "--"]),
                SudoAsks::Password => command.args(["-k", "-S", "-p", "", "--"]),
            };
            command.arg(&program).args(["server", "--sudo"]);
            command
        }
    };
    crate::limits::restore_in_child(&mut command);
    interrupt::unblock_in_child(&mut command);
    Ok(command)
}
