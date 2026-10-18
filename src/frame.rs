//! Frames: pieces of the record stream, each compressed on its own.
//!
//! The Source cuts the record stream into frames of at most [`FRAME`] bytes,
//! without regard for where records begin or end, and compresses each with
//! zstd. A frame that does not shrink is sent as it is, so already compressed
//! files cost no CPU twice. A frame is
//!
//! ```text
//! [kind: u8][raw length: u32][body length: u32][body]
//! ```
//!
//! where the kind says whether the body is raw or zstd. Frames are carried in
//! data packets of their own size, which split a frame wherever they like; an
//! [`Assembler`] puts frames back together.

use anyhow::{Context, Result, bail};

/// The most record-stream bytes in one frame. Beyond about 1 MiB a larger
/// frame barely compresses better, and memory grows with it.
pub const FRAME: usize = 2 * 1024 * 1024;

const RAW: u8 = 0;
const ZSTD: u8 = 1;

/// Encodes `raw` as a frame, compressed at `level` unless `compress` is off or
/// compressing does not shrink it.
pub fn encode(raw: &[u8], compress: bool, level: i32) -> Result<Vec<u8>> {
    crate::inject::fail_if("frame-encode")?;
    let mut frame = Vec::with_capacity(5 + raw.len());
    let compressed = if compress {
        Some(zstd::bulk::compress(raw, level).context("compress a frame")?)
    } else {
        None
    };
    let (kind, body) = match &compressed {
        Some(compressed) if compressed.len() < raw.len() => (ZSTD, compressed.as_slice()),
        _ => (RAW, raw),
    };
    frame.push(kind);
    frame.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body);
    Ok(frame)
}

const HEADER: usize = 9;

/// Puts frames back together from the pieces data packets carry.
#[derive(Default)]
pub struct Assembler {
    buffer: Vec<u8>,
}

impl Assembler {
    /// Takes the next piece, and returns every frame it completes, still
    /// encoded, so that they can be decoded in parallel.
    pub fn push(&mut self, piece: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(piece);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < HEADER {
                break;
            }
            let body =
                u32::from_le_bytes(self.buffer[5..9].try_into().expect("four bytes")) as usize;
            if body > FRAME + FRAME / 8 + 1024 {
                bail!("a frame body of {body} bytes exceeds the limit");
            }
            if self.buffer.len() < HEADER + body {
                break;
            }
            let rest = self.buffer.split_off(HEADER + body);
            let frame = std::mem::replace(&mut self.buffer, rest);
            frames.push(frame);
        }
        Ok(frames)
    }
}

/// Decodes a frame back to the record-stream bytes it holds.
pub fn decode(frame: &[u8]) -> Result<Vec<u8>> {
    if frame.len() < HEADER {
        bail!("a frame of {} bytes is too short", frame.len());
    }
    let raw_length = u32::from_le_bytes(frame[1..5].try_into().expect("four bytes")) as usize;
    if raw_length > FRAME {
        bail!("a frame of {raw_length} bytes exceeds the limit");
    }
    let body_length = u32::from_le_bytes(frame[5..9].try_into().expect("four bytes")) as usize;
    let body = &frame[HEADER..];
    if body.len() != body_length {
        bail!("a frame body of {} bytes, not {body_length}", body.len());
    }
    let raw = match frame[0] {
        RAW => body.to_vec(),
        ZSTD => zstd::bulk::decompress(body, raw_length).context("decompress a frame")?,
        kind => bail!("unknown frame kind {kind}"),
    };
    if raw.len() != raw_length {
        bail!("a frame held {} bytes, not {raw_length}", raw.len());
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressible_data_shrinks_and_round_trips() {
        let raw = vec![b'a'; 100_000];
        let frame = encode(&raw, true, 3).unwrap();
        assert_eq!(frame[0], ZSTD);
        assert!(frame.len() < 1_000);
        assert_eq!(decode(&frame).unwrap(), raw);
    }

    #[test]
    fn incompressible_data_is_sent_as_is() {
        // Bytes that zstd cannot shrink: a counter scrambled by a hash.
        let raw: Vec<u8> = (0..10_000u32)
            .flat_map(|i| *blake3::hash(&i.to_le_bytes()).as_bytes())
            .collect();
        let frame = encode(&raw, true, 3).unwrap();
        assert_eq!(frame[0], RAW);
        assert_eq!(decode(&frame).unwrap(), raw);
    }

    #[test]
    fn without_compression_frames_are_raw() {
        let raw = vec![b'a'; 1_000];
        let frame = encode(&raw, false, 3).unwrap();
        assert_eq!(frame[0], RAW);
        assert_eq!(decode(&frame).unwrap(), raw);
    }

    #[test]
    fn corrupt_frames_are_refused() {
        assert!(decode(&[ZSTD, 10, 0, 0, 0, 3, 0, 0, 0, 1, 2, 3]).is_err());
        assert!(decode(&[9, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(decode(&[RAW]).is_err());
        let mut oversized = vec![RAW];
        oversized.extend_from_slice(&(FRAME as u32 + 1).to_le_bytes());
        oversized.extend_from_slice(&0u32.to_le_bytes());
        assert!(decode(&oversized).is_err());
    }

    #[test]
    fn frames_split_anywhere_are_put_back_together() {
        let first = vec![b'a'; 50_000];
        let second: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
        let mut stream = encode(&first, true, 3).unwrap();
        stream.extend(encode(&second, false, 3).unwrap());
        let mut assembler = Assembler::default();
        let mut frames = Vec::new();
        for piece in stream.chunks(7) {
            for frame in assembler.push(piece).unwrap() {
                frames.push(decode(&frame).unwrap());
            }
        }
        assert_eq!(frames, [first, second]);
    }
}
