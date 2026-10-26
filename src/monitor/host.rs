//! Ferry's monitor on a host: what a host's first session becomes once Ferry
//! is there, so that nothing else Ferry runs on the host needs a session of
//! its own, each of which runs the user's login shell and its startup files;
//! and the top of what Ferry runs there, which it starts, watches, and ends.
//! See the [drafts](../../../docs/drafts/silent-drops.md), and
//! [one-session.md](../../../docs/drafts/one-session.md).
//!
//! It listens on a Unix socket, which the client reaches as it reaches a
//! server's: straight, over a channel SSH forwards to it, or by a joining
//! process, where SSH will not forward to a socket. It says where, and its
//! build, on its standard output, in lines that a run's tag marks among what
//! the host's shell prints there. The first connection the client makes is
//! the one it follows the monitor by, within the silence: the link between
//! the client's monitor and this one, for their health, as a heartbeat each
//! way, and for control, as the client saying to stop, and this one saying
//! how each thing it started ended, and when one was paused. On each other
//! connection, the client asks it to start one thing, a worker, which then
//! has the connection as its input and output: no shell, nor what one left
//! running, can write to it; and its standard error as its lifeline, which
//! breaks once the monitor is gone, however it went.
//!
//! What to do is the core's to say; see [`crate::monitor`]. Once the link is
//! lost, as when nothing came on it for the silence, or it ended, or the
//! client was gone before it ever followed, which closes the first session's
//! output, the monitor sends what it started SIGTERM, gives it the grace,
//! kills what is left, and ends.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use super::{Action, Core, Lost, New, Note, Phase, ROOT, Role, ScopeId, Signal, Timing};
use crate::build_info;
use crate::interrupt;
use crate::protocol::{self, Launch, Monitor, SudoAsks};
use crate::transport::child::{self, Child, Handle};
use crate::transport::join::{Listening, Silent};
use crate::transport::lifeline;
use crate::transport::scope::Scope;
use crate::transport::stream::{self, Ends, Writing};
use crate::transport::{Joined, start};

/// The kind of the lines the monitor says where it is in.
pub const LINE: &str = "monitor";

/// The most of what something run to its end says on standard error that
/// goes to the client.
const SAID: usize = 64 << 10;

/// What the monitor's core hears of, from the threads that watch.
enum Event {
    /// The client follows the monitor: the link is there.
    Followed,
    /// Something came on the link.
    Heard,
    /// The client said to stop.
    Stop,
    /// The link is lost.
    Lost(Lost),
    /// Something was started, as `id`.
    Launched { id: u32, child: Handle },
    /// The client's first connection went wrong, before it followed.
    Failed(anyhow::Error),
}

/// What the threads share: the link's writing end, once it came, and the
/// scope everything waits in, which ends as the link is lost.
struct Shared {
    following: Mutex<Option<Writing>>,
    scope: Scope,
}

/// Runs the monitor, its lines marked with `tag`, until it ends, as the
/// link is lost.
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
    let timing = Timing::wanted();
    let (events_in, events) = crossbeam_channel::unbounded();
    let (children_in, children) = crossbeam_channel::unbounded();
    let shared = Arc::new(Shared {
        following: Mutex::new(None),
        scope: Scope::new()?,
    });
    // The client holds the other end of this output for as long as it keeps
    // the first session: once that closes, it is gone, or done.
    {
        let events_in = events_in.clone();
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
                        break;
                    }
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => break,
                }
            }
            let _ = events_in.send(Event::Lost(Lost::Ended));
        });
    }
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || accept(&listening, &shared, &events_in, &children_in));
    }
    drive(timing, &events, &children, &shared)
}

/// Waits for connections until the link is lost, and takes each in turn.
fn accept(
    listening: &Listening,
    shared: &Arc<Shared>,
    events: &Sender<Event>,
    children: &child::Told,
) {
    let mut kept = false;
    loop {
        // Until the client follows the monitor, its first connection, which
        // it follows the monitor by, going wrong, or saying nothing, ends
        // the monitor. After, one gone wrong is only that connection's
        // matter, and is closed, as is one on which nothing came, or whose
        // first packet does not come: neither holds up the rest, and the link
        // says whether the client is there.
        let followed = shared.following.lock().expect("not poisoned").is_some();
        let (via, joined) = match listening.accept(&shared.scope) {
            Ok(accepted) => accepted,
            Err(error) if followed && error.is::<Silent>() => continue,
            Err(error) if !followed => {
                let error = error.context("the client did not follow the monitor");
                let _ = events.send(Event::Failed(error));
                return;
            }
            // The link is lost: nothing more is taken.
            Err(_) => return,
        };
        // The first says which way the client reaches the monitor: the
        // other closes. One by the port gave its secret.
        if !kept {
            listening.keep(via);
            kept = true;
        }
        // Taken in turn, so that the client's first connection, the one it
        // follows the monitor by, is taken before any it opens after it; the
        // client writes what each is for at once.
        if let Err(error) = take(joined, shared, events, children)
            && !followed
        {
            let error = error.context("the client did not follow the monitor");
            let _ = events.send(Event::Failed(error));
            return;
        }
    }
}

/// Takes a connection: the link, first, then each asking it to start
/// something.
fn take(
    joined: Joined,
    shared: &Arc<Shared>,
    events: &Sender<Event>,
    children: &child::Told,
) -> Result<()> {
    let Joined {
        input,
        output,
        held,
    } = joined;
    let input = File::from(input);
    let output = File::from(output);
    let packet =
        protocol::receive(&mut Within::silence(&input))?.context("a connection closed at once")?;
    match packet.decode::<Monitor>()? {
        Monitor::Follow => {
            let mut follower = shared.following.lock().expect("not poisoned");
            if follower.is_some() {
                bail!("the monitor is followed already");
            }
            // What a joining process handed over is held as long as the
            // monitor is followed by it: the joining process waits for it,
            // and its session, which `sshd` ends with it, closes the input
            // it handed over. The heartbeat goes on it, among the rest.
            let (mut reading, writing, _) = stream::start_plain(
                Ends {
                    input,
                    output,
                    held,
                    marked: false,
                    before: Vec::new(),
                },
                shared.scope.clone(),
            )?;
            *follower = Some(writing);
            drop(follower);
            let _ = events.send(Event::Followed);
            let events = events.clone();
            std::thread::spawn(move || {
                // Whatever comes is word from the client; it ends as the
                // client is done, or as the link breaks.
                loop {
                    let packet = match protocol::receive(&mut reading) {
                        Ok(Some(packet)) => packet,
                        Ok(None) | Err(_) => break,
                    };
                    let event = match packet.decode::<Monitor>() {
                        Ok(Monitor::Stop) => Event::Stop,
                        _ => Event::Heard,
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
                let _ = events.send(Event::Lost(Lost::Ended));
            });
            Ok(())
        }
        Monitor::Launch { id, what } => {
            if shared.following.lock().expect("not poisoned").is_none() {
                bail!("asked to start something before being followed");
            }
            let mut command = command(&what)?;
            // What is run to its end says on standard error why it failed, as
            // `sudo` of a password, for the client to show; a server's own
            // goes where the monitor's does. Either way, its standard error
            // is its lifeline: once the monitor is gone, however it went, a
            // server sees it, and stops.
            let ran_to_its_end = !matches!(what, Launch::Server { .. });
            let spawned = lifeline::pair().and_then(|(lifeline, stderr)| {
                command
                    .stdin(Stdio::from(input))
                    .stdout(Stdio::from(output))
                    .stderr(stderr);
                Ok((
                    Child::spawn(&mut command, Some(children.clone()))?,
                    lifeline,
                ))
            });
            // Its copies of the connection go: the child's are the only ones.
            drop(command);
            let (child, lifeline) = match spawned {
                Ok(spawned) => spawned,
                Err(error) => {
                    tell(
                        shared,
                        &Monitor::NotStarted {
                            id,
                            reason: error.to_string(),
                        },
                    );
                    return Ok(());
                }
            };
            let _ = events.send(Event::Launched {
                id,
                child: child.handle(),
            });
            let shared = Arc::clone(shared);
            std::thread::spawn(move || {
                // What it says, read as it says it, all of it, so that it
                // never waits for room to say more: kept up to a size for
                // the client, or passed on.
                let mut said = Vec::new();
                lifeline.read(|part| {
                    if ran_to_its_end {
                        let room = SAID.saturating_sub(said.len());
                        said.extend_from_slice(&part[..part.len().min(room)]);
                    } else {
                        let _ = io::stderr().write_all(part);
                    }
                });
                let status = child.wait();
                // What a joining process handed over is held until the child
                // is done with it: the joining process waits for it.
                drop(held);
                let (code, signal) = (status.code(), status.signal());
                tell(
                    &shared,
                    &Monitor::Ended {
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

/// The monitor's core, driven by what the threads hear, and by the time:
/// what it says to do is done here, until it says the monitor is over.
fn drive(
    timing: Timing,
    events: &Receiver<Event>,
    children: &Receiver<(u32, child::Event)>,
    shared: &Arc<Shared>,
) -> Result<()> {
    let mut core = Core::host(timing, Instant::now());
    // What was started, by scope and by process ID.
    let mut launched: HashMap<ScopeId, (u32, Handle)> = HashMap::new();
    let mut scopes: HashMap<u32, ScopeId> = HashMap::new();
    let mut beating = false;
    loop {
        let timeout = core.next().map_or(Duration::from_secs(3600), |next| {
            next.saturating_duration_since(Instant::now())
        });
        crossbeam_channel::select! {
            recv(events) -> event => {
                let now = Instant::now();
                match event {
                    Ok(Event::Followed) => {
                        core.phase(ROOT, Phase::Linked, now);
                        if !beating {
                            beating = true;
                            beat(timing, shared);
                        }
                    }
                    Ok(Event::Heard) => core.heard(ROOT, now),
                    Ok(Event::Stop) => {
                        core.heard(ROOT, now);
                        core.stop(now);
                    }
                    Ok(Event::Lost(how)) => core.lose(ROOT, how, now),
                    Ok(Event::Launched { id, child }) => {
                        let pid = child.id();
                        let scope = core.open(
                            ROOT,
                            New::Process {
                                pid,
                                role: Role::Worker,
                            },
                            now,
                        );
                        scopes.insert(pid, scope);
                        launched.insert(scope, (id, child));
                    }
                    Ok(Event::Failed(error)) => return Err(error),
                    Err(_) => core.lose(ROOT, Lost::Ended, now),
                }
            },
            recv(children) -> told => {
                let now = Instant::now();
                if let Ok((pid, event)) = told
                    && let Some(&scope) = scopes.get(&pid)
                {
                    match event {
                        child::Event::Paused => core.paused(scope),
                        child::Event::Resumed => core.resumed(scope),
                        child::Event::Exited(_) => core.exited(scope, now),
                    }
                }
            },
            default(timeout) => {},
        }
        core.tick(Instant::now());
        let id = |scope: &ScopeId| launched.get(scope).map(|(id, _)| *id);
        for action in core.take() {
            match action {
                Action::Signal(scope, signal) => {
                    if let Some((_, child)) = launched.get(&scope) {
                        child.signal(match signal {
                            Signal::Term => child::Signal::Term,
                            Signal::Kill => child::Signal::Kill,
                        });
                    }
                }
                // What waits for the link, or for connections, stops.
                Action::End(ROOT, _) => shared.scope.end("the link to the client was lost"),
                Action::Note(Note::Paused(scope)) => {
                    if let Some(id) = id(&scope) {
                        tell(shared, &Monitor::Paused { id });
                    }
                }
                Action::Note(Note::Resumed(scope)) => {
                    if let Some(id) = id(&scope) {
                        tell(shared, &Monitor::Resumed { id });
                    }
                }
                Action::Note(Note::Awaiting(scopes)) => {
                    let ids = scopes.iter().filter_map(id).collect();
                    tell(shared, &Monitor::Awaiting { ids });
                }
                // The socket it listened on goes, as on a signal.
                Action::Over(_) => interrupt::stop(0),
                Action::End(..)
                | Action::Close(_)
                | Action::SendStop(_)
                | Action::Note(Note::Stopping { .. } | Note::Abandoned(_)) => {}
            }
        }
    }
}

/// Sends the client a heartbeat on the link every beat, in a thread of its
/// own, until the link is lost.
fn beat(timing: Timing, shared: &Arc<Shared>) {
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        while shared.scope.ended().is_none() {
            tell(&shared, &Monitor::Beat);
            std::thread::sleep(timing.beat);
        }
    });
}

/// Tells the client `message` on the link, should it still be there.
fn tell(shared: &Shared, message: &Monitor) {
    let mut follower = shared.following.lock().expect("not poisoned");
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
            command.args(["server", "--lifeline"]);
            if *port {
                command.arg("--port");
            }
            command
        }
        Launch::Server {
            sudo: Some(asks),
            port: false,
        } => through_sudo(asks, &["server", "--sudo", "--lifeline"]),
        Launch::Server {
            sudo: Some(asks),
            port: true,
        } => through_sudo(asks, &["server", "--sudo", "--lifeline", "--port"]),
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

/// A connection read until a deadline, the silence from when it is made:
/// what has not come by then fails with [`Silent`].
struct Within<'a> {
    file: &'a File,
    silence: Duration,
    deadline: Instant,
}

impl<'a> Within<'a> {
    fn silence(file: &'a File) -> Self {
        let silence = Timing::wanted().silence;
        Self {
            file,
            silence,
            deadline: Instant::now() + silence,
        }
    }
}

impl Read for Within<'_> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    Silent(self.silence),
                ));
            }
            let timeout = Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            };
            match poll(&mut [PollFd::new(self.file, PollFlags::IN)], Some(&timeout)) {
                Ok(0) | Err(rustix::io::Errno::INTR) => {}
                Ok(_) => return (&mut &*self.file).read(into),
                Err(error) => return Err(error.into()),
            }
        }
    }
}
