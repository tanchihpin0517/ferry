//! Ferry's monitor on a host, as the client has it: the host's first
//! session, become [`crate::monitor::host`]. The client follows it on one
//! connection, which it lives as long as, and asks it on each other to start
//! one thing, which then has that connection as its input and output: a
//! server. It says on the connection it is followed by, the link, how each
//! ended. A heartbeat goes on the link each way every
//! [`BEAT`](crate::monitor::BEAT), so that either side takes the other for
//! gone once nothing came on it for [`SILENCE`](crate::monitor::SILENCE), as
//! a connection that dropped without a word would leave it: neither waits
//! for `sshd` to find that, which may take hours. See the
//! [draft](../../../docs/drafts/silent-drops.md).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::child::Child;
use super::wire::{Wire, Writing};
use super::{ControlConnection, FirstSession, NotForwarded, Program, Reach, Transport};
use crate::monitor::client::Node;
use crate::monitor::{Lost, Phase, Timing};
use crate::protocol::{self, Launch, Monitor as Message, SudoAsks};
use crate::remote::shell::quote;

/// The monitor on a host.
pub struct Monitor {
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

/// What the monitor said of what it started, and whether it is gone.
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
    /// The servers it started, as the client's monitor has them, by ID: told
    /// what the monitor says of each.
    workers: HashMap<u32, Node>,
    /// The connection it is followed by ended: it says nothing more.
    gone: bool,
    /// Why that connection was taken for broken, if it was, rather than
    /// ending.
    broke: Option<String>,
    /// When the answer to each `Ping` came, by its ID.
    pongs: HashMap<u32, Instant>,
}

/// The connection the monitor is followed by, as the client writes it: its
/// writing end, shared with what sends the heartbeat, and taken to close it;
/// and what carries it.
struct Following {
    writing: Arc<Mutex<Option<Writing>>>,
    carrier: Option<Child>,
}

impl Monitor {
    /// Has the host's first session, `session`, become the monitor, running
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
        // monitor on its own.
        if !session.becoming_monitor() {
            session.send(&format!(
                "ferry_become {} monitor --tag {}",
                quote(program),
                quote(&tag)
            ))?;
        }
        let lines = session
            .read_part()
            .with_context(|| format!("Ferry's monitor on {place} did not start"))?;
        session.became_monitor();
        // It is to be followed within the silence.
        transport.node().phase(Phase::Linking);
        let value = |key: &str| {
            lines.iter().find_map(|line| {
                (line.kind == crate::monitor::host::LINE)
                    .then(|| line.content.strip_prefix(format!("{key}=").as_bytes()))
                    .flatten()
                    .map(|value| String::from_utf8_lossy(value).into_owned())
            })
        };
        let build = value("build").context("the monitor did not say its build")?;
        let ours = crate::build_info::identity();
        if build != ours {
            bail!("the monitor on {place} is build {build}, but the client is build {ours}");
        }
        let socket = value("socket").context("the monitor did not say where it is")?;
        let reaches = match join(&socket) {
            Some(join) => vec![Reach::Join(join)],
            None => {
                let number = value("port")
                    .and_then(|port| port.parse().ok())
                    .context("the monitor did not say its port")?;
                let secret = value("secret").context("the monitor did not say its secret")?;
                vec![Reach::Socket(socket), Reach::Port { number, secret }]
            }
        };
        let monitor = Arc::new(Self {
            transport,
            place,
            reaches,
            reach: Mutex::new(None),
            following: Mutex::new(None),
            session: Mutex::new(Some(session)),
            said: Arc::new(Said::default()),
            next: AtomicU32::new(0),
        });
        monitor.follow()?;
        Ok(monitor)
    }

    /// The round trip to the host's monitor, over the link, its heartbeat's
    /// way: the least of three, each answered at once; none once the link
    /// is gone. What waits for the answer waits as the link lives, as the
    /// heartbeat says.
    pub fn round_trip(&self) -> Option<Duration> {
        let writing = Arc::clone(
            &self
                .following
                .lock()
                .expect("not poisoned")
                .as_ref()?
                .writing,
        );
        let mut least: Option<Duration> = None;
        for _ in 0..3 {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let sent = Instant::now();
            {
                let mut out = writing.lock().expect("not poisoned");
                let out = out.as_mut()?;
                protocol::send(out, &Message::Ping { id })
                    .and_then(|()| Ok(out.flush()?))
                    .ok()?;
            }
            let mut state = self.said.state.lock().expect("not poisoned");
            let came = loop {
                if let Some(came) = state.pongs.remove(&id) {
                    break came;
                }
                if state.gone {
                    return None;
                }
                state = self.said.changed.wait(state).expect("not poisoned");
            };
            let round_trip = came.saturating_duration_since(sent);
            least = Some(least.map_or(round_trip, |least| least.min(round_trip)));
        }
        least
    }

    /// Opens the connection the monitor is followed by, the first way that
    /// reaches it, which the rest then take, and reads what it says there
    /// until it ends.
    fn follow(&self) -> Result<()> {
        let host = self.transport.node().clone();
        let link = self.transport.link().clone();
        let name = format!("the link to {}", self.place);
        let mut reached = None;
        for reach in &self.reaches {
            match self.transport.connect(reach, &link, &name) {
                Ok(wire) => {
                    reached = Some((reach.clone(), wire));
                    break;
                }
                Err(error) if error.is::<NotForwarded>() => {}
                Err(error) => return Err(error),
            }
        }
        let Some((reach, mut carried)) = reached else {
            bail!(
                "{}: SSH forwarded a channel neither to Ferry's socket there nor to its port, \
                 as where sshd forwards nothing; --remote-join has what reaches it join it \
                 through the shell on the host instead",
                self.place
            );
        };
        *self.reach.lock().expect("not poisoned") = Some(reach);
        protocol::send(&mut carried, &Message::Follow)
            .with_context(|| format!("follow the monitor on {}", self.place))?;
        // Once it knows what the connection is for, the monitor goes on it
        // over the stream layer, and so does the client, with the heartbeat
        // among the rest: it is the host's Liveness from now on, and what
        // telling its monitor to stop goes on.
        let scope = link.scope().clone();
        let carrier = carried.take_carrier();
        let (mut reading, writing, _) = carried
            .stream_in(scope.child()?)
            .with_context(|| format!("follow the monitor on {}", self.place))?;
        let writing = Arc::new(Mutex::new(Some(writing)));
        host.phase(Phase::Linked);
        beat(Timing::wanted(), &writing);
        {
            let writing = Arc::clone(&writing);
            host.on_look(move || {
                let mut out = writing.lock().expect("not poisoned");
                if let Some(out) = out.as_mut() {
                    let _ = protocol::send(out, &Message::Look).and_then(|()| Ok(out.flush()?));
                }
            });
        }
        {
            let writing = Arc::clone(&writing);
            host.on_stop(move || {
                let mut out = writing.lock().expect("not poisoned");
                if let Some(out) = out.as_mut() {
                    let _ = protocol::send(out, &Message::Stop).and_then(|()| Ok(out.flush()?));
                }
            });
        }
        {
            // Closed once what works on the host is done: the monitor there
            // then ends, and so does what carries the link.
            let writing = Arc::clone(&writing);
            link.on_close(move || {
                if let Some(writing) = writing.lock().expect("not poisoned").take() {
                    let _ = writing.finish();
                }
            });
        }
        let said = Arc::clone(&self.said);
        let heard = self.transport.heard();
        std::thread::spawn(move || {
            let broke = loop {
                let packet = match protocol::receive(&mut reading) {
                    Ok(Some(packet)) => packet,
                    Ok(None) => break None,
                    Err(error) => break Some(format!("{error:#}")),
                };
                host.heard();
                if let Some(heard) = &heard {
                    *heard.lock().expect("not poisoned") = Some(Instant::now());
                }
                let worker = |id: u32| {
                    said.state
                        .lock()
                        .expect("not poisoned")
                        .workers
                        .get(&id)
                        .cloned()
                };
                let (id, ended) = match packet.decode::<Message>() {
                    Ok(Message::Paused { id }) => {
                        if let Some(worker) = worker(id) {
                            worker.paused();
                        }
                        continue;
                    }
                    Ok(Message::Resumed { id }) => {
                        if let Some(worker) = worker(id) {
                            worker.resumed();
                        }
                        continue;
                    }
                    Ok(Message::Beat) => {
                        crate::inject::beat("client heard");
                        continue;
                    }
                    Ok(Message::Pong { id }) => {
                        let mut state = said.state.lock().expect("not poisoned");
                        state.pongs.insert(id, Instant::now());
                        said.changed.notify_all();
                        continue;
                    }
                    Ok(Message::Looked { id, outside }) => {
                        if let Some(worker) = worker(id) {
                            worker.looked(outside);
                        }
                        continue;
                    }
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
                if let (Some(worker), Ok((status, _))) = (worker(id), &ended) {
                    worker.ended(*status);
                }
                said.state
                    .lock()
                    .expect("not poisoned")
                    .ended
                    .insert(id, ended);
                said.changed.notify_all();
            };
            // Ended, or broken, the link says no more: its monitor is gone,
            // which the client's monitor may have found first.
            host.lose(Lost::Ended);
            let mut state = said.state.lock().expect("not poisoned");
            state.gone = true;
            state.broke = broke;
            drop(state);
            said.changed.notify_all();
        });
        *self.following.lock().expect("not poisoned") = Some(Following { writing, carrier });
        Ok(())
    }

    /// A new connection to the monitor, the way that reached it first, what
    /// carries it a scope of `under`'s, `name`d.
    fn connect(&self, under: &Node, name: &str) -> Result<Wire> {
        let reach = self.reach.lock().expect("not poisoned").clone();
        let reach = reach.context("the monitor is not followed")?;
        self.transport
            .connect(&reach, under, name)
            .with_context(|| format!("reach the monitor on {}", self.place))
    }

    /// Whether the monitor is reached by its port: then so are the servers
    /// it starts.
    fn by_port(&self) -> bool {
        matches!(
            *self.reach.lock().expect("not poisoned"),
            Some(Reach::Port { .. })
        )
    }

    /// Has the monitor start a server, through `sudo` as `sudo` says,
    /// writing `first` to it before anything else, as the password `sudo`
    /// reads; returns its control connection. The server is `worker`, as
    /// the client's monitor has it, told what the monitor says of it.
    pub fn server(
        self: &Arc<Self>,
        sudo: Option<SudoAsks>,
        first: &[u8],
        worker: &Node,
    ) -> Result<ControlConnection> {
        let port = self.by_port();
        let (wire, program) = self.launch(Launch::Server { sudo, port }, first, Some(worker))?;
        Ok(ControlConnection::new(&self.place, program, wire, worker))
    }

    /// Has the monitor run `what`, which reads nothing but `first`, until
    /// it ends; returns what it wrote, what it said, and how it ended. Its
    /// input stays open until then, as something that stops once its input
    /// closes would.
    pub fn run(self: &Arc<Self>, what: Launch, first: &[u8]) -> Result<Ran> {
        let (mut wire, program) = self.launch(what, first, None)?;
        let Program::Launched {
            mut carrier,
            launched,
        } = program
        else {
            unreachable!("the monitor starts it");
        };
        let mut output = Vec::new();
        let _ = wire.read_to_end(&mut output);
        let ended = launched.ended(&mut carrier);
        // What carries its connection ends as the connection has: waited
        // for, unless the host is lost meanwhile.
        if let Some(carrier) = &carrier {
            let _ = carrier.wait_in(self.transport.link().scope(), None);
        }
        wire.end();
        if let Some(carrier) = carrier {
            carrier.kill();
            carrier.wait();
        }
        Ok(match ended? {
            Ok((status, said)) => Ran::Ended {
                status,
                output,
                said,
            },
            Err(reason) => Ran::NotStarted(reason),
        })
    }

    /// Has the monitor start `what`, writing `first` to it before anything
    /// else; returns the connection, its input and output, and what was
    /// started. What it starts is `worker`, if given, as the client's
    /// monitor has it, and what carries its connection a scope of it; else
    /// that is of the link's.
    pub(super) fn launch(
        self: &Arc<Self>,
        what: Launch,
        first: &[u8],
        worker: Option<&Node>,
    ) -> Result<(Wire, Program)> {
        let under = worker.unwrap_or(self.transport.link());
        let name = format!("the channel to {}", self.place);
        let mut wire = self.connect(under, &name)?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if let Some(worker) = worker {
            self.said
                .state
                .lock()
                .expect("not poisoned")
                .workers
                .insert(id, worker.clone());
        }
        protocol::send(&mut wire, &Message::Launch { id, what })
            .and_then(|()| Ok(wire.write_all(first)?))
            .with_context(|| format!("ask the monitor on {} to start a server", self.place))?;
        let carrier = wire.take_carrier();
        let launched = Launched {
            id,
            monitor: Arc::clone(self),
        };
        Ok((wire, Program::Launched { carrier, launched }))
    }
}

impl Drop for Monitor {
    /// The link closed, and what carries it, and the first session the
    /// monitor runs in, ended, should they not be already: the client's
    /// monitor closes the link in order once all on the host is done, and
    /// waits for them to end, so that this is only for what ends without it,
    /// as on a failure, or `ferry test`'s end.
    fn drop(&mut self) {
        if let Some(following) = self.following.lock().expect("not poisoned").take() {
            let Following { writing, carrier } = following;
            if let Some(writing) = writing.lock().expect("not poisoned").take() {
                let _ = writing.finish();
            }
            if let Some(carrier) = carrier.as_ref() {
                carrier.kill();
                carrier.wait();
            }
        }
        drop(self.session.lock().expect("not poisoned").take());
    }
}

/// Sends the monitor a heartbeat on the link every beat, in a thread of its
/// own, until the link is closed, or broken.
fn beat(timing: Timing, writing: &Arc<Mutex<Option<Writing>>>) {
    let writing = Arc::clone(writing);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(timing.beat);
            let mut out = writing.lock().expect("not poisoned");
            let Some(out) = out.as_mut() else {
                return;
            };
            crate::inject::beat("client sent");
            if protocol::send(out, &Message::Beat)
                .and_then(|()| Ok(out.flush()?))
                .is_err()
            {
                return;
            }
        }
    });
}

/// What came of something the monitor ran to its end.
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

/// Something the monitor started, as the client waits for it to end.
pub struct Launched {
    id: u32,
    monitor: Arc<Monitor>,
}

impl Launched {
    /// Waits until the monitor says how it ended. Should the monitor be
    /// gone first, as when its SSH connection dropped, `carrier`, what
    /// carries its connection, says why.
    pub(super) fn wait(&self, carrier: &mut Option<Child>) -> Result<ExitStatus> {
        self.ended(carrier)?
            .map(|(status, _)| status)
            .map_err(|reason| anyhow!("it could not be started: {reason}"))
    }

    /// Waits until the monitor says how it ended, or why it could not be
    /// started; as [`Launched::wait`].
    fn ended(
        &self,
        carrier: &mut Option<Child>,
    ) -> Result<std::result::Result<(ExitStatus, Vec<u8>), String>> {
        let said = &self.monitor.said;
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

    /// Why the monitor went before saying how this ended: its SSH connection
    /// dropped, as `ssh` says, with 255, once it ends too, as it does once
    /// the client's monitor takes the host for lost; or else it ended.
    fn gone(&self, carrier: &mut Option<Child>) -> anyhow::Error {
        if let Some(carrier) = carrier
            && let Ok(Some(status)) = carrier.wait_in(self.monitor.transport.link().scope(), None)
            && status.code() == Some(255)
        {
            return anyhow!("the SSH connection its server ran over dropped");
        }
        let broke = self
            .monitor
            .said
            .state
            .lock()
            .expect("not poisoned")
            .broke
            .clone();
        match broke {
            Some(why) => anyhow!(
                "Ferry's monitor on {} was taken for gone before it said how the server did: {why}",
                self.monitor.place
            ),
            None => anyhow!(
                "Ferry's monitor on {} ended before it said how the server did",
                self.monitor.place
            ),
        }
    }
}
