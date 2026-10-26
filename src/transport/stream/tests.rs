use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::*;
use crate::protocol::{self, Control, PacketType};
use crate::test_support::{PATIENCE, within};

/// A stream's two ends, each started on one end of a socket pair.
fn pair() -> ((Reading, Writing, Carried), (Reading, Writing, Carried)) {
    let (one, other) = UnixStream::pair().unwrap();
    let start = |socket| start(Ends::socket(socket).unwrap(), Scope::new().unwrap()).unwrap();
    (start(one), start(other))
}

/// One end of a stream on a socket pair, and the other end as it is, with
/// no stream on it.
fn half() -> ((Reading, Writing, Carried), UnixStream) {
    let (one, other) = UnixStream::pair().unwrap();
    let started = start(Ends::socket(one).unwrap(), Scope::new().unwrap()).unwrap();
    (started, other)
}

/// Why reading the next packet failed: it must.
fn failure(reading: &mut Reading) -> String {
    match protocol::receive(reading) {
        Err(error) => format!("{error:#}"),
        Ok(_) => panic!("a packet was read"),
    }
}

/// The peers' packets go as they are, both small and large; the stream's
/// end reads as the end.
#[test]
fn packets_go_as_they_are_and_the_end_reads_as_the_end() {
    within(
        "packets go as they are and the end reads as the end",
        PATIENCE,
        || {
            let ((_reading, mut writing, _), (mut reading, _writing, _)) = pair();
            let large: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
            let sending = std::thread::spawn(move || {
                protocol::send(&mut writing, &Control::Done).unwrap();
                protocol::send_data(&mut writing, &large).unwrap();
                for byte in 0..100u8 {
                    protocol::send_data(&mut writing, &[byte]).unwrap();
                }
                writing.finish().unwrap();
                large
            });
            assert!(matches!(
                protocol::expect::<Control>(&mut reading).unwrap(),
                Control::Done
            ));
            let data = protocol::receive(&mut reading).unwrap().unwrap();
            assert_eq!(data.kind, PacketType::Data);
            let large = sending.join().unwrap();
            assert_eq!(data.into_payload(), large);
            for byte in 0..100u8 {
                let small = protocol::receive(&mut reading).unwrap().unwrap();
                assert_eq!(small.into_payload(), [byte]);
            }
            assert!(protocol::receive(&mut reading).unwrap().is_none());
        },
    );
}

/// A stream that ends without its end broke: the reader says so, rather
/// than read to its end.
#[test]
fn a_stream_ended_without_its_end_broke() {
    within("a stream ended without its end broke", PATIENCE, || {
        let ((mut reading, _writing, _), mut other) = half();
        protocol::send(&mut other, &Control::Done).unwrap();
        drop(other);
        assert!(matches!(
            protocol::expect::<Control>(&mut reading).unwrap(),
            Control::Done
        ));
        let said = failure(&mut reading);
        assert!(
            said.contains("the stream ended before the other side was done"),
            "{said}"
        );
    });
}

/// Ended inside a packet, it broke too.
#[test]
fn a_stream_ended_inside_a_packet_broke() {
    within("a stream ended inside a packet broke", PATIENCE, || {
        let ((mut reading, _writing, _), mut other) = half();
        let packet = protocol::encode_data(&[1; 100]).unwrap();
        other.write_all(&packet[..50]).unwrap();
        drop(other);
        let said = failure(&mut reading);
        assert!(said.contains("inside a packet"), "{said}");
    });
}

/// A writer that waits for room the other side never makes stops once its
/// stream is taken for broken, as its scope ends.
#[test]
fn a_writer_waiting_for_room_stops_once_the_stream_breaks() {
    within(
        "a writer waiting for room stops once the stream breaks",
        PATIENCE,
        || {
            let (one, _other) = UnixStream::pair().unwrap();
            let scope = Scope::new().unwrap();
            let (mut reading, mut writing, _) =
                start(Ends::socket(one).unwrap(), scope.clone()).unwrap();
            let ending = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                scope.end("the host was lost");
            });
            let writer = std::thread::spawn(move || {
                let chunk = vec![0u8; 1 << 16];
                loop {
                    if let Err(error) = writing.write_all(&chunk) {
                        return error;
                    }
                }
            });
            failure(&mut reading);
            let started = Instant::now();
            let error = writer.join().unwrap();
            ending.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe, "{error}");
            assert!(error.to_string().contains("the host was lost"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(5));
        },
    );
}

/// What a shell printed before the start mark is skipped; a stream that
/// ends before its start mark never started.
#[test]
fn a_marked_stream_skips_to_its_start_mark() {
    within("a marked stream skips to its start mark", PATIENCE, || {
        let (one, mut other) = UnixStream::pair().unwrap();
        let mut ends = Ends::socket(one).unwrap();
        ends.marked = true;
        other.write_all(b"noise from a shell\n").unwrap();
        other.write_all(start::MARK).unwrap();
        protocol::send(&mut other, &Control::Done).unwrap();
        other
            .write_all(&[PacketType::StreamEnd as u8, 0, 0, 0, 0])
            .unwrap();
        drop(other);
        let (mut reading, _writing, _) = start(ends, Scope::new().unwrap()).unwrap();
        assert!(matches!(
            protocol::expect::<Control>(&mut reading).unwrap(),
            Control::Done
        ));
        assert!(protocol::receive(&mut reading).unwrap().is_none());

        let (one, mut other) = UnixStream::pair().unwrap();
        let mut ends = Ends::socket(one).unwrap();
        ends.marked = true;
        other.write_all(b"no mark").unwrap();
        drop(other);
        let error = start(ends, Scope::new().unwrap()).err().unwrap();
        assert!(
            error.to_string().contains("ended before it started"),
            "{error}"
        );
    });
}

/// On pipes, which cannot be shut down, the end still comes: the writing
/// end closes once the stream's end is written.
#[test]
fn on_pipes_the_end_comes_as_the_writing_end_closes() {
    within(
        "on pipes the end comes as the writing end closes",
        PATIENCE,
        || {
            let (from_other, to_this) = std::io::pipe().unwrap();
            let (from_this, to_other) = std::io::pipe().unwrap();
            let ends = Ends {
                input: File::from(OwnedFd::from(from_other)),
                output: File::from(OwnedFd::from(to_other)),
                held: None,
                marked: false,
                before: Vec::new(),
            };
            let (_reading, mut writing, _) = start(ends, Scope::new().unwrap()).unwrap();
            protocol::send(&mut writing, &Control::Done).unwrap();
            writing.finish().unwrap();
            let mut from_this = from_this;
            let mut came = Vec::new();
            from_this.read_to_end(&mut came).unwrap();
            let mut expected = protocol::encode(&Control::Done).unwrap();
            expected.extend([PacketType::StreamEnd as u8, 0, 0, 0, 0]);
            assert_eq!(came, expected);
            drop(to_this);
        },
    );
}

/// The stream ends with its end, though what carries it stays open, as a
/// pipe a joining process still holds: the reader waits for nothing more.
#[test]
fn the_end_ends_the_stream_though_what_carries_it_stays_open() {
    within(
        "the end ends the stream though what carries it stays open",
        PATIENCE,
        || {
            let (from_other, mut to_this) = std::io::pipe().unwrap();
            let (_from_this, to_other) = std::io::pipe().unwrap();
            let ends = Ends {
                input: File::from(OwnedFd::from(from_other)),
                output: File::from(OwnedFd::from(to_other)),
                held: None,
                marked: false,
                before: Vec::new(),
            };
            let (mut reading, _writing, _) = start(ends, Scope::new().unwrap()).unwrap();
            protocol::send(&mut to_this, &Control::Done).unwrap();
            to_this
                .write_all(&[PacketType::StreamEnd as u8, 0, 0, 0, 0])
                .unwrap();
            let started = Instant::now();
            assert!(matches!(
                protocol::expect::<Control>(&mut reading).unwrap(),
                Control::Done
            ));
            assert!(protocol::receive(&mut reading).unwrap().is_none());
            assert!(started.elapsed() < Duration::from_secs(5));
            // Still open.
            drop(to_this);
        },
    );
}

/// `stream` read through a deframer, what carries it giving at most `gives`
/// bytes at a time, and the peer asking for `asks`: what the peer got, and
/// why it stopped, if not at the stream's end.
fn deframed(stream: &[u8], gives: usize, asks: usize) -> (Vec<u8>, Option<&'static str>) {
    let mut deframer = Deframer::default();
    let mut at = 0;
    let mut more = |into: &mut [u8]| -> io::Result<usize> {
        assert!(!into.is_empty(), "asked to read into nothing");
        let given = gives.min(into.len()).min(stream.len() - at);
        into[..given].copy_from_slice(&stream[at..at + given]);
        at += given;
        Ok(given)
    };
    let mut got = Vec::new();
    let mut into = vec![0; asks];
    loop {
        match deframer.read(&mut into, &mut more) {
            Ok(0) => return (got, None),
            Ok(read) => got.extend_from_slice(&into[..read]),
            Err(Fault::Wrong(_, why)) => return (got, Some(why)),
            Err(Fault::Read(error)) => panic!("{error}"),
        }
    }
}

/// What comes is handed on as it came, the stream's end taken out, however
/// what carries it cuts it, and however much the peer asks for: small
/// packets, empty ones, and large ones, which go past the reader's buffer,
/// straight to the peer.
#[test]
fn the_peers_packets_come_through_as_they_were_however_they_are_cut() {
    let packets = [
        protocol::encode_data(&[1; 10]).unwrap(),
        protocol::encode_data(&(0..100_000u32).map(|i| i as u8).collect::<Vec<_>>()).unwrap(),
        protocol::encode_data(&[]).unwrap(),
        protocol::encode_data(&[3; 70_000]).unwrap(),
    ];
    let mut stream = Vec::new();
    for packet in &packets {
        stream.extend_from_slice(packet);
    }
    stream.extend_from_slice(&[PacketType::StreamEnd as u8, 0, 0, 0, 0]);
    // Nothing after the end is read.
    stream.extend_from_slice(b"after");
    let sent = packets.concat();
    for gives in [1, 3, 5, 4096, 70_000, 1 << 20] {
        for asks in [1, 7, STRAIGHT, 100_000] {
            assert_eq!(
                deframed(&stream, gives, asks),
                (sent.clone(), None),
                "{gives} given at a time, {asks} asked for"
            );
        }
    }
}

/// A stream wrong, or cut short, is said to be, what came before handed on:
/// a packet of the stream's own with a payload; an end inside a packet, or
/// inside a header; and the end of what carries it before the stream's.
#[test]
fn a_stream_wrong_or_cut_short_is_said_to_be() {
    let packet = protocol::encode_data(&[5; 30_000]).unwrap();
    let after = |tail: &[u8]| [&packet[..], tail].concat();
    let cases = [
        (
            after(&[PacketType::StreamEnd as u8, 1, 0, 0, 0]),
            packet.len(),
            "a packet of the stream's own with a payload",
        ),
        (
            packet[..20_000].to_vec(),
            20_000,
            "the stream ended inside a packet",
        ),
        (
            after(&packet[..3]),
            packet.len(),
            "the stream ended inside a packet",
        ),
        (
            packet.clone(),
            packet.len(),
            "the stream ended before the other side was done",
        ),
    ];
    for (stream, handed, why) in cases {
        for (gives, asks) in [(1, 1), (7, 100_000), (1 << 20, STRAIGHT)] {
            let (got, said) = deframed(&stream, gives, asks);
            assert_eq!(got.as_slice(), &stream[..handed], "{why}");
            assert_eq!(said, Some(why));
        }
    }
}

/// What was read from the stream before it was handed over, as by a
/// buffered reader past a handshake, comes first, however it is cut: the
/// half packet in it is read as the rest comes.
#[test]
fn what_was_read_before_the_stream_started_comes_first() {
    within(
        "what was read before the stream started comes first",
        PATIENCE,
        || {
            let (one, mut other) = UnixStream::pair().unwrap();
            let mut before = Vec::new();
            let done = protocol::encode(&Control::Done).unwrap();
            let snapshot = protocol::encode(&Control::Snapshot).unwrap();
            before.extend(&done);
            before.extend(&snapshot[..2]);
            let ends = Ends {
                before,
                ..Ends::socket(one).unwrap()
            };
            let (mut reading, _writing, _) = start(ends, Scope::new().unwrap()).unwrap();
            other.write_all(&snapshot[2..]).unwrap();
            assert!(matches!(
                protocol::expect::<Control>(&mut reading).unwrap(),
                Control::Done
            ));
            assert!(matches!(
                protocol::expect::<Control>(&mut reading).unwrap(),
                Control::Snapshot
            ));
        },
    );
}

/// A stream let go of stops its reader, waiting, at once, though what
/// carries it cannot be shut down, as a pipe.
#[test]
fn a_stream_let_go_of_stops_its_reader_at_once() {
    within(
        "a stream let go of stops its reader at once",
        PATIENCE,
        || {
            // On pipes, which cannot be shut down as a socket is.
            let (from_other, _to_this) = std::io::pipe().unwrap();
            let (_from_this, to_other) = std::io::pipe().unwrap();
            let ends = Ends {
                input: File::from(OwnedFd::from(from_other)),
                output: File::from(OwnedFd::from(to_other)),
                held: None,
                marked: false,
                before: Vec::new(),
            };
            let (mut reading, _writing, carried) = start(ends, Scope::new().unwrap()).unwrap();
            let letting = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                carried.let_go();
            });
            let started = Instant::now();
            let error = failure(&mut reading);
            assert!(error.contains("let go"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(5));
            letting.join().unwrap();
        },
    );
}
