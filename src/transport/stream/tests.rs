use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::*;
use crate::protocol::{self, Control, PacketType};
use crate::test_support::{PATIENCE, within};

/// A stream's two ends, each started on one end of a socket pair, with the
/// times given.
fn pair(
    keepalive: Duration,
    silence: Duration,
) -> ((Reading, Writing, Carried), (Reading, Writing, Carried)) {
    let (one, other) = UnixStream::pair().unwrap();
    let start = |socket| start_with(Ends::socket(socket).unwrap(), keepalive, silence).unwrap();
    (start(one), start(other))
}

/// One end of a stream on a socket pair, and the other end as it is, with
/// no stream on it.
fn half(keepalive: Duration, silence: Duration) -> ((Reading, Writing, Carried), UnixStream) {
    let (one, other) = UnixStream::pair().unwrap();
    let started = start_with(Ends::socket(one).unwrap(), keepalive, silence).unwrap();
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
            let ((_reading, mut writing, _), (mut reading, _writing, _)) = pair(KEEPALIVE, SILENCE);
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
        let ((mut reading, _writing, _), mut other) = half(KEEPALIVE, SILENCE);
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
        let ((mut reading, _writing, _), mut other) = half(KEEPALIVE, SILENCE);
        let packet = protocol::encode_data(&[1; 100]).unwrap();
        other.write_all(&packet[..50]).unwrap();
        drop(other);
        let said = failure(&mut reading);
        assert!(said.contains("inside a packet"), "{said}");
    });
}

/// Nothing coming for the silence the stream is allowed, its reader takes
/// it for broken, and says how long it waited.
#[test]
fn a_silent_stream_is_taken_for_broken() {
    within("a silent stream is taken for broken", PATIENCE, || {
        let ((mut reading, _writing, _), _other) = half(KEEPALIVE, Duration::from_secs(1));
        let started = Instant::now();
        let said = failure(&mut reading);
        assert!(
            said.contains("nothing came on the stream for 1 s"),
            "{said}"
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(900), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    });
}

/// While nothing else goes, each side's keepalives keep the other's reader
/// from taking the stream for broken, and are never read as packets.
#[test]
fn keepalives_keep_a_quiet_stream_going() {
    within("keepalives keep a quiet stream going", PATIENCE, || {
        let keepalive = Duration::from_millis(50);
        let silence = Duration::from_millis(400);
        let ((_reading, mut writing, _), (mut reading, _writing, carried)) =
            pair(keepalive, silence);
        let sending = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            protocol::send(&mut writing, &Control::Done).unwrap();
            writing.finish().unwrap();
        });
        assert!(matches!(
            protocol::expect::<Control>(&mut reading).unwrap(),
            Control::Done
        ));
        assert!(protocol::receive(&mut reading).unwrap().is_none());
        sending.join().unwrap();
        // The keepalives were heard, as bytes come.
        let (bytes, heard) = carried.now();
        assert!(bytes > 0, "{bytes}");
        assert!(heard.is_some());
    });
}

/// The keeper waits while it has nothing to write: profiled, it is waiting,
/// not at work, so that it is never what holds a transfer back.
#[test]
fn the_keeper_waits_while_it_has_nothing_to_write() {
    within(
        "the keeper waits while it has nothing to write",
        PATIENCE,
        || {
            if !profile::enabled() {
                return;
            }
            let ((_reading, _writing, _), _other) =
                half(Duration::from_secs(4), Duration::from_secs(60));
            // Every keeper this process has, this one among them, as it is
            // now and then: asleep, each is waiting.
            let (mut waiting, mut working) = (0, 0);
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(10));
                for thread in profile::snapshot() {
                    if thread.role == "stream keeper" {
                        if thread.wait {
                            waiting += 1;
                        } else {
                            working += 1;
                        }
                    }
                }
            }
            assert!(
                waiting > 0 && working * 2 < waiting,
                "{waiting} seen waiting, {working} at work"
            );
        },
    );
}

/// A keepalive never goes inside a packet, however long its writer waits
/// between two pieces of it.
#[test]
fn a_keepalive_never_goes_inside_a_packet() {
    within("a keepalive never goes inside a packet", PATIENCE, || {
        let keepalive = Duration::from_millis(20);
        let ((_reading, mut writing, _), (mut reading, _writing, _)) =
            pair(keepalive, Duration::from_secs(10));
        let packet = protocol::encode_data(&[9; 1000]).unwrap();
        let sending = std::thread::spawn(move || {
            for piece in [&packet[..3], &packet[3..500], &packet[500..]] {
                writing.write_all(piece).unwrap();
                std::thread::sleep(Duration::from_millis(200));
            }
            writing.finish().unwrap();
        });
        let data = protocol::receive(&mut reading).unwrap().unwrap();
        assert_eq!(data.into_payload(), [9; 1000]);
        assert!(protocol::receive(&mut reading).unwrap().is_none());
        sending.join().unwrap();
    });
}

/// A writer that waits for room the other side never makes stops once its
/// stream is taken for broken.
#[test]
fn a_writer_waiting_for_room_stops_once_the_stream_breaks() {
    within(
        "a writer waiting for room stops once the stream breaks",
        PATIENCE,
        || {
            let ((mut reading, mut writing, _), _other) = half(KEEPALIVE, Duration::from_secs(1));
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
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe, "{error}");
            // Why, as the reader found it: the writer may be the first to
            // say so.
            assert!(error.to_string().contains("nothing came"), "{error}");
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
        let (mut reading, _writing, _) = start_with(ends, KEEPALIVE, SILENCE).unwrap();
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
        let error = start_with(ends, KEEPALIVE, SILENCE).err().unwrap();
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
            let (_reading, mut writing, _) = start_with(ends, KEEPALIVE, SILENCE).unwrap();
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
            let (mut reading, _writing, _) =
                start_with(ends, KEEPALIVE, Duration::from_secs(10)).unwrap();
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

/// The keeper ends as soon as its stream is done with, rather than at its
/// next look: as its writer writes the end, and as the writer goes without
/// it.
#[test]
fn the_keeper_ends_as_soon_as_its_stream_is_done_with() {
    within(
        "the keeper ends as soon as its stream is done with",
        PATIENCE,
        || {
            for finished in [true, false] {
                // Its looks 20 s apart.
                let ((reading, writing, carried), _other) =
                    half(Duration::from_secs(80), Duration::from_secs(60));
                drop(reading);
                if finished {
                    writing.finish().unwrap();
                } else {
                    drop(writing);
                }
                // Beside this, only the keeper holds the stream, until it ends.
                let ended = Instant::now();
                while Arc::strong_count(&carried.0) > 1 {
                    assert!(
                        ended.elapsed() < Duration::from_secs(2),
                        "the keeper outlived its stream (finished: {finished})"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
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

/// What comes is handed on as it came, the stream's own packets taken out,
/// however what carries it cuts it, and however much the peer asks for:
/// small packets, empty ones, and large ones, which go past the reader's
/// buffer, straight to the peer.
#[test]
fn the_peers_packets_come_through_as_they_were_however_they_are_cut() {
    let keepalive = [PacketType::Keepalive as u8, 0, 0, 0, 0];
    let packets = [
        protocol::encode_data(&[1; 10]).unwrap(),
        protocol::encode_data(&(0..100_000u32).map(|i| i as u8).collect::<Vec<_>>()).unwrap(),
        protocol::encode_data(&[]).unwrap(),
        protocol::encode_data(&[3; 70_000]).unwrap(),
    ];
    let mut stream = keepalive.to_vec();
    for packet in &packets {
        stream.extend_from_slice(packet);
        stream.extend_from_slice(&keepalive);
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
            after(&[PacketType::Keepalive as u8, 1, 0, 0, 0]),
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
/// keepalive and the half packet in it are read as the rest comes.
#[test]
fn what_was_read_before_the_stream_started_comes_first() {
    within(
        "what was read before the stream started comes first",
        PATIENCE,
        || {
            let (one, mut other) = UnixStream::pair().unwrap();
            let mut before = vec![PacketType::Keepalive as u8, 0, 0, 0, 0];
            let done = protocol::encode(&Control::Done).unwrap();
            let snapshot = protocol::encode(&Control::Snapshot).unwrap();
            before.extend(&done);
            before.extend(&snapshot[..2]);
            let ends = Ends {
                before,
                ..Ends::socket(one).unwrap()
            };
            let (mut reading, _writing, _) = start_with(ends, KEEPALIVE, SILENCE).unwrap();
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

/// A reader held up past the silence the stream is allowed still reads what
/// came meanwhile: the stream is silent only once nothing is there to read,
/// as when the client's display holds up the thread that reads a server's
/// control connection.
#[test]
fn a_reader_held_up_reads_what_came_meanwhile() {
    within(
        "a reader held up reads what came meanwhile",
        PATIENCE,
        || {
            let ((mut reading, _writing, _), mut other) =
                half(KEEPALIVE, Duration::from_millis(300));
            protocol::send(&mut other, &Control::Done).unwrap();
            std::thread::sleep(Duration::from_millis(900));
            assert!(matches!(
                protocol::expect::<Control>(&mut reading).unwrap(),
                Control::Done
            ));
            // Then nothing more: silent.
            let error = failure(&mut reading);
            assert!(error.contains("nothing came"), "{error}");
        },
    );
}

/// A stream let go of stops its reader, waiting, at once, though the
/// silence it is allowed is long, and what carries it cannot be shut
/// down, as a pipe.
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
            let (mut reading, _writing, carried) =
                start_with(ends, KEEPALIVE, Duration::from_secs(30)).unwrap();
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
