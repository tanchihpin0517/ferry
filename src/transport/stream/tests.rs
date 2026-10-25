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
