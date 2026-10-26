//! What comes on the stream between the two servers, as its reader takes
//! it. Whatever bytes come, however what carries them cuts them, and
//! however much the peer asks for at a time, the peer gets what came with
//! the stream's own packets taken out, up to the stream's end, and the
//! reader says the stream broke where, and why, it must, never panicking.
//! And a peer's packets, written however cut, the writer knowing at each
//! point whether it is between two of them, read back as they were.
#![no_main]

use std::io;

use arbitrary::{Result, Unstructured};
use ferry::protocol::PacketType;
use ferry::transport::stream::{Boundaries, Deframer, Fault, HEADER};
use libfuzzer_sys::fuzz_target;

const END: u8 = PacketType::StreamEnd as u8;

/// The most a stream made from one input holds: past the reader's buffer,
/// several times, and no more, so that each input goes quickly.
const MOST: usize = 256 * 1024;

/// How much at a time: what carries the stream gives, and the peer asks
/// for. Drawn from a small generator the input seeds, at sizes from a byte
/// to more than the reader's buffer.
struct Sizes(u64);

impl Sizes {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// At least one, and at most `most`.
    fn next(&mut self, most: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let drawn = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let scale = [1, 2, 5, 17, 300, 4096, 16_384, 65_536, 200_000][(drawn >> 59) as usize % 9];
        1 + (drawn as usize) % scale.min(most).max(1)
    }
}

/// A header, of a packet of `kind` with `length` bytes of payload.
fn header(kind: u8, length: u32) -> [u8; HEADER] {
    let mut header = [kind; HEADER];
    header[1..].copy_from_slice(&length.to_le_bytes());
    header
}

/// A packet's payload, made cheaply, so that a small input makes a large
/// packet; its bytes differ, so that one out of place shows.
fn payload(length: u32, first: u8) -> impl Iterator<Item = u8> {
    (0..length).map(move |at| first.wrapping_add((at % 251) as u8))
}

/// How a stream ends, as the reader takes it.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// At its end.
    End,
    /// Wrong, or cut short, for this reason.
    Broke(&'static str),
}

/// What the reader must make of `stream`, worked out plainly: what it hands
/// on, and how the stream ends.
fn expected(stream: &[u8]) -> (Vec<u8>, Ended) {
    let mut handed = Vec::new();
    let mut at = 0;
    loop {
        let rest = &stream[at..];
        if rest.len() < HEADER {
            let why = if rest.is_empty() {
                "the stream ended before the other side was done"
            } else {
                "the stream ended inside a packet"
            };
            return (handed, Ended::Broke(why));
        }
        let kind = rest[0];
        let length = u32::from_le_bytes(rest[1..HEADER].try_into().unwrap()) as usize;
        if kind == END {
            if length != 0 {
                return (
                    handed,
                    Ended::Broke("a packet of the stream's own with a payload"),
                );
            }
            return (handed, Ended::End);
        }
        // What came of a packet is handed on as it comes, whether the rest
        // of it does or not.
        let whole = HEADER + length;
        if rest.len() < whole {
            handed.extend_from_slice(rest);
            return (handed, Ended::Broke("the stream ended inside a packet"));
        }
        handed.extend_from_slice(&rest[..whole]);
        at += whole;
    }
}

/// What the reader makes of `stream`, cut as `sizes` says: what it hands on,
/// and how the stream ends.
fn read_through(stream: &[u8], sizes: &mut Sizes) -> (Vec<u8>, Ended) {
    let mut gives = Sizes::new(sizes.0.rotate_left(32));
    let mut at = 0;
    let mut more = |into: &mut [u8]| -> io::Result<usize> {
        assert!(!into.is_empty(), "the reader asked to read into nothing");
        let given = gives.next(into.len()).min(stream.len() - at);
        into[..given].copy_from_slice(&stream[at..at + given]);
        at += given;
        Ok(given)
    };
    let mut deframer = Deframer::default();
    let mut into = vec![0; 200_000];
    let mut handed = Vec::new();
    loop {
        let asks = sizes.next(into.len());
        match deframer.read(&mut into[..asks], &mut more) {
            Ok(0) => {
                // Nothing after the end is read.
                assert_eq!(deframer.read(&mut into[..asks], &mut more).unwrap(), 0);
                return (handed, Ended::End);
            }
            Ok(read) => {
                assert!(read <= asks);
                handed.extend_from_slice(&into[..read]);
            }
            Err(Fault::Wrong(_, why)) => return (handed, Ended::Broke(why)),
            Err(Fault::Read(error)) => panic!("what the stream is read from never fails: {error}"),
        }
    }
}

/// A stream as a peer gone wrong, or ill-willed, may send it: packets of
/// any kind and length, the stream's own among them, with payloads or not,
/// payloads that never come, and bytes of no packet at all.
fn stream(u: &mut Unstructured) -> Result<Vec<u8>> {
    let mut stream = Vec::new();
    while !u.is_empty() && stream.len() < MOST {
        match u.int_in_range(0..=5)? {
            0 => stream.extend_from_slice(&header(END, u.int_in_range(0..=1)?)),
            1 => stream.extend_from_slice(&header(END, 0)),
            2 | 3 => {
                let length = u.int_in_range(0..=100_000)?;
                stream.extend_from_slice(&header(u.arbitrary()?, length));
                stream.extend(payload(length, u.arbitrary()?));
            }
            4 => {
                let length = u.int_in_range(0..=16)?;
                stream.extend_from_slice(u.bytes(length)?);
            }
            _ => stream.extend_from_slice(&header(u.arbitrary()?, u.arbitrary()?)),
        }
    }
    Ok(stream)
}

/// A peer's packets, written however cut, the writer knowing where it is
/// among them, then the end: the reader hands on the packets as they were,
/// then reads the end.
fn round_trip(u: &mut Unstructured, sizes: &mut Sizes) -> Result<()> {
    let mut written = Vec::new();
    // Where each packet ends in what is written.
    let mut ends = vec![0];
    for _ in 0..u.int_in_range(0..=8)? {
        let kind = match u.arbitrary()? {
            END => 0,
            kind => kind,
        };
        let length = u.int_in_range(0..=100_000)?;
        written.extend_from_slice(&header(kind, length));
        written.extend(payload(length, u.arbitrary()?));
        ends.push(written.len());
    }
    let mut boundaries = Boundaries::default();
    let mut wire = Vec::new();
    let mut at = 0;
    loop {
        assert_eq!(
            boundaries.between(),
            ends.contains(&at),
            "between two packets, or not, {at} bytes in"
        );
        if at == written.len() {
            break;
        }
        let piece = &written[at..at + sizes.next(written.len() - at)];
        boundaries.follow(piece);
        wire.extend_from_slice(piece);
        at += piece.len();
    }
    wire.extend_from_slice(&header(END, 0));
    let (handed, ended) = read_through(&wire, sizes);
    assert_eq!(ended, Ended::End);
    assert!(
        handed == written,
        "{} bytes handed on of {} written",
        handed.len(),
        written.len()
    );
    Ok(())
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let (Ok(seed), Ok(written)) = (u.arbitrary::<u64>(), u.arbitrary::<bool>()) else {
        return;
    };
    let mut sizes = Sizes::new(seed);
    if written {
        let _ = round_trip(&mut u, &mut sizes);
        return;
    }
    let Ok(stream) = stream(&mut u) else {
        return;
    };
    let (handed, ended) = read_through(&stream, &mut sizes);
    let (expected, ends) = expected(&stream);
    assert_eq!(ended, ends);
    assert!(
        handed == expected,
        "{} bytes handed on, not the {} expected",
        handed.len(),
        expected.len()
    );
});
