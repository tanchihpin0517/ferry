//! Ferry's launcher on a host, as the client has it: the host's first
//! session, become [`crate::launcher`]. The client follows it on one
//! connection, which it lives as long as, and asks it on each other to start
//! one thing, which then has that connection as its input and output: a
//! server. It says on the connection it is followed by how each ended. That
//! connection goes over the stream layer once the launcher knows what it is
//! for, so that either side takes the other for gone once nothing came on it
//! for [`SILENCE`](super::stream::SILENCE), as a connection that dropped
//! without a word would leave it: the launcher does not wait for `sshd` to
//! find that, which may take hours. See the
//! [draft](../../../docs/drafts/one-session.md).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use super::child::Child;
use super::stream::{self, Writing};
use super::{
    ControlConnection, ENDING, FirstSession, Link, NotForwarded, Program, Reach, Transport,
};
use crate::protocol::{self, Launch, Launcher as Message, SudoAsks};
use crate::remote::shell::quote;

/// The launcher on a host.
pub struct Launcher {
    transport: Arc<Transport>,
    /// Where it runs, as a message names it.
    place: String,
    /// The ways to reach it, in the order tried: its socket, then its port;
    /// or, where what reaches it joins it through the shell, that alone.
    reaches: Vec<Reach>,
    /// The way that reached it first, which the rest take.
    reach: Mutex<Option<Reach>>,
    /// The connection it is followed by: closed, it ends.
    following: Mutex<Option<Following>>,
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
    /// How each ended, and what it said on standard error, or why it could
    /// not be started, by its ID.
    ended: HashMap<u32, std::result::Result<(ExitStatus, Vec<u8>), String>>,
    /// The connection it is followed by ended: it says nothing more.
    gone: bool,
    /// Why that connection was taken for broken, if it was, rather than
    /// ending.
    broke: Option<String>,
}

/// The connection the launcher is followed by, as the client writes it: its
/// stream's writing end, which keeps it alive, and what carries it.
struct Following {
    writing: Writing,
    carrier: Option<Child>,
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
        // An install that read the rest of the session's input became the
        // launcher on its own.
        if !session.becoming_launcher() {
            session.send(&format!(
                "exec {} launcher --tag {}",
                quote(program),
                quote(&tag)
            ))?;
        }
        let lines = session
            .read_part()
            .with_context(|| format!("Ferry's launcher on {place} did not start"))?;
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
        let reaches = match join(&socket) {
            Some(join) => vec![Reach::Join(join)],
            None => {
                let number = value("port")
                    .and_then(|port| port.parse().ok())
                    .context("the launcher did not say its port")?;
                let secret = value("secret").context("the launcher did not say its secret")?;
                vec![Reach::Socket(socket), Reach::Port { number, secret }]
            }
        };
        let launcher = Arc::new(Self {
            transport,
            place,
            reaches,
            reach: Mutex::new(None),
            following: Mutex::new(None),
            session: Mutex::new(Some(session)),
            said: Arc::new(Said::default()),
            next: AtomicU32::new(0),
        });
        launcher.follow()?;
        Ok(launcher)
    }

    /// Opens the connection the launcher is followed by, the first way that
    /// reaches it, which the rest then take, and reads what it says there
    /// until it ends.
    fn follow(&self) -> Result<()> {
        let mut reached = None;
        for reach in &self.reaches {
            match self.transport.connect(reach) {
                Ok(link) => {
                    reached = Some((reach.clone(), link));
                    break;
                }
                Err(error) if error.is::<NotForwarded>() => {}
                Err(error) => return Err(error),
            }
        }
        let Some((reach, mut link)) = reached else {
            bail!(
                "{}: SSH forwarded a channel neither to Ferry's socket there nor to its port, \
                 as where sshd forwards nothing; --remote-join has what reaches it join it \
                 through the shell on the host instead",
                self.place
            );
        };
        *self.reach.lock().expect("not poisoned") = Some(reach);
        protocol::send(&mut link.writer, &Message::Follow)
            .and_then(|()| Ok(link.writer.flush()?))
            .with_context(|| format!("follow the launcher on {}", self.place))?;
        // Once it knows what the connection is for, the launcher goes on it
        // over the stream layer, and so does the client.
        let (ends, carrier) = link.into_ends()?;
        let (mut reading, writing, _) = stream::start(ends)
            .with_context(|| format!("follow the launcher on {}", self.place))?;
        let said = Arc::clone(&self.said);
        std::thread::spawn(move || {
            let broke = loop {
                let packet = match protocol::receive(&mut reading) {
                    Ok(Some(packet)) => packet,
                    Ok(None) => break None,
                    Err(error) => break Some(format!("{error:#}")),
                };
                let (id, ended) = match packet.decode::<Message>() {
                    Ok(Message::Ended {
                        id,
                        code,
                        signal,
                        said,
                    }) => {
                        let raw = match (code, signal) {
                            (Some(code), _) => (code & 0xff) << 8,
                            (None, Some(signal)) => signal & 0x7f,
                            (None, None) => 0xff << 8,
                        };
                        (id, Ok((ExitStatus::from_raw(raw), said)))
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
            };
            let mut state = said.state.lock().expect("not poisoned");
            state.gone = true;
            state.broke = broke;
            drop(state);
            said.changed.notify_all();
        });
        *self.following.lock().expect("not poisoned") = Some(Following { writing, carrier });
        Ok(())
    }

    /// A new connection to the launcher, the way that reached it first.
    fn connect(&self) -> Result<Link> {
        let reach = self.reach.lock().expect("not poisoned").clone();
        let reach = reach.context("the launcher is not followed")?;
        self.transport
            .connect(&reach)
            .with_context(|| format!("reach the launcher on {}", self.place))
    }

    /// Whether the launcher is reached by its port: then so are the servers
    /// it starts.
    fn by_port(&self) -> bool {
        matches!(
            *self.reach.lock().expect("not poisoned"),
            Some(Reach::Port { .. })
        )
    }

    /// Has the launcher start a server, through `sudo` as `sudo` says,
    /// writing `first` to it before anything else, as the password `sudo`
    /// reads; returns its control connection.
    pub fn server(
        self: &Arc<Self>,
        sudo: Option<SudoAsks>,
        first: &[u8],
    ) -> Result<ControlConnection> {
        let port = self.by_port();
        let (link, program) = self.launch(Launch::Server { sudo, port }, first)?;
        Ok(ControlConnection::new(&self.place, program, link))
    }

    /// Has the launcher run `what`, which reads nothing but `first`, until
    /// it ends; returns what it wrote, what it said, and how it ended. Its
    /// input stays open until then, as something that stops once its input
    /// closes would.
    pub fn run(self: &Arc<Self>, what: Launch, first: &[u8]) -> Result<Ran> {
        let (mut link, program) = self.launch(what, first)?;
        let Program::Launched {
            mut carrier,
            launched,
        } = program
        else {
            unreachable!("the launcher starts it");
        };
        let mut output = Vec::new();
        let _ = link.reader.read_to_end(&mut output);
        let ended = launched.ended(&mut carrier);
        link.child = carrier;
        super::end_within(link, ENDING);
        Ok(match ended? {
            Ok((status, said)) => Ran::Ended {
                status,
                output,
                said,
            },
            Err(reason) => Ran::NotStarted(reason),
        })
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
        // It ends as the connection it is followed by does, at its end, and
        // so does the first session it runs in.
        if let Some(following) = self.following.lock().expect("not poisoned").take() {
            let Following { writing, carrier } = following;
            let _ = writing.finish();
            if let Some(carrier) = carrier.as_ref() {
                super::ends_by(carrier, Instant::now() + ENDING);
                carrier.kill();
                carrier.wait();
            }
        }
        if let Some(session) = self.session.lock().expect("not poisoned").take() {
            session.end();
        }
    }
}

/// What came of something the launcher ran to its end.
pub enum Ran {
    /// It ran, wrote `output`, said `said` on standard error, and ended so.
    Ended {
        status: ExitStatus,
        output: Vec<u8>,
        said: Vec<u8>,
    },
    /// It could not be started, for this reason.
    NotStarted(String),
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
        self.ended(carrier)?
            .map(|(status, _)| status)
            .map_err(|reason| anyhow!("it could not be started: {reason}"))
    }

    /// Waits until the launcher says how it ended, or why it could not be
    /// started; as [`Launched::wait`].
    fn ended(
        &self,
        carrier: &mut Option<Child>,
    ) -> Result<std::result::Result<(ExitStatus, Vec<u8>), String>> {
        let said = &self.launcher.said;
        let mut state = said.state.lock().expect("not poisoned");
        loop {
            if let Some(ended) = state.ended.remove(&self.id) {
                return Ok(ended);
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
                .map(|(status, _)| Some(status))
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
        if let Some(carrier) = carrier
            && let Some(status) = carrier.wait_until(Instant::now() + ENDING)
            && status.code() == Some(255)
        {
            return anyhow!("the SSH connection its server ran over dropped");
        }
        let broke = self
            .launcher
            .said
            .state
            .lock()
            .expect("not poisoned")
            .broke
            .clone();
        match broke {
            Some(why) => anyhow!(
                "Ferry's launcher on {} was taken for gone before it said how the server did: {why}",
                self.launcher.place
            ),
            None => anyhow!(
                "Ferry's launcher on {} ended before it said how the server did",
                self.launcher.place
            ),
        }
    }
}
