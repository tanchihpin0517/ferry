//! The session against another, over streams made up, as the draft's A2
//! asks: frames come late, streams die, both ways or one, and are joined
//! again, all from a seed, the same each time; and its health, on events
//! made up, at times given.

use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BinaryHeap, HashSet};
use std::hash::{Hash, Hasher};

use super::*;

/// Random numbers from a seed, the same each time for the same seed:
/// xorshift64*.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// True `per_thousand` times in a thousand.
    fn chance(&mut self, per_thousand: u64) -> bool {
        self.below(1000) < per_thousand
    }

    fn millis(&mut self, low: u64, high: u64) -> Duration {
        Duration::from_millis(low + self.below(high - low + 1))
    }
}

/// The session both sides are of.
const ID: [u8; 16] = [7; 16];

/// What happens next, in the simulation.
#[derive(Debug)]
enum Happens {
    /// A frame comes to a side on a stream.
    Comes(usize, StreamId, Frame),
    /// A side's writer wrote what it was handed.
    Wrote(usize, StreamId, usize),
    /// A side finds a stream broken.
    Breaks(usize, StreamId),
    /// A stream is joined to both sides, as the client would join one.
    Join(StreamId),
    /// A side is told the time.
    Tick(usize),
    /// A side's peer sends its next control message or block.
    Sends(usize),
    /// A stream alive, chosen then, dies under both sides, or one way only:
    /// what is on its way is lost.
    Dies,
}

/// One way of a stream: when the last frame on it comes, and from when it
/// carries nothing more: what would come after is lost.
#[derive(Clone, Copy)]
struct Way {
    last: Duration,
    dead_from: Option<Duration>,
}

impl Way {
    fn carries(&self, at: Duration) -> bool {
        self.dead_from.is_none_or(|dead| at <= dead)
    }
}

/// A side: its session, and its peer, which sends control messages and
/// blocks, keeps what it is handed, and closes once done.
struct Side {
    session: Session,
    /// What it is to send, and has sent.
    controls: usize,
    blocks: Vec<usize>,
    sent_controls: usize,
    sent_blocks: usize,
    asked_to_close: bool,
    closed: bool,
    lost: bool,
    got_controls: Vec<Vec<u8>>,
    got_blocks: Vec<Vec<u8>>,
    /// When it is to be told the time next.
    tick_at: Option<Duration>,
}

fn control(side: usize, n: usize) -> Vec<u8> {
    format!("control {n} of side {side}").into_bytes()
}

/// Block `n` of `side`'s, of `len` bytes, at least 16: told from every
/// other by its first.
fn block(side: usize, n: usize, len: usize) -> Vec<u8> {
    let mut bytes = vec![(n * 7 + side) as u8; len];
    bytes[..8].copy_from_slice(&(side as u64).to_le_bytes());
    bytes[8..16].copy_from_slice(&(n as u64).to_le_bytes());
    bytes
}

/// Two sessions, against each other, over streams of the simulation.
struct Sim {
    random: Random,
    start: Instant,
    /// What happens, by when, and in the order it was scheduled at a time.
    queue: BinaryHeap<Reverse<(Duration, u64)>>,
    happenings: HashMap<u64, Happens>,
    order: u64,
    /// Each stream's two ways, by the side that writes it.
    ways: HashMap<StreamId, [Way; 2]>,
    sides: [Side; 2],
    next_stream: StreamId,
    /// The streams the client heard broke, each joined again once.
    replaced: HashSet<StreamId>,
    /// What happens is printed, as `SIM_TRACE` asks.
    printed: bool,
    /// The client joins another stream when it hears one broke.
    rejoins: bool,
    /// The streams each side closed: none is written after.
    closed: [HashSet<StreamId>; 2],
    /// How long a peer takes between what it sends, at least and at most,
    /// in milliseconds.
    pace: (u64, u64),
    /// Each stream that dies dies both ways, and both sides find it soon,
    /// rather than some only by its silence; and the client joins another
    /// sooner.
    found: bool,
    breaks: usize,
    trace: DefaultHasher,
}

/// What a run came to.
struct Outcome {
    /// Every command each session gave, hashed: the same for the same seed.
    trace: u64,
    closed: [bool; 2],
    lost: [bool; 2],
    got_controls: [Vec<Vec<u8>>; 2],
    got_blocks: [Vec<Vec<u8>>; 2],
    sent_blocks: [Vec<usize>; 2],
    controls: [usize; 2],
    breaks: usize,
}

impl Sim {
    /// Two sides, from `seed`: each to send `controls` control messages,
    /// and side 0 `blocks` blocks of up to 200 KiB, side 1 a fourth as
    /// many; `streams` streams joined at the start.
    fn new(seed: u64, streams: StreamId, controls: usize, blocks: usize) -> Self {
        let mut random = Random::new(seed);
        let start = Instant::now();
        let side = |random: &mut Random, controls: usize, blocks: usize| Side {
            session: Session::new(ID, start),
            controls,
            blocks: (0..blocks)
                .map(|_| 16 + random.below(200 << 10) as usize)
                .collect(),
            sent_controls: 0,
            sent_blocks: 0,
            asked_to_close: false,
            closed: false,
            lost: false,
            got_controls: Vec::new(),
            got_blocks: Vec::new(),
            tick_at: None,
        };
        let sides = [
            side(&mut random, controls, blocks),
            side(&mut random, controls + 1, blocks / 4),
        ];
        let mut sim = Self {
            random,
            start,
            queue: BinaryHeap::new(),
            happenings: HashMap::new(),
            order: 0,
            ways: HashMap::new(),
            sides,
            next_stream: 0,
            replaced: HashSet::new(),
            printed: std::env::var_os("SIM_TRACE").is_some(),
            rejoins: true,
            closed: [HashSet::new(), HashSet::new()],
            pace: (0, 4),
            found: false,
            breaks: 0,
            trace: DefaultHasher::new(),
        };
        for _ in 0..streams {
            let at = sim.random.millis(0, 5);
            sim.join_later(at);
        }
        for side in 0..2 {
            let at = sim.random.millis(0, 3);
            sim.schedule(at, Happens::Sends(side));
        }
        sim
    }

    fn schedule(&mut self, at: Duration, what: Happens) {
        self.happenings.insert(self.order, what);
        self.queue.push(Reverse((at, self.order)));
        self.order += 1;
    }

    /// The client joins a new stream at `at`.
    fn join_later(&mut self, at: Duration) {
        let stream = self.next_stream;
        self.next_stream += 1;
        self.schedule(at, Happens::Join(stream));
    }

    /// The peers take `low` to `high` milliseconds between what they send.
    fn paced(mut self, low: u64, high: u64) -> Self {
        self.pace = (low, high);
        self
    }

    /// Each stream that dies is found soon by both sides, and another joined
    /// soon.
    fn found(mut self) -> Self {
        self.found = true;
        self
    }

    /// The client joins no stream after one breaks.
    fn gives_up(mut self) -> Self {
        self.rejoins = false;
        self
    }

    /// Streams die now and then, `count` of them, each `low` to `high`
    /// milliseconds after the one before.
    fn breaking(&mut self, count: usize, low: u64, high: u64) {
        let mut at = Duration::ZERO;
        for _ in 0..count {
            at += self.random.millis(low, high);
            self.schedule(at, Happens::Dies);
        }
    }

    /// Runs until both sides are closed, or one is lost, or simulated
    /// `limit` passes.
    fn run(mut self, limit: Duration) -> Outcome {
        while let Some(Reverse((at, order))) = self.queue.pop() {
            let done = self.sides.iter().all(|side| side.closed || side.lost);
            if at > limit || done {
                break;
            }
            let what = self.happenings.remove(&order).expect("once");
            self.happen(what, at);
        }
        let [zero, one] = self.sides;
        Outcome {
            trace: self.trace.finish(),
            closed: [zero.closed, one.closed],
            lost: [zero.lost, one.lost],
            controls: [zero.controls, one.controls],
            sent_blocks: [zero.blocks, one.blocks],
            got_controls: [zero.got_controls, one.got_controls],
            got_blocks: [zero.got_blocks, one.got_blocks],
            breaks: self.breaks,
        }
    }

    fn happen(&mut self, what: Happens, at: Duration) {
        if self.printed {
            eprintln!("{at:?} happens {what:?}");
        }
        match what {
            Happens::Comes(side, stream, frame) => {
                // Lost, if its way died before it came.
                if self.ways[&stream][1 - side].carries(at) {
                    self.tell(side, Event::Frame(stream, frame), at);
                }
            }
            Happens::Wrote(side, stream, bytes) => self.tell(side, Event::Wrote(stream, bytes), at),
            Happens::Breaks(side, stream) => self.tell(side, Event::Broken(stream), at),
            Happens::Join(stream) => {
                let way = Way {
                    last: at,
                    dead_from: None,
                };
                self.ways.insert(stream, [way; 2]);
                // Each server is handed its end.
                self.tell(0, Event::Joined(stream), at);
                self.tell(1, Event::Joined(stream), at);
            }
            Happens::Tick(side) => {
                if self.sides[side].tick_at == Some(at) {
                    self.sides[side].tick_at = None;
                    self.tell(side, Event::Tick, at);
                }
            }
            Happens::Sends(side) => self.sends(side, at),
            Happens::Dies => {
                let mut alive: Vec<StreamId> = self
                    .ways
                    .iter()
                    .filter(|(_, both)| both.iter().all(|way| way.dead_from.is_none()))
                    .map(|(&stream, _)| stream)
                    .collect();
                if alive.is_empty() {
                    return;
                }
                alive.sort_unstable();
                let stream = alive[self.random.below(alive.len() as u64) as usize];
                // Both ways, mostly; or one, which only silence tells.
                let ways = if self.found || self.random.chance(700) {
                    vec![0, 1]
                } else {
                    vec![self.random.below(2) as usize]
                };
                for &way in &ways {
                    self.ways.get_mut(&stream).expect("there")[way].dead_from = Some(at);
                }
                if ways.len() == 2 {
                    // Each side finds it soon, or only by its silence.
                    for side in 0..2 {
                        if self.found || self.random.chance(700) {
                            let after = self.random.millis(1, 500);
                            self.schedule(at + after, Happens::Breaks(side, stream));
                        }
                    }
                }
            }
        }
    }

    /// A side's peer sends its next control message or block, at random,
    /// and schedules the next.
    fn sends(&mut self, side: usize, at: Duration) {
        let peer = &self.sides[side];
        let controls_left = peer.sent_controls < peer.controls;
        let blocks_left = peer.sent_blocks < peer.blocks.len();
        if !controls_left && !blocks_left {
            return;
        }
        let event = if controls_left && (!blocks_left || self.random.chance(500)) {
            let n = self.sides[side].sent_controls;
            self.sides[side].sent_controls += 1;
            Event::Control(control(side, n))
        } else {
            let n = self.sides[side].sent_blocks;
            self.sides[side].sent_blocks += 1;
            Event::Block(Bytes::new(block(side, n, self.sides[side].blocks[n])))
        };
        let after = self.random.millis(self.pace.0, self.pace.1);
        self.schedule(at + after, Happens::Sends(side));
        self.tell(side, event, at);
    }

    /// Tells a side's session an event, then that nothing more is to be
    /// told now, and carries out what it says.
    fn tell(&mut self, side: usize, event: Event, at: Duration) {
        let now = self.start + at;
        let mut commands = self.sides[side].session.on(event, now);
        commands.extend(self.sides[side].session.on(Event::Idle, now));
        self.carry_out(side, commands, at);
    }

    /// Carries out what a side's session said: frames go on their way, or
    /// are lost on a way dead; the peer keeps what it is handed, and closes
    /// once it has sent all and had all; and the client joins a new stream a
    /// while after it hears one broke, once for each.
    fn carry_out(&mut self, side: usize, commands: Vec<Command>, at: Duration) {
        let other = 1 - side;
        for command in commands {
            if self.printed {
                eprintln!("{at:?}   {side} does {command:?}");
            }
            format!("{side} {at:?} {command:?}").hash(&mut self.trace);
            match command {
                Command::Write(stream, frame) => {
                    assert!(
                        !self.closed[side].contains(&stream),
                        "{side} wrote on {stream}, closed"
                    );
                    let len = frame.size();
                    let arrives = at + self.random.millis(1, 30);
                    let way = &mut self.ways.get_mut(&stream).expect("joined")[side];
                    let arrives = way.last.max(arrives);
                    way.last = arrives;
                    if way.carries(at) {
                        self.schedule(arrives, Happens::Comes(other, stream, frame));
                        let after = self.random.millis(0, 3);
                        self.schedule(at + after, Happens::Wrote(side, stream, len));
                    } else if self.random.chance(300) {
                        // Lost; the writer may find it broken.
                        let after = self.random.millis(1, 20);
                        self.schedule(at + after, Happens::Breaks(side, stream));
                    }
                }
                Command::Close(stream) => {
                    // Its other end reads its end once what was on the way
                    // came; nothing more goes either way.
                    self.closed[side].insert(stream);
                    let both = self.ways.get_mut(&stream).expect("joined");
                    let last = both[side].last.max(at) + Duration::from_millis(1);
                    let carried = both[side].carries(at);
                    for way in both.iter_mut() {
                        way.dead_from = Some(way.dead_from.map_or(last, |dead| dead.min(last)));
                    }
                    if carried {
                        self.schedule(last, Happens::Breaks(other, stream));
                    }
                }
                Command::Deliver(message) => self.sides[side].got_controls.push(message.to_vec()),
                Command::DeliverBlock(bytes) => self.sides[side].got_blocks.push(bytes.to_vec()),
                Command::Broke(stream) => {
                    self.breaks += 1;
                    if self.rejoins && self.replaced.insert(stream) {
                        let after = if self.found {
                            self.random.millis(10, 200)
                        } else {
                            self.random.millis(50, 3_000)
                        };
                        self.join_later(at + after);
                    }
                }
                Command::Warn(why) => panic!("{side}: {why}"),
                Command::Lost => self.sides[side].lost = true,
                Command::Closed => self.sides[side].closed = true,
            }
        }
        // A peer done closes.
        let peer = &self.sides[side];
        let all_sent = peer.sent_controls == peer.controls && peer.sent_blocks == peer.blocks.len();
        let all_had = peer.got_controls.len() == self.sides[other].controls
            && peer.got_blocks.len() == self.sides[other].blocks.len();
        if all_sent && all_had && !peer.asked_to_close && !peer.lost {
            self.sides[side].asked_to_close = true;
            self.tell(side, Event::Close, at);
        }
        if let Some(deadline) = self.sides[side].session.next_deadline() {
            assert!(
                deadline > self.start + at,
                "{side}: a deadline not ahead, at {at:?}"
            );
            let at = deadline.saturating_duration_since(self.start);
            if self.sides[side]
                .tick_at
                .is_none_or(|scheduled| at < scheduled)
            {
                self.sides[side].tick_at = Some(at);
                self.schedule(at, Happens::Tick(side));
            }
        }
    }
}

/// Checks a run: both sides closed, neither lost, and each had every
/// control message of the other's, once, in order, and every block, once.
fn check(outcome: &Outcome, seed: u64) {
    assert_eq!(outcome.lost, [false, false], "seed {seed}: lost");
    assert_eq!(outcome.closed, [true, true], "seed {seed}: not closed");
    for side in 0..2 {
        let other = 1 - side;
        let wanted: Vec<Vec<u8>> = (0..outcome.controls[other])
            .map(|n| control(other, n))
            .collect();
        assert_eq!(
            outcome.got_controls[side], wanted,
            "seed {seed}: side {side}'s control messages"
        );
        let mut wanted: Vec<Vec<u8>> = outcome.sent_blocks[other]
            .iter()
            .enumerate()
            .map(|(n, &len)| block(other, n, len))
            .collect();
        let mut got = outcome.got_blocks[side].clone();
        wanted.sort();
        got.sort();
        assert!(
            got == wanted,
            "seed {seed}: side {side}'s blocks: {} had, {} sent",
            got.len(),
            wanted.len()
        );
    }
}

/// Over streams that never break, each side has every control message of
/// the other's, once, in order, and every block once, and both close.
#[test]
fn over_streams_that_hold_everything_comes_once() {
    for seed in 0..50 {
        let outcome = Sim::new(seed, 3, 40, 30).run(Duration::from_secs(600));
        check(&outcome, seed);
        assert_eq!(outcome.breaks, 0, "seed {seed}");
    }
}

/// A run of `seed` over one to three streams that die now and then, while
/// the peers send, and as they close.
fn breaking(seed: u64) -> Outcome {
    let mut sim = Sim::new(seed, 1 + (seed % 3) as StreamId, 30, 20).paced(5, 50);
    sim.breaking(8, 20, 400);
    sim.run(Duration::from_secs(600))
}

/// Over streams that die now and then, found broken soon or only by their
/// silence, and joined again a while after, each side still has every
/// control message of the other's, exactly once, in order, and every block
/// exactly once; and the same seed comes to the same end. `SIM_TRACE=1`
/// prints what happens.
#[test]
fn over_streams_that_break_and_come_back_everything_comes_once() {
    let mut broke = 0;
    for seed in 0..300 {
        let outcome = breaking(seed);
        check(&outcome, seed);
        assert!(outcome.breaks > 0, "seed {seed}: no stream broke");
        broke += outcome.breaks;
        if seed % 50 == 0 {
            assert_eq!(breaking(seed).trace, outcome.trace, "seed {seed}");
        }
    }
    // Most more than once, as the peers send: 1,580 times when written.
    assert!(broke > 300 * 4, "streams broke {broke} times only");
}

/// The time `seconds` after `start`.
fn at(start: Instant, seconds: u64) -> Instant {
    start + Duration::from_secs(seconds)
}

/// Whether the commands write `wanted` on `stream`.
fn writes(commands: &[Command], stream: StreamId, wanted: &Frame) -> bool {
    commands.iter().any(
        |command| matches!(command, Command::Write(on, frame) if *on == stream && frame == wanted),
    )
}

/// A session with one stream joined, greeted by the other end.
fn greeted(start: Instant) -> Session {
    let mut session = Session::new(ID, start);
    session.on(Event::Joined(0), start);
    let hello = Frame::Hello {
        session: ID,
        stream: 0,
    };
    session.on(Event::Frame(0, hello), start);
    session
}

/// A stream silent too long is taken for broken, and closed, so that its
/// other end sees it so; one not told anything a while is told it is alive.
#[test]
fn a_stream_silent_too_long_is_broken_and_one_idle_is_kept_alive() {
    let start = Instant::now();
    let mut session = greeted(start);
    let commands = session.on(Event::Tick, at(start, 5));
    assert!(writes(&commands, 0, &Frame::Keepalive), "{commands:?}");
    session.on(Event::Wrote(0, 1 << 20), at(start, 5));
    // Hearing from it keeps it.
    session.on(Event::Frame(0, Frame::Keepalive), at(start, 15));
    let commands = session.on(Event::Tick, at(start, 30));
    assert!(!commands.contains(&Command::Broke(0)), "{commands:?}");
    let commands = session.on(Event::Tick, at(start, 35));
    assert!(commands.contains(&Command::Close(0)), "{commands:?}");
    assert!(commands.contains(&Command::Broke(0)), "{commands:?}");
}

/// A stream heard from only one way, as one whose other way is cut, is
/// broken the same, however much is written to it.
#[test]
fn a_stream_heard_only_one_way_is_broken() {
    let start = Instant::now();
    let mut session = greeted(start);
    let mut commands = Vec::new();
    for second in 1..=25 {
        commands.extend(session.on(Event::Control(vec![1; 10]), at(start, second)));
        commands.extend(session.on(Event::Wrote(0, 1 << 20), at(start, second)));
        commands.extend(session.on(Event::Tick, at(start, second)));
    }
    assert!(commands.contains(&Command::Broke(0)));
}

/// With no stream left, the session waits the grace for one to be joined:
/// one joined in time takes what was not had, and the session goes on; with
/// none, it gives up, and its peer is told it is lost, and a stream joined
/// after is closed.
#[test]
fn with_no_stream_left_the_session_waits_the_grace_then_is_lost() {
    let start = Instant::now();
    let mut session = greeted(start);
    session.on(Event::Control(b"one".to_vec()), start);
    session.on(Event::Broken(0), at(start, 1));
    // Joined again in time, the message not had is sent again on it.
    let commands = session.on(Event::Joined(1), at(start, 50));
    let again = Frame::Control {
        seq: 1,
        ack: 0,
        message: Bytes::new(b"one".to_vec()),
    };
    assert!(writes(&commands, 1, &again), "{commands:?}");
    assert!(!commands.contains(&Command::Lost));
    session.on(Event::Broken(1), at(start, 60));
    assert!(
        !session
            .on(Event::Tick, at(start, 119))
            .contains(&Command::Lost)
    );
    let commands = session.on(Event::Tick, at(start, 120));
    assert!(commands.contains(&Command::Lost), "{commands:?}");
    assert!(session.is_over());
    assert_eq!(
        session.on(Event::Joined(2), at(start, 121)),
        [Command::Close(2)]
    );
}

/// One stream, that breaks and comes back again and again, found broken at
/// once and joined again soon, still carries every control message, once,
/// in order.
#[test]
fn streams_that_come_and_go_carry_every_control_message_once() {
    for seed in 0..100 {
        let mut sim = Sim::new(1_000 + seed, 1, 100, 0).paced(50, 150).found();
        sim.breaking(150, 50, 250);
        let outcome = sim.run(Duration::from_secs(900));
        check(&outcome, seed);
        // 62 times at the least when written.
        assert!(
            outcome.breaks >= 40,
            "seed {seed}: {} breaks",
            outcome.breaks
        );
    }
}

/// A stream that begins with anything but its session's Hello is broken,
/// and nothing that came on it is taken.
#[test]
fn a_stream_of_another_session_is_broken() {
    let start = Instant::now();
    let mut session = Session::new(ID, start);
    session.on(Event::Joined(0), start);
    let hello = Frame::Hello {
        session: [8; 16],
        stream: 0,
    };
    let commands = session.on(Event::Frame(0, hello), start);
    assert!(commands.contains(&Command::Broke(0)), "{commands:?}");
    session.on(Event::Joined(1), start);
    let message = Frame::Control {
        seq: 1,
        ack: 0,
        message: Bytes::new(b"early".to_vec()),
    };
    let commands = session.on(Event::Frame(1, message), start);
    assert!(commands.contains(&Command::Broke(1)), "{commands:?}");
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, Command::Deliver(_)))
    );
}

/// A block's fragments all go on one stream, in order, the last said so;
/// control messages go ahead of the fragments still to be written; and the
/// writer is handed no more than its window.
#[test]
fn a_block_goes_in_fragments_on_one_stream_behind_control_messages() {
    let start = Instant::now();
    let mut session = greeted(start);
    session.on(Event::Wrote(0, 1 << 20), start);
    let commands = session.on(Event::Block(Bytes::new(vec![9; 10 * FRAGMENT + 5])), start);
    let fragments: Vec<(u32, bool)> = commands
        .iter()
        .filter_map(|command| match command {
            Command::Write(0, Frame::Fragment { offset, last, .. }) => Some((*offset, *last)),
            _ => None,
        })
        .collect();
    let written: usize = commands
        .iter()
        .filter_map(|command| match command {
            Command::Write(_, frame) => Some(frame.size()),
            _ => None,
        })
        .sum();
    assert!(written <= WINDOW + FRAGMENT + 16, "{written}");
    assert_eq!(fragments[0], (0, false));
    let commands = session.on(Event::Control(b"now".to_vec()), start);
    assert!(commands.is_empty(), "the window is full: {commands:?}");
    let commands = session.on(Event::Wrote(0, WINDOW), start);
    assert!(
        matches!(
            commands.first(),
            Some(Command::Write(0, Frame::Control { .. }))
        ),
        "{commands:?}"
    );
}

/// Once the peer is done, and all it sent is had, the session ends each
/// stream; once the other side ends them too, its peer is told it is
/// closed, and a stream's end then is no break.
#[test]
fn a_session_ends_once_all_is_had_and_the_other_side_ends() {
    let start = Instant::now();
    let mut session = greeted(start);
    session.on(Event::Control(b"last".to_vec()), start);
    session.on(Event::Wrote(0, 1 << 20), start);
    let commands = session.on(Event::Close, start);
    assert!(
        !writes(&commands, 0, &Frame::End),
        "not had yet: {commands:?}"
    );
    let had = Frame::Ack {
        ack: 1,
        below: 0,
        blocks: Vec::new(),
    };
    let commands = session.on(Event::Frame(0, had), start);
    assert!(writes(&commands, 0, &Frame::End), "{commands:?}");
    session.on(Event::Wrote(0, 1 << 20), start);
    let commands = session.on(Event::Frame(0, Frame::End), start);
    assert!(commands.contains(&Command::Closed), "{commands:?}");
    assert!(session.is_over());
}

/// With the client giving up on the streams, both sides wait the grace and
/// are lost; what each delivered of the other's was still each control
/// message once, in order, from the first, and each block once.
#[test]
fn with_the_streams_gone_for_good_both_sides_are_lost() {
    for seed in 0..50 {
        let mut sim = Sim::new(2_000 + seed, 2, 40, 20).paced(5, 50).gives_up();
        sim.breaking(2, 100, 600);
        let outcome = sim.run(Duration::from_secs(600));
        assert_eq!(outcome.lost, [true, true], "seed {seed}");
        for side in 0..2 {
            let other = 1 - side;
            let got = &outcome.got_controls[side];
            let wanted: Vec<Vec<u8>> = (0..got.len()).map(|n| control(other, n)).collect();
            assert_eq!(got, &wanted, "seed {seed}: side {side}");
            let mut blocks = outcome.got_blocks[side].clone();
            let had = blocks.len();
            blocks.sort();
            blocks.dedup();
            assert_eq!(
                blocks.len(),
                had,
                "seed {seed}: side {side} had a block twice"
            );
        }
    }
}

/// A session closed answers a stream joined after with its end, should the
/// other side have missed it, and closes it; one lost only closes it.
#[test]
fn a_session_closed_says_its_end_on_a_stream_joined_after() {
    let start = Instant::now();
    let mut session = greeted(start);
    session.on(Event::Close, start);
    session.on(Event::Frame(0, Frame::End), start);
    let commands = session.on(Event::Wrote(0, 1 << 20), start);
    assert!(commands.contains(&Command::Closed), "{commands:?}");
    let commands = session.on(Event::Joined(1), start);
    let hello = Frame::Hello {
        session: ID,
        stream: 1,
    };
    assert_eq!(
        commands,
        [
            Command::Write(1, hello),
            Command::Write(1, Frame::End),
            Command::Close(1)
        ]
    );
}

/// Once both sides ended, a stream's end is no break, and the client is not
/// told of it; before, the side ended still says it is alive, as the other
/// may still be sending, and joins a stream joined after to the end.
#[test]
fn once_both_sides_ended_a_stream_that_ends_is_no_break() {
    let start = Instant::now();
    let mut session = greeted(start);
    session.on(Event::Joined(1), start);
    let hello = Frame::Hello {
        session: ID,
        stream: 1,
    };
    session.on(Event::Frame(1, hello), start);
    let commands = session.on(Event::Close, start);
    assert!(writes(&commands, 0, &Frame::End), "{commands:?}");
    session.on(Event::Wrote(0, 1 << 20), start);
    session.on(Event::Wrote(1, 1 << 20), start);
    let commands = session.on(Event::Tick, at(start, 5));
    assert!(writes(&commands, 0, &Frame::Keepalive), "{commands:?}");
    let commands = session.on(Event::Joined(2), at(start, 5));
    assert!(writes(&commands, 2, &Frame::End), "{commands:?}");
    // The other side ends stream 0, then closes it; stream 1 still has a
    // keepalive to write.
    session.on(Event::Frame(0, Frame::End), at(start, 5));
    let commands = session.on(Event::Broken(0), at(start, 5));
    assert_eq!(commands, [Command::Close(0)]);
    assert!(!session.is_over());
}

/// Should the stream that says what a side has break, what it said may be
/// lost: the next says again every block above those all had.
#[test]
fn what_was_had_is_said_again_once_its_stream_breaks() {
    let start = Instant::now();
    let mut session = greeted(start);
    session.on(Event::Joined(1), start);
    let hello = Frame::Hello {
        session: ID,
        stream: 1,
    };
    session.on(Event::Frame(1, hello), start);
    for block in [0, 2, 3] {
        let fragment = Frame::Fragment {
            block,
            offset: 0,
            last: true,
            bytes: Bytes::new(vec![block as u8; 10]),
        };
        let commands = session.on(Event::Frame(1, fragment), start);
        assert!(
            matches!(commands.as_slice(), [Command::DeliverBlock(_)]),
            "{commands:?}"
        );
    }
    let commands = session.on(Event::Idle, start);
    let said = Frame::Ack {
        ack: 0,
        below: 1,
        blocks: vec![2, 3],
    };
    assert!(writes(&commands, 0, &said), "{commands:?}");
    // Block 2 again, sent again from the other side: taken once.
    let again = Frame::Fragment {
        block: 2,
        offset: 0,
        last: true,
        bytes: Bytes::new(vec![2; 10]),
    };
    assert!(session.on(Event::Frame(1, again), start).is_empty());
    session.on(Event::Broken(0), start);
    let commands = session.on(Event::Idle, start);
    assert!(writes(&commands, 1, &said), "{commands:?}");
}

/// A stream whose Hello says another number than it was joined as is
/// broken.
#[test]
fn a_stream_joined_as_another_is_broken() {
    let start = Instant::now();
    let mut session = Session::new(ID, start);
    session.on(Event::Joined(0), start);
    let hello = Frame::Hello {
        session: ID,
        stream: 1,
    };
    let commands = session.on(Event::Frame(0, hello), start);
    assert!(commands.contains(&Command::Broke(0)), "{commands:?}");
}
