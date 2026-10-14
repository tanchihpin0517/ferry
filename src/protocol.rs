//! Packets: the unit peers exchange over their connection.
//!
//! A packet is `[type: u8][length: u32, little-endian][payload]`. The payload
//! is a message encoded with postcard; its type decides which message.

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
}

impl PacketType {
    fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => Self::Control,
            1 => Self::Manifest,
            2 => Self::Needs,
            _ => bail!("unknown packet type {byte}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// The client's first message.
    Hello(Hello),
    /// The server accepts the client's Hello.
    Welcome,
    /// The server refuses the client's Hello, and exits.
    Refused { reason: String },
    /// The sender failed, and stops.
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The client's build identity; the server must be the same build.
    pub build: String,
    /// Where the server writes: the Destination's root, as path bytes.
    pub destination: Vec<u8>,
    /// Compare only; never write.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Needs {
    /// Files the Destination needs, in manifest order.
    Files(Vec<FileId>),
    /// Every manifest entry has been compared.
    End,
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

pub fn send<M: Message>(writer: &mut impl Write, message: &M) -> Result<()> {
    let payload = postcard::to_stdvec(message).context("encode a packet")?;
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|&length| length <= MAX_PAYLOAD)
        .with_context(|| format!("a {:?} packet is too large", M::TYPE))?;
    writer.write_all(&[M::TYPE as u8])?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    Ok(())
}

/// A packet as read, before its payload is decoded.
pub struct Packet {
    pub kind: PacketType,
    payload: Vec<u8>,
}

impl Packet {
    pub fn decode<M: Message>(&self) -> Result<M> {
        if self.kind != M::TYPE {
            bail!("expected a {:?} packet, got {:?}", M::TYPE, self.kind);
        }
        postcard::from_bytes(&self.payload)
            .with_context(|| format!("decode a {:?} packet", self.kind))
    }
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

    #[test]
    fn round_trip() {
        let mut buffer = Vec::new();
        send(&mut buffer, &Needs::Files(vec![1, 2, 3])).unwrap();
        send(&mut buffer, &Control::Welcome).unwrap();
        let mut reader = buffer.as_slice();
        assert_eq!(
            expect::<Needs>(&mut reader).unwrap(),
            Needs::Files(vec![1, 2, 3])
        );
        assert_eq!(expect::<Control>(&mut reader).unwrap(), Control::Welcome);
        assert!(receive(&mut reader).unwrap().is_none());
    }

    #[test]
    fn wrong_type_is_an_error() {
        let mut buffer = Vec::new();
        send(&mut buffer, &Control::Welcome).unwrap();
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
        send(&mut buffer, &Control::Welcome).unwrap();
        buffer.pop();
        assert!(receive(&mut buffer.as_slice()).is_err());
    }
}
