//! Ferry's launcher on a host, as the client has it: the host's first
//! session, become [`crate::launcher`]. The client follows it on one
//! connection, which it lives as long as, and asks it on each other to start
//! one thing, which then has that connection as its input and output: a
//! server. It says on the connection it is followed by how each ended. See
//! the [draft](../../../docs/drafts/one-session.md).

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::{ControlConnection, ENDING, FirstSession, Link, Program, Transport};
use crate::protocol::{self, Launch, Launcher as Message, SudoAsks};
use crate::remote::shell::quote;

/// The launcher on a host.
pub struct Launcher {
    transport: Arc<Transport>,
    /// Where it runs, as a message names it.
    place: String,
    /// Its socket on the host.
    socket: String,
    /// What a channel runs to join it, where they join through the shell.
    join: Option<String>,
    /// The connection it is followed by: closed, it ends.
    following: Mutex<Option<Link>>,
    /// The first session, which it runs in.
    session: Mutex<Option<FirstSession>>,
    /// How what it started ended, as it said.
    said: Arc<Said>,
    /// The ID of the next thing it starts.
    next: AtomicU32,
}

/// What the launcher said of what it started, and whether it is gone.
#[derive(Default)]
struct Said {
    state: Mutex<SaidState>,
    changed: Condvar,
}

#[derive(Default)]
struct SaidState {
    /// How each ended, or why it could not be started, by its ID.
    ended: HashMap<u32, std::result::Result<ExitStatus, String>>,
    /// The connection it is followed by ended: it says nothing more.
    gone: bool,
}

impl Launcher {
    /// Has the host's first session, `session`, become the launcher, running
    /// `program`, and follows it once it says where it is. Given `join`,
    /// what the client opens to it runs what that makes of its socket.
    pub fn start(
        transport: Arc<Transport>,
        mut session: FirstSession,
        program: &str,
        join: impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<Self>> {
        let place = transport.place().to_owned();
        let tag = session.tag().to_owned();
        session.send(&format!(
            "exec {} launcher --tag {}",
            quote(program),
            quote(&tag)
        ))?;
        let lines = session
            .read_part()
            .with_context(|| format!("start Ferry's launcher on {place}"))?;
        let value = |key: &str| {
            lines.iter().find_map(|line| {
                (line.kind == crate::launcher::LINE)
                    .then(|| line.content.strip_prefix(format!("{key}=").as_bytes()))
                    .flatten()
                    .map(|value| String::from_utf8_lossy(value).into_owned())
            })
        };
        let build = value("build").context("the launcher did not say its build")?;
        let ours = crate::build_info::identity();
        if build != ours {
            bail!("the launcher on {place} is build {build}, but the client is build {ours}");
        }
        let socket = value("socket").context("the launcher did not say where it is")?;
        let launcher = Arc::new(Self {
            transport,
            place,
            join: join(&socket),
            socket,
            following: Mutex::new(None),
            session: Mutex::new(Some(session)),
            said: Arc::new(Said::default()),
            next: AtomicU32::new(0),
        });
        launcher.follow()?;
        Ok(launcher)
    }

    /// Opens the connection the launcher is followed by, and reads what it
    /// says there until it ends.
    fn follow(&self) -> Result<()> {
        let mut link = self.connect()?;
        protocol::send(&mut link.writer, &Message::Follow)
            .and_then(|()| Ok(link.writer.flush()?))
            .with_context(|| format!("follow the launcher on {}", self.place))?;
        let mut reader = std::mem::replace(
            &mut link.reader,
            std::io::BufReader::new(Box::new(std::io::empty())),
        );
        let said = Arc::clone(&self.said);
        std::thread::spawn(move || {
            while let Ok(Some(packet)) = protocol::receive(&mut reader) {
                let (id, ended) = match packet.decode::<Message>() {
                    Ok(Message::Ended { id, code, signal }) => {
                        let raw = match (code, signal) {
                            (Some(code), _) => (code & 0xff) << 8,
                            (None, Some(signal)) => signal & 0x7f,
                            (None, None) => 0xff << 8,
                        };
                        (id, Ok(ExitStatus::from_raw(raw)))
                    }
                    Ok(Message::NotStarted { id, reason }) => (id, Err(reason)),
                    _ => continue,
                };
                said.state
                    .lock()
                    .expect("not poisoned")
                    .ended
                    .insert(id, ended);
                said.changed.notify_all();
            }
            said.state.lock().expect("not poisoned").gone = true;
            said.changed.notify_all();
        });
        *self.following.lock().expect("not poisoned") = Some(link);
        Ok(())
    }

    /// A new connection to the launcher.
    fn connect(&self) -> Result<Link> {
        self.transport.connect(&self.socket, self.join.as_deref())
    }

    /// Has the launcher start a server, through `sudo` as `sudo` says,
    /// writing `first` to it before anything else, as the password `sudo`
    /// reads; returns its control connection.
    pub fn server(
        self: &Arc<Self>,
        sudo: Option<SudoAsks>,
        first: &[u8],
    ) -> Result<ControlConnection> {
        let (link, program) = self.launch(Launch::Server { sudo }, first)?;
        Ok(ControlConnection::new(&self.place, program, link))
    }

    /// Has the launcher start `what`, writing `first` to it before anything
    /// else; returns the connection, its input and output, and what was
    /// started.
    pub(super) fn launch(self: &Arc<Self>, what: Launch, first: &[u8]) -> Result<(Link, Program)> {
        let mut link = self.connect()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        protocol::send(&mut link.writer, &Message::Launch { id, what })
            .and_then(|()| Ok(link.writer.write_all(first)?))
            .and_then(|()| Ok(link.writer.flush()?))
            .with_context(|| format!("ask the launcher on {} to start a server", self.place))?;
        let carrier = link.child.take();
        let launched = Launched {
            id,
            launcher: Arc::clone(self),
        };
        Ok((link, Program::Launched { carrier, launched }))
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        // It ends as the connection it is followed by closes, and so does the
        // first session it runs in.
        if let Some(link) = self.following.lock().expect("not poisoned").take() {
            link.end();
        }
        if let Some(session) = self.session.lock().expect("not poisoned").take() {
            session.end();
        }
    }
}

/// Something the launcher started, as the client waits for it to end.
pub struct Launched {
    id: u32,
    launcher: Arc<Launcher>,
}

impl Launched {
    /// Waits until the launcher says how it ended. Should the launcher be
    /// gone first, as when its SSH connection dropped, `carrier`, what
    /// carries its connection, says why.
    pub(super) fn wait(&self, carrier: &mut Option<Child>) -> Result<ExitStatus> {
        let said = &self.launcher.said;
        let mut state = said.state.lock().expect("not poisoned");
        loop {
            if let Some(ended) = state.ended.remove(&self.id) {
                return ended.map_err(|reason| anyhow!("it could not be started: {reason}"));
            }
            if state.gone {
                drop(state);
                return Err(self.gone(carrier));
            }
            state = said.changed.wait(state).expect("not poisoned");
        }
    }

    /// How it ended, if the launcher has said so.
    pub(super) fn try_wait(&self, carrier: &mut Option<Child>) -> Result<Option<ExitStatus>> {
        let said = &self.launcher.said;
        let mut state = said.state.lock().expect("not poisoned");
        if let Some(ended) = state.ended.remove(&self.id) {
            return ended
                .map(Some)
                .map_err(|reason| anyhow!("it could not be started: {reason}"));
        }
        if state.gone {
            drop(state);
            return Err(self.gone(carrier));
        }
        Ok(None)
    }

    /// Why the launcher went before saying how this ended: its SSH connection
    /// dropped, as `ssh` says, with 255, once it ends too; or else it ended.
    fn gone(&self, carrier: &mut Option<Child>) -> anyhow::Error {
        if let Some(carrier) = carrier {
            let started = Instant::now();
            while started.elapsed() < ENDING {
                if let Ok(Some(status)) = carrier.try_wait() {
                    if status.code() == Some(255) {
                        return anyhow!("the SSH connection its server ran over dropped");
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        anyhow!(
            "Ferry's launcher on {} ended before it said how the server did",
            self.launcher.place
        )
    }
}
