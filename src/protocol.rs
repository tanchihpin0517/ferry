//! Packets: the unit peers exchange over their connection.
//!
//! A packet is `[type: u8][length: u32, little-endian][payload]`. The payload
//! is a message encoded with postcard, its type deciding which message, except
//! in a data packet: that carries the next bytes of the blocks of files, as
//! they are; see [`crate::block`]. Where the data is compressed, so is the
//! manifest, as one stream; see [`ManifestEncoder`].

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::manifest::{FileId, Manifest};

/// The largest payload a peer accepts, so a corrupt length cannot make it
/// allocate without bound.
///
/// So no message grows with what is transferred, and none comes near it:
/// - a manifest part has at most [`crate::scan::PART`] entries, and
///   [`crate::scan::PART_BYTES`];
/// - the names sent before a part are those of the users and groups its
///   entries use, each once;
/// - needs, of files or attributes, name at most a part's IDs;
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
    Needs = 2,
    Data = 3,
    Results = 4,
    /// A report from a server's peer, for the client to show; see
    /// [`crate::report`].
    Report = 5,
    /// A manifest message, compressed after those before it; see
    /// [`ManifestEncoder`].
    ManifestStream = 6,
    FileHash = 7,
    Credit = 8,
    /// Nothing went on the stream between the two servers for a while, and
    /// its writer is alive. No peer sends or reads one: see
    /// [`crate::transport::stream`].
    Keepalive = 9,
    /// The stream between the two servers ends here, its writer done; one
    /// that ends without it broke. No peer sends or reads one.
    StreamEnd = 10,
}

impl PacketType {
    fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => Self::Control,
            1 => Self::Manifest,
            2 => Self::Needs,
            3 => Self::Data,
            4 => Self::Results,
            5 => Self::Report,
            6 => Self::ManifestStream,
            7 => Self::FileHash,
            8 => Self::Credit,
            9 => Self::Keepalive,
            10 => Self::StreamEnd,
            _ => bail!("unknown packet type {byte}"),
        })
    }
}

/// Where a server waits for the stream, and for its control connection's
/// channel: a Unix socket on its host, which each reaches straight, through
/// SSH, or by running `ferry server --join <token>` there, or which the
/// client hands the stream to on its own machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The socket's path, which a stream reaches, or joins through.
    pub socket: Vec<u8>,
    /// What the server's root is, for the client to check the two roots
    /// against each other before anything changes.
    pub root: RootIdentity,
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
    /// paths come next. A server whose control connection closes instead
    /// ends, having changed nothing.
    Proceed(Guard),
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
    /// Compress the blocks of files sent.
    pub compress: bool,
    /// The zstd level to compress at.
    pub compress_level: i32,
    /// Profile the run, and report where the time went when it is done.
    pub profile: bool,
    /// For tests: the Source sends at most this many entries of a directory
    /// in a manifest part, rather than [`crate::scan::PART`]; 0 for that.
    pub part_entries: u32,
    /// For tests: the Source's scan waits while more than this many needed
    /// files wait to be sent, rather than 100,000; 0 for that.
    pub backlog_files: u32,
    /// For tests: the Source's manifest waits for room once this many bytes
    /// of messages wait to be written, rather than 8 MiB; 0 for that.
    pub paced_bytes: u32,
    /// For tests: the Source's scan waits while more than this many entries
    /// it sent are not compared yet, rather than [`IN_FLIGHT_ENTRIES`]; 0
    /// for that.
    pub in_flight_entries: u32,
    /// For measurements: the data credit the Destination gives, rather than
    /// [`DATA_CREDIT`]; 0 for that. See [`data_credit`].
    pub data_credit: u64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Needs {
    /// Entries the Destination needs created or sent, in manifest order. Only
    /// files' content is sent; the Destination creates the rest itself.
    Files(Vec<FileId>),
    /// Entries whose permissions or owner the Destination sets, which a dry
    /// run lists.
    Attributes(Vec<FileId>),
    /// Every manifest entry up to this ID has been compared, and those of
    /// them needed have been asked for: the Source need keep none of them,
    /// and its scan, waiting for the Destination, may go on. Said every
    /// [`through_every`] entries or so.
    Through(FileId),
    /// Every manifest entry has been compared.
    End,
}

/// How many manifest entries may be on the way, sent by the Source and not
/// yet compared by the Destination, before the scan waits: the needs of
/// those not compared yet are still to come, so what waits to be sent stays
/// bounded by this and the needed files' own bound.
pub const IN_FLIGHT_ENTRIES: u32 = 262_144;

/// The most entries apart the Destination says, by [`Needs::Through`], that
/// it has compared them.
pub const THROUGH_EVERY: FileId = 4096;

/// How many entries apart the Destination says it has compared them, where
/// `in_flight` may be on the way, as [`Hello::in_flight_entries`] sets it:
/// half, so that a Source waiting for it hears of it however few may be;
/// at most [`THROUGH_EVERY`].
pub fn through_every(in_flight: u32) -> FileId {
    (FileId::from(in_flight) / 2).clamp(1, THROUGH_EVERY)
}

/// How many entries may be on the way, where `in_flight` is what
/// [`Hello::in_flight_entries`] says: [`IN_FLIGHT_ENTRIES`] for 0, and never
/// fewer than twice [`through_every`], so that the Source, waiting at a
/// directory's start, is always told before the Destination has nothing
/// more to compare.
pub fn in_flight_limit(in_flight: u32) -> FileId {
    let in_flight = if in_flight == 0 {
        IN_FLIGHT_ENTRIES
    } else {
        in_flight
    };
    FileId::from(in_flight).max(2 * through_every(in_flight))
}

/// What became of entries on the Destination, as it finds out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Results {
    /// An entry could not be made to match; the transfer goes on without it.
    Failed { id: FileId, message: String },
    /// An entry `sync` found extra, which it could not delete.
    NotDeleted { path: Vec<u8>, message: String },
}

/// The hash of a file the Source sent in segments, merged from theirs once
/// all are read, for the Destination to check its own against before it
/// gives the file its name; see [`crate::block`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHash {
    pub file: FileId,
    pub hash: [u8; 32],
}

/// More of the data credit, given back by the Destination as it lets go of
/// blocks it has written, or given at the start: how many bytes, as
/// [`crate::block::cost`] counts them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    pub bytes: u64,
}

/// How many bytes of blocks, as [`crate::block::cost`] counts them, the
/// Source may have sent that the Destination has not let go of: what both
/// hold of them, sent and not written, is bounded by it, and so the
/// Destination never stops reading its connection for want of room.
pub const DATA_CREDIT: u64 = 64 * 1024 * 1024;

/// The data credit, where `asked` is what [`Hello::data_credit`] says:
/// [`DATA_CREDIT`] for 0, and never less than two of the largest block, so
/// that one always fits.
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

impl Message for Needs {
    const TYPE: PacketType = PacketType::Needs;
}

impl Message for Results {
    const TYPE: PacketType = PacketType::Results;
}

impl Message for FileHash {
    const TYPE: PacketType = PacketType::FileHash;
}

impl Message for Credit {
    const TYPE: PacketType = PacketType::Credit;
}

impl Message for crate::report::Report {
    const TYPE: PacketType = PacketType::Report;
}

/// A packet, whole, ready to write.
pub fn encode<M: Message>(message: &M) -> Result<Vec<u8>> {
    let payload = postcard::to_stdvec(message).context("encode a packet")?;
    packet(M::TYPE, &payload)
}

/// A data packet carrying the next bytes of the blocks of files, whole.
pub fn encode_data(bytes: &[u8]) -> Result<Vec<u8>> {
    packet(PacketType::Data, bytes)
}

/// The manifest's messages, compressed as one zstd stream, each flushed into
/// a packet of its own, which decodes as it comes: a directory's part alone,
/// as most directories are small, has little to compress, and the parts
/// before it are its context. Measured on `/usr`, a crate registry's sources
/// and a build's output, each part alone compressed 1.5 to 2 times, and as
/// one stream 2.6 to 3.4. Only messages sent in order go in it: a problem,
/// sent ahead of them, goes as a [`PacketType::Manifest`] of its own.
pub struct ManifestEncoder {
    stream: zstd::stream::write::Encoder<'static, Vec<u8>>,
}

impl ManifestEncoder {
    pub fn new(level: i32) -> Result<Self> {
        let stream = zstd::stream::write::Encoder::new(Vec::new(), level)
            .context("compress the manifest")?;
        Ok(Self { stream })
    }

    /// A packet, whole, holding `message`, to be decoded after those before.
    pub fn encode(&mut self, message: &Manifest) -> Result<Vec<u8>> {
        let payload = postcard::to_stdvec(message).context("encode a packet")?;
        if payload.len() > max_payload() as usize {
            bail!("a manifest message is too large");
        }
        self.stream
            .write_all(&payload)
            .and_then(|()| self.stream.flush())
            .context("compress the manifest")?;
        packet(
            PacketType::ManifestStream,
            &std::mem::take(self.stream.get_mut()),
        )
    }
}

/// Decodes what a [`ManifestEncoder`] encoded, each packet in the order it
/// was: none that holds a message larger than a packet may be.
pub struct ManifestDecoder {
    stream: zstd::stream::write::Decoder<'static, Bounded>,
}

impl ManifestDecoder {
    pub fn new() -> Result<Self> {
        let stream = zstd::stream::write::Decoder::new(Bounded(Vec::new()))
            .context("decompress the manifest")?;
        Ok(Self { stream })
    }

    pub fn decode(&mut self, packet: &Packet) -> Result<Manifest> {
        if packet.kind != PacketType::ManifestStream {
            bail!("expected a manifest stream packet, got {:?}", packet.kind);
        }
        self.stream
            .write_all(&packet.payload)
            .and_then(|()| self.stream.flush())
            .context("decompress the manifest")?;
        let payload = std::mem::take(&mut self.stream.get_mut().0);
        postcard::from_bytes(&payload).context("decode a manifest stream packet")
    }
}

/// Bytes written, as many as a packet may hold.
struct Bounded(Vec<u8>);

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len() + bytes.len() > max_payload() as usize {
            return Err(io::Error::other("a manifest message is too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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

/// Sends the next bytes of the blocks of files.
pub fn send_data(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    writer.write_all(&encode_data(bytes)?)?;
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

    /// A tree of small directories as its scan sends it: each part alone
    /// has little to compress.
    fn small_directories() -> Vec<Manifest> {
        let mut id = 0;
        (0..200)
            .map(|directory| Manifest::Directory {
                path: format!("src/module{directory:03}").into_bytes(),
                entries: (0..5)
                    .map(|file| {
                        id += 1;
                        Entry {
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
                        }
                    })
                    .collect(),
                more: false,
            })
            .chain([Manifest::End])
            .collect()
    }

    /// Each packet decodes, in the order sent, to the message encoded; and
    /// as one stream the manifest takes fewer bytes than each part compressed
    /// alone, a third fewer than as it is, though each entry's size and mtime
    /// here are as good as random.
    #[test]
    fn the_manifest_stream_decodes_in_order_and_is_smaller() {
        let messages = small_directories();
        let mut encoder = ManifestEncoder::new(1).unwrap();
        let mut decoder = ManifestDecoder::new().unwrap();
        let (mut plain, mut alone, mut stream) = (0, 0, 0);
        for message in &messages {
            let as_it_is = encode(message).unwrap();
            plain += as_it_is.len();
            alone += zstd::bulk::compress(&as_it_is, 1).unwrap().len();
            let packet = encoder.encode(message).unwrap();
            stream += packet.len();
            let packet = receive(&mut packet.as_slice()).unwrap().unwrap();
            assert_eq!(&decoder.decode(&packet).unwrap(), message);
        }
        assert!(
            stream < alone && stream * 3 < plain * 2,
            "{stream} bytes, {alone} each alone, {plain} as it is"
        );
    }

    /// One that decodes to more than a packet may hold is refused, as a
    /// packet that says it holds more is.
    #[test]
    fn a_manifest_message_larger_than_a_packet_is_refused() {
        let mut stream = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        stream
            .write_all(&vec![0; max_payload() as usize + 1])
            .unwrap();
        stream.flush().unwrap();
        let packet = Packet {
            kind: PacketType::ManifestStream,
            payload: std::mem::take(stream.get_mut()),
        };
        let error = ManifestDecoder::new().unwrap().decode(&packet).unwrap_err();
        assert!(format!("{error:#}").contains("too large"), "{error:#}");
    }

    fn welcome() -> Welcome {
        Welcome {
            socket: b"/tmp/s".to_vec(),
            root: RootIdentity::default(),
        }
    }

    #[test]
    fn round_trip() {
        let mut buffer = Vec::new();
        send(&mut buffer, &Needs::Files(vec![1, 2, 3])).unwrap();
        send(&mut buffer, &Control::Welcome(welcome())).unwrap();
        let mut reader = buffer.as_slice();
        assert_eq!(
            expect::<Needs>(&mut reader).unwrap(),
            Needs::Files(vec![1, 2, 3])
        );
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
        assert!(expect::<Needs>(&mut buffer.as_slice()).is_err());
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
