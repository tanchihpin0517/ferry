//! Pipes: what a connection carries once it is ready. Each kind of content
//! has a pipe of its own, one way, in order, reliable: a program writes what
//! it has, as it would a file, the write waiting while the pipe can take no
//! more; and reads what is there, records cut however they came. How much
//! goes in a packet, when, among what else, and whether the far end can take
//! more, is the transport's: no program sees it. See the
//! [transport](../../docs/design/architecture.md#transport) and
//! [what the pipes carry](../../docs/design/architecture.md#what-the-pipes-carry).
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
//! - **Compression.** A connection's pipes may be compressed, each but the
//!   data's as one zstd stream, which its pump compresses as it takes what
//!   waits, at most a frame's worth, and flushes, and its reader
//!   decompresses as its program reads: bytes gather while the wire is
//!   busy, and compress the better. The data's chunks are compressed each
//!   alone, by [`COMPRESSORS`] threads, as few ahead of the pumps, a chunk
//!   that does not shrink going as it is, and decompressed so on the far
//!   end, put back in order. Windows are of bytes on the wire, compressed.
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

/// How many threads compress the data's chunks, where the connection is
/// compressed, and decompress them: and how many chunks may be compressed,
/// or being compressed, ahead of the pumps.
pub const COMPRESSORS: usize = 8;

/// How many bytes of the data's chunks may wait to be read, decompressed,
/// before the decompressors wait: but for the chunk next in turn, which
/// they always take.
const DECOMPRESSED: usize = DATA_BUFFER;

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
    /// The zstd level its bytes go at, compressed, the connection's; none
    /// to go as they are.
    pub compress: Option<i32>,
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
/// Destination's; compressed at `compress`, if any.
pub fn stream(source: bool, windows: Windows, compress: Option<i32>) -> Vec<Pipe> {
    compressed(
        vec![
            Pipe {
                name: "control",
                writes: true,
                reads: true,
                window: None,
                rank: Rank::First,
                compress: None,
            },
            Pipe {
                name: "manifest",
                writes: source,
                reads: !source,
                window: Some(windows.manifest),
                rank: Rank::Paced,
                compress: None,
            },
            Pipe {
                name: "data",
                writes: source,
                reads: !source,
                window: Some(windows.data),
                rank: Rank::Data,
                compress: None,
            },
            Pipe {
                name: "answers",
                writes: !source,
                reads: source,
                window: Some(windows.answers),
                rank: Rank::Second,
                compress: None,
            },
        ],
        compress,
    )
}

/// `pipes`, compressed at `compress`.
fn compressed(mut pipes: Vec<Pipe>, compress: Option<i32>) -> Vec<Pipe> {
    for pipe in &mut pipes {
        pipe.compress = compress;
    }
    pipes
}

/// The pipes of a server's control connection, by number: control, and the
/// server's reports.
pub const REPORTS: u8 = 1;

/// A control connection's pipes, as the server has them, or the client:
/// control, both ways; the reports, the server's, of `reports` window;
/// compressed at `compress`, if any.
pub fn control_connection(server: bool, reports: u32, compress: Option<i32>) -> Vec<Pipe> {
    compressed(
        vec![
            Pipe {
                name: "control",
                writes: true,
                reads: true,
                window: None,
                rank: Rank::First,
                compress: None,
            },
            Pipe {
                name: "reports",
                writes: server,
                reads: !server,
                window: Some(reports),
                rank: Rank::Second,
                compress: None,
            },
        ],
        compress,
    )
}

/// The reports' window: about 130,000 of a dry run's paths, compressed.
pub const REPORTS_WINDOW: u32 = 1 << 20;

/// The link's one pipe, control, both ways, between the client's monitor
/// and a host's, compressed at `compress`, if any.
pub fn link(compress: Option<i32>) -> Vec<Pipe> {
    vec![Pipe {
        name: "control",
        writes: true,
        reads: true,
        window: None,
        rank: Rank::First,
        compress,
    }]
}

/// The most room on the wire `length` bytes take, compressed as a zstd
/// stream and flushed: what zstd may add to bytes that do not compress,
/// and a block's and a frame's headers, with room to spare.
pub fn compressed_bound(length: usize) -> usize {
    length + length / 128 + 1024
}

/// The most bytes that, compressed, surely fit in `room` on the wire.
fn fits_compressed(room: usize) -> usize {
    room.saturating_sub(1024) / 129 * 128
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
    /// Its content to compress first, on its pipe's stream: its pump does,
    /// and gives back what of the window it took it did not use.
    squeeze: Option<Squeeze>,
}

/// How a frame's content is compressed, and how much of its pipe's window
/// was taken for it: the most it may take.
#[derive(Clone, Copy, Debug)]
struct Squeeze {
    level: i32,
    taken: usize,
}

impl Outgoing {
    fn new(pipe: u8, kind: Kind, content: Vec<Piece>) -> Self {
        let length: usize = content.iter().map(Piece::len).sum();
        let mut header = [pipe, kind as u8, 0, 0, 0, 0];
        header[2..].copy_from_slice(&(length as u32).to_le_bytes());
        Self {
            header,
            content,
            squeeze: None,
        }
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

/// A frame of `kind` on pipe `number` with `content`, as the transport
/// writes it: for what makes one by hand, as a test.
pub fn frame(number: u8, kind: Kind, content: &[u8]) -> Vec<u8> {
    Outgoing::of(number, kind, content.to_vec()).bytes()
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

/// What came of a pipe, waiting to be read: its bytes, and what they took
/// on the wire. A frame's bytes are given back as they are read; a chunk's,
/// once it is read whole.
struct Came {
    bytes: Vec<u8>,
    wire: usize,
}

/// A chunk of the data for a compressor: its number, its bytes, the level,
/// and how much of the window was taken for it, the most it may take.
pub struct Compress {
    number: u64,
    pieces: Vec<Piece>,
    level: i32,
    taken: usize,
}

/// One pipe, as one end's transport has it.
struct PipeState {
    spec: Pipe,

    /// Written, waiting to be sent, and how many bytes.
    sending: VecDeque<Piece>,
    sending_bytes: usize,
    /// How many bytes the window still allows on the wire.
    window: u64,
    /// Bytes written by the program, ever, and of those, written on the
    /// wire, but for the data's, which go in chunks.
    written: u64,
    sent: u64,
    /// No more is written; its end goes once all is sent, taken by a pump,
    /// then written on the wire.
    closed: bool,
    end_sent: bool,
    end_written: bool,
    /// The next chunk's number, on the data pipe.
    next_chunk: u64,
    /// Where the data is compressed: its chunks being compressed, and those
    /// compressed, waiting for a pump.
    compressing: usize,
    ready: VecDeque<Sending>,
    /// Syncs asked, by number, and those answered.
    next_sync: u32,
    asked: Vec<u32>,
    answered: Vec<u32>,

    /// Received, waiting to be read: whole frames, or chunks, the first
    /// read up to `offset`.
    received: VecDeque<Came>,
    offset: usize,
    /// Bytes received and not read.
    unread: u64,
    /// Bytes on the wire received and not yet given back: unread, read and
    /// not given back, and of chunks still being put together.
    outstanding: u64,
    /// Read and not yet given back.
    consumed: u64,
    /// Bytes read by the program, ever, and what they took on the wire,
    /// given back.
    read: u64,
    wire_read: u64,
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
    ahead: BTreeMap<u64, Came>,
    awaited: u64,
    chunks: Option<u64>,
    /// The numbers of the chunks that came and are not in turn yet: ahead,
    /// compressed, or being decompressed.
    came: std::collections::BTreeSet<u64>,
    /// Chunks that came compressed, waiting for a decompressor, by number.
    compressed: BTreeMap<u64, Vec<u8>>,
    /// Bytes of chunks ahead, or in turn and not read.
    held: usize,
}

impl PipeState {
    fn new(spec: Pipe) -> Self {
        Self {
            spec,
            sending: VecDeque::new(),
            sending_bytes: 0,
            window: u64::from(spec.window.unwrap_or(0)),
            written: 0,
            sent: 0,
            closed: false,
            end_sent: false,
            end_written: false,
            next_chunk: 0,
            compressing: 0,
            ready: VecDeque::new(),
            next_sync: 0,
            asked: Vec::new(),
            answered: Vec::new(),
            received: VecDeque::new(),
            offset: 0,
            unread: 0,
            outstanding: 0,
            consumed: 0,
            read: 0,
            wire_read: 0,
            ended: false,
            abandoned: false,
            syncs: Vec::new(),
            ahead: BTreeMap::new(),
            awaited: 0,
            chunks: None,
            came: std::collections::BTreeSet::new(),
            compressed: BTreeMap::new(),
            held: 0,
        }
    }

    fn full_window(&self) -> u64 {
        u64::from(self.spec.window.unwrap_or(0))
    }

    /// Everything came, in turn: its end, and on the data pipe, every chunk
    /// before it.
    fn complete(&self) -> bool {
        self.ended && self.chunks.is_none_or(|chunks| self.awaited == chunks)
    }

    /// Everything came off the wire, if not all in turn yet: some chunks may
    /// still be decompressing.
    fn all_came(&self) -> bool {
        self.ended
            && self
                .chunks
                .is_none_or(|chunks| self.awaited + self.came.len() as u64 == chunks)
    }

    /// All written went on the wire, and its end.
    fn drained(&self) -> bool {
        !self.spec.writes || (self.end_written && self.sending_bytes == 0)
    }

    /// All written was taken to go on the wire, and its end: gone, as the
    /// far end sees it, once it has its end, though the pump that wrote it
    /// has not yet said so.
    fn all_taken(&self) -> bool {
        !self.spec.writes || (self.end_sent && self.sending_bytes == 0)
    }

    /// The next frame of it to send, if the window allows, or its end, once
    /// all is sent.
    fn next(&mut self, number: u8) -> Option<Outgoing> {
        if self.sending_bytes > 0 {
            let room = match self.spec.window {
                Some(_) => self.window as usize,
                None => usize::MAX,
            };
            // Compressed, it may take a little more room than it is long:
            // room is taken for the most it may.
            let most = match self.spec.compress {
                Some(_) => MOST.min(fits_compressed(room)),
                None => MOST.min(room),
            };
            if most == 0 {
                return None;
            }
            let content = take(&mut self.sending, most);
            let length: usize = content.iter().map(Piece::len).sum();
            self.sending_bytes -= length;
            let taken = match self.spec.compress {
                Some(_) => compressed_bound(length),
                None => length,
            };
            if self.spec.window.is_some() {
                self.window -= taken as u64;
            }
            let mut frame = Outgoing::new(number, Kind::Bytes, content);
            frame.squeeze = self.spec.compress.map(|level| Squeeze { level, taken });
            return Some(frame);
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
    /// The compressors may take a chunk: room in the window, or ahead of
    /// the pumps.
    pub compress: bool,
    /// The decompressors may take a chunk: one came, or room was read.
    pub decompress: bool,
}

impl Woke {
    fn and(self, other: Self) -> Self {
        Self {
            readable: self.readable | other.readable,
            pumps: self.pumps || other.pumps,
            synced: self.synced || other.synced,
            compress: self.compress || other.compress,
            decompress: self.decompress || other.decompress,
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
    /// Chunks taken by a pump whose last frame is not yet written.
    chunks_sending: usize,
    /// Chunks the pumps took of those compressed, ever: as each is, a
    /// compressor may go on.
    chunks_taken: u64,
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
            chunks_taken: 0,
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
                pipe.received.push_back(Came {
                    bytes: content.to_vec(),
                    wire: content.len(),
                });
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
                    compress: true,
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
                    let highest = pipe.came.last().copied();
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
        if number < pipe.awaited || pipe.came.contains(&number) {
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

    /// A chunk came whole, compressed or not: one that is not goes in turn,
    /// with those ahead of it that are next; one that is waits for a
    /// decompressor.
    fn chunk(&mut self, number: u64, compressed: bool, content: Vec<u8>) -> Result<Woke, Fault> {
        let pipe = self.pipe(DATA)?;
        // Checked as it began; since, another of its number may have come,
        // or the data's end, short of it.
        if number < pipe.awaited || pipe.came.contains(&number) {
            return Err(Fault("a chunk twice"));
        }
        if pipe.chunks.is_some_and(|chunks| number >= chunks) {
            return Err(Fault("a chunk past the data's end"));
        }
        pipe.came.insert(number);
        if compressed {
            if pipe.spec.compress.is_none() {
                return Err(Fault("a chunk compressed on a pipe that is not"));
            }
            pipe.compressed.insert(number, content);
            return Ok(Woke {
                decompress: true,
                ..Woke::default()
            });
        }
        let wire = CHUNK_HEADER + content.len();
        Ok(self.in_turn(number, content, wire))
    }

    /// Chunk `number`, `bytes`, of `wire` bytes on the wire, goes in turn,
    /// with those ahead of it that are next: its bytes are given back as it
    /// is read whole. The decompressors are woken for the one next in turn
    /// then, should it wait for them: they may have left it, as what waits
    /// decompressed left no room.
    fn in_turn(&mut self, number: u64, bytes: Vec<u8>, wire: usize) -> Woke {
        let pipe = &mut self.pipes[usize::from(DATA)];
        pipe.held += bytes.len();
        pipe.ahead.insert(number, Came { bytes, wire });
        let mut woke = Woke::default();
        while let Some(came) = pipe.ahead.remove(&pipe.awaited) {
            pipe.came.remove(&pipe.awaited);
            pipe.awaited += 1;
            if pipe.abandoned {
                pipe.consumed += came.wire as u64;
                pipe.held -= came.bytes.len();
                continue;
            }
            pipe.unread += came.wire as u64;
            pipe.received.push_back(came);
            woke.readable = 1 << DATA;
        }
        woke.decompress = pipe.compressed.contains_key(&pipe.awaited);
        woke.and(self.give_back(DATA))
    }

    /// The next chunk that came compressed for a decompressor, and what it
    /// took on the wire: the least numbered, if it is the one next in turn,
    /// or if what waits decompressed leaves room.
    pub fn next_decompress(&mut self) -> Option<(u64, Vec<u8>, usize)> {
        let pipe = self
            .pipes
            .iter_mut()
            .find(|pipe| pipe.spec.reads && pipe.spec.rank == Rank::Data)?;
        let (&number, _) = pipe.compressed.first_key_value()?;
        if number != pipe.awaited && pipe.held >= DECOMPRESSED {
            return None;
        }
        let content = pipe.compressed.remove(&number).expect("there");
        let wire = CHUNK_HEADER + content.len();
        Some((number, content, wire))
    }

    /// Chunk `number`, of `wire` bytes on the wire, decompressed to `bytes`:
    /// it goes in turn.
    pub fn decompressed(&mut self, number: u64, bytes: Vec<u8>, wire: usize) -> Woke {
        self.in_turn(number, bytes, wire)
    }

    /// The next chunk of the data for a compressor, if one may take it: a
    /// whole one, [`CHUNK`], once so much waits; or, as nothing is being
    /// compressed nor waits for a pump, so that the wire would be idle, as
    /// much as waits. So long as fewer than [`COMPRESSORS`] are compressed,
    /// or being compressed, ahead of the pumps, and the window has room for
    /// the most it may take compressed.
    pub fn next_compress(&mut self) -> Option<Compress> {
        let length = self.may_compress()?;
        let pipe = self
            .pipes
            .iter_mut()
            .find(|pipe| pipe.spec.writes && pipe.spec.rank == Rank::Data)?;
        let level = pipe.spec.compress?;
        let pieces = take(&mut pipe.sending, length);
        pipe.sending_bytes -= length;
        let taken = CHUNK_HEADER + compressed_bound(length);
        pipe.window -= taken as u64;
        pipe.compressing += 1;
        let number = pipe.next_chunk;
        pipe.next_chunk += 1;
        Some(Compress {
            number,
            pieces,
            level,
            taken,
        })
    }

    /// Compresses the next chunk a compressor may take, here, as one would:
    /// for what has no threads, as the fuzzer. Whether there was one.
    pub fn compress_one(&mut self) -> bool {
        let Some(job) = self.next_compress() else {
            return false;
        };
        let plain: Vec<u8> = job
            .pieces
            .iter()
            .flat_map(Piece::as_slice)
            .copied()
            .collect();
        match zstd::bulk::compress(&plain, job.level) {
            Ok(encoded) if encoded.len() < plain.len() => self.compressed(&job, true, encoded),
            _ => self.compressed(&job, false, plain),
        };
        true
    }

    /// Decompresses the next chunk a decompressor may take, here, as one
    /// would: for what has no threads, as the fuzzer. Whether there was one.
    pub fn decompress_one(&mut self) -> Result<bool, Fault> {
        let Some((number, content, wire)) = self.next_decompress() else {
            return Ok(false);
        };
        match zstd::bulk::decompress(&content, CHUNK) {
            Ok(bytes) if !bytes.is_empty() => {
                self.decompressed(number, bytes, wire);
                Ok(true)
            }
            _ => Err(Fault("a chunk came that does not decompress")),
        }
    }

    /// How long a chunk a compressor may take, if one may take one: see
    /// [`Core::next_compress`].
    fn may_compress(&self) -> Option<usize> {
        let pipe = self
            .pipes
            .iter()
            .find(|pipe| pipe.spec.writes && pipe.spec.rank == Rank::Data)?;
        pipe.spec.compress?;
        let idle = pipe.compressing == 0 && pipe.ready.is_empty();
        if pipe.sending_bytes == 0
            || (pipe.sending_bytes < CHUNK && !idle)
            || pipe.compressing + pipe.ready.len() >= COMPRESSORS
        {
            return None;
        }
        let room = (pipe.window as usize).saturating_sub(CHUNK_HEADER);
        let length = pipe.sending_bytes.min(CHUNK).min(fits_compressed(room));
        (length > 0).then_some(length)
    }

    /// The chunk of `job` is compressed to `content`, or not, as it did not
    /// shrink: ready for a pump, the window it did not take back.
    fn compressed(&mut self, job: &Compress, compressed: bool, content: Vec<u8>) -> Woke {
        let pipe = &mut self.pipes[usize::from(DATA)];
        pipe.compressing -= 1;
        pipe.window += (job.taken - CHUNK_HEADER - content.len()) as u64;
        let mut header = Vec::with_capacity(CHUNK_HEADER);
        header.extend_from_slice(&job.number.to_le_bytes());
        header.push(u8::from(compressed));
        header.extend_from_slice(&(content.len() as u32).to_le_bytes());
        let mut pieces = VecDeque::new();
        for bytes in [header, content] {
            let end = bytes.len();
            pieces.push_back(Piece {
                bytes: Arc::new(bytes),
                start: 0,
                end,
            });
        }
        pipe.ready.push_back(Sending { pieces });
        Woke {
            pumps: true,
            ..Woke::default()
        }
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
            // the other side's pipes all did was let go of early, as when
            // the other side stopped; so was one that ends before a pipe
            // with a window this side writes did, as the other side shuts
            // its way out only once none it reads needs window given back.
            // Its end taken is enough: the other side shuts once it has it,
            // maybe before the pump that wrote it says it did.
            let unread = self.pipes.iter().any(|pipe| pipe.spec.reads && !pipe.ended);
            let unsent = self
                .pipes
                .iter()
                .any(|pipe| pipe.spec.window.is_some() && !pipe.all_taken());
            if unread || unsent {
                return Err(Fault("the stream ended before the other side was done"));
            }
        }
        if self.wires_ended == self.wires
            && self
                .pipes
                .iter()
                .any(|pipe| pipe.spec.reads && !pipe.all_came())
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
        let read: usize = pipe.received.iter().map(|came| came.bytes.len()).sum();
        pipe.held -= read.min(pipe.held);
        pipe.received.clear();
        pipe.offset = 0;
        let unread = std::mem::take(&mut pipe.unread);
        let woke = self.let_go(number, unread);
        woke.and(Woke {
            readable: 1 << number,
            ..Woke::default()
        })
    }

    /// The program read `plain` bytes of `number`, of `wire` bytes that
    /// came, compressed or not: window to give back, and syncs to answer.
    fn was_read(&mut self, number: u8, wire: usize, plain: usize) -> Woke {
        let pipe = &mut self.pipes[usize::from(number)];
        let mut woke = Woke::default();
        if pipe.spec.rank == Rank::Data {
            // The decompressors, if the read made room for what waits.
            let before = pipe.held;
            pipe.held -= plain;
            woke.decompress =
                !pipe.compressed.is_empty() && before >= DECOMPRESSED && pipe.held < DECOMPRESSED;
        }
        pipe.read += plain as u64;
        pipe.wire_read += wire as u64;
        pipe.unread -= wire as u64;
        if pipe.spec.window.is_some() {
            pipe.consumed += wire as u64;
        } else {
            pipe.outstanding -= wire as u64;
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

    /// What a frame carried was written on the wire.
    pub fn written(&mut self, carried: Carrying) {
        match carried {
            Carrying::Bytes(number, length) => {
                self.pipes[usize::from(number)].sent += length as u64;
            }
            Carrying::ChunkDone => self.chunks_sending -= 1,
            Carrying::End(number) => self.pipes[usize::from(number)].end_written = true,
            Carrying::Other => {}
        }
    }

    /// Reads all there is to read on `number`, as its program would: for
    /// what has no threads, as the fuzzer.
    pub fn read_all(&mut self, number: u8) -> Vec<u8> {
        let pipe = &mut self.pipes[usize::from(number)];
        let offset = std::mem::take(&mut pipe.offset);
        let mut read = Vec::new();
        let mut wire = 0;
        for came in pipe.received.drain(..) {
            read.extend(came.bytes);
            wire += came.wire;
        }
        read.drain(..offset);
        let plain = read.len() + offset;
        // Of a frame, what was read before is given back already.
        let wire = if pipe.spec.rank == Rank::Data {
            wire
        } else {
            wire - offset
        };
        self.was_read(number, wire, plain);
        read
    }

    /// `spare` of the window taken for a frame of `number`, compressed, it
    /// did not use: back for what is next.
    fn spare(&mut self, number: u8, spare: usize) {
        let pipe = &mut self.pipes[usize::from(number)];
        if pipe.spec.window.is_some() {
            pipe.window += spare as u64;
        }
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
            return Some(sending.frame());
        }
        let number = self
            .pipes
            .iter()
            .position(|pipe| pipe.spec.writes && pipe.spec.rank == Rank::Data)?;
        let pipe = &mut self.pipes[number];
        if pipe.spec.compress.is_some() {
            if let Some(chunk) = pipe.ready.pop_front() {
                *sending = chunk;
                self.chunks_sending += 1;
                self.chunks_taken += 1;
                return Some(sending.frame());
            }
            // Its end, once nothing waits, nor is being compressed.
            if wire == 0
                && pipe.closed
                && !pipe.end_sent
                && pipe.sending_bytes == 0
                && pipe.compressing == 0
            {
                pipe.end_sent = true;
                return Some(Outgoing::of(
                    number as u8,
                    Kind::End,
                    pipe.next_chunk.to_le_bytes().to_vec(),
                ));
            }
            return None;
        }
        match self.pipes[number].next_chunk(number as u8, wire == 0)? {
            Ok(chunk) => {
                *sending = chunk;
                self.chunks_sending += 1;
                Some(sending.frame())
            }
            Err(end) => Some(end),
        }
    }

    /// Nothing more can go this way: all it writes went, and each pipe's
    /// end, and none it reads needs window given back. The way out is shut
    /// then, as what carries the connection may wait for that to end.
    fn done_sending(&self) -> bool {
        self.drained()
            && self
                .pipes
                .iter()
                .all(|pipe| !pipe.spec.reads || pipe.spec.window.is_none() || pipe.complete())
    }

    /// Everything this end writes went on the wire, and each pipe's end.
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
            if being.header[8] > 1 {
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
            let compressed = done.header[8] == 1;
            woke = woke.and(core.chunk(done.number, compressed, done.content)?);
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
    /// Woken as the data's compressors, or decompressors, may take a chunk.
    compressors: Condvar,
    decompressors: Condvar,
    synced: Condvar,
    drained: Condvar,
    /// Woken as a frame is written on the wire.
    sent: Condvar,
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
        if woke.compress {
            self.compressors.notify_all();
        }
        if woke.decompress {
            self.decompressors.notify_all();
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
        self.compressors.notify_all();
        self.decompressors.notify_all();
        self.synced.notify_all();
        self.drained.notify_all();
        self.sent.notify_all();
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
    /// The data's compressors, or decompressors.
    workers: Vec<JoinHandle<()>>,
}

impl Connection {
    /// Carries `pipes`, numbered in turn, on `wires`, ready: their pipes
    /// begin at once. Data goes on all of them; all else on the first.
    pub fn open(wires: Vec<Wire>, pipes: &[Pipe]) -> io::Result<Self> {
        let mut ends = Vec::new();
        for wire in wires {
            ends.push(wire.ready()?);
        }
        Self::carry(ends, pipes)
    }

    /// As [`Connection::open`], one wire within `scope` rather than its
    /// own: it ends with it.
    pub fn open_within(wire: Wire, pipes: &[Pipe], scope: super::scope::Scope) -> io::Result<Self> {
        Self::carry(vec![wire.ready_in(scope)?], pipes)
    }

    fn carry(ends: Vec<(WireIn, WireOut, Carried)>, pipes: &[Pipe]) -> io::Result<Self> {
        let count = ends.len();
        let shared = Arc::new(Shared {
            core: Mutex::new(Core::new(pipes, count)),
            readable: pipes.iter().map(|_| Condvar::new()).collect(),
            writable: pipes.iter().map(|_| Condvar::new()).collect(),
            pumps: Condvar::new(),
            compressors: Condvar::new(),
            decompressors: Condvar::new(),
            synced: Condvar::new(),
            drained: Condvar::new(),
            sent: Condvar::new(),
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
        // Where the data is compressed, a pool compresses its chunks, as this
        // end writes it, or decompresses them, as it reads it.
        let mut workers = Vec::new();
        for pipe in pipes.iter().filter(|pipe| pipe.rank == Rank::Data) {
            if pipe.compress.is_none() {
                continue;
            }
            for _ in 0..crate::pool::threads(COMPRESSORS) {
                let working = Arc::clone(&shared);
                if pipe.writes {
                    workers.push(std::thread::spawn(move || compressor(&working)));
                } else if pipe.reads {
                    workers.push(std::thread::spawn(move || decompressor(&working)));
                }
            }
        }
        let readers = pipes
            .iter()
            .enumerate()
            .map(|(number, pipe)| {
                let decoder = match pipe.compress {
                    Some(_) if pipe.rank != Rank::Data => {
                        Some(zstd::stream::raw::Decoder::new().map_err(|error| {
                            io::Error::other(format!("decompress the {}: {error}", pipe.name))
                        })?)
                    }
                    _ => None,
                };
                Ok(pipe.reads.then(|| PipeReader {
                    shared: Arc::clone(&shared),
                    number: number as u8,
                    decoder,
                    pending: false,
                    chunks: pipe.rank == Rank::Data,
                }))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self {
            shared,
            readers,
            pumps,
            receivers,
            workers,
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
        for pump in self.pumps.drain(..).chain(self.workers.drain(..)) {
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

/// Compresses the data's chunks, as one may be taken, each alone, a chunk
/// that does not shrink left as it is, until the connection is let go of,
/// or breaks.
fn compressor(shared: &Shared) {
    let _thread = profile::thread("compress");
    // Its own, kept from chunk to chunk.
    let mut compressor = None;
    loop {
        let job = {
            let mut core = shared.lock();
            loop {
                if core.broken.is_some() || core.closing {
                    return;
                }
                if let Some(job) = core.next_compress() {
                    if let Some(gauge) = &shared.gauges[usize::from(DATA)] {
                        gauge.set(core.pipes[usize::from(DATA)].sending_bytes);
                    }
                    // Another, if another may be taken.
                    if core.may_compress().is_some() {
                        shared.compressors.notify_one();
                    }
                    break job;
                }
                core = profile::wait(State::Work, || {
                    shared.compressors.wait(core).expect("not poisoned")
                });
            }
        };
        // Room was made in the data's send buffer.
        shared.writable[usize::from(DATA)].notify_all();
        let plain = job.pieces.iter().fold(Vec::new(), |mut plain, piece| {
            plain.extend_from_slice(piece.as_slice());
            plain
        });
        let encoded = profile::wait(State::Compress, || {
            let compressor = match &mut compressor {
                Some(compressor) => compressor,
                None => compressor.insert(zstd::bulk::Compressor::new(job.level)?),
            };
            compressor.compress(&plain)
        });
        let (compressed, content) = match encoded {
            Ok(encoded) if encoded.len() < plain.len() => (true, encoded),
            _ => (false, plain),
        };
        let woke = shared.lock().compressed(&job, compressed, content);
        shared.wake(woke);
    }
}

/// Decompresses the data's chunks that came compressed, as one may be
/// taken, until the connection is let go of, or breaks: one that does not
/// decompress, or to more than a chunk holds, or to nothing, breaks it.
fn decompressor(shared: &Shared) {
    let _thread = profile::thread("decompress");
    // Its own, kept from chunk to chunk.
    let mut decompressor = zstd::bulk::Decompressor::new().ok();
    loop {
        let (number, content, wire) = {
            let mut core = shared.lock();
            loop {
                if core.broken.is_some() || core.closing {
                    return;
                }
                if let Some(job) = core.next_decompress() {
                    break job;
                }
                core = profile::wait(State::Work, || {
                    shared.decompressors.wait(core).expect("not poisoned")
                });
            }
        };
        let bytes = profile::wait(State::Decompress, || match &mut decompressor {
            Some(decompressor) => decompressor.decompress(&content, CHUNK),
            None => zstd::bulk::decompress(&content, CHUNK),
        });
        let mut core = shared.lock();
        match bytes {
            Ok(bytes) if !bytes.is_empty() => {
                let woke = core.decompressed(number, bytes, wire);
                drop(core);
                shared.wake(woke);
            }
            _ => {
                shared.broke(
                    &mut core,
                    io::ErrorKind::InvalidData,
                    "a chunk came that does not decompress",
                );
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
    // Each compressed pipe's stream, as this pump, the first wire's, sends
    // it.
    let mut streams: BTreeMap<u8, zstd::stream::write::Encoder<'static, Vec<u8>>> = BTreeMap::new();
    // What the last frame carried, written since, and what of the window
    // taken for it it did not use.
    let mut written = None;
    loop {
        let (next, carried) = {
            let mut core = shared.lock();
            if let Some((carried, spare)) = written.take() {
                if let (Carrying::Bytes(number, _), Some(spare)) = (carried, spare) {
                    core.spare(number, spare);
                }
                core.written(carried);
                shared.sent.notify_all();
                if core.drained() {
                    shared.drained.notify_all();
                }
            }
            loop {
                let done = core.done_sending() && !sending.busy();
                if core.broken.is_some() || core.closing && !sending.busy() || done {
                    output.shut();
                    return;
                }
                let taken = core.chunks_taken;
                if let Some(next) = core.next(wire, &mut sending) {
                    // A chunk taken: a compressor may go on.
                    if core.chunks_taken != taken {
                        shared.compressors.notify_one();
                    }
                    let carried = Carrying::of(&next, &sending);
                    // Room was made in the pipe's send buffer.
                    if next.header[1] == Kind::Bytes as u8 {
                        let number = usize::from(next.header[0]);
                        if let Some(gauge) = &shared.gauges[number] {
                            gauge.set(core.pipes[number].sending_bytes);
                        }
                        shared.writable[number].notify_all();
                    }
                    break (next, carried);
                }
                core = profile::wait(State::Work, || {
                    shared.pumps.wait(core).expect("not poisoned")
                });
            }
        };
        frame.clear();
        let mut spare = None;
        match next.squeeze {
            None => {
                frame.extend_from_slice(&next.header);
                for piece in &next.content {
                    frame.extend_from_slice(piece.as_slice());
                }
            }
            // Compressed and flushed, in as many frames as it takes.
            Some(Squeeze { level, taken }) => {
                let number = next.header[0];
                let compressed = profile::wait(State::Compress, || {
                    let stream = match streams.entry(number) {
                        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(zstd::stream::write::Encoder::new(Vec::new(), level)?)
                        }
                    };
                    for piece in &next.content {
                        io::Write::write_all(stream, piece.as_slice())?;
                    }
                    io::Write::flush(stream)?;
                    Ok::<_, io::Error>(std::mem::take(stream.get_mut()))
                });
                let compressed = match compressed {
                    Ok(compressed) => compressed,
                    Err(error) => {
                        let mut core = shared.lock();
                        shared.broke(&mut core, error.kind(), &format!("compress: {error}"));
                        return;
                    }
                };
                assert!(
                    compressed.len() <= taken,
                    "compressed to more than the room taken for it"
                );
                for part in compressed.chunks(MOST) {
                    frame.extend_from_slice(&[number, Kind::Bytes as u8]);
                    frame.extend_from_slice(&(part.len() as u32).to_le_bytes());
                    frame.extend_from_slice(part);
                }
                spare = Some(taken - compressed.len());
            }
        }
        if let Err(error) = profile::wait(State::WriteConnection, || output.write_all(&frame)) {
            let mut core = shared.lock();
            if !core.closing {
                shared.broke(&mut core, error.kind(), &error.to_string());
            }
            return;
        }
        written = Some((carried, spare));
    }
}

/// What a frame carried, counted once it is written on the wire.
#[derive(Clone, Copy)]
pub enum Carrying {
    /// Bytes of a pipe but the data's.
    Bytes(u8, usize),
    /// The last frame of a chunk.
    ChunkDone,
    /// A pipe's end.
    End(u8),
    Other,
}

impl Carrying {
    /// What `frame` carries, `sending` the chunk its pump sends.
    pub fn of(frame: &Outgoing, sending: &Sending) -> Self {
        let (number, kind) = (frame.header[0], frame.header[1]);
        let length = u32::from_le_bytes(frame.header[2..].try_into().expect("four bytes")) as usize;
        if kind == Kind::End as u8 {
            Self::End(number)
        } else if kind != Kind::Bytes as u8 {
            Self::Other
        } else if number != DATA {
            Self::Bytes(number, length)
        } else if !sending.busy() {
            Self::ChunkDone
        } else {
            Self::Other
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
            // A compressor, as another whole chunk waits, or as the first
            // bytes do and none is busy.
            let before = pipe.sending_bytes - taken;
            let cut = pipe.spec.rank == Rank::Data
                && pipe.spec.compress.is_some()
                && (pipe.sending_bytes / CHUNK > before / CHUNK
                    || before == 0 && pipe.compressing == 0 && pipe.ready.is_empty());
            drop(core);
            shared.pumps.notify_all();
            if cut {
                shared.compressors.notify_one();
            }
        }
        Ok(())
    }

    /// Writes all of `bytes` if that takes no waiting at all, on a pipe
    /// with no window, such as control, and nothing else is writing it: as
    /// from a thread that panicked, which may hold it. False if not written.
    pub fn try_write_all(&self, bytes: &[u8]) -> bool {
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let Ok(_writing) = shared.writing[number].try_lock() else {
            return false;
        };
        let Ok(mut core) = shared.core.try_lock() else {
            return false;
        };
        if core.broken.is_some() {
            return false;
        }
        let pipe = &mut core.pipes[number];
        if pipe.closed || pipe.spec.window.is_some() {
            return false;
        }
        pipe.sending.push_back(Piece {
            bytes: Arc::new(bytes.to_vec()),
            start: 0,
            end: bytes.len(),
        });
        pipe.sending_bytes += bytes.len();
        pipe.written += bytes.len() as u64;
        drop(core);
        shared.pumps.notify_all();
        true
    }

    /// Waits until all written on the pipe went on the wire, for at most
    /// `patience`: false if it did not, or the connection broke. Not for the
    /// data's pipe, which goes in chunks.
    pub fn wait_sent(&self, patience: std::time::Duration) -> bool {
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let deadline = std::time::Instant::now() + patience;
        let mut core = shared.lock();
        let written = core.pipes[number].written;
        loop {
            if core.pipes[number].sent >= written {
                return true;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if core.broken.is_some() || left.is_zero() {
                return false;
            }
            core = shared
                .sent
                .wait_timeout(core, left)
                .expect("not poisoned")
                .0;
        }
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
    /// Its stream's decompression, if the pipe is compressed, and whether
    /// it may hold what it decompressed and has not given yet.
    decoder: Option<zstd::stream::raw::Decoder<'static>>,
    pending: bool,
    /// It is the data's, read a chunk at a time.
    chunks: bool,
}

impl PipeReader {
    /// What what its program read so far took on the wire, compressed or
    /// not: a chunk's, once it is read whole.
    pub fn wire(&self) -> u64 {
        self.shared.lock().pipes[usize::from(self.number)].wire_read
    }

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

impl PipeReader {
    /// The next of what came, and how far into it was read, waiting for
    /// some: none once the pipe ended, all it carried read, or its program
    /// stopped reading it.
    fn came(&self) -> io::Result<Option<(Came, usize)>> {
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let mut core = shared.lock();
        loop {
            let pipe = &mut core.pipes[number];
            if pipe.abandoned {
                return Ok(None);
            }
            if let Some(piece) = pipe.received.pop_front() {
                return Ok(Some((piece, std::mem::take(&mut pipe.offset))));
            }
            if pipe.complete() {
                return Ok(None);
            }
            if let Some(broken) = &core.broken {
                return Err(error(broken));
            }
            core = profile::wait(State::ReadConnection, || {
                shared.readable[number].wait(core).expect("not poisoned")
            });
        }
    }

    /// Of `piece`, read up to `offset`, `taken` bytes more were read, giving
    /// `plain` bytes: what is left of it is read next. Of a frame, what was
    /// taken is given back; of a chunk, all it took on the wire, once it is
    /// read whole.
    fn was_read(&self, piece: Came, offset: usize, taken: usize, plain: usize) {
        let shared = &*self.shared;
        let number = usize::from(self.number);
        let whole = offset + taken == piece.bytes.len();
        let wire = match (self.chunks, whole) {
            (false, _) => taken,
            (true, true) => piece.wire,
            (true, false) => 0,
        };
        let mut core = shared.lock();
        // Stopped meanwhile: the piece was let go of with the rest.
        if core.pipes[number].abandoned {
            return;
        }
        if !whole {
            let pipe = &mut core.pipes[number];
            pipe.received.push_front(piece);
            pipe.offset = offset + taken;
        }
        let woke = core.was_read(self.number, wire, plain);
        drop(core);
        shared.wake(woke);
    }
}

/// Decompresses what of `input` `decoder` takes into `output`: how many
/// bytes it took, and how many it gave.
fn decompress(
    decoder: &mut zstd::stream::raw::Decoder<'static>,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<(usize, usize)> {
    use zstd::stream::raw::{InBuffer, Operation, OutBuffer};
    let mut input = InBuffer::around(input);
    let mut output = OutBuffer::around(output);
    decoder.run(&mut input, &mut output).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("what came does not decompress: {error}"),
        )
    })?;
    Ok((input.pos(), output.pos()))
}

impl io::Read for PipeReader {
    /// Reads what came, waiting for some, decompressed if the pipe is: 0
    /// once the pipe ended, all it carried read.
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if into.is_empty() {
            return Ok(0);
        }
        if self.decoder.is_none() {
            let Some((piece, offset)) = self.came()? else {
                return Ok(0);
            };
            let taken = (piece.bytes.len() - offset).min(into.len());
            into[..taken].copy_from_slice(&piece.bytes[offset..offset + taken]);
            self.was_read(piece, offset, taken, taken);
            return Ok(taken);
        }
        loop {
            // What the decoder holds of what it took before comes first.
            if self.pending {
                let decoder = self.decoder.as_mut().expect("compressed");
                let (_, given) = decompress(decoder, &[], into)?;
                self.pending = given == into.len();
                if given > 0 {
                    let woke = self.shared.lock().was_read(self.number, 0, given);
                    self.shared.wake(woke);
                    return Ok(given);
                }
            }
            let Some((piece, offset)) = self.came()? else {
                return Ok(0);
            };
            let decoder = self.decoder.as_mut().expect("compressed");
            let (taken, given) = decompress(decoder, &piece.bytes[offset..], into)?;
            self.pending = given == into.len();
            self.was_read(piece, offset, taken, given);
            if given > 0 {
                return Ok(given);
            }
        }
    }
}

#[cfg(test)]
mod tests;
