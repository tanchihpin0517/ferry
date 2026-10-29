//! Packets and records: what peers write on their pipes.
//!
//! A packet is `[type: u8][length: u32, little-endian][payload]`. The payload
//! is a message encoded with postcard, its type deciding which message.
//!
//! A record is its length, a varint, then its postcard encoding, the pipe
//! deciding what it is: the manifest's pipe, the answers' and the data's
//! carry records; see [`put_record`] and [`read_record`], and for the
//! data's, [`crate::block`]. Control and the monitors carry packets.

use std::io::{self, BufRead, Read, Write};

use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::manifest::{FileId, Manifest};

/// The largest payload a peer accepts, so a corrupt length cannot make it
/// allocate without bound.
///
/// So no message grows with what is transferred, and none comes near it:
/// - the lines a dry run lists go a mebibyte at a time;
/// - the files being transferred are those the writers have under way;
/// - anything else is one path, or one message, such as an error's.
///
/// What `sync` deletes goes in no message: the Destination deletes it, and
/// tells only what it could not, one at a time.
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// The largest payload a packet may have: [`MAX_PAYLOAD`], or a test's.
fn max_payload() -> u32 {
    crate::inject::max_payload().unwrap_or(MAX_PAYLOAD)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Control = 0,
    Manifest = 1,
    /// An answer of the Destination's, as the tests and the fuzzer run a
    /// peer over one stream of packets: the pipe carries it as a record.
    Answer = 2,
    /// A record of the data, as the tests and the fuzzer run a peer over
    /// one stream of packets: the pipe carries it as a record.
    Data = 3,
    /// A report from a server's peer, for the client to show; see
    /// [`crate::report`].
    Report = 5,
    // 4 was the Destination's results, which its answers took the place
    // of; 6 a message of the manifest's compressed stream, which the
    // manifest's records did; 7 the hash of a file in segments, and 12 the
    // end of the data's blocks, which the data's records did; 8 the data
    // credit, which the data pipe's window did; 9 a stream's keepalive,
    // which the monitors' heartbeat did; 10 a stream's end, which each
    // pipe's end did.
    /// Between the client and the monitor on a host; see [`Monitor`].
    Monitor = 11,
}

impl PacketType {
    fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => Self::Control,
            1 => Self::Manifest,
            2 => Self::Answer,
            3 => Self::Data,
            5 => Self::Report,
            11 => Self::Monitor,
            _ => bail!("unknown packet type {byte}"),
        })
    }
}

/// Where a server waits for the stream: a Unix socket on its host, which the
/// stream reaches straight, through SSH, or by running `ferry server --join
/// <token>` there, or which the client hands the stream to on its own
/// machine; or, where SSH forwards to no socket, a port on loopback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The socket's path, which a stream reaches, or joins through; empty
    /// where the server listens on a port instead.
    pub socket: Vec<u8>,
    /// The port on loopback the server listens on instead of a socket.
    pub port: Option<Port>,
    /// What the server's root is, for the client to check the two roots
    /// against each other before anything changes.
    pub root: RootIdentity,
}

/// A port on loopback a server, or the monitor, listens on, and the secret
/// each connection to it gives first: anyone on the host may connect to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub number: u16,
    pub secret: String,
}

/// A directory or file, as one kernel knows it: comparable only between two
/// roots whose `kernel` is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileKey {
    pub dev: u64,
    pub ino: u64,
}

/// What a server's root is, as its kernel knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootIdentity {
    /// The running kernel, as it names its boot: two roots are compared by
    /// device and inode only when this is the same, and not empty.
    pub kernel: String,
    /// The root's canonical path; for a root not there yet, its nearest
    /// ancestor's that is, joined with the rest.
    pub canonical: Vec<u8>,
    /// Whether the root is there.
    pub exists: bool,
    /// The root, if it is there, then each directory above it up to `/`,
    /// nearest first; for a root not there yet, from its nearest ancestor
    /// that is.
    pub chain: Vec<FileKey>,
    /// The canonical home directories of the users the server acts for.
    pub homes: Vec<Vec<u8>>,
}

/// What a peer must never go into or delete, as the client found it from the
/// two roots: a check during the transfer, behind the one before it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guard {
    /// For the Source: the Destination's root, which its scan must never
    /// reach.
    pub avoid: Vec<FileKey>,
    /// For the Destination: the Source's root, then each directory above it,
    /// none of which it may delete, nor go into the Source's root.
    pub protect: Vec<FileKey>,
}

/// What a server is told as it goes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proceed {
    /// What it must never go into or delete.
    pub guard: Guard,
    /// How many streams reach it, numbered from 0: the first carries all
    /// but the data's chunks, which any carries, Source to Destination.
    pub streams: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// The client's first message. It stays the first variant, its build the
    /// first field, in every build: see [`hello_build`].
    Hello(Hello),
    /// The server accepts the client's Hello, and is ready for the stream.
    Welcome(Welcome),
    /// The server refuses the client's Hello, and exits. It stays the third
    /// variant, of its reason alone, in every build: a client of another
    /// build reads it.
    Refused { reason: String },
    /// The client has checked the two roots, and the server may go on: its
    /// streams come next. A server whose control connection closes instead
    /// ends, having changed nothing.
    Proceed(Proceed),
    /// The sender failed, and stops.
    Error { message: String },
    /// The Destination has written everything it was sent.
    Done,
    /// The client asks a server, on its control connection, where its
    /// threads are; it answers there with a snapshot report.
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The client's build identity; the server must be the same build. The
    /// first field, in every build.
    pub build: String,
    /// Which peer the server is.
    pub role: Role,
    /// The server's root, as path bytes: what it reads, or where it writes.
    pub root: Vec<u8>,
    /// Compare only; never write.
    pub dry_run: bool,
    /// How close two mtimes must be to count as the same; see
    /// [`crate::compare::same_mtime`].
    pub modify_window: i64,
    /// Keep owners' numeric IDs, without mapping them by name.
    pub numeric_ids: bool,
    /// Delete what the Source does not have: `ferry sync`.
    pub delete: bool,
    /// Go into no directory on another filesystem than the root's, neither
    /// to send it nor to delete in it: `-x`.
    pub one_file_system: bool,
    /// Compress this control connection's pipes.
    pub compress: bool,
    /// The zstd level to compress them at.
    pub compress_level: i32,
    /// Profile the run, and report where the time went when it is done.
    pub profile: bool,
    /// For tests: the Source's scan stats, and the Destination compares, at
    /// most this many names of a directory at a time, rather than
    /// [`crate::scan::BATCH`]; 0 for that.
    pub batch_entries: u32,
    /// For tests: the Source's scan waits while more than this many needed
    /// files wait to be sent, rather than 100,000; 0 for that.
    pub backlog_files: u32,
    /// For tests: the manifest's window, rather than 2 MiB; 0 for that. Its
    /// writer, the scan, waits once it is used up.
    pub paced_bytes: u32,
    /// For measurements: the data's window, and what the Destination holds
    /// of the files' bytes, rather than [`DATA_CREDIT`]; 0 for that. See
    /// [`data_credit`].
    pub data_credit: u64,
    /// How many directories each side lists, and compares, at once: on the
    /// Destination, those compared on threads of their own; 0 for one.
    pub listers: u32,
    /// How many of its host's cores the server runs on; 0 for all.
    pub cpus: u32,
    /// The place its cores are taken from, as [`crate::cpus::limit`] takes
    /// it: the client's process ID, which the client's own are taken from.
    pub place: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Source,
    Destination,
}

/// What the Destination answers, as records on its answers' pipe, in the
/// order it writes them: an ID a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    /// The entry `id` is needed, created or sent, in manifest order. Only a
    /// file's content is sent; the Destination creates the rest itself.
    File(FileId),
    /// The entry `id` has its permissions or owner set, which a dry run
    /// lists.
    Attributes(FileId),
    /// Every manifest entry from `first` to `last` has been compared, and
    /// those of them needed have been asked for: the Source need keep none
    /// of them. Said of each batch, once its needs are, in whatever order
    /// the batches are.
    Compared { first: FileId, last: FileId },
    /// The entry `id` could not be made to match; the transfer goes on
    /// without it.
    Failed { id: FileId, message: String },
    /// An entry `sync` found extra, which it could not delete.
    NotDeleted { path: Vec<u8>, message: String },
    /// Every manifest entry has been compared, and every one needed asked
    /// for.
    NeedsEnd,
    /// The Destination is done: it said what failed, and wrote everything
    /// else.
    Done,
}

/// What the Destination needs, as its decider says it and the Source's
/// owner takes it: many IDs at once, each an [`Answer`] of its own on the
/// pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Needs {
    /// Entries needed created or sent: [`Answer::File`].
    Files(Vec<FileId>),
    /// Entries whose permissions or owner are set: [`Answer::Attributes`].
    Attributes(Vec<FileId>),
    /// [`Answer::Compared`].
    Compared { first: FileId, last: FileId },
    /// [`Answer::NeedsEnd`].
    End,
}

impl Needs {
    /// The answers it is, in order.
    pub fn into_answers(self) -> Vec<Answer> {
        match self {
            Needs::Files(ids) => ids.into_iter().map(Answer::File).collect(),
            Needs::Attributes(ids) => ids.into_iter().map(Answer::Attributes).collect(),
            Needs::Compared { first, last } => vec![Answer::Compared { first, last }],
            Needs::End => vec![Answer::NeedsEnd],
        }
    }
}

/// The data's window, in bytes on the stream, and how much of the files'
/// bytes that came the Destination holds, as [`crate::block::cost`] counts it,
/// before its data's reader waits: so what comes waits in the pipe, its
/// window not given back, and the Source waits. Once a credit the
/// Destination gave, which the window took the place of.
pub const DATA_CREDIT: u64 = 64 * 1024 * 1024;

/// The data's window and what the Destination holds, where `asked` is what
/// [`Hello::data_credit`] says: [`DATA_CREDIT`] for 0, and never less than
/// two of the largest record, so that one always fits.
pub fn data_credit(asked: u64) -> u64 {
    let credit = if asked == 0 { DATA_CREDIT } else { asked };
    credit.max(2 * crate::block::MOST as u64)
}

/// A message and the packet type that carries it.
pub trait Message: Serialize + DeserializeOwned {
    const TYPE: PacketType;
}

impl Message for Control {
    const TYPE: PacketType = PacketType::Control;
}

impl Message for Manifest {
    const TYPE: PacketType = PacketType::Manifest;
}

impl Message for Answer {
    const TYPE: PacketType = PacketType::Answer;
}

impl Message for crate::block::Data<crate::block::ByteBuf> {
    const TYPE: PacketType = PacketType::Data;
}

impl Message for crate::report::Report {
    const TYPE: PacketType = PacketType::Report;
}

impl Message for Monitor {
    const TYPE: PacketType = PacketType::Monitor;
}

/// What goes between the client and Ferry's monitor on a host, which
/// starts what the client asks for there: first on each connection to the
/// monitor, and then on the one the client follows it by. See
/// [`crate::monitor::host`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Monitor {
    /// First on the connection the client follows the monitor by: the
    /// monitor lives as long as it, and says on it how what it started
    /// ended. Its pipe is compressed at `compress`, if any.
    Follow { compress: Option<i32> },
    /// First on a connection: start `what`, with the connection as its
    /// input and output. `id` names it in what the monitor says of it.
    Launch { id: u32, what: Launch },
    /// What `id` came to: its exit code, or the signal that ended it; and,
    /// for what is run to its end, what it said on standard error.
    Ended {
        id: u32,
        code: Option<i32>,
        signal: Option<i32>,
        said: Vec<u8>,
    },
    /// `id` could not be started, and why.
    NotStarted { id: u32, reason: String },
    /// Both ways on the connection the client follows the monitor by, every
    /// [`crate::monitor::BEAT`]: the far end is there. Once nothing came for
    /// [`crate::monitor::SILENCE`], it is taken for lost.
    Beat,
    /// To the monitor: stop what it started, as the user asked, and say how
    /// each ended.
    Stop,
    /// `id` was stopped, as by SIGSTOP, or Ctrl-Z.
    Paused { id: u32 },
    /// `id` went on.
    Resumed { id: u32 },
    /// The monitor still waits for these to end, as it was told to stop.
    Awaiting { ids: Vec<u32> },
    /// To the monitor: say how what it started looks from outside.
    Look,
    /// To the monitor: answer at once, for the client to time the round
    /// trip, which says how many streams a transfer needs.
    Ping { id: u32 },
    /// The answer to the `Ping` of `id`.
    Pong { id: u32 },
    /// How `id` looks from outside, from the system.
    Looked {
        id: u32,
        outside: crate::monitor::look::Outside,
    },
}

/// What the monitor starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Launch {
    /// A server, through `sudo` for `--remote-sudo`; listening on a port on
    /// loopback, given `port`, where SSH forwards to no socket.
    Server { sudo: Option<SudoAsks>, port: bool },
    /// For `--remote-sudo`, `ferry build-id` through `sudo`, to find whether
    /// it asks for a password, and whether the one given is right.
    SudoCheck { asks: SudoAsks },
    /// For `ferry test`, `ferry fixture` with these arguments, which stops
    /// once the connection's input closes.
    Fixture { arguments: Vec<String> },
}

/// How `sudo` is run: asking for nothing, or reading the password from the
/// first line of its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SudoAsks {
    Nothing,
    Password,
}

/// A packet, whole, ready to write.
pub fn encode<M: Message>(message: &M) -> Result<Vec<u8>> {
    let payload = postcard::to_stdvec(message).context("encode a packet")?;
    packet(M::TYPE, &payload)
}

/// The most bytes a record may have, encoded: a segment's, and 64 KiB, as a
/// block's; see [`crate::block::MOST`]. A length past it is the far end's
/// fault.
pub const RECORD_MOST: usize = crate::block::MOST;

/// The most bytes a record's length takes, a varint.
const LENGTH_MOST: usize = 5;

/// Appends `record` to `out`, as a record: its length, a varint, then its
/// postcard encoding. Records go whole into one write, as many as there are.
pub fn put_record<T: Serialize>(out: &mut Vec<u8>, record: &T) -> Result<()> {
    let (encoded, start) = encode_record(record)?;
    out.extend_from_slice(&encoded[start..]);
    Ok(())
}

/// Encodes `record` as a record, its length first, into a buffer, from
/// the place returned: so that a record of a file's bytes is copied once,
/// as it is encoded.
pub fn encode_record<T: Serialize>(record: &T) -> Result<(Vec<u8>, usize)> {
    // Room for the longest length there is, before it.
    const ROOM: usize = LENGTH_MOST;
    let mut encoded = postcard::to_extend(record, vec![0; ROOM]).context("encode a record")?;
    let length = encoded.len() - ROOM;
    ensure!(length <= RECORD_MOST, "a record is too large");
    let mut prefix = Vec::with_capacity(ROOM);
    let mut left = length;
    loop {
        let byte = (left & 0x7f) as u8;
        left >>= 7;
        if left == 0 {
            prefix.push(byte);
            break;
        }
        prefix.push(byte | 0x80);
    }
    let start = ROOM - prefix.len();
    encoded[start..ROOM].copy_from_slice(&prefix);
    Ok((encoded, start))
}

/// A record cut short, or a pipe that broke as it was read: the I/O error
/// kept, for [`is_cut`].
fn cut(error: io::Error) -> anyhow::Error {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        anyhow::Error::new(error).context("a record cut short")
    } else {
        anyhow::Error::new(error).context("read a record")
    }
}

/// Whether reading a record failed as its pipe ended, or broke, as when the
/// far end failed: not as what came was not one, which is this end's to
/// say at once.
pub fn is_cut(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<io::Error>())
}

/// Reads the length of the next record a pipe carries, or `None` once it
/// ends, between records: one cut short, or past [`RECORD_MOST`], is an
/// error.
pub fn read_length(reader: &mut impl BufRead) -> Result<Option<usize>> {
    let mut length: usize = 0;
    for shift in (0..).step_by(7) {
        ensure!(
            shift < 7 * LENGTH_MOST,
            "a record's length of more than {LENGTH_MOST} bytes"
        );
        let mut byte = [0];
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
                return Ok(None);
            }
            Err(error) => return Err(cut(error)),
        }
        length |= usize::from(byte[0] & 0x7f) << shift;
        ensure!(
            length <= RECORD_MOST,
            "a record of more than {RECORD_MOST} bytes"
        );
        if byte[0] & 0x80 == 0 {
            return Ok(Some(length));
        }
    }
    unreachable!("the loop returns")
}

/// Reads the next record a pipe carries, or `None` once it ends, between
/// records: one cut short, of a length past [`RECORD_MOST`], or that does not
/// decode whole, is an error.
pub fn read_record<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>> {
    let Some(length) = read_length(reader)? else {
        return Ok(None);
    };
    let decoded = |bytes: &[u8]| -> Result<T> {
        let (record, rest) = postcard::take_from_bytes(bytes).context("decode a record")?;
        ensure!(
            rest.is_empty(),
            "a record with {} bytes past it",
            rest.len()
        );
        Ok(record)
    };
    // Decoded where it is, mostly, as records are small.
    let buffered = reader.fill_buf().map_err(cut)?;
    if buffered.len() >= length {
        let record = decoded(&buffered[..length]);
        reader.consume(length);
        return record.map(Some);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).map_err(cut)?;
    decoded(&bytes).map(Some)
}

fn packet(kind: PacketType, payload: &[u8]) -> Result<Vec<u8>> {
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|&length| length <= max_payload())
        .with_context(|| format!("a {kind:?} packet is too large"))?;
    let mut packet = Vec::with_capacity(5 + payload.len());
    packet.push(kind as u8);
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

pub fn send<M: Message>(writer: &mut impl Write, message: &M) -> Result<()> {
    writer.write_all(&encode(message)?)?;
    Ok(())
}

/// A packet as read, before its payload is decoded.
pub struct Packet {
    pub kind: PacketType,
    payload: Vec<u8>,
}

impl Packet {
    /// How many bytes its payload holds.
    pub fn size(&self) -> usize {
        self.payload.len()
    }

    /// The payload as it is: the bytes of a data packet.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    pub fn decode<M: Message>(&self) -> Result<M> {
        if self.kind != M::TYPE {
            bail!("expected a {:?} packet, got {:?}", M::TYPE, self.kind);
        }
        postcard::from_bytes(&self.payload)
            .with_context(|| format!("decode a {:?} packet", self.kind))
    }
}

/// The build a client's hello names, read before the rest of it, so that a
/// server of another build, whose hello is laid out otherwise, still says
/// which two builds met. The protocol need not stay the same from one build
/// to another, as both ends are one build, but for this, compared before
/// either knows whether the other is its own: a hello is a control packet
/// of [`Control`]'s first variant, its build first, and a refusal is its
/// third, of a reason alone. `None` where the packet is no hello.
pub fn hello_build(packet: &Packet) -> Option<String> {
    #[derive(Deserialize)]
    enum Prefix {
        Hello { build: String },
    }
    if packet.kind != PacketType::Control {
        return None;
    }
    match postcard::from_bytes(&packet.payload) {
        Ok(Prefix::Hello { build }) => Some(build),
        Err(_) => None,
    }
}

/// Writes a packet as it was read, to pass it on.
pub fn forward(writer: &mut impl Write, packet: &Packet) -> Result<()> {
    let length = u32::try_from(packet.payload.len()).expect("it was read with this length");
    writer.write_all(&[packet.kind as u8])?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&packet.payload)?;
    Ok(())
}

/// Reads the next packet, or `None` if the connection closed between packets.
pub fn receive(reader: &mut impl Read) -> Result<Option<Packet>> {
    let mut header = [0; 5];
    match reader.read_exact(&mut header[..1]) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read a packet"),
    }
    reader
        .read_exact(&mut header[1..])
        .context("read a packet header")?;
    let kind = PacketType::from_byte(header[0])?;
    let length = u32::from_le_bytes(header[1..].try_into().expect("four bytes"));
    if length > max_payload() {
        bail!("a {kind:?} packet of {length} bytes exceeds the limit");
    }
    let mut payload = vec![0; length as usize];
    reader
        .read_exact(&mut payload)
        .context("read a packet payload")?;
    Ok(Some(Packet { kind, payload }))
}

/// Reads the next packet, which must be there and of type `M`.
pub fn expect<M: Message>(reader: &mut impl Read) -> Result<M> {
    match receive(reader)? {
        Some(packet) => packet.decode(),
        None => bail!(
            "the connection closed while a {:?} packet was expected",
            M::TYPE
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Entry, Kind, Timestamp};

    /// A tree of small directories as its scan sends it, a batch of records
    /// each: each alone has little to compress.
    fn small_directories() -> Vec<Vec<Manifest>> {
        let mut id = 200;
        (0..200)
            .map(|directory| {
                let mut records = vec![Manifest::In { dir: directory + 1 }];
                records.extend((0..5).map(|file| {
                    id += 1;
                    Manifest::Entry(Entry {
                        id,
                        name: format!("file{file}.rs").into_bytes(),
                        kind: Kind::File,
                        size: 1000 + id * 37 % 5000,
                        mtime: Timestamp {
                            seconds: 1_700_000_000 + id as i64 * 61,
                            nanoseconds: (id as u32).wrapping_mul(7919) % 1_000_000_000,
                        },
                        mode: 0o644,
                        uid: 1000,
                        gid: 1000,
                    })
                }));
                records.push(Manifest::Listed { dir: directory + 1 });
                records
            })
            .chain([vec![Manifest::End]])
            .collect()
    }

    /// The manifest's records, as one zstd stream flushed after each batch,
    /// read back in order, as they were; and they take fewer bytes than each
    /// batch compressed alone, a third fewer than as they are, though each
    /// entry's size and mtime here are as good as random. Measured on
    /// `/usr`, a crate registry's sources and a build's output, each
    /// directory alone compressed 1.5 to 2 times, and as one stream 2.6 to
    /// 3.4.
    #[test]
    fn the_manifest_as_one_stream_reads_back_in_order_and_is_smaller() {
        let batches = small_directories();
        let mut stream = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        let (mut plain, mut alone) = (0, 0);
        for batch in &batches {
            let mut bytes = Vec::new();
            for record in batch {
                put_record(&mut bytes, record).unwrap();
            }
            plain += bytes.len();
            alone += zstd::bulk::compress(&bytes, 1).unwrap().len();
            stream.write_all(&bytes).unwrap();
            stream.flush().unwrap();
        }
        let compressed = stream.finish().unwrap();
        assert!(
            compressed.len() < alone && compressed.len() * 3 < plain * 2,
            "{} bytes, {alone} each alone, {plain} as it is",
            compressed.len()
        );
        let decoder = zstd::stream::read::Decoder::new(compressed.as_slice()).unwrap();
        let mut reader = io::BufReader::new(decoder);
        for record in batches.iter().flatten() {
            assert_eq!(
                &read_record::<Manifest>(&mut reader).unwrap().unwrap(),
                record
            );
        }
        assert!(read_record::<Manifest>(&mut reader).unwrap().is_none());
    }

    /// Records read back as they were written, whether or not one is
    /// buffered whole; the end between two is the end; one cut short, of a
    /// length past the most or longer than a length is, or with bytes past
    /// what it decodes to, is an error.
    #[test]
    fn records_read_back_and_bad_ones_are_refused() {
        let records = [
            Manifest::In { dir: 7 },
            Manifest::Problem {
                path: vec![b'x'; 300],
            },
            Manifest::End,
        ];
        let mut bytes = Vec::new();
        for record in &records {
            put_record(&mut bytes, record).unwrap();
        }
        // A buffer smaller than a record, which then goes past it.
        let mut reader = io::BufReader::with_capacity(16, bytes.as_slice());
        for record in &records {
            assert_eq!(
                &read_record::<Manifest>(&mut reader).unwrap().unwrap(),
                record
            );
        }
        assert!(read_record::<Manifest>(&mut reader).unwrap().is_none());
        let refused = |bytes: &[u8]| {
            let mut reader = io::BufReader::new(bytes);
            loop {
                match read_record::<Manifest>(&mut reader) {
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("all taken"),
                    Err(error) => return format!("{error:#}"),
                }
            }
        };
        assert!(refused(&bytes[..bytes.len() - 1]).contains("cut short"));
        assert!(refused(&[0x85]).contains("cut short"));
        let mut too_long = Vec::new();
        let mut length = RECORD_MOST + 1;
        while length >= 0x80 {
            too_long.push((length & 0x7f) as u8 | 0x80);
            length >>= 7;
        }
        too_long.push(length as u8);
        assert!(refused(&too_long).contains("more than"));
        // A length of nothing, written longer than any is.
        let mut padded = vec![0x80; 99];
        padded.push(0);
        assert!(refused(&padded).contains("length of more than"));
        let mut past = Vec::new();
        put_record(&mut past, &Manifest::End).unwrap();
        past[0] += 1;
        past.push(0);
        assert!(refused(&past).contains("past it"));
    }

    fn welcome() -> Welcome {
        Welcome {
            socket: b"/tmp/s".to_vec(),
            port: Some(Port {
                number: 4242,
                secret: "0".repeat(64),
            }),
            root: RootIdentity::default(),
        }
    }

    #[test]
    fn round_trip() {
        let mut buffer = Vec::new();
        send(&mut buffer, &Answer::File(1)).unwrap();
        send(&mut buffer, &Control::Welcome(welcome())).unwrap();
        let mut reader = buffer.as_slice();
        assert_eq!(expect::<Answer>(&mut reader).unwrap(), Answer::File(1));
        assert_eq!(
            expect::<Control>(&mut reader).unwrap(),
            Control::Welcome(welcome())
        );
        assert!(receive(&mut reader).unwrap().is_none());
    }

    #[test]
    fn wrong_type_is_an_error() {
        let mut buffer = Vec::new();
        send(&mut buffer, &Control::Welcome(welcome())).unwrap();
        assert!(expect::<Answer>(&mut buffer.as_slice()).is_err());
    }

    #[test]
    fn oversized_length_is_refused() {
        let mut buffer = vec![PacketType::Control as u8];
        buffer.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert!(receive(&mut buffer.as_slice()).is_err());
    }

    #[test]
    fn truncated_packet_is_an_error() {
        let mut buffer = Vec::new();
        send(&mut buffer, &Control::Welcome(welcome())).unwrap();
        buffer.pop();
        assert!(receive(&mut buffer.as_slice()).is_err());
    }
}
