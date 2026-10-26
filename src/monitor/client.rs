//! The client's monitor: the top of what the client runs, as the host's
//! monitor is of what runs there. It holds a scope for the transfer, each
//! host, the link to each host's monitor, each server as a worker, and each
//! process the client started; what happens to them comes to it as events,
//! from the threads that watch, and from the user, and it does what the
//! core says. See [`crate::monitor`], and the
//! [draft](../../../docs/drafts/silent-drops.md).
//!
//! What uses it holds a [`Node`] for each scope it opens: through it, it
//! says what happened, and its waits wait in the node's scope, which ends
//! as the core says. One thread drives the core, in turn with time, so that
//! a host whose Liveness says it is lost is found so whatever else waits.

use std::collections::{HashMap, VecDeque};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use super::look::{self, Outside};
use super::{
    Action, Cause, Core, Lost, New, Note, Phase, ROOT, Reason, Role, ScopeId, Signal, Timing,
};
use crate::report::Report;
use crate::transport::child::{self, Child, Handle};
use crate::transport::scope::Scope;

/// Names a scope between what uses the monitor and the thread that drives
/// it.
type Key = u64;

/// What the thread that drives the core hears.
enum Event {
    Open {
        key: Key,
        parent: Key,
        new: New,
        name: String,
        scope: Scope,
        handle: Option<Handle>,
    },
    Phase(Key, Phase),
    Heard(Key),
    Lose(Key, Lost),
    Done(Key),
    Failed(Key, String),
    Ended(Key, ExitStatus),
    Paused(Key),
    Resumed(Key),
    OnClose(Key, Arc<dyn Fn() + Send + Sync>),
    OnStop(Key, Arc<dyn Fn() + Send + Sync>),
    OnLook(Key, Arc<dyn Fn() + Send + Sync>),
    Look(Key),
    Looked(Key, Outside),
    Interrupt,
    Terminate,
    Finish,
    Fail(String),
    Show(Sender<Report>),
}

/// A scope of the client's monitor, as what uses it has it: clones are the
/// same scope.
#[derive(Clone)]
pub struct Node {
    key: Key,
    scope: Scope,
    shared: Arc<Shared>,
}

/// What every node of one monitor shares.
struct Shared {
    events: Sender<Event>,
    children: child::Told,
    next: AtomicU64,
    /// Ended once the user asks to stop: what waits for a person, as a
    /// question asked, waits in it.
    stopping: Scope,
}

/// The client's monitor, as the client holds it.
pub struct Client {
    root: Node,
    over: Receiver<Over>,
}

/// How the transfer ended, as the monitor saw it: why, and how each worker
/// ended, by name.
#[derive(Debug, Clone)]
pub struct Over {
    pub reason: Reason,
    /// What a cause names, by its scope: a worker, or a host.
    pub names: HashMap<ScopeId, String>,
    /// How each worker ended, by its scope, where that is known.
    pub statuses: HashMap<ScopeId, ExitStatus>,
}

impl Client {
    /// Starts the client's monitor, its root a transfer, and the thread
    /// that drives it.
    pub fn start(timing: Timing) -> std::io::Result<Self> {
        let (events_in, events) = crossbeam_channel::unbounded();
        let (children_in, children) = crossbeam_channel::unbounded();
        let (over_in, over) = crossbeam_channel::bounded(1);
        let scope = Scope::new()?;
        let stopping = Scope::new()?;
        let shared = Arc::new(Shared {
            events: events_in.clone(),
            children: children_in,
            next: AtomicU64::new(1),
            stopping: stopping.clone(),
        });
        let root = Node {
            key: 0,
            scope: scope.clone(),
            shared,
        };
        std::thread::Builder::new()
            .name("monitor".to_owned())
            .spawn(move || {
                Driver::new(timing, scope, stopping).run(&events, &children, &over_in);
            })?;
        Ok(Self { root, over })
    }

    /// The transfer's own node, the root.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Has the process's Ctrl-C, SIGTERM and SIGHUP come to the monitor,
    /// rather than end the process, until it is over: a first stops each
    /// side, and a second Ctrl-C soon after forces the end.
    pub fn catch_signals(&self) {
        let events = self.root.shared.events.clone();
        crate::interrupt::monitor(Some(Box::new(move |asked| {
            let _ = events.send(match asked {
                crate::interrupt::Asked::Interrupt => Event::Interrupt,
                crate::interrupt::Asked::Terminate => Event::Terminate,
            });
        })));
    }

    /// What the monitor says goes to the display from now on, rather than
    /// on standard error.
    pub fn show_on(&self, reports: Sender<Report>) {
        self.root.send(Event::Show(reports));
    }

    /// The transfer is over, all having gone well: each worker is told so,
    /// waited for, and then each host's link closed.
    pub fn finish(&self) {
        self.root.send(Event::Finish);
    }

    /// The transfer failed, as found by what uses the monitor, saying why,
    /// unless it is ending already.
    pub fn fail(&self, why: String) {
        self.root.send(Event::Fail(why));
    }

    /// Waits until the monitor is over: how, and why.
    pub fn wait(self) -> Over {
        let over = self.over.recv().unwrap_or(Over {
            reason: Reason::Failed(Cause::Said("the client's monitor ended".to_owned())),
            names: HashMap::new(),
            statuses: HashMap::new(),
        });
        crate::interrupt::monitor(None);
        over
    }
}

impl Node {
    /// A node of no monitor, as for a test, or what runs without one: what
    /// it is told goes nowhere, and its scope ends only as what holds it
    /// ends it.
    pub fn detached() -> std::io::Result<Self> {
        let (events, _) = crossbeam_channel::unbounded();
        let (children, _) = crossbeam_channel::unbounded();
        Ok(Self {
            key: 0,
            scope: Scope::new()?,
            shared: Arc::new(Shared {
                events,
                children,
                next: AtomicU64::new(1),
                stopping: Scope::new()?,
            }),
        })
    }

    /// What its waits wait in: it ends as the monitor ends the node.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// What a wait for a person, as a question asked, waits in: it ends at
    /// the first Ctrl-C, SIGTERM or SIGHUP, which stops the transfer.
    pub fn stopping(&self) -> &Scope {
        &self.shared.stopping
    }

    /// Opens a scope under this one, `name`d as the user is told of it.
    pub fn open(&self, new: New, name: &str) -> std::io::Result<Node> {
        self.opening(new, name, None)
    }

    /// Where what a child of the client's does goes, for [`Child::spawn`].
    pub fn told(&self) -> child::Told {
        self.shared.children.clone()
    }

    /// Opens a scope under this one for `child`, which the client started,
    /// in the role it has; signalled as the monitor says.
    pub fn process(&self, child: &Child, role: Role, name: &str) -> std::io::Result<Node> {
        self.opening(
            New::Process {
                pid: child.id(),
                role,
            },
            name,
            Some(child.handle()),
        )
    }

    fn opening(&self, new: New, name: &str, handle: Option<Handle>) -> std::io::Result<Node> {
        let key = self.shared.next.fetch_add(1, Ordering::Relaxed);
        let scope = self.scope.child()?;
        self.send(Event::Open {
            key,
            parent: self.key,
            new,
            name: name.to_owned(),
            scope: scope.clone(),
            handle,
        });
        Ok(Node {
            key,
            scope,
            shared: Arc::clone(&self.shared),
        })
    }

    fn send(&self, event: Event) {
        let _ = self.shared.events.send(event);
    }

    /// A host's Liveness has a new source.
    pub fn phase(&self, phase: Phase) {
        self.send(Event::Phase(self.key, phase));
    }

    /// Word came from the host.
    pub fn heard(&self) {
        self.send(Event::Heard(self.key));
    }

    /// The host's connection is lost, as `how` says.
    pub fn lose(&self, how: Lost) {
        self.send(Event::Lose(self.key, how));
    }

    /// The worker said it is done.
    pub fn done(&self) {
        self.send(Event::Done(self.key));
    }

    /// The worker said it failed, and why.
    pub fn failed(&self, message: String) {
        self.send(Event::Failed(self.key, message));
    }

    /// The worker ended so, as the host's monitor said.
    pub fn ended(&self, status: ExitStatus) {
        self.send(Event::Ended(self.key, status));
    }

    /// The worker was paused, as the host's monitor said.
    pub fn paused(&self) {
        self.send(Event::Paused(self.key));
    }

    /// The worker went on.
    pub fn resumed(&self) {
        self.send(Event::Resumed(self.key));
    }

    /// What closing the node is: for a worker, closing what the client
    /// writes on its control connection, once it is over, or to stop; for a
    /// link, closing it in order, its end written, once all it served is
    /// done.
    pub fn on_close(&self, close: impl Fn() + Send + Sync + 'static) {
        self.send(Event::OnClose(self.key, Arc::new(close)));
    }

    /// What telling the host's monitor to stop is.
    pub fn on_stop(&self, stop: impl Fn() + Send + Sync + 'static) {
        self.send(Event::OnStop(self.key, Arc::new(stop)));
    }

    /// What asking the host's monitor how what it started looks from
    /// outside is.
    pub fn on_look(&self, look: impl Fn() + Send + Sync + 'static) {
        self.send(Event::OnLook(self.key, Arc::new(look)));
    }

    /// Says how the worker, or process, looks from outside, from the system:
    /// what it says of itself, it says on its control connection.
    pub fn look(&self) {
        self.send(Event::Look(self.key));
    }

    /// How the worker looks from outside, as the host's monitor said.
    pub fn looked(&self, outside: Outside) {
        self.send(Event::Looked(self.key, outside));
    }
}

/// The thread that drives the core, and what it keeps of each scope.
struct Driver {
    core: Core,
    timing: Timing,
    scopes: HashMap<Key, ScopeId>,
    names: HashMap<ScopeId, String>,
    waits: HashMap<ScopeId, Scope>,
    handles: HashMap<ScopeId, Handle>,
    processes: HashMap<u32, (ScopeId, Role)>,
    /// What a child did before it was opened as a scope.
    early: HashMap<u32, Vec<child::Event>>,
    closes: HashMap<ScopeId, Arc<dyn Fn() + Send + Sync>>,
    stops: HashMap<ScopeId, Arc<dyn Fn() + Send + Sync>>,
    looks: HashMap<ScopeId, Arc<dyn Fn() + Send + Sync>>,
    statuses: HashMap<ScopeId, ExitStatus>,
    /// Where what the user is told goes, once the display shows it; what it
    /// had no room for yet.
    reports: Option<Sender<Report>>,
    unsaid: VecDeque<String>,
    /// Ended once the user asks to stop.
    stopping: Scope,
}

impl Driver {
    fn new(timing: Timing, root: Scope, stopping: Scope) -> Self {
        let mut scopes = HashMap::new();
        scopes.insert(0, ROOT);
        let mut waits = HashMap::new();
        waits.insert(ROOT, root);
        let mut names = HashMap::new();
        names.insert(ROOT, "the transfer".to_owned());
        Self {
            core: Core::client(timing),
            timing,
            scopes,
            names,
            waits,
            handles: HashMap::new(),
            processes: HashMap::new(),
            early: HashMap::new(),
            closes: HashMap::new(),
            stops: HashMap::new(),
            looks: HashMap::new(),
            statuses: HashMap::new(),
            reports: None,
            unsaid: VecDeque::new(),
            stopping,
        }
    }

    fn run(
        mut self,
        events: &Receiver<Event>,
        children: &Receiver<(u32, child::Event)>,
        over: &Sender<Over>,
    ) {
        loop {
            let timeout = self
                .core
                .next()
                .map_or(Duration::from_secs(3600), |next| {
                    next.saturating_duration_since(Instant::now())
                })
                // What the display had no room for is tried again soon.
                .min(if self.unsaid.is_empty() {
                    Duration::MAX
                } else {
                    Duration::from_millis(50)
                });
            crossbeam_channel::select! {
                recv(events) -> event => match event {
                    Ok(event) => self.hear(event),
                    // No one holds the monitor any more.
                    Err(_) => return,
                },
                recv(children) -> told => {
                    if let Ok((pid, event)) = told {
                        self.child(pid, event);
                    }
                },
                default(timeout) => {},
            }
            self.core.tick(Instant::now());
            self.flush();
            for action in self.core.take() {
                if let Some(ended) = self.act(action) {
                    let _ = over.send(ended);
                    return;
                }
            }
        }
    }

    fn hear(&mut self, event: Event) {
        let now = Instant::now();
        let scope = |driver: &Self, key: Key| driver.scopes.get(&key).copied();
        match event {
            Event::Open {
                key,
                parent,
                new,
                name,
                scope: wait,
                handle,
            } => {
                let Some(parent) = scope(self, parent) else {
                    return;
                };
                let id = self.core.open(parent, new, now);
                self.scopes.insert(key, id);
                // A worker's own process is the worker, to the user.
                let name = match new {
                    New::Process {
                        role: Role::Worker, ..
                    } => self.names.get(&parent).cloned().unwrap_or(name),
                    _ => name,
                };
                self.names.insert(id, name);
                self.waits.insert(id, wait);
                if let (New::Process { pid, role }, Some(handle)) = (new, handle) {
                    self.handles.insert(id, handle);
                    self.processes.insert(pid, (id, role));
                    for event in self.early.remove(&pid).unwrap_or_default() {
                        self.child(pid, event);
                    }
                }
            }
            Event::Phase(key, phase) => {
                if let Some(id) = scope(self, key) {
                    self.core.phase(id, phase, now);
                }
            }
            Event::Heard(key) => {
                if let Some(id) = scope(self, key) {
                    self.core.heard(id, now);
                }
            }
            Event::Lose(key, how) => {
                if let Some(id) = scope(self, key) {
                    self.core.lose(id, how, now);
                }
            }
            Event::Done(key) => {
                if let Some(id) = scope(self, key) {
                    self.core.done(id, now);
                }
            }
            Event::Failed(key, message) => {
                if let Some(id) = scope(self, key) {
                    self.core.failed(id, message, now);
                }
            }
            Event::Ended(key, status) => {
                if let Some(id) = scope(self, key) {
                    self.statuses.insert(id, status);
                    self.core.worker_ended(id, now);
                }
            }
            Event::Paused(key) => {
                if let Some(id) = scope(self, key) {
                    self.core.paused(id);
                }
            }
            Event::Resumed(key) => {
                if let Some(id) = scope(self, key) {
                    self.core.resumed(id);
                }
            }
            Event::OnClose(key, close) => {
                if let Some(id) = scope(self, key) {
                    self.closes.insert(id, close);
                }
            }
            Event::OnStop(key, stop) => {
                if let Some(id) = scope(self, key) {
                    self.stops.insert(id, stop);
                }
            }
            Event::OnLook(key, look) => {
                if let Some(id) = scope(self, key) {
                    self.looks.insert(id, look);
                }
            }
            Event::Look(key) => {
                if let Some(id) = scope(self, key) {
                    self.look(id);
                }
            }
            Event::Looked(key, outside) => {
                if let Some(id) = scope(self, key) {
                    let name = self.names.get(&id).cloned().unwrap_or_default();
                    self.unsaid.extend(look::describe(&name, &outside));
                }
            }
            Event::Interrupt => {
                self.stopping.end("stopped, as asked");
                self.core.interrupt(now);
            }
            Event::Terminate => {
                self.stopping.end("stopped, as asked");
                self.core.terminate(now);
            }
            Event::Finish => self.core.finish(now),
            Event::Fail(why) => self.core.fail(Cause::Said(why), now),
            Event::Show(reports) => self.reports = Some(reports),
        }
    }

    /// What a child of the client's did.
    fn child(&mut self, pid: u32, event: child::Event) {
        let Some(&(id, role)) = self.processes.get(&pid) else {
            self.early.entry(pid).or_default().push(event);
            return;
        };
        let now = Instant::now();
        match event {
            child::Event::Paused => self.core.paused(id),
            child::Event::Resumed => self.core.resumed(id),
            child::Event::Exited(status) => {
                // A server's process is its worker's end.
                if role == Role::Worker
                    && let Some(worker) = self.core.parent(id)
                {
                    self.statuses.insert(worker, status);
                }
                self.core.exited(id, now);
            }
        }
    }

    /// Does what the core said; the end, once it is over.
    fn act(&mut self, action: Action) -> Option<Over> {
        match action {
            Action::End(id, reason) => {
                let why = self.why(id, &reason);
                if let Some(wait) = self.waits.get(&id) {
                    wait.end(&why);
                }
            }
            Action::Signal(id, signal) => {
                if let Some(handle) = self.handles.get(&id) {
                    handle.signal(match signal {
                        Signal::Term => child::Signal::Term,
                        Signal::Kill => child::Signal::Kill,
                    });
                }
            }
            // Each in a thread of its own, should what it writes on wait for
            // room.
            Action::Close(id) => {
                if let Some(close) = self.closes.get(&id) {
                    let close = Arc::clone(close);
                    std::thread::spawn(move || close());
                }
            }
            Action::SendStop(id) => {
                if let Some(stop) = self.stops.get(&id) {
                    let stop = Arc::clone(stop);
                    std::thread::spawn(move || stop());
                }
            }
            Action::Note(note) => {
                let said = self.said(&note);
                if let Some(said) = said {
                    self.unsaid.push_back(said);
                }
                // What it waits for, or what was paused, is said as it looks
                // from outside too, whatever it can say of itself.
                match &note {
                    Note::Awaiting(ids) => ids.iter().for_each(|id| self.look(*id)),
                    Note::Paused(id) | Note::Resumed(id) => self.look(*id),
                    _ => {}
                }
                self.flush();
            }
            Action::Over(reason) => {
                // Forced, the client goes at once, as before: what it
                // started was killed, and nothing more is waited for.
                if reason == Reason::Forced {
                    self.flush_now();
                    crate::interrupt::exit_now(130);
                }
                return Some(Over {
                    reason,
                    names: std::mem::take(&mut self.names),
                    statuses: std::mem::take(&mut self.statuses),
                });
            }
        }
        None
    }

    /// Says how a worker, or process, looks from outside: one of the
    /// client's own as the system says, at once; one a host's monitor
    /// started, as that monitor will say, once asked.
    fn look(&mut self, id: ScopeId) {
        let process = if self.handles.contains_key(&id) {
            Some(id)
        } else {
            // A worker on this machine: its process.
            self.processes
                .values()
                .find(|(process, role)| {
                    *role == Role::Worker && self.core.parent(*process) == Some(id)
                })
                .map(|(process, _)| *process)
        };
        if let Some(process) = process
            && let Some(handle) = self.handles.get(&process)
        {
            let name = self.names.get(&process).cloned().unwrap_or_default();
            self.unsaid
                .extend(look::describe(&name, &look::outside(handle.id())));
            return;
        }
        // A host, or a worker on one: its monitor is asked.
        let mut at = Some(id);
        while let Some(scope) = at {
            if let Some(ask) = self.looks.get(&scope) {
                let ask = Arc::clone(ask);
                std::thread::spawn(move || ask());
                return;
            }
            at = self.core.parent(scope);
        }
    }

    /// Why a scope ended, as the waits in it say it.
    fn why(&self, id: ScopeId, reason: &Reason) -> String {
        match reason {
            Reason::Lost(how) => {
                let host = self.host_of(id);
                lost(&host, *how, self.timing)
            }
            Reason::Stopped => "stopped, as asked".to_owned(),
            Reason::Forced => "forced to stop".to_owned(),
            Reason::Done => "done".to_owned(),
            Reason::Failed(cause) => self.cause(cause),
        }
    }

    /// The name of the host a scope is on, or of the scope.
    fn host_of(&self, id: ScopeId) -> String {
        let mut at = Some(id);
        let mut name = None;
        while let Some(scope) = at {
            if self.core.is_host(scope) {
                return self.names.get(&scope).cloned().unwrap_or_default();
            }
            name = name.or_else(|| self.names.get(&scope).cloned());
            at = self.core.parent(scope);
        }
        name.unwrap_or_default()
    }

    fn cause(&self, cause: &Cause) -> String {
        describe(cause, &self.names, self.timing)
    }

    /// What the user is told of a note, if anything.
    fn said(&self, note: &Note) -> Option<String> {
        let name = |id: &ScopeId| {
            self.names
                .get(id)
                .cloned()
                .unwrap_or_else(|| "something".to_owned())
        };
        Some(match note {
            Note::Stopping { within } => format!(
                "stopping; press Ctrl-C again within {} s to stop at once",
                within.as_secs()
            ),
            Note::Awaiting(ids) if ids.is_empty() => return None,
            Note::Awaiting(ids) => format!(
                "waiting for {} to end",
                ids.iter().map(name).collect::<Vec<_>>().join(", ")
            ),
            Note::Paused(id) => format!(
                "{} was stopped, as by Ctrl-Z or SIGSTOP; the transfer waits for it to go on",
                name(id)
            ),
            Note::Resumed(id) => format!("{} goes on", name(id)),
            Note::Abandoned(ids) => format!(
                "gave up on {}",
                ids.iter().map(name).collect::<Vec<_>>().join(", ")
            ),
        })
    }

    /// Tells the user what waits to be told: on the display, once it shows
    /// it, as it has room; on standard error before.
    fn flush(&mut self) {
        while let Some(said) = self.unsaid.pop_front() {
            match &self.reports {
                None => crate::status::say(&said),
                Some(reports) => match reports.try_send(Report::Said(said)) {
                    Ok(()) => {}
                    Err(crossbeam_channel::TrySendError::Full(Report::Said(said))) => {
                        self.unsaid.push_front(said);
                        return;
                    }
                    // The display is gone: standard error, then.
                    Err(error) => {
                        if let Report::Said(said) = error.into_inner() {
                            crate::status::say(&said);
                        }
                        self.reports = None;
                    }
                },
            }
        }
    }

    /// As [`Driver::flush`], on standard error whatever is left, as the
    /// client is about to exit.
    fn flush_now(&mut self) {
        self.reports = None;
        self.flush();
    }
}

/// What the waits on a host say once its connection is lost.
fn lost(host: &str, how: Lost, timing: Timing) -> String {
    let _ = timing;
    match how {
        Lost::Silent(silence) => {
            format!("nothing came from {host} for {} s", silence.as_secs())
        }
        Lost::Late(silence) => format!("{host} did not answer within {} s", silence.as_secs()),
        Lost::Ended => format!("the connection to {host} ended"),
        Lost::Cut => "stopped, as asked".to_owned(),
    }
}

/// What a cause says, its scopes named by `names`.
pub fn describe(cause: &Cause, names: &HashMap<ScopeId, String>, timing: Timing) -> String {
    let name = |id: &ScopeId| names.get(id).cloned().unwrap_or_default();
    match cause {
        Cause::Failed { worker, message } => format!("{} failed: {message}", name(worker)),
        Cause::Gone { worker } => format!("{} ended before it was done", name(worker)),
        Cause::Lost { host, how } => {
            format!("{}: {}", name(host), lost("it", *how, timing))
        }
        Cause::Said(why) => why.clone(),
    }
}

#[cfg(test)]
impl Client {
    /// As a Ctrl-C does, for tests, which send no signal.
    fn interrupt(&self) {
        self.root.send(Event::Interrupt);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Times short enough for a test: silent after 1 s.
    const QUICK: Timing = Timing {
        beat: Duration::from_millis(200),
        silence: Duration::from_secs(1),
        say_after: Duration::from_millis(100),
        force_within: Duration::from_secs(3),
    };

    fn sleeping(node: &Node) -> Child {
        Child::spawn(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
            Some(node.told()),
        )
        .unwrap()
    }

    fn counting() -> (Arc<AtomicUsize>, impl Fn() + Send + 'static) {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        (count, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn a_host_that_goes_silent_ends_its_waits_and_fails_the_transfer() {
        crate::test_support::within(
            "a host that goes silent ends its waits and fails the transfer",
            crate::test_support::PATIENCE,
            || {
                let client = Client::start(QUICK).unwrap();
                let host = client.root().open(New::Host, "nano4").unwrap();
                host.phase(Phase::Linked);
                let link = host.open(New::Plain, "the link").unwrap();
                let carrier = sleeping(&link);
                link.process(&carrier, Role::Carrier, "ssh to nano4")
                    .unwrap();
                // Its waits end, saying why, and what carried it is killed.
                let (quiet, _other) = std::os::unix::net::UnixStream::pair().unwrap();
                let error = link
                    .scope()
                    .wait(
                        &quiet,
                        rustix::event::PollFlags::IN,
                        None,
                        crate::profile::State::ReadConnection,
                    )
                    .unwrap_err();
                assert_eq!(
                    crate::transport::scope::Ended::of(&error).unwrap().0,
                    "nothing came from nano4 for 1 s"
                );
                let over = client.wait();
                assert!(
                    matches!(over.reason, Reason::Failed(Cause::Lost { .. })),
                    "{:?}",
                    over.reason
                );
                assert_eq!(carrier.wait().signal(), Some(libc::SIGKILL));
            },
        );
    }

    #[test]
    fn workers_done_are_closed_and_the_transfer_is_over_once_they_end() {
        crate::test_support::within(
            "workers done are closed and the transfer is over once they end",
            crate::test_support::PATIENCE,
            || {
                let client = Client::start(QUICK).unwrap();
                let worker = client.root().open(New::Worker, "the Source").unwrap();
                let server = sleeping(&worker);
                worker
                    .process(&server, Role::Worker, "the Source's process")
                    .unwrap();
                let (closed, close) = counting();
                worker.on_close(close);
                worker.done();
                // Closed, it ends, as a server does once its control
                // connection closes.
                while closed.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                server.signal(child::Signal::Term);
                let over = client.wait();
                assert_eq!(over.reason, Reason::Done);
                assert_eq!(closed.load(Ordering::SeqCst), 1);
            },
        );
    }

    #[test]
    fn a_ctrl_c_stops_each_side_and_the_transfer_is_over_once_they_end() {
        crate::test_support::within(
            "a ctrl c stops each side and the transfer is over once they end",
            crate::test_support::PATIENCE,
            || {
                let client = Client::start(QUICK).unwrap();
                let host = client.root().open(New::Host, "nano4").unwrap();
                host.phase(Phase::Linked);
                let (stops, stop) = counting();
                host.on_stop(stop);
                let remote = host.open(New::Worker, "the Destination").unwrap();
                let (closes, close) = counting();
                remote.on_close(close);
                let local = client.root().open(New::Worker, "the Source").unwrap();
                let server = sleeping(&local);
                local
                    .process(&server, Role::Worker, "the Source's process")
                    .unwrap();
                client.interrupt();
                // The local server is sent SIGTERM, and ends so; the host's
                // monitor says how its server ended.
                assert_eq!(server.wait().signal(), Some(libc::SIGTERM));
                remote.ended(ExitStatus::from_raw(143 << 8));
                let over = client.wait();
                assert_eq!(over.reason, Reason::Stopped);
                // Each runs in a thread of its own.
                while stops.load(Ordering::SeqCst) == 0 || closes.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(stops.load(Ordering::SeqCst), 1);
                assert_eq!(closes.load(Ordering::SeqCst), 1);
            },
        );
    }
}
