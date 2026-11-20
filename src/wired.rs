//! A peer on the stream's real pipes, on a socket pair, the other peer
//! played by one stream of its packets, as the peers sent them before they
//! had pipes: for the tests and the fuzzer, which say what the other peer
//! sends as packets, and read what the peer sends back as packets. Built
//! for them alone, never in Ferry itself.
//!
//! Each packet of the other peer goes on the pipe its kind does, as it is
//! read: a manifest's, a data's, and an answer's as the record it holds, an
//! empty data packet ending the data; a failure on control. At the end of
//! its packets, the other peer's pipes end, as at its end; should reading
//! them fail, the stream breaks. What the peer sends, on each of its pipes,
//! comes back as packets, each record as one, the data's end as an empty
//! data packet, as it comes: the peer's packets wait on the pipes' windows,
//! as on a connection, for the other peer to read them.

#![allow(
    clippy::disallowed_methods,
    reason = "what plays the other peer starts its threads as it likes"
)]

use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use anyhow::Result;

use crate::block::{self, ByteBuf};
use crate::manifest::Manifest;
use crate::protocol::{self, Answer, Control, Packet, PacketType};
use crate::transport::pipes::{self, Connection, PipeReader, PipeWriter, Windows};
use crate::transport::scope::Scope;
use crate::transport::wire::Wire;
use crate::{destination, source};

/// The two ends of a stream on a socket pair, the Source's and the
/// Destination's, with `windows`, within `scope`.
fn pair(windows: Windows, scope: &Scope) -> io::Result<(Connection, Connection)> {
    let (source, destination) = UnixStream::pair()?;
    Ok((
        Connection::open(
            vec![Wire::socket(source, scope)?],
            &pipes::stream(true, windows, None),
        )?,
        Connection::open(
            vec![Wire::socket(destination, scope)?],
            &pipes::stream(false, windows, None),
        )?,
    ))
}

/// Runs the Destination on its end of a stream, the Source played by its
/// packets on `reader`, writing the Destination's on `writer`.
pub fn destination(
    reader: &mut (impl Read + Send),
    writer: &mut (impl Write + Send),
    options: &destination::Options,
) -> Result<destination::Summary> {
    let scope = Scope::new()?;
    let (mut theirs, mut ours) = pair(Windows::default(), &scope)?;
    let control = ours.writer(pipes::CONTROL);
    let (reading, mux) = destination::on_stream(&mut ours, control);
    let back = [
        (theirs.reader(pipes::ANSWERS), Back::Answers),
        (theirs.reader(pipes::CONTROL), Back::Control),
    ];
    let fed = Fed {
        control: theirs.writer(pipes::CONTROL),
        records: theirs.writer(pipes::MANIFEST),
        record: PacketType::Manifest,
        data: Some(theirs.writer(pipes::DATA)),
    };
    run(reader, writer, &scope, &theirs, fed, back, || {
        let result = destination::run_on_pipes(reading, mux, options);
        let _ = ours.finish();
        result
    })
}

/// Runs the Source on its end of a stream, the Destination played by its
/// packets on `reader`, writing the Source's on `writer`.
pub fn source(
    reader: &mut (impl Read + Send),
    writer: &mut (impl Write + Send),
    options: &source::Options,
) -> Result<source::Summary> {
    source_within(reader, writer, options, Windows::default())
}

/// As [`source`], the stream's pipes with `windows`: small ones, read back
/// by a slow `writer`, hold the Source back as a slow connection does.
pub fn source_within(
    reader: &mut (impl Read + Send),
    writer: &mut (impl Write + Send),
    options: &source::Options,
    windows: Windows,
) -> Result<source::Summary> {
    let scope = Scope::new()?;
    let (mut ours, mut theirs) = pair(windows, &scope)?;
    let control = ours.writer(pipes::CONTROL);
    let (reading, mux) = source::on_stream(&mut ours, control);
    let back = [
        (theirs.reader(pipes::MANIFEST), Back::Manifest),
        (theirs.reader(pipes::DATA), Back::Data),
        (theirs.reader(pipes::CONTROL), Back::Control),
    ];
    let fed = Fed {
        control: theirs.writer(pipes::CONTROL),
        records: theirs.writer(pipes::ANSWERS),
        record: PacketType::Answer,
        data: None,
    };
    run(reader, writer, &scope, &theirs, fed, back, || {
        let result = source::run_on_pipes(reading, mux, options);
        let _ = ours.finish();
        result
    })
}

/// The other peer's pipes, as its packets are fed to them.
struct Fed {
    control: PipeWriter,
    /// Where its records go, the Source's manifest or the Destination's
    /// answers, and what else it sends, for the peer to find it is none.
    records: PipeWriter,
    /// The kind of its packets that hold a record there.
    record: PacketType,
    /// The Source's data, until its end.
    data: Option<PipeWriter>,
}

/// One of the peer's pipes, read back as packets.
#[derive(Clone, Copy)]
enum Back {
    Manifest,
    Data,
    Answers,
    Control,
}

/// Feeds the other peer's packets to its pipes, reads the peer's back, and
/// runs the peer, `peer`, until it is done and each pipe it writes is read
/// to its end; then breaks the stream, should the other peer's packets
/// still wait for room, as they may once the peer stopped reading.
fn run<T>(
    reader: &mut (impl Read + Send),
    writer: &mut (impl Write + Send),
    scope: &Scope,
    theirs: &Connection,
    fed: Fed,
    back: impl IntoIterator<Item = (PipeReader, Back)>,
    peer: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let writer = Mutex::new(writer);
    std::thread::scope(|threads| {
        let feeding = threads.spawn(|| feed(reader, theirs, fed, scope));
        let reading: Vec<_> = back
            .into_iter()
            .map(|(pipe, back)| {
                let writer = &writer;
                threads.spawn(move || read_back(pipe, back, writer))
            })
            .collect();
        let result = peer();
        for reading in reading {
            let _ = reading.join();
        }
        scope.end("the peer is done");
        let _ = feeding.join();
        result
    })
}

/// Feeds `reader`'s packets to the other peer's pipes until they end, and
/// then ends its pipes; or, should reading them fail, breaks the stream.
fn feed(reader: &mut (impl Read + Send), theirs: &Connection, mut fed: Fed, scope: &Scope) {
    loop {
        let packet = match protocol::receive(reader) {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                scope.end(&format!("{error:#}"));
                return;
            }
        };
        if feed_one(packet, &mut fed).is_err() {
            return;
        }
    }
    drop(fed);
    let _ = theirs.finish();
}

/// Puts one packet on the pipe its kind goes on.
fn feed_one(packet: Packet, fed: &mut Fed) -> io::Result<()> {
    // An empty data packet ends the data.
    if packet.kind == PacketType::Data && packet.size() == 0 {
        if let Some(data) = fed.data.take() {
            data.close();
        }
        return Ok(());
    }
    // A record, of the manifest, the data, or the answers, as the record
    // it holds, whatever it holds; the data's, once it ended, let go of.
    if packet.kind == PacketType::Data {
        return match &fed.data {
            Some(data) => data.write_all(&record(&packet.into_payload())),
            None => Ok(()),
        };
    }
    if packet.kind == fed.record {
        return fed.records.write_all(&record(&packet.into_payload()));
    }
    // The Source's control goes on control, and the Destination's failure;
    // anything else, as it is, where the other peer's records go, for the
    // peer to find it is no record.
    let on_control = packet.kind == PacketType::Control
        && (fed.record == PacketType::Manifest
            || matches!(packet.decode::<Control>(), Ok(Control::Error { .. })));
    let pipe = if on_control {
        &fed.control
    } else {
        &fed.records
    };
    let kind = packet.kind;
    let payload = packet.into_payload();
    let mut bytes = vec![kind as u8];
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&payload);
    pipe.write_all(&bytes)
}

/// `payload` as a record: its length, as a varint, then itself.
fn record(payload: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(payload.len() + 5);
    let mut length = payload.len();
    loop {
        let byte = (length & 0x7f) as u8;
        length >>= 7;
        if length == 0 {
            record.push(byte);
            break;
        }
        record.push(byte | 0x80);
    }
    record.extend_from_slice(payload);
    record
}

/// Reads one of the peer's pipes to its end, each record, or packet, as a
/// packet on `writer`.
fn read_back(pipe: PipeReader, back: Back, writer: &Mutex<&mut (impl Write + Send)>) {
    let mut pipe = BufReader::new(pipe);
    let write = |packet: &[u8]| {
        let _ = writer.lock().expect("not poisoned").write_all(packet);
    };
    loop {
        let packet = match back {
            Back::Manifest => match protocol::read_record::<Manifest>(&mut pipe) {
                Ok(Some(record)) => protocol::encode(&record),
                _ => return,
            },
            Back::Answers => match protocol::read_record::<Answer>(&mut pipe) {
                Ok(Some(answer)) => protocol::encode(&answer),
                _ => return,
            },
            Back::Data => match block::read(&mut pipe) {
                Ok(Some((buffer, record))) => {
                    protocol::encode(&record.map(|range| ByteBuf(buffer[range].to_vec())))
                }
                // The data's end.
                Ok(None) => {
                    write(&[PacketType::Data as u8, 0, 0, 0, 0]);
                    return;
                }
                Err(_) => return,
            },
            Back::Control => match protocol::receive(&mut pipe) {
                Ok(Some(packet)) => {
                    let kind = packet.kind;
                    let payload = packet.into_payload();
                    let mut bytes = vec![kind as u8];
                    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                    bytes.extend_from_slice(&payload);
                    Ok(bytes)
                }
                _ => return,
            },
        };
        match packet {
            Ok(packet) => write(&packet),
            Err(_) => return,
        }
    }
}
