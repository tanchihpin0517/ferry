//! Where each thread's time goes, kept as a kernel keeps it.
//!
//! A thread of Ferry's registers with a role, such as `scan` or `pump`, and
//! is then always in one [`State`]: running some named work, or waiting for
//! something named. It changes state only at the few places every piece of
//! work passes: a queue, a lock, a connection, a pool's job. There, and only
//! there, the time since the last change goes to the state it leaves, so
//! nothing in between is timed.
//!
//! Profiling is always on, as part of what Ferry is: each change reads the
//! clock, and adds the time to the state's total, its count, and its
//! histogram. The client follows each server's times as they go, a second at
//! a time, in [`Metrics`], to see where a transfer is held back, and a
//! transfer that hangs can say where each thread waits, and for how long;
//! see [`snapshot`]. Only tests and benchmarks turn it off, with [`disable`],
//! to measure what it costs: then nothing is kept at all.
//!
//! Each thread writes only its own slot, so threads never contend over it. A
//! slot is folded into its role's totals when its thread ends.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What a thread is doing: running, or waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Work not named below.
    Run,
    List,
    Stat,
    Names,
    Compare,
    Apply,
    ReadFile,
    Compress,
    Decompress,
    WriteFile,
    Fsync,
    Rename,

    /// Nothing to do: a worker with no job, a pump with nothing to write.
    Work,
    Paced,
    DataQueue,
    ToStat,
    ToCompare,
    ToCompress,
    ToDecompress,
    ToReaders,
    ToWriters,
    ToWriter,
    ToSync,
    ToRename,
    ToRenamers,
    ToFinish,
    ToParser,
    Backlog,
    InFlight,
    Result,
    Stream,
    Bytes,
    Outcome,
    ReadConnection,
    WriteConnection,
    ReadPeer,
    WritePeer,
    Receiver,
}

impl State {
    pub const ALL: [State; 38] = [
        State::Run,
        State::List,
        State::Stat,
        State::Names,
        State::Compare,
        State::Apply,
        State::ReadFile,
        State::Compress,
        State::Decompress,
        State::WriteFile,
        State::Fsync,
        State::Rename,
        State::Work,
        State::Paced,
        State::DataQueue,
        State::ToStat,
        State::ToCompare,
        State::ToCompress,
        State::ToDecompress,
        State::ToReaders,
        State::ToWriters,
        State::ToWriter,
        State::ToSync,
        State::ToRename,
        State::ToRenamers,
        State::ToFinish,
        State::ToParser,
        State::Backlog,
        State::InFlight,
        State::Result,
        State::Stream,
        State::Bytes,
        State::Outcome,
        State::ReadConnection,
        State::WriteConnection,
        State::ReadPeer,
        State::WritePeer,
        State::Receiver,
    ];

    pub fn name(self) -> &'static str {
        match self {
            State::Run => "run",
            State::List => "list directories",
            State::Stat => "stat entries",
            State::Names => "look up owners' names",
            State::Compare => "compare",
            State::Apply => "apply comparisons",
            State::ReadFile => "read files",
            State::Compress => "compress",
            State::Decompress => "decompress",
            State::WriteFile => "write files",
            State::Fsync => "fsync files",
            State::Rename => "rename files",
            State::Work => "wait for work",
            State::Paced => "wait for room for messages",
            State::DataQueue => "wait for room for data",
            State::ToStat => "wait for the stat threads",
            State::ToCompare => "wait for the compare thread",
            State::ToCompress => "wait for the compressors",
            State::ToDecompress => "wait for the decompressors",
            State::ToReaders => "wait for the readers",
            State::ToWriters => "wait for the writers",
            State::ToWriter => "wait for a writer",
            State::ToSync => "wait for the fsync threads",
            State::ToRename => "wait for the renamer",
            State::ToRenamers => "wait for the other renamers",
            State::ToFinish => "wait for the directories to be finished",
            State::ToParser => "wait for the parser",
            State::Backlog => "wait for needed files to be sent",
            State::InFlight => "wait for the Destination to compare",
            State::Result => "wait for the next result",
            State::Stream => "wait for the record stream",
            State::Bytes => "wait for a file's bytes",
            State::Outcome => "wait for the writers to finish",
            State::ReadConnection => "read the connection",
            State::WriteConnection => "write the connection",
            State::ReadPeer => "read from the peer",
            State::WritePeer => "write to the peer",
            State::Receiver => "wait for the receiver",
        }
    }

    /// Whether a thread in this state waits, rather than runs.
    pub fn is_wait(self) -> bool {
        self as u8 >= State::Work as u8
    }

    pub(crate) fn from_index(index: u8) -> State {
        State::ALL[index as usize]
    }

    /// The state numbered `index`, if any is: a number another process sent,
    /// which may be none.
    pub fn try_from_index(index: u8) -> Option<State> {
        State::ALL.get(usize::from(index)).copied()
    }
}

const STATES: usize = State::ALL.len();

/// Histogram buckets: bucket `i` counts times of `i` bits in nanoseconds,
/// from 2^(i-1) up to 2^i; the last takes everything longer, from 2^38 ns,
/// about four and a half minutes.
const BUCKETS: usize = 40;

/// Profiling is on: changes are timed.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turns profiling off, for every thread of this process: for measuring
/// what it costs.
pub fn disable() {
    ENABLED.store(false, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Nanoseconds on a monotonic clock.
#[cfg(target_os = "linux")]
fn clock() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// On macOS, whose monotonic clock counts only microseconds, the clock
/// `Instant` reads.
#[cfg(target_os = "macos")]
fn clock() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec to write to.
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut now) };
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// The time a thread spent in one state.
struct Kept {
    nanos: AtomicU64,
    count: AtomicU64,
    max: AtomicU64,
    histogram: [AtomicU64; BUCKETS],
}

impl Default for Kept {
    fn default() -> Self {
        Kept {
            nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
            max: AtomicU64::new(0),
            histogram: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// One thread's state and times. Only its thread writes them: with a load
/// and a store, not a read-modify-write, since no other thread adds.
#[repr(align(128))]
struct Slot {
    role: &'static str,
    state: AtomicU8,
    /// When the thread entered its state, by [`clock`].
    since: AtomicU64,
    kept: [Kept; STATES],
}

/// The histogram bucket of a stay of `nanos`.
fn bucket(nanos: u64) -> usize {
    ((u64::BITS - nanos.leading_zeros()) as usize).min(BUCKETS - 1)
}

fn add(cell: &AtomicU64, amount: u64) {
    cell.store(cell.load(Relaxed) + amount, Relaxed);
}

impl Slot {
    fn new(role: &'static str) -> Self {
        Slot {
            role,
            state: AtomicU8::new(State::Run as u8),
            since: AtomicU64::new(clock()),
            kept: std::array::from_fn(|_| Kept::default()),
        }
    }

    /// Adds `nanos` spent in `state`.
    fn keep(&self, state: u8, nanos: u64) {
        let kept = &self.kept[state as usize];
        add(&kept.nanos, nanos);
        add(&kept.count, 1);
        if nanos > kept.max.load(Relaxed) {
            kept.max.store(nanos, Relaxed);
        }
        add(&kept.histogram[bucket(nanos)], 1);
    }

    /// Leaves the current state for `next`; returns the one left.
    fn change(&self, next: State) -> State {
        let previous = self.state.load(Relaxed);
        let now = clock();
        let since = self.since.load(Relaxed);
        self.keep(previous, now.saturating_sub(since));
        self.state.store(next as u8, Relaxed);
        self.since.store(now, Relaxed);
        State::from_index(previous)
    }

    /// What it kept, with the time in its current state so far, at `now`.
    fn read(&self, now: u64) -> Times {
        let mut times = Times::default();
        for (index, kept) in self.kept.iter().enumerate() {
            let state = &mut times.states[index];
            state.nanos = kept.nanos.load(Relaxed);
            state.count = kept.count.load(Relaxed);
            state.max = kept.max.load(Relaxed);
            for (bucket, count) in kept.histogram.iter().enumerate() {
                state.histogram[bucket] = count.load(Relaxed);
            }
        }
        // The stay so far in the current state counts as if it ended now.
        let current = &mut times.states[self.state.load(Relaxed) as usize];
        let nanos = now.saturating_sub(self.since.load(Relaxed));
        current.nanos += nanos;
        current.count += 1;
        current.max = current.max.max(nanos);
        current.histogram[bucket(nanos)] += 1;
        times
    }
}

/// A role's times, or a thread's, in plain numbers.
#[derive(Clone)]
struct Times {
    states: [StateTimes; STATES],
}

impl Default for Times {
    fn default() -> Self {
        Times {
            states: std::array::from_fn(|_| StateTimes::default()),
        }
    }
}

#[derive(Clone)]
struct StateTimes {
    nanos: u64,
    count: u64,
    max: u64,
    histogram: [u64; BUCKETS],
}

impl Default for StateTimes {
    fn default() -> Self {
        StateTimes {
            nanos: 0,
            count: 0,
            max: 0,
            histogram: [0; BUCKETS],
        }
    }
}

impl Times {
    fn add(&mut self, other: &Times) {
        for (mine, theirs) in self.states.iter_mut().zip(&other.states) {
            mine.nanos += theirs.nanos;
            mine.count += theirs.count;
            mine.max = mine.max.max(theirs.max);
            for (mine, theirs) in mine.histogram.iter_mut().zip(&theirs.histogram) {
                *mine += theirs;
            }
        }
    }
}

/// Every thread registered in this process: those running, and the times of
/// those that ended, by role, in the order roles first registered.
struct Registry {
    live: Vec<Arc<Slot>>,
    ended: Vec<(&'static str, u64, Times)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    live: Vec::new(),
    ended: Vec::new(),
});

thread_local! {
    /// This thread's slot, while it is registered.
    static SLOT: Cell<*const Slot> = const { Cell::new(std::ptr::null()) };
}

/// A thread registered under a role, until this is dropped. It must be
/// dropped on the thread that registered.
#[must_use]
pub struct Thread {
    /// None with profiling off: the thread is not registered.
    slot: Option<Arc<Slot>>,
    previous: *const Slot,
}

/// Registers this thread under `role`, starting in [`State::Run`]; with
/// profiling off, does nothing.
pub fn thread(role: &'static str) -> Thread {
    if !enabled() {
        return Thread {
            slot: None,
            previous: SLOT.get(),
        };
    }
    let slot = Arc::new(Slot::new(role));
    REGISTRY
        .lock()
        .expect("not poisoned")
        .live
        .push(Arc::clone(&slot));
    let previous = SLOT.replace(Arc::as_ptr(&slot));
    Thread {
        slot: Some(slot),
        previous,
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        let Some(slot) = &self.slot else {
            return;
        };
        SLOT.set(self.previous);
        let now = clock();
        let times = slot.read(now);
        let mut registry = REGISTRY.lock().expect("not poisoned");
        registry.live.retain(|live| !Arc::ptr_eq(live, slot));
        let role = slot.role;
        match registry.ended.iter_mut().find(|(ended, ..)| *ended == role) {
            Some((_, threads, kept)) => {
                *threads += 1;
                kept.add(&times);
            }
            None => registry.ended.push((role, 1, times)),
        }
    }
}

/// The role this thread registered under, if it did: none with profiling
/// off.
pub fn current_role() -> Option<&'static str> {
    let slot = SLOT.get();
    // SAFETY: as in `change`: the slot is the one `Thread` registered on this
    // thread, alive until it drops, which sets this back first.
    (!slot.is_null()).then(|| unsafe { &*slot }.role)
}

/// Changes this thread's state to `next`, returning the one it leaves; does
/// nothing on a thread not registered.
fn change(next: State) -> Option<State> {
    let slot = SLOT.get();
    if slot.is_null() {
        return None;
    }
    // SAFETY: the slot is the one `Thread` registered on this thread, which
    // keeps it alive until it drops, and sets this back before it does.
    Some(unsafe { &*slot }.change(next))
}

/// This thread in a state, until this is dropped, when it goes back to the
/// state it was in.
#[must_use]
pub struct Entered {
    previous: Option<State>,
    _thread: PhantomData<*const ()>,
}

/// Puts this thread in `state` until the returned guard drops.
pub fn enter(state: State) -> Entered {
    Entered {
        previous: change(state),
        _thread: PhantomData,
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            change(previous);
        }
    }
}

/// Runs `work` with this thread in `state`.
pub fn wait<T>(state: State, work: impl FnOnce() -> T) -> T {
    let _entered = enter(state);
    work()
}

/// Where a process's threads spent their time, by role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub roles: Vec<RoleProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProfile {
    pub role: String,
    /// How many threads had the role, one after another or at once.
    pub threads: u64,
    /// The states its threads were in, leaving out those never entered.
    pub states: Vec<StateProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateProfile {
    pub state: String,
    pub wait: bool,
    /// The time in it, added over the role's threads.
    pub nanos: u64,
    /// How many stays in it, the one under way included.
    pub count: u64,
    /// The longest stay.
    pub max: u64,
    /// See [`BUCKETS`]; trailing empty buckets left out.
    pub histogram: Vec<u64>,
}

impl RoleProfile {
    /// The time its threads spent, in every state.
    pub fn nanos(&self) -> u64 {
        self.states.iter().map(|state| state.nanos).sum()
    }
}

/// Each role's times so far, of its threads still running and those that
/// ended: the role, how many threads it has had, how many run now, and the
/// times.
fn totals() -> Vec<(&'static str, u64, u64, Times)> {
    let now = clock();
    let registry = REGISTRY.lock().expect("not poisoned");
    let mut roles: Vec<(&'static str, u64, u64, Times)> = registry
        .ended
        .iter()
        .map(|(role, threads, times)| (*role, *threads, 0, times.clone()))
        .collect();
    for slot in &registry.live {
        let times = slot.read(now);
        match roles.iter_mut().find(|(role, ..)| *role == slot.role) {
            Some((_, threads, live, kept)) => {
                *threads += 1;
                *live += 1;
                kept.add(&times);
            }
            None => roles.push((slot.role, 1, 1, times)),
        }
    }
    roles
}

/// Where this process's threads spent their time so far, those still
/// running and those that ended.
pub fn collect() -> Profile {
    Profile {
        roles: totals()
            .into_iter()
            .map(|(role, threads, _, times)| RoleProfile {
                role: role.to_owned(),
                threads,
                states: State::ALL
                    .iter()
                    .zip(times.states)
                    .filter(|(_, times)| times.nanos > 0 || times.count > 0)
                    .map(|(state, times)| {
                        let used = BUCKETS
                            - times
                                .histogram
                                .iter()
                                .rev()
                                .take_while(|&&count| count == 0)
                                .count();
                        StateProfile {
                            state: state.name().to_owned(),
                            wait: state.is_wait(),
                            nanos: times.nanos,
                            count: times.count,
                            max: times.max,
                            histogram: times.histogram[..used].to_vec(),
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Where a process's threads spent an interval, by role, and how full its
/// queues were: what the client follows as a transfer goes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    /// Counts the intervals, from 1.
    pub sequence: u64,
    /// How long the interval was.
    pub nanos: u64,
    pub roles: Vec<RoleMetrics>,
    pub queues: Vec<QueueMetrics>,
}

/// How full a queue was: what waited in it at the interval's end, the most
/// that did during it, and its room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMetrics {
    pub name: String,
    pub level: u64,
    pub peak: u64,
    /// Its room; 0 for a queue with no bound.
    pub capacity: u64,
    /// Whether it counts bytes, rather than things.
    pub bytes: bool,
}

/// How full a side's queues were at a moment of a transfer, as the client
/// heard: for the profile file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSample {
    pub side: String,
    /// Since the transfer started.
    pub at_nanos: u64,
    pub queues: Vec<QueueMetrics>,
}

/// How full a queue is: what waits in it, the most that has since its peak
/// was last taken, and the room it has. Every gauge is listed, for each
/// interval's [`Metrics`] to say how full it was; clones measure the same
/// queue.
#[derive(Clone)]
pub struct Gauge(Arc<GaugeLevel>);

struct GaugeLevel {
    name: String,
    level: AtomicU64,
    peak: AtomicU64,
    capacity: u64,
    bytes: bool,
}

/// Every gauge, while it is in use.
static GAUGES: Mutex<Vec<std::sync::Weak<GaugeLevel>>> = Mutex::new(Vec::new());

impl Gauge {
    /// A gauge for a queue called `name`, with room for `capacity` things, or
    /// no bound if 0.
    pub fn new(name: impl Into<String>, capacity: usize) -> Self {
        Self::listed(name.into(), capacity as u64, false)
    }

    /// A gauge for a queue of `capacity` bytes, or no bound if 0.
    pub fn of_bytes(name: impl Into<String>, capacity: usize) -> Self {
        Self::listed(name.into(), capacity as u64, true)
    }

    fn listed(name: String, capacity: u64, bytes: bool) -> Self {
        let level = Arc::new(GaugeLevel {
            name,
            level: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            capacity,
            bytes,
        });
        let mut gauges = GAUGES.lock().expect("not poisoned");
        gauges.retain(|gauge| gauge.strong_count() > 0);
        gauges.push(Arc::downgrade(&level));
        Self(level)
    }

    /// One thing went into the queue.
    pub fn up(&self) {
        self.add(1);
    }

    /// One thing came out of it; says how many are left.
    pub fn down(&self) -> u64 {
        self.sub(1)
    }

    /// `amount` went into the queue.
    pub fn add(&self, amount: u64) {
        let level = self.0.level.fetch_add(amount, Relaxed) + amount;
        self.0.peak.fetch_max(level, Relaxed);
    }

    /// `amount` came out of it, or what is left if less; says how much is
    /// left.
    pub fn sub(&self, amount: u64) -> u64 {
        match self
            .0
            .level
            .try_update(Relaxed, Relaxed, |level| Some(level.saturating_sub(amount)))
        {
            Ok(was) | Err(was) => was.saturating_sub(amount),
        }
    }

    /// How much waits in it now.
    pub fn set(&self, level: usize) {
        let level = level as u64;
        self.0.level.store(level, Relaxed);
        self.0.peak.fetch_max(level, Relaxed);
    }

    /// How much waits in it now.
    pub fn level(&self) -> usize {
        self.0.level.load(Relaxed) as usize
    }

    /// How full it is, in percent; 0 for a queue with no bound.
    pub fn percent(&self) -> u8 {
        if self.0.capacity == 0 {
            return 0;
        }
        let level = self.0.level.load(Relaxed).min(self.0.capacity);
        (level * 100 / self.0.capacity) as u8
    }
}

/// How full every gauge in use is, each one's peak taken: from now, its
/// peak is what waits in it.
fn queues() -> Vec<QueueMetrics> {
    let mut gauges = GAUGES.lock().expect("not poisoned");
    gauges.retain(|gauge| gauge.strong_count() > 0);
    gauges
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .map(|gauge| {
            let level = gauge.level.load(Relaxed);
            let peak = gauge.peak.swap(level, Relaxed).max(level);
            QueueMetrics {
                name: gauge.name.clone(),
                level,
                peak,
                capacity: gauge.capacity,
                bytes: gauge.bytes,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleMetrics {
    pub role: String,
    /// How many of its threads run at the interval's end.
    pub threads: u64,
    /// The states its threads were in during the interval, by their index
    /// in [`State::ALL`]: a client and its servers are the same build. Each
    /// with the time in it, added over the threads, and how many stays.
    pub states: Vec<(u8, u64, u64)>,
}

impl RoleMetrics {
    /// The time in `state`.
    pub fn nanos(&self, state: State) -> u64 {
        self.states
            .iter()
            .find(|(index, ..)| *index == state as u8)
            .map_or(0, |(_, nanos, _)| *nanos)
    }
}

/// Takes [`Metrics`] interval by interval: what each role's times grew by
/// since the last.
pub struct Meter {
    sequence: u64,
    at: u64,
    last: Vec<(&'static str, [(u64, u64); STATES])>,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    /// A meter whose first interval starts now.
    pub fn new() -> Self {
        let mut meter = Meter {
            sequence: 0,
            at: clock(),
            last: Vec::new(),
        };
        meter.last = Self::sums(&totals());
        meter
    }

    fn sums(
        totals: &[(&'static str, u64, u64, Times)],
    ) -> Vec<(&'static str, [(u64, u64); STATES])> {
        totals
            .iter()
            .map(|(role, _, _, times)| {
                (
                    *role,
                    std::array::from_fn(|index| {
                        (times.states[index].nanos, times.states[index].count)
                    }),
                )
            })
            .collect()
    }

    /// The interval since the last, which ends now.
    pub fn take(&mut self) -> Metrics {
        let now = clock();
        let totals = totals();
        let sums = Self::sums(&totals);
        let mut roles = Vec::new();
        for ((role, now_sums), (_, _, live, _)) in sums.iter().zip(&totals) {
            let before = self
                .last
                .iter()
                .find(|(last, _)| last == role)
                .map(|(_, sums)| *sums);
            let states = (0..STATES)
                .filter_map(|index| {
                    let (nanos, count) = now_sums[index];
                    let (was_nanos, was_count) = before.map_or((0, 0), |sums| sums[index]);
                    let (nanos, count) = (
                        nanos.saturating_sub(was_nanos),
                        count.saturating_sub(was_count),
                    );
                    (nanos > 0 || count > 0).then_some((index as u8, nanos, count))
                })
                .collect::<Vec<_>>();
            if !states.is_empty() || *live > 0 {
                roles.push(RoleMetrics {
                    role: (*role).to_owned(),
                    threads: *live,
                    states,
                });
            }
        }
        self.sequence += 1;
        let metrics = Metrics {
            sequence: self.sequence,
            nanos: now.saturating_sub(self.at),
            roles,
            queues: queues(),
        };
        self.at = now;
        self.last = sums;
        metrics
    }
}

/// A thread as it is now: its role, its state, and how long it has been in
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Now {
    pub role: String,
    pub state: String,
    pub wait: bool,
    pub for_millis: u64,
}

/// Every registered thread as it is now; none with profiling off.
pub fn snapshot() -> Vec<Now> {
    let now = clock();
    let registry = REGISTRY.lock().expect("not poisoned");
    registry
        .live
        .iter()
        .map(|slot| {
            let state = State::from_index(slot.state.load(Relaxed));
            Now {
                role: slot.role.to_owned(),
                state: state.name().to_owned(),
                wait: state.is_wait(),
                for_millis: now.saturating_sub(slot.since.load(Relaxed)) / 1_000_000,
            }
        })
        .collect()
}

/// A snapshot for people, a line for each role and state that threads are
/// in, with how many are, and the longest any has been: `stat ×8: wait for
/// work, for up to 12.30 s`.
pub fn describe(side: &str, threads: &[Now]) -> Vec<String> {
    let mut groups: Vec<(&str, &str, u64, u64)> = Vec::new();
    for thread in threads {
        match groups
            .iter_mut()
            .find(|(role, state, ..)| *role == thread.role && *state == thread.state)
        {
            Some((.., count, longest)) => {
                *count += 1;
                *longest = (*longest).max(thread.for_millis);
            }
            None => groups.push((&thread.role, &thread.state, 1, thread.for_millis)),
        }
    }
    groups
        .into_iter()
        .map(|(role, state, count, longest)| {
            let up_to = if count > 1 { "up to " } else { "" };
            format!(
                "{side}: {role} ×{count}: {state}, for {up_to}{}",
                millis(longest)
            )
        })
        .collect()
}

/// A time in milliseconds for people: `5 ms`, `12.30 s`.
fn millis(millis: u64) -> String {
    if millis < 1_000 {
        format!("{millis} ms")
    } else {
        format!("{:.2} s", millis as f64 / 1e3)
    }
}

/// A time for people: `850 ns`, `12.4 µs`, `3.1 ms`, `2.48 s`.
pub fn duration(nanos: u64) -> String {
    let duration = Duration::from_nanos(nanos);
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.1} µs", nanos as f64 / 1e3)
    } else if nanos < 1_000_000_000 {
        format!("{:.1} ms", nanos as f64 / 1e6)
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

/// The profiles of a transfer's sides, for people, in short: each role on a
/// line, with its time and the states that took at least a twentieth of it,
/// the longest first, four at most. The rest, with each state's histogram,
/// is in [`json`].
pub fn render(sides: &[(String, Profile)]) -> String {
    let mut out = String::new();
    for (side, profile) in sides {
        out.push_str(&format!("{side}\n"));
        for role in &profile.roles {
            let total = role.nanos();
            let mut states: Vec<&StateProfile> = role
                .states
                .iter()
                .filter(|state| total > 0 && state.nanos * 20 >= total)
                .collect();
            states.sort_by_key(|state| std::cmp::Reverse(state.nanos));
            let shown: Vec<String> = states
                .iter()
                .take(4)
                .map(|state| {
                    format!(
                        "{} {:.0}%",
                        state.state,
                        state.nanos as f64 * 100.0 / total as f64
                    )
                })
                .collect();
            out.push_str(&format!(
                "  {} ×{}, {}: {}\n",
                role.role,
                role.threads,
                duration(total),
                shown.join(" · ")
            ));
        }
    }
    out
}

/// The profiles as JSON, every state with its histogram, and how full each
/// side's queues were, second by second.
pub fn json(elapsed: Duration, sides: &[(String, Profile)], queues: &[QueueSample]) -> String {
    let mut out = format!(
        "{{\"elapsed_nanos\":{},\"buckets\":\"bucket i counts stays from 2^(i-1) to 2^i ns\",\"sides\":[",
        elapsed.as_nanos()
    );
    for (index, (side, profile)) in sides.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"side\":{},\"roles\":[", quoted(side)));
        for (index, role) in profile.roles.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"role\":{},\"threads\":{},\"states\":[",
                quoted(&role.role),
                role.threads
            ));
            for (index, state) in role.states.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                let histogram: Vec<String> = state
                    .histogram
                    .iter()
                    .map(|count| count.to_string())
                    .collect();
                out.push_str(&format!(
                    "{{\"state\":{},\"wait\":{},\"nanos\":{},\"count\":{},\"max_nanos\":{},\"histogram\":[{}]}}",
                    quoted(&state.state),
                    state.wait,
                    state.nanos,
                    state.count,
                    state.max,
                    histogram.join(",")
                ));
            }
            out.push_str("]}");
        }
        out.push_str("]}");
    }
    // How full each side's queues were, second by second.
    out.push_str("],\"queues\":[");
    for (index, sample) in queues.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let levels: Vec<String> = sample
            .queues
            .iter()
            .map(|queue| {
                format!(
                    "{{\"name\":{},\"level\":{},\"peak\":{},\"capacity\":{},\"bytes\":{}}}",
                    quoted(&queue.name),
                    queue.level,
                    queue.peak,
                    queue.capacity,
                    queue.bytes
                )
            })
            .collect();
        out.push_str(&format!(
            "{{\"side\":{},\"at_nanos\":{},\"queues\":[{}]}}",
            quoted(&sample.side),
            sample.at_nanos,
            levels.join(",")
        ));
    }
    out.push_str("]}\n");
    out
}

/// A JSON string.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held by each test that takes a meter: a meter's take starts every
    /// queue's next peak, so two at once would take each other's.
    static METERED: Mutex<()> = Mutex::new(());

    /// The profile of `role` alone: other tests register threads too.
    fn role(role: &str) -> RoleProfile {
        collect()
            .roles
            .into_iter()
            .find(|profile| profile.role == role)
            .unwrap()
    }

    fn state(role: &RoleProfile, state: State) -> &StateProfile {
        role.states
            .iter()
            .find(|profile| profile.state == state.name())
            .unwrap()
    }

    #[test]
    fn every_state_is_listed_in_order() {
        for (index, state) in State::ALL.iter().enumerate() {
            assert_eq!(*state as usize, index);
        }
        assert!(!State::Rename.is_wait());
        assert!(State::Work.is_wait());
        assert!(State::Receiver.is_wait());
    }

    #[test]
    fn time_goes_to_the_state_left() {
        std::thread::spawn(|| {
            let _thread = thread("test: states");
            wait(State::Stat, || {
                std::thread::sleep(Duration::from_millis(20));
                // Nested: the inner state takes its own time.
                wait(State::Work, || {
                    std::thread::sleep(Duration::from_millis(10))
                });
            });
        })
        .join()
        .unwrap();
        let profile = role("test: states");
        assert_eq!(profile.threads, 1);
        let stat = state(&profile, State::Stat);
        let lock = state(&profile, State::Work);
        assert_eq!(stat.count, 2, "entered, and again after the lock");
        // The run before and after, the last one ended as the thread did.
        assert_eq!(state(&profile, State::Run).count, 2);
        assert_eq!(lock.count, 1);
        assert!(stat.nanos >= 20_000_000, "{}", stat.nanos);
        assert!(lock.nanos >= 10_000_000, "{}", lock.nanos);
        assert!(lock.wait && !stat.wait);
        assert_eq!(lock.histogram.iter().sum::<u64>(), 1);
    }

    #[test]
    fn a_meter_takes_what_each_interval_added() {
        let _metered = METERED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (step, stepped) = crossbeam_channel::unbounded::<()>();
        // Each stay, as long as the worker found it: a sleep may well be
        // longer than asked, on a busy machine.
        let (done, finished) = crossbeam_channel::unbounded::<Duration>();
        let worker = std::thread::spawn(move || {
            let _thread = thread("test: meter");
            for _ in 0..2 {
                stepped.recv().unwrap();
                let started = std::time::Instant::now();
                wait(State::Compress, || {
                    std::thread::sleep(Duration::from_millis(10))
                });
                done.send(started.elapsed()).unwrap();
            }
            stepped.recv().unwrap();
        });
        let mut meter = Meter::new();
        let compress = |metrics: &Metrics| {
            metrics
                .roles
                .iter()
                .find(|role| role.role == "test: meter")
                .map(|role| (role.threads, role.nanos(State::Compress)))
        };
        step.send(()).unwrap();
        finished.recv().unwrap();
        let first = meter.take();
        let (threads, nanos) = compress(&first).unwrap();
        assert_eq!(threads, 1);
        assert!(nanos >= 10_000_000, "{nanos}");
        step.send(()).unwrap();
        let stayed = finished.recv().unwrap().as_nanos() as u64;
        let second = meter.take();
        let (_, again) = compress(&second).unwrap();
        // Only the second stay, at most as long as the worker found it: the
        // first, at least 10 ms more, was in the first interval.
        assert!(
            (10_000_000..=stayed + 5_000_000).contains(&again),
            "{again} {stayed} {nanos}"
        );
        assert_eq!(second.sequence, first.sequence + 1);
        step.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn threads_of_a_role_add_up_and_live_ones_count() {
        let (stop, stopped) = crossbeam_channel::unbounded::<()>();
        let (ready, readied) = crossbeam_channel::unbounded();
        let live = std::thread::spawn(move || {
            let _thread = thread("test: role");
            ready.send(()).unwrap();
            wait(State::Work, || stopped.recv().unwrap());
        });
        for _ in 0..3 {
            std::thread::spawn(|| {
                let _thread = thread("test: role");
                wait(State::Compress, || {});
            })
            .join()
            .unwrap();
        }
        readied.recv().unwrap();
        let now = snapshot();
        assert!(
            now.iter()
                .any(|thread| thread.role == "test: role" && thread.state == "wait for work"),
            "{now:?}"
        );
        let profile = role("test: role");
        assert_eq!(profile.threads, 4);
        assert_eq!(state(&profile, State::Compress).count, 3);
        stop.send(()).unwrap();
        live.join().unwrap();
    }

    #[test]
    fn an_unregistered_thread_is_left_alone() {
        std::thread::spawn(|| {
            let _entered = enter(State::Stat);
            assert!(SLOT.get().is_null());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_snapshot_reads_well() {
        let now = |role: &str, state: &str, for_millis| Now {
            role: role.into(),
            state: state.into(),
            wait: true,
            for_millis,
        };
        let threads = [
            now("stat", "wait for work", 1_200),
            now("pump", "wait for work", 5),
            now("stat", "wait for work", 12_300),
        ];
        assert_eq!(
            describe("the Source on ntu", &threads),
            [
                "the Source on ntu: stat ×2: wait for work, for up to 12.30 s",
                "the Source on ntu: pump ×1: wait for work, for 5 ms",
            ]
        );
    }

    #[test]
    fn profiles_read_well() {
        let sides = vec![(
            "the Source on \"ntu\"".to_owned(),
            Profile {
                roles: vec![RoleProfile {
                    role: "stat".into(),
                    threads: 16,
                    states: vec![
                        StateProfile {
                            state: "stat entries".into(),
                            wait: false,
                            nanos: 3_000_000_000,
                            count: 24_376,
                            max: 2_000_000,
                            histogram: vec![
                                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
                            ],
                        },
                        StateProfile {
                            state: "wait for a lock".into(),
                            wait: true,
                            nanos: 1_000_000_000,
                            count: 3,
                            max: 500_000_000,
                            histogram: vec![],
                        },
                    ],
                }],
            },
        )];
        let text = render(&sides);
        assert_eq!(
            text,
            "the Source on \"ntu\"\n  stat ×16, 4.00 s: stat entries 75% · wait for a lock 25%\n"
        );
        let queues = [QueueSample {
            side: "the client".to_owned(),
            at_nanos: 1_000_000_000,
            queues: vec![QueueMetrics {
                name: "Source send path 1".to_owned(),
                level: 3,
                peak: 8,
                capacity: 8,
                bytes: false,
            }],
        }];
        let json = json(Duration::from_secs(1), &sides, &queues);
        assert!(
            json.contains("\"side\":\"the Source on \\\"ntu\\\"\""),
            "{json}"
        );
        assert!(json.contains("\"threads\":16"), "{json}");
        assert!(
            json.ends_with(
                "\"queues\":[{\"side\":\"the client\",\"at_nanos\":1000000000,\"queues\":\
                 [{\"name\":\"Source send path 1\",\"level\":3,\"peak\":8,\"capacity\":8,\
                 \"bytes\":false}]}]}\n"
            ),
            "{json}"
        );
    }

    #[test]
    fn a_queue_s_level_and_peak_go_into_each_interval() {
        let _metered = METERED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gauge = Gauge::new("a queue of this test", 8);
        let mut meter = Meter::new();
        let find = |metrics: &Metrics| {
            metrics
                .queues
                .iter()
                .find(|queue| queue.name == "a queue of this test")
                .cloned()
                .unwrap()
        };
        gauge.up();
        gauge.up();
        gauge.up();
        assert_eq!(gauge.down(), 2);
        assert_eq!(gauge.down(), 1);
        let first = find(&meter.take());
        assert_eq!((first.level, first.peak, first.capacity), (1, 3, 8));
        // The next interval's peak starts from what waits.
        let second = find(&meter.take());
        assert_eq!((second.level, second.peak), (1, 1));
        // A gauge no longer in use is no longer listed.
        drop(gauge);
        assert!(
            meter
                .take()
                .queues
                .iter()
                .all(|queue| queue.name != "a queue of this test")
        );
    }
}
