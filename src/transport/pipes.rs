//! Pipes: what a connection carries once it is ready. Each kind of content
//! has a pipe of its own, one way, in order, reliable: a program writes what
//! it has, as it would a file, the write waiting while the pipe can take no
//! more; and reads what is there, records cut however they came. How much
//! goes in a packet, when, among what else, and whether the far end can take
//! more, is the transport's: no program sees it. See the
//! [pipes draft](../../docs/drafts/pipes.md) and
//! [protocol](../../docs/drafts/protocol.md#4-走-pipe-的連線怎麼送).
//!
//! On the wire, once ready, there is only one packet, a frame:
//! `[pipe: u8][kind: u8][length: u32, little-endian][content]`, its content
//! at most [`MOST`] bytes. Its kind is [`Kind`]: a pipe's next bytes, window
//! given back, its end, a sync and its answer.
//!
//! - **Windows.** Each pipe but control has one, in bytes on the wire, which
//!   its reader gives: a writer's bytes wait in the pipe's send buffer, and
//!   go no faster than the window allows; the reader's transport gives back
//!   what its program reads, a quarter of the window at a time. So what
//!   comes always has room, and the thread that reads the wire never waits
//!   for a program. Control has none: its writer never waits for its
//!   reader, and more than [`CONTROL_GUARD`] unread there is the far end's
//!   fault.
//! - **Data.** The data pipe is cut into chunks, `[number: u64][encoding:
//!   u8][length: u32][content]`, each whole on one wire, any wire, numbered
//!   in turn so that the reader's transport puts them back in order: a
//!   connection of several wires carries data on all, and all else on the
//!   first. A chunk's number comes off the wire, and is not trusted.
//! - **Sync.** [`PipeWriter::sync`] returns once the far program has read
//!   all that was written before it: a `Sync` says how many bytes that is,
//!   and the far transport answers `Synced` once its program has read so
//!   many, as it gives back window. Nothing is added where no one syncs.
//! - **Order.** The pump of the first wire sends control, window given back
//!   and syncs first; then answers and reports; then, of data and the
//!   manifest, data while a quarter of its send buffer is full, else the
//!   manifest first. Each pump takes what is there as it has room to write
//!   it, so bytes gather while the wire is busy, and go at once while it is
//!   not.
//!
//! What the reader makes of a wire's bytes, and what each end knows of its
//! pipes, are [`Inbound`]'s and [`Core`]'s, which do no I/O, so that any
//! bytes, however they come, can go through them, as in the fuzzer; the
//! threads only move bytes between them and the wires.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use super::wire::{Carried, LET_GO, Wire, WireIn, WireOut};
use crate::profile::{self, Gauge, State};

/// A frame's header: its pipe, its kind, then its content's length.
pub const HEADER: usize = 6;

/// The most content in a frame.
pub const MOST: usize = 64 * 1024;

/// The most bytes of the data pipe in one chunk.
pub const CHUNK: usize = 2 << 20;

/// A chunk's header: its number, its encoding, and its length.
pub const CHUNK_HEADER: usize = 13;

/// The most bytes unread on the control pipe, which has no window: past it,
/// the far end is at fault, as nothing sends there more than a little at a
/// time.
pub const CONTROL_GUARD: usize = 16 << 20;

/// The most syncs a reader's transport keeps unanswered: each writer waits
/// for its answer, so a few at most.
const SYNCS: usize = 1024;

/// How many bytes may wait to be sent on the data pipe, before its writer
/// waits: few, so that what waits stays in the cache.
pub const DATA_BUFFER: usize = 16 << 20;

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// The pipe's next bytes.
    Bytes = 0,
    /// Window given back, the pipe's writer's: how many bytes, u32.
    Window = 1,
    /// The pipe ends: nothing more on it; on the data pipe, how many chunks
    /// it had, u64.
    End = 2,
    /// A sync: its number, u32, and how many bytes were written before it,
    /// u64.
    Sync = 3,
    /// A sync answered, by its number, u32: the program read so many.
    Synced = 4,
}

impl Kind {
    fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Bytes,
            1 => Self::Window,
            2 => Self::End,
            3 => Self::Sync,
            4 => Self::Synced,
            _ => return None,
        })
    }
}

/// Which of a connection's pipes go first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rank {
    /// Control, never waiting.
    First,
    /// Answers and reports.
    Second,
    /// The manifest, which goes after data while enough data waits.
    Paced,
    /// The data, cut into chunks, on every wire.
    Data,
}

/// One of a connection's pipes, as one end has it.
#[derive(Clone, Copy, Debug)]
pub struct Pipe {
    pub name: &'static str,
    /// This end writes it.
    pub writes: bool,
    /// This end reads it.
    pub reads: bool,
    /// Its window, in bytes on the wire; none for control.
    pub window: Option<u32>,
    pub rank: Rank,
}

impl Pipe {
    /// How many bytes may wait to be sent before its writer waits: as many
    /// as its window, but data's, which has [`DATA_BUFFER`]; control's never
    /// waits.
    fn buffer(&self) -> usize {
        match (self.rank, self.window) {
            (Rank::Data, _) => DATA_BUFFER,
            (_, Some(window)) => window as usize,
            (_, None) => usize::MAX,
        }
    }
}

/// The pipes of the stream between the two servers, by number.
pub const CONTROL: u8 = 0;
pub const MANIFEST: u8 = 1;
pub const DATA: u8 = 2;
pub const ANSWERS: u8 = 3;

/// The windows of the stream's pipes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    pub manifest: u32,
    pub data: u32,
    pub answers: u32,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            manifest: 2 << 20,
            data: crate::protocol::DATA_CREDIT as u32,
            answers: 1 << 20,
        }
    }
}

/// The stream's pipes, as the Source has them, or the Destination: control,
/// both ways; the manifest and the data, the Source's; the answers, the
/// Destination's.
pub fn stream(source: bool, windows: Windows) -> Vec<Pipe> {
    vec![
        Pipe {
            name: "control",
            writes: true,
            reads: true,
            window: None,
            rank: Rank::First,
        },
        Pipe {
            name: "manifest",
            writes: source,
            reads: !source,
            window: Some(windows.manifest),
            rank: Rank::Paced,
        },
        Pipe {
            name: "data",
            writes: source,
            reads: !source,
            window: Some(windows.data),
            rank: Rank::Data,
        },
        Pipe {
            name: "answers",
            writes: !source,
            reads: source,
            window: Some(windows.answers),
            rank: Rank::Second,
        },
    ]
}

/// Why what came on a wire goes no further: the far end is at fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault(pub &'static str);

/// Bytes waiting to be sent, shared rather than copied as they are cut.
#[derive(Clone)]
struct Piece {
    bytes: Arc<Vec<u8>>,
    start: usize,
    end: usize,
}

impl Piece {
    fn len(&self) -> usize {
        self.end - self.start
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[self.start..self.end]
    }
}

/// A frame to send, its content in pieces.
pub struct Outgoing {
    header: [u8; HEADER],
    content: Vec<Piece>,
}

impl Outgoing {
    fn new(pipe: u8, kind: Kind, content: Vec<Piece>) -> Self {
        let length: usize = content.iter().map(Piece::len).sum();
        let mut header = [pipe, kind as u8, 0, 0, 0, 0];
        header[2..].copy_from_slice(&(length as u32).to_le_bytes());
        Self { header, content }
    }

    fn of(pipe: u8, kind: Kind, content: Vec<u8>) -> Self {
        let end = content.len();
        let piece = Piece {
            bytes: Arc::new(content),
            start: 0,
            end,
        };
        Self::new(pipe, kind, vec![piece])
    }

    /// The frame, whole.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = self.header.to_vec();
        for piece in &self.content {
            bytes.extend_from_slice(piece.as_slice());
        }
        bytes
    }
}

/// A chunk being sent, by a pump: the frames still to send of it.
#[derive(Default)]
pub struct Sending {
    pieces: VecDeque<Piece>,
}

impl Sending {
    fn busy(&self) -> bool {
        !self.pieces.is_empty()
    }

    /// The next frame of the chunk.
    fn frame(&mut self) -> Outgoing {
        let content = take(&mut self.pieces, MOST);
        Outgoing::new(DATA, Kind::Bytes, content)
    }
}

/// Takes up to `most` bytes from the front of `pieces`.
fn take(pieces: &mut VecDeque<Piece>, most: usize) -> Vec<Piece> {
    let mut taken = Vec::new();
    let mut left = most;
    while left > 0 {
        let Some(front) = pieces.front_mut() else {
            break;
        };
        if front.len() <= left {
            left -= front.len();
            taken.push(pieces.pop_front().expect("one"));
        } else {
            let mut cut = front.clone();
            cut.end = cut.start + left;
            front.start += left;
            taken.push(cut);
            left = 0;
        }
    }
    taken
}

/// One pipe, as one end's transport has it.
struct PipeState {
    spec: Pipe,

    /// Written, waiting to be sent, and how many bytes.
    sending: VecDeque<Piece>,
    sending_bytes: usize,
    /// How many bytes the window still allows on the wire.
    window: u64,
    /// Bytes written by the program, ever.
    written: u64,
    /// No more is written; its end goes once all is sent.
    closed: bool,
    end_sent: bool,
    /// The next chunk's number, on the data pipe.
    next_chunk: u64,
    /// Syncs asked, by number, and those answered.
    next_sync: u32,
    asked: Vec<u32>,
    answered: Vec<u32>,

    /// Received, waiting to be read: whole frames, or chunks, the first
    /// read up to `offset`.
    received: VecDeque<Vec<u8>>,
    offset: usize,
    /// Bytes received and not read.
    unread: u64,
    /// Bytes on the wire received and not yet given back: unread, read and
    /// not given back, and of chunks still being put together.
    outstanding: u64,
    /// Read and not yet given back.
    consumed: u64,
    /// Bytes read by the program, ever.
    read: u64,
    /// Its end came.
    ended: bool,
    /// Its program stopped reading it: what comes is let go of, its window
    /// given back at once.
    abandoned: bool,
    /// Syncs the far end asked that the program has not read to: their
    /// numbers, and how many bytes.
    syncs: Vec<(u32, u64)>,
    /// On the data pipe: chunks come ahead of their turn, by number; the
    /// next in turn; how many there were, once its end says.
    ahead: BTreeMap<u64, Vec<u8>>,
    awaited: u64,
    chunks: Option<u64>,
}

impl PipeState {
    fn new(spec: Pipe) -> Self {
        Self {
            spec,
            sending: VecDeque::new(),
            sending_bytes: 0,
            window: u64::from(spec.window.unwrap_or(0)),
            written: 0,
            closed: false,
            end_sent: false,
            next_chunk: 0,
            next_sync: 0,
            asked: Vec::new(),
            answered: Vec::new(),
            received: VecDeque::new(),
            offset: 0,
            unread: 0,
            outstanding: 0,
            consumed: 0,
            read: 0,
            ended: false,
            abandoned: false,
            syncs: Vec::new(),
            ahead: BTreeMap::new(),
            awaited: 0,
            chunks: None,
        }
    }

    fn full_window(&self) -> u64 {
        u64::from(self.spec.window.unwrap_or(0))
    }

    /// Everything came: its end, and on the data pipe, every chunk before it.
    fn complete(&self) -> bool {
        self.ended && self.chunks.is_none_or(|chunks| self.awaited == chunks)
    }

    /// All written was sent, and its end.
    fn drained(&self) -> bool {
        !self.spec.writes || (self.end_sent && self.sending_bytes == 0)
    }

    /// The next frame of it to send, if the window allows, or its end, once
    /// all is sent.
    fn next(&mut self, number: u8) -> Option<Outgoing> {
        if self.sending_bytes > 0 {
            let most = match self.spec.window {
                Some(_) => MOST.min(self.window as usize),
                None => MOST,
            };
            if most == 0 {
                return None;
            }
            let content = take(&mut self.sending, most);
            let length: usize = content.iter().map(Piece::len).sum();
            self.sending_bytes -= length;
            if self.spec.window.is_some() {
                self.window -= length as u64;
            }
            return Some(Outgoing::new(number, Kind::Bytes, content));
        }
        if self.closed && !self.end_sent {
            self.end_sent = true;
            return Some(Outgoing::of(number, Kind::End, Vec::new()));
        }
        None
    }

    /// The next chunk of the data pipe to send, if the window allows any of
    /// it: as much as waits, up to [`CHUNK`]; or, if `may_end`, its end,
    /// with how many chunks there were, once all is sent.
    fn next_chunk(&mut self, number: u8, may_end: bool) -> Option<Result<Sending, Outgoing>> {
        if self.sending_bytes > 0 {
            if self.window <= CHUNK_HEADER as u64 {
                return None;
            }
            let length = self
                .sending_bytes
                .min(CHUNK)
                .min(self.window as usize - CHUNK_HEADER);
            let mut pieces: VecDeque<Piece> = take(&mut self.sending, length).into();
            self.sending_bytes -= length;
            self.window -= (CHUNK_HEADER + length) as u64;
            let mut header = Vec::with_capacity(CHUNK_HEADER);
            header.extend_from_slice(&self.next_chunk.to_le_bytes());
            header.push(0);
            header.extend_from_slice(&(length as u32).to_le_bytes());
            self.next_chunk += 1;
            pieces.push_front(Piece {
                bytes: Arc::new(header),
                start: 0,
                end: CHUNK_HEADER,
            });
            return Some(Ok(Sending { pieces }));
        }
        if may_end && self.closed && !self.end_sent {
            self.end_sent = true;
            return Some(Err(Outgoing::of(
                number,
                Kind::End,
                self.next_chunk.to_le_bytes().to_vec(),
            )));
        }
        None
    }
}

/// What a change woke: whose waits to end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Woke {
    /// The pipes with more to read, a bit each.
    pub readable: u64,
    /// The pumps have more to send.
    pub pumps: bool,
    /// A sync was answered.
    pub synced: bool,
}

impl Woke {
    fn and(self, other: Self) -> Self {
        Self {
            readable: self.readable | other.readable,
            pumps: self.pumps || other.pumps,
            synced: self.synced || other.synced,
        }
    }
}

/// What one end of a connection knows of its pipes, as its threads share it,
/// doing no I/O.
pub struct Core {
    pipes: Vec<PipeState>,
    /// Frames of the transport's own to send first: window given back,
    /// syncs and their answers.
    urgent: VecDeque<Outgoing>,
    /// Chunks taken by a pump and not yet all written.
    chunks_sending: usize,
    /// The wires that ended, and how many there are.
    wires_ended: usize,
    wires: usize,
    /// Broken, of this kind, for this reason, as first said.
    broken: Option<(io::ErrorKind, String)>,
    /// Let go: the pumps end.
    closing: bool,
}

impl Core {
    /// One end of a connection of `wires` wires, with `pipes`, numbered in
    /// turn.
    pub fn new(pipes: &[Pipe], wires: usize) -> Self {
        assert!(pipes.len() <= 64, "a bit each, in what is woken");
        Self {
            pipes: pipes.iter().copied().map(PipeState::new).collect(),
            urgent: VecDeque::new(),
            chunks_sending: 0,
            wires_ended: 0,
            wires,
            broken: None,
            closing: false,
        }
    }

    fn pipe(&mut self, number: u8) -> Result<&mut PipeState, Fault> {
        self.pipes
            .get_mut(usize::from(number))
            .ok_or(Fault("a frame of a pipe there is none of"))
    }

    /// Takes what came in one frame on wire `wire`, but bytes of the data
    /// pipe, which [`Inbound`] puts together into chunks first.
    fn frame(
        &mut self,
        wire: usize,
        number: u8,
        kind: Kind,
        content: &[u8],
    ) -> Result<Woke, Fault> {
        if wire != 0 {
            return Err(Fault("a frame on a wire but the first, not of data"));
        }
        let pipe = self.pipe(number)?;
        match kind {
            Kind::Bytes => {
                if !pipe.spec.reads {
                    return Err(Fault("bytes on a pipe the other side does not write"));
                }
                if pipe.ended {
                    return Err(Fault("bytes on a pipe after its end"));
                }
                let length = content.len() as u64;
                match pipe.spec.window {
                    Some(window) if pipe.outstanding + length > u64::from(window) => {
                        return Err(Fault("bytes past a pipe's window"));
                    }
                    None if pipe.unread + length > CONTROL_GUARD as u64 => {
                        return Err(Fault("more than 16 MiB unread on the control pipe"));
                    }
                    _ => {}
                }
                if content.is_empty() {
                    return Ok(Woke::default());
                }
                pipe.outstanding += length;
                if pipe.abandoned {
                    return Ok(self.let_go(number, length));
                }
                pipe.received.push_back(content.to_vec());
                pipe.unread += length;
                Ok(Woke {
                    readable: 1 << number,
                    ..Woke::default()
                })
            }
            Kind::Window => {
                let grant = u32::from_le_bytes(
                    content
                        .try_into()
                        .map_err(|_| Fault("window given back of no four bytes"))?,
                );
                if !pipe.spec.writes || pipe.spec.window.is_none() {
                    return Err(Fault(
                        "window given back on a pipe with none of this side's",
                    ));
                }
                pipe.window += u64::from(grant);
                if pipe.window > pipe.full_window() {
                    return Err(Fault("more window given back than was taken"));
                }
                Ok(Woke {
                    pumps: true,
                    ..Woke::default()
                })
            }
            Kind::End => {
                if !pipe.spec.reads {
                    return Err(Fault("the end of a pipe the other side does not write"));
                }
                if pipe.ended {
                    return Err(Fault("a pipe's end twice"));
                }
                if pipe.spec.rank == Rank::Data {
                    let chunks = u64::from_le_bytes(
                        content
                            .try_into()
                            .map_err(|_| Fault("the data's end without its chunks"))?,
                    );
                    let highest = pipe.ahead.last_key_value().map(|(number, _)| *number);
                    if chunks < pipe.awaited || highest.is_some_and(|highest| highest >= chunks) {
                        return Err(Fault("the data's end short of the chunks that came"));
                    }
                    pipe.chunks = Some(chunks);
                } else if !content.is_empty() {
                    return Err(Fault("a pipe's end with content"));
                }
                pipe.ended = true;
                Ok(Woke {
                    readable: 1 << number,
                    ..Woke::default()
                })
            }
            Kind::Sync => {
                if content.len() != 12 {
                    return Err(Fault("a sync of no twelve bytes"));
                }
                if !pipe.spec.reads {
                    return Err(Fault("a sync on a pipe the other side does not write"));
                }
                let id = u32::from_le_bytes(content[..4].try_into().expect("four bytes"));
                let bytes = u64::from_le_bytes(content[4..].try_into().expect("eight bytes"));
                if pipe.read >= bytes {
                    self.urgent.push_back(Outgoing::of(
                        number,
                        Kind::Synced,
                        id.to_le_bytes().to_vec(),
                    ));
                    return Ok(Woke {
                        pumps: true,
                        ..Woke::default()
                    });
                }
                if pipe.syncs.len() >= SYNCS {
                    return Err(Fault("too many syncs unanswered"));
                }
                pipe.syncs.push((id, bytes));
                Ok(Woke::default())
            }
            Kind::Synced => {
                let id = u32::from_le_bytes(
                    content
                        .try_into()
                        .map_err(|_| Fault("a sync's answer of no four bytes"))?,
                );
                let Some(at) = pipe.asked.iter().position(|asked| *asked == id) else {
                    return Err(Fault("the answer to a sync not asked"));
                };
                pipe.asked.swap_remove(at);
                pipe.answered.push(id);
                Ok(Woke {
                    synced: true,
                    ..Woke::default()
                })
            }
        }
    }

    /// A chunk of `length` bytes, numbered `number`, began on a wire: room
    /// is taken for it in the window, and its number checked.
    fn chunk_begins(&mut self, number: u64, length: usize) -> Result<(), Fault> {
        let pipe = self.pipe(DATA)?;
        if !pipe.spec.reads || pipe.spec.rank != Rank::Data {
            return Err(Fault("a chunk on a pipe the other side does not write"));
        }
        if length == 0 || length > CHUNK {
            return Err(Fault("a chunk of no bytes, or more than a chunk holds"));
        }
        let window = pipe.full_window();
        // Numbered from 0, no chunk is the last a number holds: the next
        // after it would be past it.
        if number == u64::MAX {
            return Err(Fault("a chunk past any a sender can number"));
        }
        if number < pipe.awaited || pipe.ahead.contains_key(&number) {
            return Err(Fault("a chunk twice"));
        }
        if pipe.chunks.is_some_and(|chunks| number >= chunks) {
            return Err(Fault("a chunk past the data's end"));
        }
        // No more chunks can be on their way ahead of the one awaited than
        // the window holds, each of a byte at least.
        if number - pipe.awaited > window / (CHUNK_HEADER as u64 + 1) {
            return Err(Fault("a chunk further ahead than the window holds"));
        }
        let bytes = (CHUNK_HEADER + length) as u64;
        if pipe.outstanding + bytes > window {
            return Err(Fault("a chunk past the data's window"));
        }
        pipe.outstanding += bytes;
        Ok(())
    }

    /// A chunk came whole: it goes in turn, with those ahead of it that are
    /// next. Its header is given back as it goes: the reader reads none of
    /// it.
    fn chunk(&mut self, number: u64, content: Vec<u8>) -> Result<Woke, Fault> {
        let pipe = self.pipe(DATA)?;
        // Checked as it began; since, another of its number may have come,
        // or the data's end, short of it.
        if number < pipe.awaited || pipe.ahead.contains_key(&number) {
            return Err(Fault("a chunk twice"));
        }
        if pipe.chunks.is_some_and(|chunks| number >= chunks) {
            return Err(Fault("a chunk past the data's end"));
        }
        pipe.ahead.insert(number, content);
        let mut woke = Woke::default();
        while let Some(content) = pipe.ahead.remove(&pipe.awaited) {
            pipe.awaited += 1;
            pipe.consumed += CHUNK_HEADER as u64;
            if pipe.abandoned {
                pipe.consumed += content.len() as u64;
                continue;
            }
            pipe.unread += content.len() as u64;
            pipe.received.push_back(content);
            woke.readable = 1 << DATA;
        }
        Ok(woke.and(self.give_back(DATA)))
    }

    /// Wire `wire` ended, `inbound` having read it.
    pub fn wire_ended(&mut self, wire: usize, inbound: &Inbound) -> Result<(), Fault> {
        if !inbound.buffer.is_empty() {
            return Err(Fault("a wire ended inside a frame"));
        }
        if inbound.chunk.is_some() {
            return Err(Fault("a wire ended inside a chunk"));
        }
        self.wires_ended += 1;
        if wire == 0 {
            // The first wire carries every pipe's end: one that ends before
            // all came, both ways, was let go of early, as when the other
            // side stopped.
            let unread = self.pipes.iter().any(|pipe| pipe.spec.reads && !pipe.ended);
            let unsent = self.pipes.iter().any(|pipe| !pipe.drained());
            if unread || unsent {
                return Err(Fault("the stream ended before the other side was done"));
            }
        }
        if self.wires_ended == self.wires
            && self
                .pipes
                .iter()
                .any(|pipe| pipe.spec.reads && !pipe.complete())
        {
            return Err(Fault("the stream ended before every chunk came"));
        }
        Ok(())
    }

    /// Window to give back on `number`, once a quarter of it was read.
    fn give_back(&mut self, number: u8) -> Woke {
        let pipe = &mut self.pipes[usize::from(number)];
        let Some(window) = pipe.spec.window else {
            return Woke::default();
        };
        let due = if pipe.abandoned {
            1
        } else {
            (u64::from(window) / 4).max(1)
        };
        if pipe.consumed < due || pipe.ended && pipe.complete() {
            return Woke::default();
        }
        let grant = pipe.consumed.min(u64::from(u32::MAX));
        pipe.consumed -= grant;
        pipe.outstanding -= grant;
        self.urgent.push_back(Outgoing::of(
            number,
            Kind::Window,
            (grant as u32).to_le_bytes().to_vec(),
        ));
        Woke {
            pumps: true,
            ..Woke::default()
        }
    }

    /// `bytes` of `number`, which its program stopped reading, are let go
    /// of as they come: as if read, but for syncs, which stay unanswered.
    fn let_go(&mut self, number: u8, bytes: u64) -> Woke {
        let pipe = &mut self.pipes[usize::from(number)];
        if pipe.spec.window.is_some() {
            pipe.consumed += bytes;
        } else {
            pipe.outstanding -= bytes;
        }
        self.give_back(number)
    }

    /// The program stopped reading `number`: what came is let go of, and
    /// what comes will be, its window given back.
    pub fn abandon(&mut self, number: u8) -> Woke {
        let pipe = &mut self.pipes[usize::from(number)];
        pipe.abandoned = true;
        pipe.received.clear();
        pipe.offset = 0;
        let unread = std::mem::take(&mut pipe.unread);
        let woke = self.let_go(number, unread);
        woke.and(Woke {
            readable: 1 << number,
            ..Woke::default()
        })
    }

    /// The program read `bytes` of `number`: window to give back, and syncs
    /// to answer.
    fn was_read(&mut self, number: u8, bytes: usize) -> Woke {
        let pipe = &mut self.pipes[usize::from(number)];
        pipe.read += bytes as u64;
        pipe.unread -= bytes as u64;
        if pipe.spec.window.is_some() {
            pipe.consumed += bytes as u64;
        } else {
            pipe.outstanding -= bytes as u64;
        }
        let read = pipe.read;
        let mut answered = Vec::new();
        pipe.syncs.retain(|(id, wanted)| {
            let due = read >= *wanted;
            if due {
                answered.push(*id);
            }
            !due
        });
        let mut woke = Woke::default();
        for id in answered {
            self.urgent.push_back(Outgoing::of(
                number,
                Kind::Synced,
                id.to_le_bytes().to_vec(),
            ));
            woke.pumps = true;
        }
        woke.and(self.give_back(number))
    }

    /// Reads all there is to read on `number`, as its program would: for
    /// what has no threads, as the fuzzer.
    pub fn read_all(&mut self, number: u8) -> Vec<u8> {
        let pipe = &mut self.pipes[usize::from(number)];
        let offset = std::mem::take(&mut pipe.offset);
        let mut read: Vec<u8> = pipe.received.drain(..).flatten().collect();
        read.drain(..offset);
        let length = read.len();
        self.was_read(number, length);
        read
    }

    /// Bytes on the wire `number` holds that were not given back, and its
    /// window: never more.
    pub fn outstanding(&self, number: u8) -> (u64, Option<u32>) {
        let pipe = &self.pipes[usize::from(number)];
        (pipe.outstanding, pipe.spec.window)
    }

    /// The next frame for wire `wire`'s pump to send, if any, and on the
    /// data pipe, the next frame of the chunk it is sending, or a new chunk.
    pub fn next(&mut self, wire: usize, sending: &mut Sending) -> Option<Outgoing> {
        if wire != 0 {
            return self.data(wire, sending);
        }
        if let Some(frame) = self.urgent.pop_front() {
            return Some(frame);
        }
        for rank in [Rank::First, Rank::Second] {
            if let Some(frame) = self.ranked(rank) {
                return Some(frame);
            }
        }
        let data_first = self
            .pipes
            .iter()
            .any(|pipe| pipe.spec.rank == Rank::Data && pipe.sending_bytes >= DATA_BUFFER / 4);
        if data_first {
            self.data(wire, sending)
                .or_else(|| self.ranked(Rank::Paced))
        } else {
            self.ranked(Rank::Paced)
                .or_else(|| self.data(wire, sending))
        }
    }

    /// The next frame of a pipe of `rank`.
    fn ranked(&mut self, rank: Rank) -> Option<Outgoing> {
        self.pipes
            .iter_mut()
            .enumerate()
            .filter(|(_, pipe)| pipe.spec.writes && pipe.spec.rank == rank)
            .find_map(|(number, pipe)| pipe.next(number as u8))
    }

    /// The next frame of data for wire `wire`: of the chunk being sent, or of
    /// a new one, or the data's end, on the first wire only.
    fn data(&mut self, wire: usize, sending: &mut Sending) -> Option<Outgoing> {
        if sending.busy() {
            let frame = sending.frame();
            if !sending.busy() {
                self.chunks_sending -= 1;
            }
            return Some(frame);
        }
        let number = self
            .pipes
            .iter()
            .position(|pipe| pipe.spec.writes && pipe.spec.rank == Rank::Data)?;
        match self.pipes[number].next_chunk(number as u8, wire == 0)? {
            Ok(chunk) => {
                *sending = chunk;
                self.chunks_sending += 1;
                let frame = sending.frame();
                if !sending.busy() {
                    self.chunks_sending -= 1;
                }
                Some(frame)
            }
            Err(end) => Some(end),
        }
    }

    /// Everything this end writes was sent, and each pipe's end.
    fn drained(&self) -> bool {
        self.chunks_sending == 0 && self.pipes.iter().all(PipeState::drained)
    }
}

/// What one wire's reader makes of what comes on it: frames, as far as they
/// came, and on the data pipe, its chunks put together. It does no I/O.
pub struct Inbound {
    wire: usize,
    /// What came and is not yet taken: a frame's header, or the frame being
    /// read.
    buffer: Vec<u8>,
    /// The chunk being put together on this wire.
    chunk: Option<Chunk>,
}

/// A chunk, as it comes.
struct Chunk {
    header: [u8; CHUNK_HEADER],
    filled: usize,
    number: u64,
    length: usize,
    content: Vec<u8>,
}

impl Inbound {
    pub fn new(wire: usize) -> Self {
        Self {
            wire,
            buffer: Vec::new(),
            chunk: None,
        }
    }

    /// Takes `bytes`, just come, into `core`: what it woke.
    pub fn take(&mut self, bytes: &[u8], core: &mut Core) -> Result<Woke, Fault> {
        self.buffer.extend_from_slice(bytes);
        let mut woke = Woke::default();
        let mut at = 0;
        while self.buffer.len() - at >= HEADER {
            let header = &self.buffer[at..at + HEADER];
            let number = header[0];
            let kind = Kind::from_byte(header[1]).ok_or(Fault("a frame of no kind there is"))?;
            let length = u32::from_le_bytes(header[2..].try_into().expect("four bytes")) as usize;
            if length > MOST {
                return Err(Fault("a frame longer than any is"));
            }
            if self.buffer.len() - at < HEADER + length {
                break;
            }
            let content = &self.buffer[at + HEADER..at + HEADER + length];
            let is_data = core
                .pipes
                .get(usize::from(number))
                .is_some_and(|pipe| pipe.spec.rank == Rank::Data);
            let more = if is_data && kind == Kind::Bytes {
                take_chunk(&mut self.chunk, content, core)?
            } else {
                core.frame(self.wire, number, kind, content)?
            };
            woke = woke.and(more);
            at += HEADER + length;
        }
        self.buffer.drain(..at);
        Ok(woke)
    }
}

/// Takes `bytes` of the data pipe into the chunk being put together, as
/// many chunks as they hold.
fn take_chunk(chunk: &mut Option<Chunk>, mut bytes: &[u8], core: &mut Core) -> Result<Woke, Fault> {
    let mut woke = Woke::default();
    while !bytes.is_empty() {
        let being = chunk.get_or_insert_with(|| Chunk {
            header: [0; CHUNK_HEADER],
            filled: 0,
            number: 0,
            length: 0,
            content: Vec::new(),
        });
        if being.filled < CHUNK_HEADER {
            let taken = (CHUNK_HEADER - being.filled).min(bytes.len());
            being.header[being.filled..being.filled + taken].copy_from_slice(&bytes[..taken]);
            being.filled += taken;
            bytes = &bytes[taken..];
            if being.filled < CHUNK_HEADER {
                break;
            }
            being.number = u64::from_le_bytes(being.header[..8].try_into().expect("eight bytes"));
            if being.header[8] != 0 {
                return Err(Fault("a chunk of an encoding there is none of"));
            }
            being.length =
                u32::from_le_bytes(being.header[9..].try_into().expect("four bytes")) as usize;
            core.chunk_begins(being.number, being.length)?;
            being.content.reserve_exact(being.length);
        }
        let taken = (being.length - being.content.len()).min(bytes.len());
        being.content.extend_from_slice(&bytes[..taken]);
        bytes = &bytes[taken..];
        if being.content.len() == being.length {
            let done = chunk.take().expect("being put together");
            woke = woke.and(core.chunk(done.number, done.content)?);
        }
    }
    Ok(woke)
}

/// What a connection's threads, readers and writers share.
struct Shared {
    core: Mutex<Core>,
    /// Woken as a pipe has more to read, or room to write; as the pumps
    /// have more to send; as a sync is answered; as all is sent.
    readable: Vec<Condvar>,
    writable: Vec<Condvar>,
    pumps: Condvar,
    synced: Condvar,
    drained: Condvar,
    /// One writer at a time on each pipe, whole writes apart.
    writing: Vec<Mutex<()>>,
    /// How much waits to be sent on each pipe this end writes, for the
    /// dashboard.
    gauges: Vec<Option<Gauge>>,
    /// The wires, to take for broken.
    carried: Vec<Carried>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Core> {
        self.core.lock().expect("not poisoned")
    }

    fn wake(&self, woke: Woke) {
        for (number, readable) in self.readable.iter().enumerate() {
            if woke.readable & (1 << number) != 0 {
                readable.notify_all();
            }
        }
        if woke.pumps {
            self.pumps.notify_all();
        }
        if woke.synced {
            self.synced.notify_all();
        }
    }

    /// Takes the connection for broken, of `kind`, for `why`, unless it was
    /// already: every wait on it ends, and every wire is taken for broken,
    /// so that the far end sees it so.
    fn broke(&self, core: &mut Core, kind: io::ErrorKind, why: &str) {
        if core.broken.is_none() {
            core.broken = Some((kind, format!("the stream broke: {why}")));
        }
        for carried in &self.carried {
            carried.take_for_broken(why);
        }
        self.wake_all();
    }

    fn wake_all(&self) {
        for condvar in self.readable.iter().chain(&self.writable) {
            condvar.notify_all();
        }
        self.pumps.notify_all();
        self.synced.notify_all();
        self.drained.notify_all();
    }
}

fn error((kind, why): &(io::ErrorKind, String)) -> io::Error {
    io::Error::new(*kind, why.clone())
}

/// One end of a connection whose wires are ready, carrying its pipes.
pub struct Connection {
    shared: Arc<Shared>,
    readers: Vec<Option<PipeReader>>,
    pumps: Vec<JoinHandle<()>>,
    receivers: Vec<JoinHandle<()>>,
}

impl Connection {
    /// Carries `pipes`, numbered in turn, on `wires`, ready: their pipes
    /// begin at once. Data goes on all of them; all else on the first.
    pub fn open(wires: Vec<Wire>, pipes: &[Pipe]) -> io::Result<Self> {
        let mut ends = Vec::new();
        for wire in wires {
            ends.push(wire.ready()?);
        }
        let count = ends.len();
        let shared = Arc::new(Shared {
            core: Mutex::new(Core::new(pipes, count)),
            readable: pipes.iter().map(|_| Condvar::new()).collect(),
            writable: pipes.iter().map(|_| Condvar::new()).collect(),
            pumps: Condvar::new(),
            synced: Condvar::new(),
            drained: Condvar::new(),
            writing: pipes.iter().map(|_| Mutex::new(())).collect(),
            gauges: pipes
                .iter()
                .map(|pipe| {
                    let buffer = pipe.buffer();
                    pipe.writes.then(|| {
                        let capacity = if buffer == usize::MAX { 0 } else { buffer };
                        Gauge::of_bytes(format!("{} to send", pipe.name), capacity)
                    })
                })
                .collect(),
            carried: ends.iter().map(|(_, _, carried)| carried.clone()).collect(),
        });
        let mut pumps = Vec::new();
        let mut receivers = Vec::new();
        for (wire, (input, output, _)) in ends.into_iter().enumerate() {
            let receiving = Arc::clone(&shared);
            receivers.push(std::thread::spawn(move || receive(wire, input, &receiving)));
            let pumping = Arc::clone(&shared);
            pumps.push(std::thread::spawn(move || pump(wire, output, &pumping)));
        }
        let readers = pipes
            .iter()
            .enumerate()
            .map(|(number, pipe)| {
                pipe.reads.then(|| PipeReader {
                    shared: Arc::clone(&shared),
                    number: number as u8,
                })
            })
            .collect();
        Ok(Self {
            shared,
            readers,
            pumps,
            receivers,
        })
    }

    /// The writing end of pipe `number`, which this end writes: clones write
    /// the same pipe, each write whole.
    pub fn writer(&self, number: u8) -> PipeWriter {
        assert!(
            self.shared.lock().pipes[usize::from(number)].spec.writes,
            "a pipe this end does not write"
        );
        PipeWriter {
            shared: Arc::clone(&self.shared),
            number,
        }
    }

    /// The reading end of pipe `number`, which this end reads, once.
    pub fn reader(&mut self, number: u8) -> PipeReader {
        self.readers[usize::from(number)]
            .take()
            .expect("a pipe this end reads, not taken yet")
    }

    /// What each wire carried, for the dashboard.
    pub fn carried(&self) -> Vec<Carried> {
        self.shared.carried.clone()
    }

    /// Closes every pipe this end writes, once its writers are done: each
    /// end goes once all written before it is sent. Returns once all went,
    /// or the connection broke.
    pub fn finish(&self) -> io::Result<()> {
        let count = self.shared.writing.len();
        for number in 0..count {
            let _writing = self.shared.writing[number].lock().expect("not poisoned");
            let mut core = self.shared.lock();
            let pipe = &mut core.pipes[number];
            if pipe.spec.writes {
                pipe.closed = true;
            }
        }
        self.shared.pumps.notify_all();
        let mut core = self.shared.lock();
        loop {
            if core.drained() {
                return Ok(());
            }
            if let Some(broken) = &core.broken {
                return Err(error(broken));
            }
            core = profile::wait(State::WriteConnection, || {
                self.shared.drained.wait(core).expect("not poisoned")
            });
        }
    }
}

impl Drop for Connection {
    /// Lets go of the connection. Once all went both ways, each pump ends
    /// after the frame it is writing, and shuts its wire, so that the far end
    /// reads to its end; then each wire's reader stops. One let go of before
    /// that is broken, as the far end sees it.
    fn drop(&mut self) {
        {
            let mut core = self.shared.lock();
            core.closing = true;
            if core.broken.is_none() {
                let done = core.drained()
                    && core
                        .pipes
                        .iter()
                        .all(|pipe| !pipe.spec.reads || pipe.complete());
                if !done {
                    self.shared
                        .broke(&mut core, io::ErrorKind::ConnectionAborted, LET_GO);
                }
            }
            self.shared.wake_all();
        }
        for pump in self.pumps.drain(..) {
            let _ = pump.join();
        }
        for carried in &self.shared.carried {
            carried.end_scope(LET_GO);
        }
        for receiver in self.receivers.drain(..) {
            let _ = receiver.join();
        }
    }
}

/// Reads wire `wire`, putting what comes into the pipes, until it ends or
/// breaks: it never waits for a program.
fn receive(wire: usize, mut input: WireIn, shared: &Shared) {
    let _thread = profile::thread("inbound");
    let mut inbound = Inbound::new(wire);
    let mut buffer = vec![0; HEADER + MOST];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                let mut core = shared.lock();
                if !core.closing {
                    shared.broke(&mut core, error.kind(), &error.to_string());
                }
                return;
            }
        };
        let mut core = shared.lock();
        let taken = if read == 0 {
            core.wire_ended(wire, &inbound).map(|()| None)
        } else {
            inbound.take(&buffer[..read], &mut core).map(Some)
        };
        match taken {
            Ok(Some(woke)) => {
                drop(core);
                shared.wake(woke);
            }
            Ok(None) => {
                shared.wake_all();
                return;
            }
            Err(Fault(why)) => {
                let kind = if read == 0 {
                    io::ErrorKind::ConnectionAborted
                } else {
                    io::ErrorKind::InvalidData
                };
                if !core.closing {
                    shared.broke(&mut core, kind, why);
                }
                return;
            }
        }
    }
}

/// Sends what the pipes have for wire `wire`, as it has room, until the
/// connection is let go of, or breaks.
fn pump(wire: usize, mut output: WireOut, shared: &Shared) {
    let _thread = profile::thread("pump");
    let mut sending = Sending::default();
    let mut frame = Vec::with_capacity(HEADER + MOST);
    loop {
        let next = {
            let mut core = shared.lock();
            loop {
                if core.broken.is_some() || core.closing && !sending.busy() {
                    output.shut();
                    return;
                }
                if let Some(next) = core.next(wire, &mut sending) {
                    if core.drained() {
                        shared.drained.notify_all();
                    }
                    // Room was made in the pipe's send buffer.
                    if next.header[1] == Kind::Bytes as u8 {
                        let number = usize::from(next.header[0]);
                        if let Some(gauge) = &shared.gauges[number] {
                            gauge.set(core.pipes[number].sending_bytes);
                        }
                        shared.writable[number].notify_all();
                    }
                    break next;
                }
                core = profile::wait(State::Work, || {
                    shared.pumps.wait(core).expect("not poisoned")
                });
            }
        };
        frame.clear();
        frame.extend_from_slice(&next.header);
        for piece in &next.content {
            frame.extend_from_slice(piece.as_slice());
        }
        if let Err(error) = profile::wait(State::WriteConnection, || output.write_all(&frame)) {
            let mut core = shared.lock();
            if !core.closing {
                shared.broke(&mut core, error.kind(), &error.to_string());
            }
            return;
        }
    }
}

/// The writing end of a pipe: each write whole, among those of its clones.
#[derive(Clone)]
pub struct PipeWriter {
    shared: Arc<Shared>,
    number: u8,
}

impl PipeWriter {
    /// Writes all of `bytes`, waiting while the pipe's send buffer is full,
    /// until the connection breaks.
    pub fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let _writing = shared.writing[number].lock().expect("not poisoned");
        let state = if shared.lock().pipes[number].spec.rank == Rank::Data {
            State::DataQueue
        } else {
            State::Paced
        };
        while !bytes.is_empty() {
            // Room only grows meanwhile: this is the pipe's one writer.
            let room = {
                let mut core = shared.lock();
                loop {
                    if let Some(broken) = &core.broken {
                        return Err(error(broken));
                    }
                    let pipe = &core.pipes[number];
                    if pipe.closed {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("the {} pipe is closed", pipe.spec.name),
                        ));
                    }
                    let room = pipe.spec.buffer().saturating_sub(pipe.sending_bytes);
                    if room > 0 {
                        break room;
                    }
                    core = profile::wait(state, || {
                        shared.writable[number].wait(core).expect("not poisoned")
                    });
                }
            };
            let taken = room.min(bytes.len());
            let piece = Piece {
                bytes: Arc::new(bytes[..taken].to_vec()),
                start: 0,
                end: taken,
            };
            bytes = &bytes[taken..];
            let mut core = shared.lock();
            if let Some(broken) = &core.broken {
                return Err(error(broken));
            }
            let pipe = &mut core.pipes[number];
            pipe.sending.push_back(piece);
            pipe.sending_bytes += taken;
            pipe.written += taken as u64;
            if let Some(gauge) = &shared.gauges[number] {
                gauge.set(pipe.sending_bytes);
            }
            drop(core);
            shared.pumps.notify_all();
        }
        Ok(())
    }

    /// Returns once the far program has read all written before it, or the
    /// connection broke.
    pub fn sync(&self) -> io::Result<()> {
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let id = {
            let _writing = shared.writing[number].lock().expect("not poisoned");
            let mut core = shared.lock();
            let pipe = &mut core.pipes[number];
            let id = pipe.next_sync;
            pipe.next_sync = pipe.next_sync.wrapping_add(1);
            pipe.asked.push(id);
            let mut content = id.to_le_bytes().to_vec();
            content.extend_from_slice(&pipe.written.to_le_bytes());
            core.urgent
                .push_back(Outgoing::of(self.number, Kind::Sync, content));
            id
        };
        shared.pumps.notify_all();
        let mut core = shared.lock();
        loop {
            let pipe = &mut core.pipes[number];
            if let Some(at) = pipe.answered.iter().position(|answered| *answered == id) {
                pipe.answered.swap_remove(at);
                return Ok(());
            }
            if let Some(broken) = &core.broken {
                return Err(error(broken));
            }
            core = profile::wait(State::WriteConnection, || {
                shared.synced.wait(core).expect("not poisoned")
            });
        }
    }

    /// How much waits to be sent on the pipe, for the dashboard.
    pub fn gauge(&self) -> Gauge {
        self.shared.gauges[usize::from(self.number)]
            .clone()
            .expect("a pipe this end writes")
    }

    /// Closes the pipe: its end goes once all written before is sent.
    pub fn close(&self) {
        let number = usize::from(self.number);
        let _writing = self.shared.writing[number].lock().expect("not poisoned");
        self.shared.lock().pipes[number].closed = true;
        self.shared.pumps.notify_all();
    }
}

impl io::Write for PipeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        PipeWriter::write_all(self, bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The reading end of a pipe.
pub struct PipeReader {
    shared: Arc<Shared>,
    number: u8,
}

impl PipeReader {
    /// A way to stop reading the pipe, from another thread.
    pub fn stopper(&self) -> Stopper {
        Stopper {
            shared: Arc::clone(&self.shared),
            number: self.number,
        }
    }
}

/// Stops reading a pipe, as its program does once it stops: a read waiting
/// returns as at its end, and what comes is let go of, its window given back
/// at once, so that its writer never waits for a reader that is gone.
#[derive(Clone)]
pub struct Stopper {
    shared: Arc<Shared>,
    number: u8,
}

impl Stopper {
    pub fn stop(&self) {
        let woke = self.shared.lock().abandon(self.number);
        self.shared.wake(woke);
    }
}

impl io::Read for PipeReader {
    /// Reads what came, waiting for some: 0 once the pipe ended, all it
    /// carried read.
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if into.is_empty() {
            return Ok(0);
        }
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let (piece, offset) = {
            let mut core = shared.lock();
            loop {
                let pipe = &mut core.pipes[number];
                if pipe.abandoned {
                    return Ok(0);
                }
                if let Some(piece) = pipe.received.pop_front() {
                    break (piece, std::mem::take(&mut pipe.offset));
                }
                if pipe.complete() {
                    return Ok(0);
                }
                if let Some(broken) = &core.broken {
                    return Err(error(broken));
                }
                core = profile::wait(State::ReadConnection, || {
                    shared.readable[number].wait(core).expect("not poisoned")
                });
            }
        };
        let taken = (piece.len() - offset).min(into.len());
        into[..taken].copy_from_slice(&piece[offset..offset + taken]);
        let mut core = shared.lock();
        if offset + taken < piece.len() {
            let pipe = &mut core.pipes[number];
            pipe.received.push_front(piece);
            pipe.offset = offset + taken;
        }
        let woke = core.was_read(self.number, taken);
        drop(core);
        shared.wake(woke);
        Ok(taken)
    }
}

#[cfg(test)]
mod tests;
