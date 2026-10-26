//! The monitor: stability and lifecycle, one in each place, the client's
//! machine and each host, where it is the top of what runs there and holds
//! every resource there: connections, listeners, processes. This is its
//! core, which the client's monitor and each host's share: a state machine
//! that does no I/O and waits on nothing, told what happens one event at a
//! time, by one thread, and saying what to do as actions. The time is given
//! it with each event, so that what it does is tested on events and times
//! made up. See the [draft](../../docs/drafts/silent-drops.md).
//!
//! What it holds is a tree of scopes. On the client, a transfer is the root;
//! under it, each host, and under each host, the servers there, as the
//! client has them, its workers, and the processes that carry their
//! connections; a server on this machine hangs from the transfer itself. On
//! a host, its monitor is the root, and what it started hangs from it. Every
//! wait belongs to a scope, and returns once what it waits for comes, or its
//! scope ends. A scope ends for one reason, from one source: a transfer as
//! the user stops it, as something fails, or as its workers are done; a host
//! as its Liveness says it is gone; and everything under a scope ends with
//! it.
//!
//! How what is under a scope ends depends on whether its state can be known.
//! What can be seen is waited for, and what is awaited is said once it takes
//! more than a moment; a second Ctrl-C soon after the first forces it. Only
//! a host whose connection is lost, which no one can see any more, gives
//! what it started a grace, then kills what is left, and ends; and the
//! client, likewise, waits for nothing of such a host.
//!
//! The strong guarantee is only for losing the connection: each side then
//! cleans up within a bound. A host's Liveness says it is lost, its source
//! changing with the phase: a first session's alive lines, then the start of
//! the connection the client follows its monitor by, then the heartbeat on
//! it, each gone after [`SILENCE`] without word.

use std::time::{Duration, Instant};

pub mod client;
pub mod host;
#[cfg(test)]
mod tests;

/// How often a heartbeat goes, each way, on the connection the client
/// follows a host's monitor by; and how often a first session's alive lines
/// go, each way, before there is one.
pub const BEAT: Duration = Duration::from_secs(5);

/// The one silence of the design: how long without word from the far end
/// before the connection is taken for lost; how long what is waited for
/// before the heartbeat may take; and the grace a host that lost its
/// connection gives what it started to end.
pub const SILENCE: Duration = Duration::from_secs(30);

/// How long a wait may go unsaid: past it, what is awaited is said, so that
/// the user can judge whether to force it.
pub const SAY_AFTER: Duration = Duration::from_secs(1);

/// How soon after a first stop a second Ctrl-C must come to force the end:
/// a later one counts as a first again, so that two presses by mistake do
/// not force.
pub const FORCE_WITHIN: Duration = Duration::from_secs(3);

/// The times the monitor keeps: the design's, or as a test shortens them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub beat: Duration,
    pub silence: Duration,
    pub say_after: Duration,
    pub force_within: Duration,
}

impl Timing {
    /// The design's.
    pub const STANDARD: Self = Self {
        beat: BEAT,
        silence: SILENCE,
        say_after: SAY_AFTER,
        force_within: FORCE_WITHIN,
    };

    /// The design's, but for the silence, and the beats within it, should
    /// `FERRY_TEST_SILENCE` shorten it.
    pub fn wanted() -> Self {
        match crate::inject::silence() {
            Some(silence) => Self {
                beat: silence / 6,
                silence,
                ..Self::STANDARD
            },
            None => Self::STANDARD,
        }
    }
}

/// A scope in a monitor's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(usize);

/// The root of every tree: a transfer on the client, a host's monitor there.
pub const ROOT: ScopeId = ScopeId(0);

/// What a scope opened under another is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum New {
    /// A host, on the client: what reaches it hangs from it. It ends as its
    /// Liveness says it is lost, logging in at first.
    Host,
    /// A server, on the client: told it is over by its control connection
    /// closing, and done once its end is known, as its process's, or as the
    /// host's monitor says.
    Worker,
    /// A process: what carries connections, as `ssh`, or a worker on the
    /// host, or a server on this machine.
    Process { pid: u32, role: Role },
    /// Anything else that ends with what it is under: a connection, a
    /// listener.
    Plain,
}

/// What a process is to the monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// It carries connections, as `ssh` does: it ends as they close, and is
    /// killed at once once its host's connection is lost.
    Carrier,
    /// It does the work: a server, sudo's check, a fixture. Told to stop, it
    /// is sent SIGTERM; on a host that lost its connection, killed after the
    /// grace.
    Worker,
}

/// Why a scope ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// All went well: what is under it was told it is over, and ended.
    Done,
    /// The user stopped it, or, on a host, the client did.
    Stopped,
    /// Something failed: the first that did is the cause.
    Failed(Cause),
    /// Its connection was lost.
    Lost(Lost),
    /// A second Ctrl-C forced it: what still ran was killed.
    Forced,
}

/// What failed, first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cause {
    /// A worker said it failed, and why.
    Failed { worker: ScopeId, message: String },
    /// A worker ended without saying it was done.
    Gone { worker: ScopeId },
    /// A host's connection was lost.
    Lost { host: ScopeId, how: Lost },
    /// What the one driving the monitor found, and said so.
    Said(String),
}

/// How a connection was taken for lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lost {
    /// Nothing came on it for this long.
    Silent(Duration),
    /// What was waited for on it did not come within this.
    Late(Duration),
    /// It ended.
    Ended,
    /// The client cut it, as stopped before the host's monitor was there to
    /// be told.
    Cut,
}

/// Where a host's Liveness comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Logging in: `ssh`'s own limits, none of the monitor's.
    LogIn,
    /// Hearing from the far end now and then, as a first session's alive
    /// lines; there is nothing yet to say stop on.
    Hearing,
    /// Waiting for what links the two monitors to start, or be followed,
    /// within the silence.
    Linking,
    /// Linked: the heartbeat, on the connection stop goes on.
    Linked,
}

/// A signal for a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// What to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The scope ended, for this reason: every wait in it returns.
    End(ScopeId, Reason),
    /// Signal the process the scope is.
    Signal(ScopeId, Signal),
    /// Close what the client writes on a worker's control connection: it is
    /// over, or it is to stop; or close a link in order, its end written,
    /// once all it served is done, so that what carries it ends.
    Close(ScopeId),
    /// Tell a host's monitor to stop what it started, on the link.
    SendStop(ScopeId),
    /// Tell the one watching: the user, or, on a host, the client.
    Note(Note),
    /// The root ended, and nothing under it is waited for any more.
    Over(Reason),
}

/// What the one watching is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// Stopping, as asked: a second Ctrl-C within this forces it.
    Stopping { within: Duration },
    /// Still waiting for these to end.
    Awaiting(Vec<ScopeId>),
    /// The process, or worker, was stopped, as by SIGSTOP.
    Paused(ScopeId),
    /// It went on, as after SIGCONT.
    Resumed(ScopeId),
    /// The grace ran out: these were killed, and are not waited for.
    Abandoned(Vec<ScopeId>),
}

/// What a scope is, and what it keeps of it.
#[derive(Debug)]
enum What {
    /// The client's root.
    Transfer,
    /// A host's monitor, its root: ended only as its Liveness says.
    Place {
        liveness: Liveness,
    },
    /// A host, on the client.
    Host {
        liveness: Liveness,
        /// Stop was sent it on its link.
        stop_sent: bool,
    },
    Worker {
        /// It said it is done.
        done: bool,
        /// Its end is known.
        over: bool,
        /// What the client writes on its control connection is closed.
        closed: bool,
    },
    Process {
        role: Role,
        /// It exited, or was killed and is not waited for.
        exited: bool,
        signalled: Option<Signal>,
    },
    Plain,
}

/// Where a Liveness is, and since when: the last word heard, or the start
/// of what is waited for.
#[derive(Clone, Copy, Debug)]
struct Liveness {
    phase: Phase,
    since: Instant,
}

impl Liveness {
    /// When it says the connection is lost, if nothing changes, and how.
    fn deadline(&self, silence: Duration) -> Option<(Instant, Lost)> {
        match self.phase {
            Phase::LogIn => None,
            Phase::Hearing | Phase::Linked => Some((self.since + silence, Lost::Silent(silence))),
            Phase::Linking => Some((self.since + silence, Lost::Late(silence))),
        }
    }
}

#[derive(Debug)]
struct Scope {
    parent: Option<ScopeId>,
    what: What,
    children: Vec<ScopeId>,
    /// Why it is ending, once it is.
    ending: Option<Reason>,
    /// Its end was said, as [`Action::End`].
    said: bool,
    /// It ended: all under it did, and so did it.
    ended: bool,
}

/// A monitor's core. See the [module](self).
pub struct Core {
    timing: Timing,
    scopes: Vec<Scope>,
    actions: Vec<Action>,
    /// When a stop was last asked, for a second Ctrl-C to force the end.
    asked: Option<Instant>,
    /// Since when something is waited for to end, and whether that was said.
    awaiting: Option<(Instant, bool)>,
    /// When the grace of a host that lost its connection runs out.
    grace: Option<Instant>,
    over: bool,
}

impl Core {
    /// The client's monitor, whose root is a transfer.
    pub fn client(timing: Timing) -> Self {
        Self::new(timing, What::Transfer)
    }

    /// A host's monitor, whose root it is: to be followed within the
    /// silence from `now`.
    pub fn host(timing: Timing, now: Instant) -> Self {
        Self::new(
            timing,
            What::Place {
                liveness: Liveness {
                    phase: Phase::Linking,
                    since: now,
                },
            },
        )
    }

    fn new(timing: Timing, root: What) -> Self {
        Self {
            timing,
            scopes: vec![Scope {
                parent: None,
                what: root,
                children: Vec::new(),
                ending: None,
                said: false,
                ended: false,
            }],
            actions: Vec::new(),
            asked: None,
            awaiting: None,
            grace: None,
            over: false,
        }
    }

    /// What to do, as decided since this was last asked, in order.
    pub fn take(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    /// Why `scope` is ending, once it is.
    pub fn ending(&self, scope: ScopeId) -> Option<&Reason> {
        self.scopes[scope.0].ending.as_ref()
    }

    /// Whether `scope` ended: all under it, and it.
    pub fn ended(&self, scope: ScopeId) -> bool {
        self.scopes[scope.0].ended
    }

    /// The scope above `scope`.
    pub fn parent(&self, scope: ScopeId) -> Option<ScopeId> {
        self.scopes[scope.0].parent
    }

    /// Whether `scope` is a host, on the client.
    pub fn is_host(&self, scope: ScopeId) -> bool {
        matches!(self.scopes[scope.0].what, What::Host { .. })
    }

    /// Opens a scope under `parent`. Under one already ending, it ends at
    /// once, as it would have with it.
    pub fn open(&mut self, parent: ScopeId, new: New, now: Instant) -> ScopeId {
        let what = match new {
            New::Host => What::Host {
                liveness: Liveness {
                    phase: Phase::LogIn,
                    since: now,
                },
                stop_sent: false,
            },
            New::Worker => What::Worker {
                done: false,
                over: false,
                closed: false,
            },
            New::Process { role, .. } => What::Process {
                role,
                exited: false,
                signalled: None,
            },
            New::Plain => What::Plain,
        };
        let id = ScopeId(self.scopes.len());
        self.scopes.push(Scope {
            parent: Some(parent),
            what,
            children: Vec::new(),
            ending: None,
            said: false,
            ended: false,
        });
        self.scopes[parent.0].children.push(id);
        if let Some(reason) = self.scopes[parent.0].ending.clone() {
            self.end(id, reason, now);
        }
        id
    }

    /// A host's Liveness has a new source.
    pub fn phase(&mut self, scope: ScopeId, phase: Phase, now: Instant) {
        if let Some(liveness) = self.liveness_mut(scope) {
            *liveness = Liveness { phase, since: now };
        }
    }

    /// Word came from the far end of a host, or, on a host, from the
    /// client: an alive line, a heartbeat, anything.
    pub fn heard(&mut self, scope: ScopeId, now: Instant) {
        if let Some(liveness) = self.liveness_mut(scope)
            && matches!(liveness.phase, Phase::Hearing | Phase::Linked)
        {
            liveness.since = now;
        }
    }

    fn liveness_mut(&mut self, scope: ScopeId) -> Option<&mut Liveness> {
        let scope = &mut self.scopes[scope.0];
        if scope.ending.is_some() {
            return None;
        }
        match &mut scope.what {
            What::Host { liveness, .. } | What::Place { liveness } => Some(liveness),
            _ => None,
        }
    }

    /// A host's connection is lost, as `how` says: on the client, it fails
    /// the transfer, and, whatever it was ending for, nothing more is waited
    /// for of the host, as nothing more will come of it; on a host, the
    /// monitor ends what it started, within the grace, and then itself.
    pub fn lose(&mut self, scope: ScopeId, how: Lost, now: Instant) {
        let entry = &self.scopes[scope.0];
        if entry.ended || matches!(entry.ending, Some(Reason::Lost(_))) {
            return;
        }
        match self.scopes[scope.0].what {
            What::Host { .. } => {
                self.cut(scope, how, now);
                self.fail(Cause::Lost { host: scope, how }, now);
            }
            What::Place { .. } if self.scopes[scope.0].ending.is_some() => {}
            What::Place { .. } => {
                self.grace = Some(now + self.timing.silence);
                self.end(scope, Reason::Lost(how), now);
            }
            _ => {}
        }
    }

    /// The transfer fails, for `cause`, unless it is ending already. What
    /// can be seen is waited for: each worker ends as its stream breaks, or
    /// as the user stops it.
    pub fn fail(&mut self, cause: Cause, now: Instant) {
        self.end(ROOT, Reason::Failed(cause), now);
    }

    /// The transfer is over, all having gone well: each worker is told so,
    /// and waited for, then each host's link closed.
    pub fn finish(&mut self, now: Instant) {
        self.end(ROOT, Reason::Done, now);
    }

    /// A worker said it is done: it is told it is over, as its control
    /// connection closes. Once all are done, and their ends known, the
    /// transfer is.
    pub fn done(&mut self, worker: ScopeId, now: Instant) {
        if let What::Worker { done, .. } = &mut self.scopes[worker.0].what {
            *done = true;
        }
        self.close(worker);
        self.settle(worker, now);
        self.finish_if_all_done(now);
    }

    /// A worker said it failed: the first is the transfer's cause.
    pub fn failed(&mut self, worker: ScopeId, message: String, now: Instant) {
        self.fail(Cause::Failed { worker, message }, now);
    }

    /// A worker's end is known: its process exited, or the host's monitor
    /// said how it ended. One not done ended before its time: the
    /// transfer fails, unless it is ending already.
    pub fn worker_ended(&mut self, worker: ScopeId, now: Instant) {
        let What::Worker { done, over, .. } = &mut self.scopes[worker.0].what else {
            return;
        };
        if *over {
            return;
        }
        *over = true;
        let done = *done;
        if !done {
            self.fail(Cause::Gone { worker }, now);
        }
        self.settle(worker, now);
        self.finish_if_all_done(now);
    }

    /// A process exited. A server's process is its worker's end.
    pub fn exited(&mut self, process: ScopeId, now: Instant) {
        let What::Process { role, exited, .. } = &mut self.scopes[process.0].what else {
            return;
        };
        if *exited {
            return;
        }
        *exited = true;
        let role = *role;
        if let Some(parent) = self.scopes[process.0].parent
            && role == Role::Worker
            && matches!(self.scopes[parent.0].what, What::Worker { .. })
        {
            self.worker_ended(parent, now);
        }
        self.settle(process, now);
    }

    /// A process, or a worker on a host, was stopped, as by SIGSTOP: said,
    /// not timed out, as it can be seen.
    pub fn paused(&mut self, scope: ScopeId) {
        self.actions.push(Action::Note(Note::Paused(scope)));
    }

    /// It went on.
    pub fn resumed(&mut self, scope: ScopeId) {
        self.actions.push(Action::Note(Note::Resumed(scope)));
    }

    /// The user pressed Ctrl-C: a first asks to stop; a second within
    /// [`Timing::force_within`] forces the end.
    pub fn interrupt(&mut self, now: Instant) {
        match self.asked {
            Some(asked) if now < asked + self.timing.force_within => self.force(),
            _ => self.ask_to_stop(now),
        }
    }

    /// The process was asked to terminate, as by SIGTERM or SIGHUP: as a
    /// first Ctrl-C, never forcing.
    pub fn terminate(&mut self, now: Instant) {
        self.ask_to_stop(now);
    }

    /// On a host: the client said to stop. What the monitor started is sent
    /// SIGTERM, and waited for; its link stays.
    pub fn stop(&mut self, now: Instant) {
        let children = self.scopes[ROOT.0].children.clone();
        for child in children {
            if matches!(
                self.scopes[child.0].what,
                What::Process {
                    role: Role::Worker,
                    ..
                }
            ) {
                self.end(child, Reason::Stopped, now);
            }
        }
        self.await_from(now);
    }

    fn ask_to_stop(&mut self, now: Instant) {
        self.asked = Some(now);
        if self.scopes[ROOT.0].ending.is_none() {
            self.end(ROOT, Reason::Stopped, now);
        } else {
            // Ending already, as for a failure, whose cause it stays: what
            // still runs is told to stop now, and what is awaited said.
            self.stop_all(ROOT, now);
            self.say_awaiting();
        }
        if !self.over {
            self.actions.push(Action::Note(Note::Stopping {
                within: self.timing.force_within,
            }));
        }
    }

    /// A second Ctrl-C: every process still running here is killed, every
    /// scope ends at once, and nothing more is waited for.
    fn force(&mut self) {
        for index in 0..self.scopes.len() {
            self.kill_unwaited(ScopeId(index));
        }
        // Deepest first, so that what is under a scope ends before it.
        for index in (0..self.scopes.len()).rev() {
            let scope = &mut self.scopes[index];
            if scope.ending.is_none() {
                scope.ending = Some(Reason::Forced);
            }
            scope.ended = true;
            if !scope.said {
                scope.said = true;
                self.actions
                    .push(Action::End(ScopeId(index), Reason::Forced));
            }
        }
        self.finish_over(Reason::Forced);
    }

    /// Looks at what is due by `now`: a Liveness that says a connection is
    /// lost, a grace that ran out, a wait that has gone on long enough to
    /// be said.
    pub fn tick(&mut self, now: Instant) {
        for index in 0..self.scopes.len() {
            let scope = &self.scopes[index];
            if scope.ending.is_some() {
                continue;
            }
            let liveness = match &scope.what {
                What::Host { liveness, .. } | What::Place { liveness } => liveness,
                _ => continue,
            };
            if let Some((deadline, how)) = liveness.deadline(self.timing.silence)
                && now >= deadline
            {
                self.lose(ScopeId(index), how, now);
            }
        }
        if let Some(grace) = self.grace
            && now >= grace
            && !self.over
        {
            self.abandon(now);
        }
        if let Some((since, false)) = self.awaiting
            && now >= since + self.timing.say_after
        {
            self.say_awaiting();
        }
    }

    /// When [`Core::tick`] next has something to do, if anything changes
    /// nothing before.
    pub fn next(&self) -> Option<Instant> {
        let silence = self.timing.silence;
        let lost = self
            .scopes
            .iter()
            .filter(|scope| scope.ending.is_none())
            .filter_map(|scope| match &scope.what {
                What::Host { liveness, .. } | What::Place { liveness } => {
                    liveness.deadline(silence).map(|(deadline, _)| deadline)
                }
                _ => None,
            });
        let grace = self.grace.filter(|_| !self.over);
        let said = match self.awaiting {
            Some((since, false)) => Some(since + self.timing.say_after),
            _ => None,
        };
        lost.chain(grace).chain(said).min()
    }

    /// Ends `scope`, for `reason`, unless it is ending already, and all
    /// under it with it: each as what it is says.
    fn end(&mut self, scope: ScopeId, reason: Reason, now: Instant) {
        if self.scopes[scope.0].ending.is_some() || self.over {
            return;
        }
        // A host with nothing to say stop on is cut: the shell there ends as
        // its input closes, or as its watchdog hears nothing.
        if reason == Reason::Stopped
            && let What::Host { liveness, .. } = &self.scopes[scope.0].what
            && liveness.phase != Phase::Linked
        {
            return self.cut(scope, Lost::Cut, now);
        }
        self.scopes[scope.0].ending = Some(reason.clone());
        // What cannot be seen any more is not waited for: its waits return
        // at once.
        let at_once = matches!(reason, Reason::Lost(_));
        if at_once {
            self.say_end(scope);
        }
        match self.scopes[scope.0].what {
            What::Host { .. } if reason == Reason::Stopped => self.send_stop(scope),
            What::Worker { .. } if reason == Reason::Stopped => self.close(scope),
            // A link ending in order is closed, so that what carries it
            // ends, and it with it.
            What::Plain if !at_once => self.actions.push(Action::Close(scope)),
            // On a host that lost its connection, or told to stop: SIGTERM,
            // and, if lost, the grace.
            What::Process {
                role: Role::Worker,
                exited: false,
                ..
            } if matches!(reason, Reason::Lost(_) | Reason::Stopped) => {
                self.signal(scope, Signal::Term);
            }
            _ => {}
        }
        // What is under it ends with it: what works first, then what links
        // it, as its connections, so that what works can say how it ended.
        let children = self.scopes[scope.0].children.clone();
        let (plain, working): (Vec<_>, Vec<_>) = children
            .into_iter()
            .partition(|child| matches!(self.scopes[child.0].what, What::Plain));
        for child in working {
            self.end(child, reason.clone(), now);
        }
        if at_once || self.working_ended(scope) {
            for child in plain {
                self.end(child, reason.clone(), now);
            }
        }
        // What is awaited is said to whoever watches: on a host that lost
        // its connection, no one does.
        if scope == ROOT && !at_once {
            self.await_from(now);
        }
        self.settle(scope, now);
    }

    /// Cuts a host, on the client, as lost, `how` says: whatever it was
    /// ending for, every wait in it returns at once, what carried its
    /// connections is killed, and not waited for, and its workers' ends
    /// are not known, nor waited for.
    fn cut(&mut self, host: ScopeId, how: Lost, now: Instant) {
        let reason = Reason::Lost(how);
        let mut under = vec![host];
        let mut index = 0;
        while index < under.len() {
            under.extend(self.scopes[under[index].0].children.iter().copied());
            index += 1;
        }
        for &scope in &under {
            if self.scopes[scope.0].ended {
                continue;
            }
            self.scopes[scope.0].ending = Some(reason.clone());
            self.say_end(scope);
            match &mut self.scopes[scope.0].what {
                What::Process { exited: false, .. } => self.kill_unwaited(scope),
                What::Worker { over, .. } => *over = true,
                _ => {}
            }
        }
        for &scope in under.iter().rev() {
            self.settle(scope, now);
        }
    }

    /// Whether all under `scope` that is not a plain one ended.
    fn working_ended(&self, scope: ScopeId) -> bool {
        self.scopes[scope.0].children.iter().all(|child| {
            matches!(self.scopes[child.0].what, What::Plain) || self.scopes[child.0].ended
        })
    }

    /// Tells what still runs under `scope` to stop: each host linked, by
    /// its link, any other cut; each worker, by closing its control
    /// connection; each process that works, by SIGTERM.
    fn stop_all(&mut self, scope: ScopeId, now: Instant) {
        if self.scopes[scope.0].ended {
            return;
        }
        match self.scopes[scope.0].what {
            What::Host { liveness, .. } if liveness.phase == Phase::Linked => {
                self.send_stop(scope);
            }
            What::Host { .. } => {
                if !matches!(self.scopes[scope.0].ending, Some(Reason::Lost(_))) {
                    self.cut(scope, Lost::Cut, now);
                }
                return;
            }
            What::Worker { .. } => self.close(scope),
            What::Process {
                role: Role::Worker,
                exited: false,
                signalled: None,
            } => self.signal(scope, Signal::Term),
            _ => {}
        }
        let children = self.scopes[scope.0].children.clone();
        for child in children {
            self.stop_all(child, now);
        }
    }

    fn signal(&mut self, process: ScopeId, signal: Signal) {
        if let What::Process { signalled, .. } = &mut self.scopes[process.0].what {
            *signalled = Some(signal);
            self.actions.push(Action::Signal(process, signal));
        }
    }

    /// Kills a process, and waits for it no more: it counts as exited.
    fn kill_unwaited(&mut self, process: ScopeId) {
        if let What::Process {
            exited, signalled, ..
        } = &mut self.scopes[process.0].what
            && !*exited
        {
            *exited = true;
            *signalled = Some(Signal::Kill);
            self.actions.push(Action::Signal(process, Signal::Kill));
        }
    }

    fn send_stop(&mut self, host: ScopeId) {
        if let What::Host { stop_sent, .. } = &mut self.scopes[host.0].what
            && !*stop_sent
        {
            *stop_sent = true;
            self.actions.push(Action::SendStop(host));
        }
    }

    fn close(&mut self, worker: ScopeId) {
        if let What::Worker { closed, .. } = &mut self.scopes[worker.0].what
            && !*closed
        {
            *closed = true;
            self.actions.push(Action::Close(worker));
        }
    }

    fn finish_if_all_done(&mut self, now: Instant) {
        if self.scopes[ROOT.0].ending.is_some() {
            return;
        }
        let workers: Vec<_> = self
            .scopes
            .iter()
            .filter_map(|scope| match scope.what {
                What::Worker { done, over, .. } => Some(done && over),
                _ => None,
            })
            .collect();
        if !workers.is_empty() && workers.iter().all(|done| *done) {
            self.finish(now);
        }
    }

    /// Ends `scope` once all under it has, and what it is says it has: then
    /// the scope above it, in turn.
    fn settle(&mut self, scope: ScopeId, now: Instant) {
        let mut at = Some(scope);
        while let Some(scope) = at {
            let entry = &self.scopes[scope.0];
            let Some(reason) = entry.ending.clone() else {
                return;
            };
            if entry.ended {
                at = entry.parent;
                continue;
            }
            // What links it ends once what works under it has.
            if self.working_ended(scope) {
                let plain: Vec<_> = self.scopes[scope.0]
                    .children
                    .iter()
                    .copied()
                    .filter(|child| {
                        matches!(self.scopes[child.0].what, What::Plain)
                            && self.scopes[child.0].ending.is_none()
                    })
                    .collect();
                for child in plain {
                    self.end(child, reason.clone(), now);
                }
            }
            let entry = &self.scopes[scope.0];
            let children_ended = entry
                .children
                .iter()
                .all(|child| self.scopes[child.0].ended);
            let itself = match &entry.what {
                What::Process { exited, .. } => *exited,
                What::Worker { over, .. } => *over,
                _ => true,
            };
            if !(children_ended && itself) {
                return;
            }
            self.scopes[scope.0].ended = true;
            self.say_end(scope);
            if scope == ROOT {
                self.finish_over(reason);
                return;
            }
            at = self.scopes[scope.0].parent;
        }
    }

    fn say_end(&mut self, scope: ScopeId) {
        let entry = &mut self.scopes[scope.0];
        if !entry.said {
            entry.said = true;
            let reason = entry.ending.clone().expect("ending");
            self.actions.push(Action::End(scope, reason));
        }
    }

    fn finish_over(&mut self, reason: Reason) {
        if !self.over {
            self.over = true;
            self.awaiting = None;
            self.actions.push(Action::Over(reason));
        }
    }

    /// Something is waited for from `now`, unless it was already.
    fn await_from(&mut self, now: Instant) {
        if self.awaiting.is_none() && !self.over {
            self.awaiting = Some((now, false));
        }
    }

    /// Says what is still awaited, if anything is.
    fn say_awaiting(&mut self) {
        let awaited = self.awaited();
        if let Some((_, said)) = &mut self.awaiting {
            *said = true;
        }
        if !awaited.is_empty() && !self.over {
            self.actions.push(Action::Note(Note::Awaiting(awaited)));
        }
    }

    /// What is ending and not ended, as the user knows it: workers; then
    /// processes not under one; then hosts with nothing else awaited under
    /// them, as one asked to stop that has not said how it did.
    fn awaited(&self) -> Vec<ScopeId> {
        let waiting = |scope: &Scope| scope.ending.is_some() && !scope.ended;
        let under_worker = |mut at: ScopeId| {
            while let Some(parent) = self.scopes[at.0].parent {
                if matches!(self.scopes[parent.0].what, What::Worker { .. }) {
                    return true;
                }
                at = parent;
            }
            false
        };
        let mut awaited: Vec<ScopeId> = Vec::new();
        for (index, scope) in self.scopes.iter().enumerate() {
            let id = ScopeId(index);
            if !waiting(scope) {
                continue;
            }
            match &scope.what {
                What::Worker { over: false, .. } => awaited.push(id),
                What::Process { exited: false, .. } if !under_worker(id) => awaited.push(id),
                _ => {}
            }
        }
        for (index, scope) in self.scopes.iter().enumerate() {
            let id = ScopeId(index);
            if waiting(scope)
                && matches!(scope.what, What::Host { .. })
                && !awaited.iter().any(|awaited| self.is_under(*awaited, id))
            {
                awaited.push(id);
            }
        }
        awaited.sort();
        awaited
    }

    /// Whether `scope` is under `above`.
    fn is_under(&self, scope: ScopeId, above: ScopeId) -> bool {
        let mut at = self.scopes[scope.0].parent;
        while let Some(parent) = at {
            if parent == above {
                return true;
            }
            at = self.scopes[parent.0].parent;
        }
        false
    }

    /// The grace ran out: what still runs is killed, and not waited for,
    /// as what is stuck in the kernel may never end; the monitor ends.
    fn abandon(&mut self, now: Instant) {
        let abandoned: Vec<_> = (0..self.scopes.len())
            .map(ScopeId)
            .filter(|&scope| {
                matches!(
                    self.scopes[scope.0].what,
                    What::Process { exited: false, .. }
                )
            })
            .collect();
        for &scope in &abandoned {
            self.kill_unwaited(scope);
        }
        if !abandoned.is_empty() {
            self.actions
                .push(Action::Note(Note::Abandoned(abandoned.clone())));
        }
        for scope in abandoned {
            self.settle(scope, now);
        }
        self.settle(ROOT, now);
    }
}
