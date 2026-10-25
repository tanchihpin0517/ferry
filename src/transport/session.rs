//! The session: the top of the transport, in each server, between the two
//! peers. It takes what its peer sends, control messages and blocks, and
//! delivers the other peer's: each control message exactly once, in order,
//! and each block exactly once, in any order, over streams that each keep
//! their own order but may break, or be judged broken, and be joined again.
//!
//! A control message is numbered, kept until the other side says it has
//! it, and sent again on another stream should its stream break; control
//! messages go on one stream at a time, so that none waits for another to
//! be put back in order. A block is numbered as it comes, kept until the
//! other side has it whole, and sent in fragments, all on one stream, the
//! one with the least to write, and again on another should that one break;
//! the other side puts its fragments together, and takes a block sent twice
//! once. A control message goes ahead of the fragments still to be written.
//!
//! What a side has is said on one stream too, the control messages' stream:
//! how far it has the other's control messages, and every block below which
//! it has them all, and the blocks above that it has since it last said.
//! Should that stream break, what was said on it may be lost, so the blocks
//! above are all said again on the next.
//!
//! Each stream is judged alone: one that ends, fails, or is silent too long
//! is broken, and closed, so that the other end sees it broken too; one
//! silent a while is told something, so that the other end hears it. With
//! no stream left, the session waits a grace for one to be joined, then
//! gives up, and its peer is told it is lost. Once its peer has said it is
//! done, and all it sent is had, it ends each stream; once the other side
//! has too, its peer is told it is closed. A session closed answers a
//! stream joined after with its end, should the other side have missed it.
//!
//! The control messages sent and kept are bounded by the peers' protocol,
//! as are the blocks, by the data credit: the session takes all it is
//! handed, and never asks its peer to wait.
//!
//! It does no I/O and keeps no clock: the time comes with each event, so
//! that what it does is tested on events made up, as the peers' owners are.
//! See the draft, `docs/drafts/architecture-redesign.md`, section 5.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ops::{Deref, Range};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};

/// A stream, as the driver numbers them, each a number of its own.
pub type StreamId = u32;

/// The most bytes of a block one fragment carries.
pub const FRAGMENT: usize = 64 << 10;

/// The most bytes a stream's writer is handed and has not written yet: a
/// few fragments, so that a control message waits behind no more than
/// these.
pub const WINDOW: usize = 4 * FRAGMENT;

/// How long a stream may be silent this way before it is told something.
pub const KEEPALIVE: Duration = Duration::from_secs(5);

/// How long a stream may be silent the other way before it is taken for
/// broken: a few keepalives' worth. Taken for broken wrongly, a stream is
/// only joined again, and what was on it sent again.
pub const SILENCE: Duration = Duration::from_secs(20);

/// How long the session waits, with no stream left, for one to be joined,
/// before it gives up: a minute, as the user decided (2026-10-24).
pub const GRACE: Duration = Duration::from_secs(60);

/// Bytes shared, as a block's are by its fragments, rather than copied.
#[derive(Clone)]
pub struct Bytes {
    data: Arc<[u8]>,
    range: Range<usize>,
}

impl Bytes {
    pub fn new(data: Vec<u8>) -> Self {
        let range = 0..data.len();
        Self {
            data: data.into(),
            range,
        }
    }

    /// The bytes from `range` of these.
    pub fn slice(&self, range: Range<usize>) -> Self {
        let start = self.range.start + range.start;
        Self {
            data: Arc::clone(&self.data),
            range: start..start + range.len(),
        }
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "{} bytes", self.len())
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Bytes {}

/// What goes on a stream: the session's frames, which the peers never see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// The first frame on a stream: the session it is for.
    Hello { session: [u8; 16], stream: u32 },
    /// A control message, numbered from 1, with how far this side has
    /// delivered the other's.
    Control { seq: u64, ack: u64, message: Bytes },
    /// How far this side has delivered the other's control messages; every
    /// block below which it has had all whole; and the blocks above that it
    /// has had since it last said.
    Ack {
        ack: u64,
        below: u64,
        blocks: Vec<u64>,
    },
    /// Part of a block, at `offset` in it: a block's fragments all go on
    /// one stream, in order, the last said so.
    Fragment {
        block: u64,
        offset: u32,
        last: bool,
        bytes: Bytes,
    },
    /// Nothing to say: the stream is alive.
    Keepalive,
    /// This side's peer is done, and all it sent is had: the stream's end,
    /// after, is no break. Only what is owed the other side follows.
    End,
}

impl Frame {
    /// About how many bytes it takes to write: what a stream's writer is
    /// counted to hold.
    pub fn size(&self) -> usize {
        16 + match self {
            Self::Control { message, .. } => message.len(),
            Self::Ack { blocks, .. } => 8 * blocks.len(),
            Self::Fragment { bytes, .. } => bytes.len(),
            Self::Hello { .. } | Self::Keepalive | Self::End => 0,
        }
    }
}

/// What the session is told.
#[derive(Debug)]
pub enum Event {
    /// A stream joined: handed to this server, or opened again.
    Joined(StreamId),
    /// A frame came on a stream.
    Frame(StreamId, Frame),
    /// A stream ended or failed, as its reader or writer found: it is
    /// broken, but for one both sides ended.
    Broken(StreamId),
    /// A stream's writer wrote this many of the bytes it was handed.
    Wrote(StreamId, usize),
    /// The peer has a control message to send.
    Control(Vec<u8>),
    /// The peer has a block to send.
    Block(Bytes),
    /// The peer is done: once all it sent is had, the streams end.
    Close,
    /// Nothing more is to be told now: what is owed the other side, as
    /// that its frames came, goes now, in one frame.
    Idle,
    /// Time has passed: it comes with every event, but this one says
    /// nothing else.
    Tick,
}

/// What the session has decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Hand a stream's writer a frame.
    Write(StreamId, Frame),
    /// Close a stream, broken or ended, once its writer has written what it
    /// was handed, or failed: its other end then sees it so. A stream may
    /// be said closed more than once.
    Close(StreamId),
    /// Hand the peer the other's next control message.
    Deliver(Bytes),
    /// Hand the peer a block of the other's.
    DeliverBlock(Bytes),
    /// Tell whoever follows the streams, the client, that one broke, so
    /// that it joins another.
    Broke(StreamId),
    /// Tell the client why a stream was taken for broken, when it broke the
    /// session's rules: a fault of the other side's, or of whoever joined
    /// it, never of the network.
    Warn(String),
    /// Tell the peer the session gave up: no stream was joined in the
    /// grace. Nothing more comes.
    Lost,
    /// Tell the peer the session ended: all each side sent is had.
    Closed,
}

/// A stream the session writes and reads.
struct Stream {
    /// Its Hello came from the other end.
    greeted: bool,
    /// Frames to write, control messages first: those of the session's own,
    /// as Acks and Hello, and the peer's, then fragments.
    controls: VecDeque<Frame>,
    fragments: VecDeque<Frame>,
    /// Bytes handed to its writer and not written yet.
    writing: usize,
    /// When anything last came on it, and when anything was last handed
    /// to its writer.
    heard: Instant,
    said: Instant,
    /// The blocks given to it, not had yet by the other side: sent again
    /// on another, should it break.
    blocks: BTreeSet<u64>,
    /// The blocks coming on it, and their bytes so far.
    receiving: HashMap<u64, Vec<u8>>,
    /// The other side ended it.
    ended: bool,
}

impl Stream {
    fn new(now: Instant) -> Self {
        Self {
            greeted: false,
            controls: VecDeque::new(),
            fragments: VecDeque::new(),
            writing: 0,
            heard: now,
            said: now,
            blocks: BTreeSet::new(),
            receiving: HashMap::new(),
            ended: false,
        }
    }

    /// Whether it has nothing to hand its writer.
    fn is_idle(&self) -> bool {
        self.controls.is_empty() && self.fragments.is_empty()
    }

    /// How much it has to write: for blocks to go to the one with least.
    fn load(&self) -> usize {
        self.writing
            + self
                .fragments
                .iter()
                .chain(&self.controls)
                .map(Frame::size)
                .sum::<usize>()
    }
}

/// The blocks had whole: few numbers kept, as they come mostly in order.
#[derive(Default)]
struct Had {
    /// Every block below this is had.
    below: u64,
    /// And these, at or above it.
    above: BTreeSet<u64>,
}

impl Had {
    /// Whether `block` was had before; it is now.
    fn insert(&mut self, block: u64) -> bool {
        if block < self.below || !self.above.insert(block) {
            return true;
        }
        while self.above.remove(&self.below) {
            self.below += 1;
        }
        false
    }
}

/// What the session is: going, given up, or ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Going,
    Lost,
    Closed,
}

/// The session; see the module.
pub struct Session {
    id: [u8; 16],
    phase: Phase,
    streams: BTreeMap<StreamId, Stream>,
    /// The stream control messages go on.
    primary: Option<StreamId>,
    /// The control messages sent, from the first not had by the other side,
    /// with their number; and the next number.
    sent: VecDeque<(u64, Bytes)>,
    next_seq: u64,
    /// How far the other side's control messages are delivered, and
    /// whether that is owed the other side.
    delivered: u64,
    owe_ack: bool,
    /// The blocks sent, not had yet by the other side, by number, with the
    /// stream given each, if any is; and the next number.
    blocks: BTreeMap<u64, (Bytes, Option<StreamId>)>,
    next_block: u64,
    /// The other side's blocks had whole, and those owed it as so.
    had: Had,
    owe_blocks: Vec<u64>,
    /// Since when no stream is left, if none is.
    alone_since: Option<Instant>,
    /// The peer is done; the streams are ended; the other side ended them.
    closing: bool,
    ended: bool,
    other_ended: bool,
}

impl Session {
    /// A session of the ID both servers were given, as of `now`: with no
    /// stream yet, it waits a grace for one.
    pub fn new(id: [u8; 16], now: Instant) -> Self {
        Self {
            id,
            phase: Phase::Going,
            streams: BTreeMap::new(),
            primary: None,
            sent: VecDeque::new(),
            next_seq: 1,
            delivered: 0,
            owe_ack: false,
            blocks: BTreeMap::new(),
            next_block: 0,
            had: Had::default(),
            owe_blocks: Vec::new(),
            alone_since: Some(now),
            closing: false,
            ended: false,
            other_ended: false,
        }
    }

    /// When the session next needs to be told the time, whatever else
    /// comes: a stream idle to be told something, or one to be taken for
    /// broken, or the grace to run out. Always later than the last event.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.phase != Phase::Going {
            return None;
        }
        let keepalives = self
            .streams
            .values()
            .filter(|stream| stream.is_idle())
            .map(|stream| stream.said + KEEPALIVE);
        let silences = self.streams.values().map(|stream| stream.heard + SILENCE);
        keepalives
            .chain(silences)
            .chain(self.alone_since.map(|since| since + GRACE))
            .min()
    }

    /// Whether the session gave up, or ended: nothing more comes of it.
    pub fn is_over(&self) -> bool {
        self.phase != Phase::Going
    }

    /// Takes an event, at `now`, and says what to do about it. A stream
    /// that breaks the session's rules, as with a frame of another session,
    /// is taken for broken, and the client told why.
    pub fn on(&mut self, event: Event, now: Instant) -> Vec<Command> {
        let mut commands = Vec::new();
        if self.phase != Phase::Going {
            // Gone: a stream joined after is closed, as is any other; a
            // session closed says its end on it first, should the other side
            // have missed it as a stream broke.
            if let Event::Joined(stream) = event
                && self.phase == Phase::Closed
            {
                let hello = Frame::Hello {
                    session: self.id,
                    stream,
                };
                commands.push(Command::Write(stream, hello));
                commands.push(Command::Write(stream, Frame::End));
            }
            if let Event::Joined(stream) | Event::Frame(stream, _) = event {
                commands.push(Command::Close(stream));
            }
            return commands;
        }
        match event {
            Event::Joined(stream) => self.joined(stream, now),
            Event::Frame(stream, frame) => {
                if let Err(wrong) = self.frame(stream, frame, now, &mut commands) {
                    commands.push(Command::Warn(format!("stream {stream}: {wrong:#}")));
                    self.broken(stream, now, &mut commands);
                }
            }
            Event::Broken(stream) => {
                // One the other side ended, once this one has too, is no
                // stream lost: neither side has anything more to send.
                let ended = self.streams.get(&stream).is_some_and(|stream| stream.ended);
                if ended && self.ended {
                    self.streams.remove(&stream);
                    commands.push(Command::Close(stream));
                } else {
                    self.broken(stream, now, &mut commands);
                }
            }
            Event::Wrote(stream, bytes) => {
                if let Some(stream) = self.streams.get_mut(&stream) {
                    stream.writing = stream.writing.saturating_sub(bytes);
                }
            }
            Event::Control(message) => self.send_control(message),
            Event::Block(bytes) => self.send_block(bytes),
            Event::Close => self.closing = true,
            Event::Idle => self.acknowledge(),
            Event::Tick => {}
        }
        self.health(now, &mut commands);
        self.end_if_done(&mut commands);
        self.write(now, &mut commands);
        commands
    }

    /// A stream joined: it is greeted, and told the end if the streams were
    /// ended; and, if it is the only one, takes the control messages not had
    /// yet, and the blocks with no stream.
    fn joined(&mut self, id: StreamId, now: Instant) {
        let mut stream = Stream::new(now);
        stream.controls.push_back(Frame::Hello {
            session: self.id,
            stream: id,
        });
        if self.ended {
            stream.controls.push_back(Frame::End);
        }
        self.streams.insert(id, stream);
        self.alone_since = None;
        if self.primary.is_none() {
            self.primary = Some(id);
            self.resend_controls();
        }
        let waiting: Vec<u64> = self
            .blocks
            .iter()
            .filter(|(_, (_, stream))| stream.is_none())
            .map(|(&block, _)| block)
            .collect();
        for block in waiting {
            self.assign(block);
        }
    }

    /// Takes a frame that came on a stream; an error if the stream broke
    /// the session's rules.
    fn frame(
        &mut self,
        id: StreamId,
        frame: Frame,
        now: Instant,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        let Some(stream) = self.streams.get_mut(&id) else {
            // A stream taken for broken, still read: nothing of it is taken.
            return Ok(());
        };
        stream.heard = now;
        if !stream.greeted {
            let Frame::Hello {
                session,
                stream: said,
            } = frame
            else {
                bail!("a stream began with {frame:?}, not its Hello");
            };
            ensure!(session == self.id, "a stream of another session joined");
            ensure!(said == id, "stream {id} said it is stream {said}");
            stream.greeted = true;
            return Ok(());
        }
        match frame {
            Frame::Hello { .. } => bail!("a stream said Hello twice"),
            Frame::Control { seq, ack, message } => {
                self.acknowledged(ack);
                if seq == self.delivered + 1 {
                    self.delivered = seq;
                    commands.push(Command::Deliver(message));
                } else {
                    // Had already, sent again: said had again.
                    ensure!(
                        seq <= self.delivered,
                        "control message {seq} came before those before it"
                    );
                }
                self.owe_ack = true;
            }
            Frame::Ack { ack, below, blocks } => {
                self.acknowledged(ack);
                self.blocks_had(below, &blocks);
            }
            Frame::Fragment {
                block,
                offset,
                last,
                bytes,
            } => {
                let stream = self.streams.get_mut(&id).expect("there");
                let so_far = stream.receiving.entry(block).or_default();
                ensure!(
                    so_far.len() == offset as usize,
                    "a fragment of block {block} at {offset}, after {} bytes",
                    so_far.len()
                );
                so_far.extend_from_slice(&bytes);
                if last {
                    let whole = stream.receiving.remove(&block).expect("there");
                    if !self.had.insert(block) {
                        commands.push(Command::DeliverBlock(Bytes::new(whole)));
                    }
                    // Had, or had again: said so either way.
                    self.owe_blocks.push(block);
                }
            }
            Frame::Keepalive => {}
            Frame::End => {
                let stream = self.streams.get_mut(&id).expect("there");
                stream.ended = true;
                self.other_ended = true;
            }
        }
        Ok(())
    }

    /// The other side has delivered control messages up to `ack`: they need
    /// be kept no more.
    fn acknowledged(&mut self, ack: u64) {
        while self.sent.front().is_some_and(|(seq, _)| *seq <= ack) {
            self.sent.pop_front();
        }
    }

    /// The other side has every block below `below` whole, and `blocks`:
    /// they need be kept no more.
    fn blocks_had(&mut self, below: u64, blocks: &[u64]) {
        let all_below: Vec<u64> = self
            .blocks
            .range(..below)
            .map(|(&block, _)| block)
            .collect();
        for block in all_below.iter().chain(blocks) {
            if let Some((_, Some(stream))) = self.blocks.remove(block)
                && let Some(stream) = self.streams.get_mut(&stream)
            {
                stream.blocks.remove(block);
            }
        }
    }

    /// The peer's control message: numbered, kept, and sent on the
    /// primary stream, if there is one.
    fn send_control(&mut self, message: Vec<u8>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let message = Bytes::new(message);
        self.sent.push_back((seq, message.clone()));
        if let Some(primary) = self.primary {
            let ack = self.delivered;
            let stream = self
                .streams
                .get_mut(&primary)
                .expect("the primary is there");
            stream
                .controls
                .push_back(Frame::Control { seq, ack, message });
            self.owe_ack = false;
        }
    }

    /// The peer's block: numbered, kept, and given to the stream with the
    /// least to write, if there is one.
    fn send_block(&mut self, bytes: Bytes) {
        let block = self.next_block;
        self.next_block += 1;
        self.blocks.insert(block, (bytes, None));
        self.assign(block);
    }

    /// Gives a block kept to the stream with the least to write, as its
    /// fragments, in order; with no stream, it waits for one.
    fn assign(&mut self, block: u64) {
        let Some((&id, _)) = self.streams.iter().min_by_key(|(_, stream)| stream.load()) else {
            return;
        };
        let (bytes, given) = self.blocks.get_mut(&block).expect("kept");
        *given = Some(id);
        let bytes = bytes.clone();
        let stream = self.streams.get_mut(&id).expect("there");
        stream.blocks.insert(block);
        let mut offset = 0;
        loop {
            let end = bytes.len().min(offset + FRAGMENT);
            stream.fragments.push_back(Frame::Fragment {
                block,
                offset: offset as u32,
                last: end == bytes.len(),
                bytes: bytes.slice(offset..end),
            });
            if end == bytes.len() {
                break;
            }
            offset = end;
        }
    }

    /// Sends the control messages not had yet on the primary stream, from
    /// the first: those sent on one broken may never have come.
    fn resend_controls(&mut self) {
        let Some(primary) = self.primary else {
            return;
        };
        let ack = self.delivered;
        let stream = self
            .streams
            .get_mut(&primary)
            .expect("the primary is there");
        for (seq, message) in &self.sent {
            stream.controls.push_back(Frame::Control {
                seq: *seq,
                ack,
                message: message.clone(),
            });
        }
        if !self.sent.is_empty() {
            self.owe_ack = false;
        }
    }

    /// Says what is owed the other side, as that its frames came, in one
    /// frame on the primary stream.
    fn acknowledge(&mut self) {
        if !self.owe_ack && self.owe_blocks.is_empty() {
            return;
        }
        let Some(primary) = self.primary else {
            return;
        };
        let below = self.had.below;
        let mut blocks = std::mem::take(&mut self.owe_blocks);
        blocks.retain(|&block| block >= below);
        let ack = self.delivered;
        let stream = self
            .streams
            .get_mut(&primary)
            .expect("the primary is there");
        stream.controls.push_back(Frame::Ack { ack, below, blocks });
        self.owe_ack = false;
    }

    /// A stream is broken: closed, so that its other end sees it so; what
    /// was on it, not had yet, goes on another; and, with none left, the
    /// grace begins.
    fn broken(&mut self, id: StreamId, now: Instant, commands: &mut Vec<Command>) {
        let Some(stream) = self.streams.remove(&id) else {
            return;
        };
        commands.push(Command::Close(id));
        commands.push(Command::Broke(id));
        if self.primary == Some(id) {
            self.primary = self.streams.keys().next().copied();
            self.resend_controls();
            // What was said on it may be lost: all is said again on the new
            // one.
            self.owe_ack = true;
            self.owe_blocks = self.had.above.iter().copied().collect();
        }
        for block in stream.blocks {
            if let Some((_, given)) = self.blocks.get_mut(&block) {
                *given = None;
                self.assign(block);
            }
        }
        if self.streams.is_empty() {
            self.alone_since = Some(now);
        }
    }

    /// Judges each stream by the time: one silent too long is broken; one
    /// not told anything a while is told it is alive. With no stream left
    /// for the grace, the session gives up.
    fn health(&mut self, now: Instant, commands: &mut Vec<Command>) {
        let silent: Vec<StreamId> = self
            .streams
            .iter()
            .filter(|(_, stream)| now.duration_since(stream.heard) >= SILENCE)
            .map(|(&id, _)| id)
            .collect();
        for id in silent {
            self.broken(id, now, commands);
        }
        for stream in self.streams.values_mut() {
            // After the end too: the other side may still be sending.
            if stream.is_idle() && now.duration_since(stream.said) >= KEEPALIVE {
                stream.controls.push_back(Frame::Keepalive);
            }
        }
        if let Some(since) = self.alone_since
            && now.duration_since(since) >= GRACE
        {
            self.phase = Phase::Lost;
            commands.push(Command::Lost);
        }
    }

    /// Once the peer is done, all it sent is had, and nothing is owed, the
    /// streams end; once the other side has ended them too, the session
    /// is closed.
    fn end_if_done(&mut self, commands: &mut Vec<Command>) {
        if self.phase != Phase::Going {
            return;
        }
        if self.closing && !self.ended && self.sent.is_empty() && self.blocks.is_empty() {
            // What is owed goes first.
            self.acknowledge();
            if !self.owe_ack && self.owe_blocks.is_empty() && !self.streams.is_empty() {
                for stream in self.streams.values_mut() {
                    stream.controls.push_back(Frame::End);
                }
                self.ended = true;
            }
        }
        if self.ended && self.other_ended && self.nothing_to_write() {
            self.phase = Phase::Closed;
            commands.push(Command::Closed);
            for &id in self.streams.keys() {
                commands.push(Command::Close(id));
            }
        }
    }

    /// Whether every stream has written all it was given.
    fn nothing_to_write(&self) -> bool {
        self.streams
            .values()
            .all(|stream| stream.is_idle() && stream.writing == 0)
    }

    /// Hands each stream's writer what it has room for, control messages
    /// first.
    fn write(&mut self, now: Instant, commands: &mut Vec<Command>) {
        for (&id, stream) in &mut self.streams {
            while stream.writing < WINDOW {
                let Some(frame) = stream
                    .controls
                    .pop_front()
                    .or_else(|| stream.fragments.pop_front())
                else {
                    break;
                };
                stream.writing += frame.size();
                stream.said = now;
                commands.push(Command::Write(id, frame));
            }
        }
    }
}

#[cfg(test)]
mod tests;
