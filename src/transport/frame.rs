//! How the session's frames go on a stream: each is `[kind: u8][length:
//! u32, little-endian][payload]`, as a packet is, its payload the frame's
//! numbers, little-endian, then its bytes, if any. See the draft,
//! `docs/drafts/architecture-redesign.md`, section 5.1.
//!
//! | Kind | Frame | Payload |
//! |---|---|---|
//! | 0 | `Hello` | session (16 bytes), stream (u32) |
//! | 1 | `Control` | seq (u64), ack (u64), the message |
//! | 2 | `Ack` | ack (u64), below (u64), each block (u64) |
//! | 3 | `Fragment` | block (u64), offset (u32), last (0 or 1), the bytes |
//! | 4 | `Keepalive` | nothing |
//! | 5 | `End` | nothing |

use std::io::{self, Read, Write};

use super::session::{Bytes, FRAGMENT, Frame};
use crate::protocol::MAX_PAYLOAD;

/// The largest payload a frame may have: a control message of the largest
/// packet, with its packet's header and the frame's numbers, and room to
/// spare. So a corrupt length cannot make a reader allocate without bound.
const MAX_FRAME: u32 = MAX_PAYLOAD + 64;

const HELLO: u8 = 0;
const CONTROL: u8 = 1;
const ACK: u8 = 2;
const FRAGMENT_KIND: u8 = 3;
const KEEPALIVE: u8 = 4;
const END: u8 = 5;

/// Writes `frame`; its bytes, as a fragment's, are written as they are,
/// rather than copied first.
pub fn write(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    // The kind and length, then the numbers: at most 5 + 20 bytes.
    let mut head = Vec::with_capacity(32);
    let mut bytes: &[u8] = &[];
    let kind = match frame {
        Frame::Hello { session, stream } => {
            head.extend_from_slice(session);
            head.extend_from_slice(&stream.to_le_bytes());
            HELLO
        }
        Frame::Control { seq, ack, message } => {
            head.extend_from_slice(&seq.to_le_bytes());
            head.extend_from_slice(&ack.to_le_bytes());
            bytes = message;
            CONTROL
        }
        Frame::Ack { ack, below, blocks } => {
            head.reserve(8 * blocks.len());
            head.extend_from_slice(&ack.to_le_bytes());
            head.extend_from_slice(&below.to_le_bytes());
            for block in blocks {
                head.extend_from_slice(&block.to_le_bytes());
            }
            ACK
        }
        Frame::Fragment {
            block,
            offset,
            last,
            bytes: fragment,
        } => {
            head.extend_from_slice(&block.to_le_bytes());
            head.extend_from_slice(&offset.to_le_bytes());
            head.push(u8::from(*last));
            bytes = fragment;
            FRAGMENT_KIND
        }
        Frame::Keepalive => KEEPALIVE,
        Frame::End => END,
    };
    let length = u32::try_from(head.len() + bytes.len())
        .ok()
        .filter(|&length| length <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a frame too large"))?;
    let mut start = [0u8; 5];
    start[0] = kind;
    start[1..].copy_from_slice(&length.to_le_bytes());
    writer.write_all(&start)?;
    writer.write_all(&head)?;
    writer.write_all(bytes)
}

/// Why no frame was read.
#[derive(Debug)]
pub enum Unread {
    /// The stream failed, or ended inside a frame, as one whose connection
    /// drops does.
    Failed(String),
    /// What came is not a frame: a fault of whoever wrote it, never of the
    /// network.
    Wrong(String),
}

impl std::fmt::Display for Unread {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Failed(why) | Self::Wrong(why) => formatter.write_str(why),
        }
    }
}

/// Reads the next frame, or `None` if the stream ended before one began.
pub fn read(reader: &mut impl Read) -> Result<Option<Frame>, Unread> {
    let wrong = |why: String| Err(Unread::Wrong(why));
    let mut start = [0u8; 5];
    if !fill(reader, &mut start)? {
        return Ok(None);
    }
    let kind = start[0];
    let length = u32::from_le_bytes(start[1..].try_into().expect("four bytes"));
    // Checked before anything is allocated.
    let (least, most) = match kind {
        HELLO => (20, 20),
        CONTROL | ACK => (16, MAX_FRAME),
        FRAGMENT_KIND => (13, 13 + FRAGMENT as u32),
        KEEPALIVE | END => (0, 0),
        _ => return wrong(format!("a frame of kind {kind}")),
    };
    if !(least..=most).contains(&length) {
        return wrong(format!("a frame of kind {kind} with {length} bytes"));
    }
    let mut payload = vec![0u8; length as usize];
    if !fill(reader, &mut payload)? {
        return Err(cut_short());
    }
    let frame = match kind {
        HELLO => Frame::Hello {
            session: payload[..16].try_into().expect("16 bytes"),
            stream: u32::from_le_bytes(payload[16..].try_into().expect("four bytes")),
        },
        CONTROL => Frame::Control {
            seq: number(&payload[..8]),
            ack: number(&payload[8..16]),
            message: Bytes::new(payload).slice(16..length as usize),
        },
        ACK => {
            if length % 8 != 0 {
                return wrong(format!("an Ack of {length} bytes"));
            }
            Frame::Ack {
                ack: number(&payload[..8]),
                below: number(&payload[8..16]),
                blocks: payload[16..].chunks_exact(8).map(number).collect(),
            }
        }
        FRAGMENT_KIND => Frame::Fragment {
            block: number(&payload[..8]),
            offset: u32::from_le_bytes(payload[8..12].try_into().expect("four bytes")),
            last: match payload[12] {
                0 => false,
                1 => true,
                other => return wrong(format!("a Fragment said last as {other}")),
            },
            bytes: Bytes::new(payload).slice(13..length as usize),
        },
        KEEPALIVE => Frame::Keepalive,
        _ => Frame::End,
    };
    Ok(Some(frame))
}

/// A number of eight bytes, little-endian.
fn number(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

fn cut_short() -> Unread {
    Unread::Failed("the stream ended inside a frame".to_owned())
}

/// Fills `buffer`, or says the stream ended before any of it came.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> Result<bool, Unread> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(cut_short()),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Unread::Failed(format!("read a stream: {error}"))),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(frame: &Frame) -> Vec<u8> {
        let mut bytes = Vec::new();
        write(&mut bytes, frame).unwrap();
        bytes
    }

    fn all_kinds() -> Vec<Frame> {
        vec![
            Frame::Hello {
                session: *b"0123456789abcdef",
                stream: 7,
            },
            Frame::Control {
                seq: 1,
                ack: u64::MAX,
                message: Bytes::new(b"a message".to_vec()),
            },
            Frame::Control {
                seq: 2,
                ack: 0,
                message: Bytes::new(Vec::new()),
            },
            Frame::Ack {
                ack: 3,
                below: 9,
                blocks: vec![10, 12, 1 << 40],
            },
            Frame::Ack {
                ack: 0,
                below: 0,
                blocks: Vec::new(),
            },
            Frame::Fragment {
                block: 5,
                offset: 3 * FRAGMENT as u32,
                last: true,
                bytes: Bytes::new(vec![0xAB; FRAGMENT]),
            },
            Frame::Fragment {
                block: 6,
                offset: 0,
                last: false,
                bytes: Bytes::new(vec![1, 2, 3]),
            },
            Frame::Keepalive,
            Frame::End,
        ]
    }

    /// Each kind of frame is read back as it was written, one after
    /// another on one stream, which then ends where a frame would begin.
    #[test]
    fn frames_are_read_as_written() {
        let frames = all_kinds();
        let mut stream = Vec::new();
        for frame in &frames {
            stream.extend(written(frame));
        }
        let mut reader = &stream[..];
        for frame in &frames {
            assert_eq!(read(&mut reader).unwrap().as_ref(), Some(frame));
        }
        assert!(read(&mut reader).unwrap().is_none());
    }

    /// A stream that ends inside a frame, in its start or its payload, is
    /// not taken for one that ended between frames.
    #[test]
    fn a_frame_cut_short_is_an_error() {
        for frame in all_kinds() {
            let bytes = written(&frame);
            for cut in 1..bytes.len() {
                let error = read(&mut &bytes[..cut]).unwrap_err();
                assert!(matches!(error, Unread::Failed(_)), "{error}");
                assert!(error.to_string().contains("ended inside"), "{error}");
            }
        }
    }

    /// What is not a frame is refused, rather than taken for some other.
    #[test]
    fn what_is_not_a_frame_is_refused() {
        let wrong = |bytes: Vec<u8>, said: &str| {
            let error = read(&mut &bytes[..]).unwrap_err();
            assert!(matches!(error, Unread::Wrong(_)), "{error}");
            assert!(error.to_string().contains(said), "{error}, not {said}");
        };
        let frame = |kind: u8, payload: &[u8]| {
            let mut bytes = vec![kind];
            bytes.extend((payload.len() as u32).to_le_bytes());
            bytes.extend(payload);
            bytes
        };
        wrong(frame(6, &[]), "kind 6");
        wrong(frame(HELLO, &[0; 19]), "kind 0 with 19 bytes");
        wrong(frame(HELLO, &[0; 21]), "kind 0 with 21 bytes");
        wrong(frame(CONTROL, &[0; 15]), "kind 1 with 15 bytes");
        wrong(frame(ACK, &[0; 15]), "kind 2 with 15 bytes");
        wrong(frame(ACK, &[0; 20]), "an Ack of 20 bytes");
        wrong(frame(FRAGMENT_KIND, &[0; 12]), "kind 3 with 12 bytes");
        let mut said_last = vec![0; 13];
        said_last[12] = 2;
        wrong(frame(FRAGMENT_KIND, &said_last), "said last as 2");
        wrong(
            frame(FRAGMENT_KIND, &vec![0; 13 + FRAGMENT + 1]),
            &format!("kind 3 with {} bytes", 13 + FRAGMENT + 1),
        );
        wrong(frame(KEEPALIVE, &[0]), "kind 4 with 1 bytes");
        wrong(frame(END, &[0; 2]), "kind 5 with 2 bytes");
        // A length beyond any frame's, refused before it is allocated: the
        // payload is not there.
        let mut huge = vec![CONTROL];
        huge.extend((MAX_FRAME + 1).to_le_bytes());
        wrong(huge, &format!("kind 1 with {} bytes", MAX_FRAME + 1));
    }

    /// A control message of the largest packet goes in a frame; one too
    /// large to read is not written.
    #[test]
    fn a_frame_too_large_is_not_written() {
        let control = |len: usize| Frame::Control {
            seq: 1,
            ack: 0,
            message: Bytes::new(vec![0; len]),
        };
        let largest = control(5 + MAX_PAYLOAD as usize);
        let mut bytes = Vec::new();
        write(&mut bytes, &largest).unwrap();
        assert_eq!(read(&mut &bytes[..]).unwrap(), Some(largest));
        let mut bytes = Vec::new();
        assert!(write(&mut bytes, &control(MAX_FRAME as usize)).is_err());
        assert!(bytes.is_empty());
    }
}
