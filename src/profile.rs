//! Where each thread's time goes, kept as a kernel keeps it.
//!
//! A thread of Ferry's registers with a role, such as `walk` or `pump`, and
//! is then always in one [`State`]: running some named work, or waiting for
//! something named. It changes state only at the few places every piece of
//! work passes: a queue, a lock, a connection, a pool's job. There, and only
//! there, the time since the last change goes to the state it leaves, so
//! nothing in between is timed.
//!
//! The state each thread is in is always kept, with a coarse clock, so that a
//! transfer that hangs can say where each thread waits, and for how long; see
//! [`snapshot`]. The time spent in each state is kept only once profiling is
//! [`enable`]d: each change then reads a precise clock, and adds the time to
//! the state's total, its count, and its histogram.
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
    Rename,

    Lock,
    /// Nothing to do: a worker with no job, a pump with nothing to write.
    Work,
    ReadAhead,
    Listing,
    Paced,
    DataQueue,
    ToCompare,
    ToCompress,
    ToDecompress,
    ToReaders,
    ToWriters,
    ToWriter,
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
    pub const ALL: [State; 32] = [
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
        State::Rename,
        State::Lock,
        State::Work,
        State::ReadAhead,
        State::Listing,
        State::Paced,
        State::DataQueue,
        State::ToCompare,
        State::ToCompress,
        State::ToDecompress,
        State::ToReaders,
        State::ToWriters,
        State::ToWriter,
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
            State::Rename => "rename files",
            State::Lock => "wait for a lock",
            State::Work => "wait for work",
            State::ReadAhead => "wait for room to read ahead",
            State::Listing => "wait for the next directory",
            State::Paced => "wait for room for the manifest",
            State::DataQueue => "wait for room for data",
            State::ToCompare => "wait for the comparers",
            State::ToCompress => "wait for the compressors",
            State::ToDecompress => "wait for the decompressors",
            State::ToReaders => "wait for the readers",
            State::ToWriters => "wait for the writers",
            State::ToWriter => "wait for a writer",
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
        self as u8 >= State::Lock as u8
    }

    fn from_index(index: u8) -> State {
        State::ALL[index as usize]
    }
}

const STATES: usize = State::ALL.len();

/// Histogram buckets: bucket `i` counts times of `i` bits in nanoseconds,
/// from 2^(i-1) up to 2^i; the last takes everything longer, from 2^38 ns,
/// about four and a half minutes.
const BUCKETS: usize = 40;

/// Profiling is on: changes are timed.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns profiling on, for every thread of this process.
pub fn enable() {
    ENABLED.store(true, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Nanoseconds on a monotonic clock: precise, or, on Linux, the coarse
/// clock, which costs a few nanoseconds and moves every few milliseconds.
#[cfg(target_os = "linux")]
fn clock(precise: bool) -> u64 {
    let id = if precise {
        rustix::time::ClockId::Monotonic
    } else {
        rustix::time::ClockId::MonotonicCoarse
    };
    let now = rustix::time::clock_gettime(id);
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// On macOS, whose monotonic clock counts only microseconds, the clock
/// `Instant` reads.
#[cfg(target_os = "macos")]
fn clock(_precise: bool) -> u64 {
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
            since: AtomicU64::new(clock(enabled())),
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
        let precise = enabled();
        let now = clock(precise);
        if precise {
            let since = self.since.load(Relaxed);
            self.keep(previous, now.saturating_sub(since));
        }
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
        if enabled() {
            let current = &mut times.states[self.state.load(Relaxed) as usize];
            let nanos = now.saturating_sub(self.since.load(Relaxed));
            current.nanos += nanos;
            current.count += 1;
            current.max = current.max.max(nanos);
            current.histogram[bucket(nanos)] += 1;
        }
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
    slot: Arc<Slot>,
    previous: *const Slot,
}

/// Registers this thread under `role`, starting in [`State::Run`].
pub fn thread(role: &'static str) -> Thread {
    let slot = Arc::new(Slot::new(role));
    REGISTRY
        .lock()
        .expect("not poisoned")
        .live
        .push(Arc::clone(&slot));
    let previous = SLOT.replace(Arc::as_ptr(&slot));
    Thread { slot, previous }
}

impl Drop for Thread {
    fn drop(&mut self) {
        SLOT.set(self.previous);
        let now = clock(true);
        let times = self.slot.read(now);
        let mut registry = REGISTRY.lock().expect("not poisoned");
        registry.live.retain(|slot| !Arc::ptr_eq(slot, &self.slot));
        let role = self.slot.role;
        match registry.ended.iter_mut().find(|(ended, ..)| *ended == role) {
            Some((_, threads, kept)) => {
                *threads += 1;
                kept.add(&times);
            }
            None => registry.ended.push((role, 1, times)),
        }
    }
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

impl StateProfile {
    /// The stay that `fraction` of the stays were no longer than, rounded up
    /// to its histogram bucket.
    pub fn percentile(&self, fraction: f64) -> u64 {
        let total: u64 = self.histogram.iter().sum();
        let wanted = (total as f64 * fraction).ceil() as u64;
        let mut counted = 0;
        for (bucket, count) in self.histogram.iter().enumerate() {
            counted += count;
            if counted >= wanted.max(1) {
                let bound = if bucket == 0 { 0 } else { 1u64 << bucket };
                return bound.min(self.max);
            }
        }
        self.max
    }
}

/// Where this process's threads spent their time so far, those still
/// running and those that ended.
pub fn collect() -> Profile {
    let now = clock(true);
    let registry = REGISTRY.lock().expect("not poisoned");
    let mut roles: Vec<(&'static str, u64, Times)> = registry.ended.clone();
    for slot in &registry.live {
        let times = slot.read(now);
        match roles.iter_mut().find(|(role, ..)| *role == slot.role) {
            Some((_, threads, kept)) => {
                *threads += 1;
                kept.add(&times);
            }
            None => roles.push((slot.role, 1, times)),
        }
    }
    drop(registry);
    Profile {
        roles: roles
            .into_iter()
            .map(|(role, threads, times)| RoleProfile {
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

/// A thread as it is now: its role, its state, and how long it has been in
/// it, by the coarse clock unless profiling is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Now {
    pub role: String,
    pub state: String,
    pub wait: bool,
    pub for_millis: u64,
}

/// Every registered thread as it is now.
pub fn snapshot() -> Vec<Now> {
    let now = clock(enabled());
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
/// in, with how many are, and the longest any has been: `walk ×16: wait for
/// room to read ahead, for up to 12.3 s`.
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
                duration(longest * 1_000_000)
            )
        })
        .collect()
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

/// The profiles of a transfer's sides, for people: each role, and the states
/// that took at least half a percent of its time, the longest first.
pub fn render(sides: &[(String, Profile)]) -> String {
    let mut out = String::new();
    for (side, profile) in sides {
        out.push_str(&format!("{side}\n"));
        for role in &profile.roles {
            let total = role.nanos();
            out.push_str(&format!(
                "  {} ×{}, {} in all\n",
                role.role,
                role.threads,
                duration(total)
            ));
            let mut states: Vec<&StateProfile> = role
                .states
                .iter()
                .filter(|state| total > 0 && state.nanos * 200 >= total)
                .collect();
            states.sort_by_key(|state| std::cmp::Reverse(state.nanos));
            for state in states {
                out.push_str(&format!(
                    "    {:<32} {:>9} {:>4.0}%  {:>11}×  p50 {:>8}  p99 {:>8}  max {:>8}\n",
                    state.state,
                    duration(state.nanos),
                    state.nanos as f64 * 100.0 / total as f64,
                    crate::progress::count(state.count),
                    duration(state.percentile(0.5)),
                    duration(state.percentile(0.99)),
                    duration(state.max),
                ));
            }
        }
    }
    out
}

/// The profiles as JSON, every state with its histogram.
pub fn json(elapsed: Duration, sides: &[(String, Profile)]) -> String {
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
        assert!(State::Lock.is_wait());
        assert!(State::Receiver.is_wait());
    }

    #[test]
    fn time_goes_to_the_state_left() {
        enable();
        std::thread::spawn(|| {
            let _thread = thread("test: states");
            wait(State::Stat, || {
                std::thread::sleep(Duration::from_millis(20));
                // Nested: the inner state takes its own time.
                wait(State::Lock, || {
                    std::thread::sleep(Duration::from_millis(10))
                });
            });
        })
        .join()
        .unwrap();
        let profile = role("test: states");
        assert_eq!(profile.threads, 1);
        let stat = state(&profile, State::Stat);
        let lock = state(&profile, State::Lock);
        assert_eq!(stat.count, 2, "entered, and again after the lock");
        // The run before and after, the last one ended as the thread did.
        assert_eq!(state(&profile, State::Run).count, 2);
        assert_eq!(lock.count, 1);
        assert!(stat.nanos >= 20_000_000, "{}", stat.nanos);
        assert!(stat.nanos < 30_000_000 + 50_000_000, "{}", stat.nanos);
        assert!(lock.nanos >= 10_000_000, "{}", lock.nanos);
        assert!(lock.wait && !stat.wait);
        assert_eq!(lock.histogram.iter().sum::<u64>(), 1);
        let p99 = lock.percentile(0.99);
        assert!(p99 >= lock.max / 2 && p99 <= lock.max, "{p99} {}", lock.max);
    }

    #[test]
    fn threads_of_a_role_add_up_and_live_ones_count() {
        enable();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (ready, readied) = std::sync::mpsc::channel();
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
            now("walk", "wait for work", 1_200),
            now("pump", "wait for work", 5),
            now("walk", "wait for work", 12_300),
        ];
        assert_eq!(
            describe("the Source on ntu", &threads),
            [
                "the Source on ntu: walk ×2: wait for work, for up to 12.30 s",
                "the Source on ntu: pump ×1: wait for work, for 5.0 ms",
            ]
        );
    }

    #[test]
    fn profiles_read_well() {
        let sides = vec![(
            "the Source on \"ntu\"".to_owned(),
            Profile {
                roles: vec![RoleProfile {
                    role: "walk".into(),
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
        assert!(text.contains("walk ×16, 4.00 s in all"), "{text}");
        assert!(text.contains("stat entries"), "{text}");
        assert!(text.contains("75%"), "{text}");
        assert!(text.contains("24,376×"), "{text}");
        let json = json(Duration::from_secs(1), &sides);
        assert!(
            json.contains("\"side\":\"the Source on \\\"ntu\\\"\""),
            "{json}"
        );
        assert!(json.contains("\"threads\":16"), "{json}");
        assert!(json.ends_with("]}]}]}]}\n"), "{json}");
    }
}
