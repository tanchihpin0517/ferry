//! Packets: the unit peers exchange over their connection.
//!
//! A packet is `[type: u8][length: u32, little-endian][payload]`. The payload
//! is a message encoded with postcard, its type deciding which message, except
//! in a data packet: that carries the next bytes of the record stream, as they
//! are; see [`crate::record`].

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::manifest::{FileId, Manifest};

/// The largest payload a peer accepts, so a corrupt length cannot make it
/// allocate without bound.
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

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
            _ => bail!("unknown packet type {byte}"),
        })
    }
}

/// Where a server waits for its paths: a Unix socket on its host, which a
/// path reaches straight, through SSH, or by running `ferry server --join
/// <token>` there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub token: String,
    /// The socket's path.
    pub socket: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// The client's first message.
    Hello(Hello),
    /// The server accepts the client's Hello, and is ready for its paths.
    Welcome(Welcome),
    /// The server refuses the client's Hello, and exits.
    Refused { reason: String },
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
    /// The client's build identity; the server must be the same build.
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
    /// Make what is written durable before finishing.
    pub fsync: bool,
    /// Delete what the Source does not have: `ferry sync`.
    pub delete: bool,
    /// Compress the record stream.
    pub compress: bool,
    /// The zstd level to compress at.
    pub compress_level: i32,
    /// Profile the run, and report where the time went when it is done.
    pub profile: bool,
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
    /// Entries the Destination has that the Source does not, which `sync`
    /// deletes: paths, and whether each is a directory.
    Deletes(Vec<(Vec<u8>, bool)>),
    /// Every manifest entry has been compared.
    End,
}

/// What became of entries on the Destination, as it finds out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Results {
    /// An entry could not be made to match; the transfer goes on without it.
    Failed { id: FileId, message: String },
    /// An entry `sync` found extra, which it could not delete.
    NotDeleted { path: Vec<u8>, message: String },
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

impl Message for crate::report::Report {
    const TYPE: PacketType = PacketType::Report;
}

/// A packet, whole, ready to write.
pub fn encode<M: Message>(message: &M) -> Result<Vec<u8>> {
    let payload = postcard::to_stdvec(message).context("encode a packet")?;
    packet(M::TYPE, &payload)
}

/// A data packet carrying the next bytes of the record stream, whole.
pub fn encode_data(bytes: &[u8]) -> Result<Vec<u8>> {
    packet(PacketType::Data, bytes)
}

fn packet(kind: PacketType, payload: &[u8]) -> Result<Vec<u8>> {
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|&length| length <= MAX_PAYLOAD)
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

/// Sends the next bytes of the record stream.
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
    if length > MAX_PAYLOAD {
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

    fn welcome() -> Welcome {
        Welcome {
            token: "t".into(),
            socket: b"/tmp/s".to_vec(),
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
