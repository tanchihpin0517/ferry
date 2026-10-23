//! Blocks: what the Source sends of the files the Destination wants.
//!
//! Each block describes itself, so that any thread can decode it, check it
//! and write what it holds, whatever came before it, and it can be sent
//! again alone. A block is a pack, of whole files of at most [`SEGMENT`]
//! bytes, or a segment: [`SEGMENT`] bytes of a larger file, from a multiple
//! of it, or what is left of the file there. Each is
//!
//! ```text
//! [header length: u32][payload length: u32][header][payload]
//! ```
//!
//! The header is postcard-encoded, and says what the payload holds: a pack's
//! files' bytes one after another, or a segment's. The payload is compressed
//! with zstd on its own, or sent as it is where that would not shrink it.
//!
//! A pack carries each file's BLAKE3 hash, and a segment the chaining value
//! of its part of its file's. BLAKE3 hashes a tree, of which a segment,
//! aligned and a power of two long, is a subtree: so a segment is checked
//! on its own, by [`chaining_value`], and its file's hash, the same as of
//! all its bytes at once, is merged from its segments' by [`root`].

use anyhow::{Context, Result, bail};
use blake3::hazmat::{
    ChainingValue, HasherExt, Mode, left_subtree_len, merge_subtrees_non_root, merge_subtrees_root,
};
use serde::{Deserialize, Serialize};

use crate::manifest::{FileId, Timestamp};

/// The most a pack holds, its files and their entries in its header
/// together, unless it holds one file alone, which may be a segment's
/// size.
///
/// Measured on crate sources, a build's output, `/usr`, a tar of sources and
/// large files of model weights, numbers and zip, each block compressed
/// alone: at zstd 1, blocks of 8 MiB send 0.2 to 1.1% less than of 2 MiB,
/// at zstd 3 1.5 to 9% less, and at zstd 9 7% less on the tar; 16 MiB sends
/// at most 1.4% less again. The large files, which compress little, are the
/// same at any size. A larger block costs memory, a block's worth for each
/// thread that reads or compresses one, and most decompress more slowly the
/// larger they are, if at about 600 MB/s a core or more at 8 MiB.
pub const PACK: usize = 8 * 1024 * 1024;

/// The size of a segment: a power of two, as BLAKE3's tree needs, and the
/// largest file a pack takes. The same as a pack's, for the same reasons: a
/// large file that compresses well, as the tar, gains from larger segments
/// as small files do from larger packs.
pub const SEGMENT: u64 = 8 * 1024 * 1024;

/// The most a block's header and payload hold together, decoded: a pack's
/// worth, and room for a long reason a file could not be read.
pub const MOST: usize = PACK + 64 * 1024;

const PREFIX: usize = 8;

/// What a block holds, and how its payload is encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub codec: Codec,
    /// The payload's length, decoded.
    pub raw_len: u32,
    pub body: Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    Raw,
    Zstd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    /// Whole files, their bytes one after another, in this order.
    Pack {
        files: Vec<PackFile>,
    },
    Segment(Segment),
}

/// A file in a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFile {
    pub file: FileId,
    /// How many of the payload's bytes are the file's: its size as the
    /// Source found it when it opened it, or none, if it could not read it
    /// whole.
    pub len: u32,
    /// Its mtime as the Source found it when it opened it.
    pub mtime: Timestamp,
    /// The BLAKE3 hash of its bytes.
    pub check: Check,
}

/// A segment of a file larger than [`SEGMENT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub file: FileId,
    /// Where in the file it begins: a multiple of [`SEGMENT`].
    pub offset: u64,
    /// How many bytes: [`SEGMENT`], or what is left of the file from the
    /// offset; or none, if the Source could not read them whole.
    pub len: u32,
    /// The file's size and mtime as the Source found them when it opened
    /// it: what it gains after is not read, and left for the next run,
    /// which finds its mtime changed.
    pub size: u64,
    pub mtime: Timestamp,
    /// The chaining value of its bytes.
    pub check: Check,
}

/// What the Destination checks bytes against, or why there are none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Check {
    /// A pack file's BLAKE3 hash, or a segment's chaining value.
    Hash([u8; 32]),
    /// Why the Source could not read them whole: the file shrank, or a read
    /// failed. None are sent, and the Destination fails the file.
    Unread(String),
}

impl Body {
    /// Refuses a body that cannot hold a payload of `raw_len` bytes.
    fn holds(&self, raw_len: usize) -> Result<()> {
        match self {
            Body::Pack { files } => {
                if files.is_empty() {
                    bail!("a pack of no files");
                }
                let mut total = 0;
                for file in files {
                    if u64::from(file.len) > SEGMENT {
                        bail!("a packed file of {} bytes", file.len);
                    }
                    if file.len > 0 && matches!(file.check, Check::Unread(_)) {
                        bail!("an unread file of {} bytes", file.len);
                    }
                    total += u64::from(file.len);
                }
                if total != raw_len as u64 {
                    bail!("a pack's files of {total} bytes, in {raw_len}");
                }
            }
            Body::Segment(segment) => {
                if segment.size <= SEGMENT
                    || segment.offset % SEGMENT != 0
                    || segment.offset >= segment.size
                {
                    bail!(
                        "a segment at {} of a file of {} bytes",
                        segment.offset,
                        segment.size
                    );
                }
                let len = match segment.check {
                    Check::Hash(_) => SEGMENT.min(segment.size - segment.offset),
                    Check::Unread(_) => 0,
                };
                if u64::from(segment.len) != len || len != raw_len as u64 {
                    bail!(
                        "a segment of {} bytes, in {raw_len}, not {len}",
                        segment.len
                    );
                }
            }
        }
        Ok(())
    }
}

/// Encodes a block of `body` and `payload`, the bytes it says it holds,
/// compressed at `level` unless `compress` is off or compressing would not
/// shrink them.
pub fn encode(body: Body, payload: &[u8], compress: bool, level: i32) -> Result<Vec<u8>> {
    body.holds(payload.len())?;
    let compressed = if compress {
        Some(zstd::bulk::compress(payload, level).context("compress a block")?)
    } else {
        None
    };
    let (codec, bytes) = match &compressed {
        Some(compressed) if compressed.len() < payload.len() => {
            (Codec::Zstd, compressed.as_slice())
        }
        _ => (Codec::Raw, payload),
    };
    let header = postcard::to_stdvec(&Header {
        codec,
        raw_len: payload.len() as u32,
        body,
    })
    .context("encode a block")?;
    if header.len() + payload.len() > MOST {
        bail!(
            "a block of {} bytes and a header of {} is too large",
            payload.len(),
            header.len()
        );
    }
    let mut block = Vec::with_capacity(PREFIX + header.len() + bytes.len());
    block.extend_from_slice(&(header.len() as u32).to_le_bytes());
    block.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    block.extend_from_slice(&header);
    block.extend_from_slice(bytes);
    Ok(block)
}

/// A block, decoded: what it holds, and its payload as it was encoded.
#[derive(Debug, PartialEq, Eq)]
pub struct Block {
    pub body: Body,
    pub payload: Vec<u8>,
}

/// Decodes a block, refusing one that [`encode`] would not have made.
pub fn decode(block: &[u8]) -> Result<Block> {
    if block.len() < PREFIX {
        bail!("a block of {} bytes is too short", block.len());
    }
    let header_len = u32::from_le_bytes(block[..4].try_into().expect("four bytes")) as usize;
    let payload_len = u32::from_le_bytes(block[4..8].try_into().expect("four bytes")) as usize;
    if header_len > MOST || block.len() - PREFIX != header_len + payload_len {
        bail!(
            "a block of {} bytes, with a header of {header_len} and a payload of {payload_len}",
            block.len()
        );
    }
    let (header, rest) = postcard::take_from_bytes::<Header>(&block[PREFIX..PREFIX + header_len])
        .context("decode a block's header")?;
    if !rest.is_empty() {
        bail!("a block's header is followed by {} bytes", rest.len());
    }
    let raw_len = header.raw_len as usize;
    if header_len + raw_len > MOST {
        bail!("a block of {raw_len} bytes and a header of {header_len} is too large");
    }
    header.body.holds(raw_len)?;
    let bytes = &block[PREFIX + header_len..];
    let payload = match header.codec {
        Codec::Raw if bytes.len() == raw_len => bytes.to_vec(),
        Codec::Zstd if bytes.len() < raw_len => {
            zstd::bulk::decompress(bytes, raw_len).context("decompress a block")?
        }
        codec => bail!(
            "a payload of {} bytes, {codec:?}, of {raw_len}",
            bytes.len()
        ),
    };
    if payload.len() != raw_len {
        bail!("a payload of {} bytes, not {raw_len}", payload.len());
    }
    Ok(Block {
        body: header.body,
        payload,
    })
}

/// What a block holds of one file.
#[derive(Debug, PartialEq, Eq)]
pub struct Part<'a> {
    pub file: FileId,
    /// Where in the file its bytes go.
    pub offset: u64,
    /// The file's size and mtime as the Source found them when it opened
    /// it: a pack's file's size is its length in the pack, none where it
    /// could not be read.
    pub size: u64,
    pub mtime: Timestamp,
    /// Its bytes, if they are those the Source read; else why not.
    pub bytes: Result<&'a [u8], String>,
    /// A segment's chaining value, once its bytes are found to match it,
    /// for [`root`] to merge with its file's others.
    pub chaining_value: Option<[u8; 32]>,
}

impl Block {
    /// What the block holds of each file, in order, each checked against
    /// what the Source made of its bytes.
    pub fn parts(&self) -> Vec<Part<'_>> {
        let unread = |why: &str| format!("the Source could not read it whole: {why}");
        let mismatch = || "the content's hash does not match the Source's".to_owned();
        match &self.body {
            Body::Pack { files } => {
                let mut start = 0;
                files
                    .iter()
                    .map(|file| {
                        let bytes = &self.payload[start..start + file.len as usize];
                        start += file.len as usize;
                        Part {
                            file: file.file,
                            offset: 0,
                            size: u64::from(file.len),
                            mtime: file.mtime,
                            bytes: match &file.check {
                                Check::Unread(why) => Err(unread(why)),
                                Check::Hash(hash) if blake3::hash(bytes) == *hash => Ok(bytes),
                                Check::Hash(_) => Err(mismatch()),
                            },
                            chaining_value: None,
                        }
                    })
                    .collect()
            }
            Body::Segment(segment) => {
                let (bytes, value) = match &segment.check {
                    Check::Unread(why) => (Err(unread(why)), None),
                    Check::Hash(value)
                        if chaining_value(segment.offset, &self.payload) == *value =>
                    {
                        (Ok(self.payload.as_slice()), Some(*value))
                    }
                    Check::Hash(_) => (Err(mismatch()), None),
                };
                vec![Part {
                    file: segment.file,
                    offset: segment.offset,
                    size: segment.size,
                    mtime: segment.mtime,
                    bytes,
                    chaining_value: value,
                }]
            }
        }
    }
}

/// How many segments a file of `size` bytes, larger than [`SEGMENT`], is
/// sent in.
pub fn segments(size: u64) -> u64 {
    size.div_ceil(SEGMENT)
}

/// The chaining value of a file's bytes from `offset`, a multiple of
/// [`SEGMENT`]: [`SEGMENT`] of them, or the rest of the file, and at least
/// one, as [`decode`] makes sure of a segment.
pub fn chaining_value(offset: u64, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.set_input_offset(offset);
    hasher.update(bytes);
    hasher.finalize_non_root()
}

/// The BLAKE3 hash of a file of `size` bytes, larger than [`SEGMENT`],
/// merged from the chaining values of its segments, in order: the same as
/// the hash of all its bytes at once.
pub fn root(values: &[[u8; 32]], size: u64) -> [u8; 32] {
    root_of(values, size, SEGMENT)
}

/// [`root`], of segments of `segment` bytes, a power of two of at least
/// BLAKE3's chunk.
fn root_of(values: &[[u8; 32]], size: u64, segment: u64) -> [u8; 32] {
    assert!(
        size > segment && values.len() as u64 == size.div_ceil(segment),
        "{} chaining values for {size} bytes",
        values.len()
    );
    // The tree's left side is the largest power of two shorter than the
    // file: so whole segments, as are the left sides below it.
    let left = left_subtree_len(size);
    let split = (left / segment) as usize;
    let hash = merge_subtrees_root(
        &subtree(&values[..split], left, segment),
        &subtree(&values[split..], size - left, segment),
        Mode::Hash,
    );
    *hash.as_bytes()
}

/// The chaining value of the subtree of `len` bytes whose segments have
/// `values`.
fn subtree(values: &[[u8; 32]], len: u64, segment: u64) -> ChainingValue {
    if len <= segment {
        return values[0];
    }
    let left = left_subtree_len(len);
    let split = (left / segment) as usize;
    merge_subtrees_non_root(
        &subtree(&values[..split], left, segment),
        &subtree(&values[split..], len - left, segment),
        Mode::Hash,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Random;

    const MTIME: Timestamp = Timestamp {
        seconds: 1_700_000_000,
        nanoseconds: 5,
    };

    /// Each segment's chaining value, as a reader would make it, merged.
    fn segmented(bytes: &[u8], segment: u64) -> [u8; 32] {
        let values: Vec<[u8; 32]> = bytes
            .chunks(segment as usize)
            .enumerate()
            .map(|(index, part)| chaining_value(index as u64 * segment, part))
            .collect();
        root_of(&values, bytes.len() as u64, segment)
    }

    /// Of segments of any size, a file of any length larger than one, the
    /// last segment as short as a byte too, hashes as it does at once.
    #[test]
    fn a_hash_merged_from_segments_is_the_files() {
        let mut random = Random::new(3);
        let bytes = random.bytes(12 * 65536 + 4096);
        for segment in [1024, 4096, 65536] {
            let mut lengths = vec![
                segment + 1,
                segment + 17,
                2 * segment - 1,
                2 * segment,
                2 * segment + 1,
                3 * segment,
                4 * segment + 1,
                5 * segment + 17,
                8 * segment,
                8 * segment + 1,
                11 * segment + 513,
                12 * segment,
            ];
            lengths.extend((0..12).map(|_| segment + 1 + random.below(11 * segment)));
            for length in lengths {
                let file = &bytes[..length as usize];
                assert_eq!(
                    segmented(file, segment),
                    *blake3::hash(file).as_bytes(),
                    "{length} bytes in segments of {segment}"
                );
            }
        }
    }

    /// The same, in segments of the size sent, as the draft's figure has
    /// it: 27 MiB in four.
    #[test]
    fn a_hash_merged_from_segments_of_the_size_sent_is_the_files() {
        let bytes = Random::new(5).bytes(27 << 20);
        for length in [SEGMENT + 1, 2 * SEGMENT, 27 << 20] {
            let file = &bytes[..length as usize];
            let values: Vec<[u8; 32]> = file
                .chunks(SEGMENT as usize)
                .enumerate()
                .map(|(index, part)| chaining_value(index as u64 * SEGMENT, part))
                .collect();
            assert_eq!(values.len() as u64, segments(length));
            assert_eq!(root(&values, length), *blake3::hash(file).as_bytes());
        }
    }

    /// A byte changed anywhere changes its segment's chaining value, and the
    /// hash merged.
    #[test]
    fn a_changed_byte_changes_the_hash() {
        let mut bytes = Random::new(9).bytes(5 * 4096 + 100);
        let whole = segmented(&bytes, 4096);
        for at in [0, 4095, 4096, 3 * 4096 + 7, bytes.len() - 1] {
            bytes[at] ^= 1;
            assert_ne!(segmented(&bytes, 4096), whole, "{at}");
            bytes[at] ^= 1;
        }
        assert_eq!(segmented(&bytes, 4096), whole);
    }

    fn pack_of(files: &[(&[u8], Option<&str>)]) -> (Body, Vec<u8>) {
        let mut payload = Vec::new();
        let files = files
            .iter()
            .enumerate()
            .map(|(id, (bytes, unread))| {
                let check = match unread {
                    Some(why) => Check::Unread(why.to_string()),
                    None => {
                        payload.extend_from_slice(bytes);
                        Check::Hash(*blake3::hash(bytes).as_bytes())
                    }
                };
                PackFile {
                    file: id as FileId + 10,
                    len: if unread.is_some() {
                        0
                    } else {
                        bytes.len() as u32
                    },
                    mtime: MTIME,
                    check,
                }
            })
            .collect();
        (Body::Pack { files }, payload)
    }

    fn segment_of(file: &[u8], offset: u64) -> (Body, Vec<u8>) {
        let end = (offset + SEGMENT).min(file.len() as u64);
        let bytes = file[offset as usize..end as usize].to_vec();
        let body = Body::Segment(Segment {
            file: 7,
            offset,
            len: bytes.len() as u32,
            size: file.len() as u64,
            mtime: MTIME,
            check: Check::Hash(chaining_value(offset, &bytes)),
        });
        (body, bytes)
    }

    /// Any pack or segment, compressed or not, comes back as it was, every
    /// file in it checked and as it was read.
    #[test]
    fn any_block_round_trips() {
        let mut random = Random::new(13);
        let large = random.bytes(2 * SEGMENT as usize + 12345);
        for turn in 0..200 {
            let compress = random.below(2) == 0;
            let (body, payload) = if turn % 5 == 4 {
                let offset = random.below(3) * SEGMENT;
                segment_of(&large, offset)
            } else {
                let count = 1 + random.below(20) as usize;
                let contents: Vec<Vec<u8>> = (0..count)
                    .map(|_| {
                        let length = match random.below(4) {
                            0 => 0,
                            1 => random.below(100),
                            2 => random.below(70_000),
                            _ => random.below(300_000),
                        };
                        random.bytes(length as usize)
                    })
                    .collect();
                let files: Vec<(&[u8], Option<&str>)> = contents
                    .iter()
                    .map(|bytes| {
                        let unread = (random.below(8) == 0).then_some("it shrank");
                        (bytes.as_slice(), unread)
                    })
                    .collect();
                pack_of(&files)
            };
            let block = encode(body.clone(), &payload, compress, 1).unwrap();
            let decoded = decode(&block).unwrap();
            assert_eq!(decoded.body, body, "{turn}");
            assert_eq!(decoded.payload, payload, "{turn}");
            let mut start = 0;
            for part in decoded.parts() {
                match part.bytes {
                    Ok(bytes) => {
                        assert_eq!(bytes, &payload[start..start + bytes.len()]);
                        start += bytes.len();
                    }
                    Err(why) => assert_eq!(
                        why, "the Source could not read it whole: it shrank",
                        "{turn}"
                    ),
                }
            }
            assert_eq!(start, payload.len(), "{turn}");
        }
    }

    #[test]
    fn compressible_payloads_shrink_and_others_go_as_they_are() {
        let text = vec![b'a'; 100_000];
        let (body, payload) = pack_of(&[(&text, None)]);
        let block = encode(body.clone(), &payload, true, 1).unwrap();
        assert!(block.len() < 1_000);
        assert_eq!(decode(&block).unwrap().payload, text);
        assert!(encode(body, &payload, false, 1).unwrap().len() > 100_000);

        // Bytes that zstd cannot shrink: a counter scrambled by a hash.
        let noise: Vec<u8> = (0..8000u32)
            .flat_map(|i| *blake3::hash(&i.to_le_bytes()).as_bytes())
            .collect();
        let (body, payload) = pack_of(&[(&noise, None)]);
        let block = encode(body, &payload, true, 1).unwrap();
        assert!(block.len() > noise.len());
        let header_len = u32::from_le_bytes(block[..4].try_into().unwrap()) as usize;
        let header: Header = postcard::from_bytes(&block[8..8 + header_len]).unwrap();
        assert_eq!(header.codec, Codec::Raw);
        assert_eq!(decode(&block).unwrap().payload, noise);
    }

    /// A file whose bytes are not those the Source hashed fails alone; a
    /// segment's, the segment.
    #[test]
    fn bytes_not_as_read_are_found() {
        let (body, mut payload) = pack_of(&[(b"first", None), (b"second", None), (b"", None)]);
        payload[7] ^= 1;
        let block = decode(&encode(body, &payload, true, 1).unwrap()).unwrap();
        let parts = block.parts();
        assert_eq!(parts[0].bytes, Ok(&b"first"[..]));
        assert_eq!(
            parts[1].bytes,
            Err("the content's hash does not match the Source's".to_owned())
        );
        assert_eq!(parts[2].bytes, Ok(&b""[..]));

        let file = Random::new(19).bytes(SEGMENT as usize + 100);
        let (body, mut payload) = segment_of(&file, SEGMENT);
        let good = decode(&encode(body.clone(), &payload, false, 1).unwrap()).unwrap();
        let Body::Segment(Segment {
            check: Check::Hash(value),
            ..
        }) = body
        else {
            unreachable!()
        };
        assert_eq!(good.parts()[0].chaining_value, Some(value));
        assert_eq!(good.parts()[0].offset, SEGMENT);
        payload[99] ^= 1;
        let bad = decode(&encode(body, &payload, false, 1).unwrap()).unwrap();
        assert!(bad.parts()[0].bytes.is_err());
        assert_eq!(bad.parts()[0].chaining_value, None);
    }

    /// The same chaining value at another offset is not the segment's.
    #[test]
    fn a_segment_is_checked_where_it_goes() {
        let file = vec![b'x'; 3 * SEGMENT as usize];
        let (body, payload) = segment_of(&file, 0);
        let Body::Segment(segment) = body else {
            unreachable!()
        };
        let moved = Body::Segment(Segment {
            offset: SEGMENT,
            ..segment
        });
        let block = decode(&encode(moved, &payload, true, 1).unwrap()).unwrap();
        assert!(block.parts()[0].bytes.is_err());
    }

    /// Bodies that do not hold their payload, and blocks cut, padded or
    /// otherwise not as encoded, are refused, never decoded.
    #[test]
    fn blocks_not_as_encoded_are_refused() {
        let (body, payload) = pack_of(&[(b"some bytes", None), (b"more", None)]);
        let good = encode(body.clone(), &payload, false, 1).unwrap();
        assert!(decode(&good).is_ok());
        assert!(decode(&good[..good.len() - 1]).is_err());
        assert!(decode(&[good.as_slice(), &[0]].concat()).is_err());
        assert!(decode(&good[..7]).is_err());

        let header = |codec, raw_len, body: &Body| {
            postcard::to_stdvec(&Header {
                codec,
                raw_len,
                body: body.clone(),
            })
            .unwrap()
        };
        let block = |header: &[u8], payload: &[u8]| {
            let mut block = (header.len() as u32).to_le_bytes().to_vec();
            block.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            block.extend_from_slice(header);
            block.extend_from_slice(payload);
            block
        };
        assert!(decode(&block(&header(Codec::Raw, 14, &body), &payload)).is_ok());
        // A header followed by a byte it does not hold.
        let padded = [header(Codec::Raw, 14, &body), vec![0]].concat();
        assert!(decode(&block(&padded, &payload)).is_err());
        // Files that do not add up to the payload.
        assert!(decode(&block(&header(Codec::Raw, 13, &body), &payload[..13])).is_err());
        // A compressed payload that does not shrink, or decodes to more.
        assert!(decode(&block(&header(Codec::Zstd, 14, &body), &payload)).is_err());
        let squeezed = zstd::bulk::compress(&[b'a'; 1000], 1).unwrap();
        let thousand = pack_of(&[(&[b'a'; 1000], None)]).0;
        assert!(decode(&block(&header(Codec::Zstd, 1000, &thousand), &squeezed)).is_ok());
        let hundred = pack_of(&[(&[b'a'; 100], None)]).0;
        assert!(decode(&block(&header(Codec::Zstd, 100, &hundred), &squeezed)).is_err());
        // A pack of nothing, of an unread file with bytes, of a file larger
        // than a segment, and one too large.
        assert!(decode(&block(&header(Codec::Raw, 0, &pack_of(&[]).0), &[])).is_err());
        let unread = Body::Pack {
            files: vec![PackFile {
                file: 1,
                len: 4,
                mtime: MTIME,
                check: Check::Unread("gone".into()),
            }],
        };
        assert!(decode(&block(&header(Codec::Raw, 4, &unread), b"abcd")).is_err());
        let huge = Body::Pack {
            files: vec![PackFile {
                file: 1,
                len: SEGMENT as u32 + 1,
                mtime: MTIME,
                check: Check::Hash([0; 32]),
            }],
        };
        assert!(encode(huge, &vec![0; SEGMENT as usize + 1], false, 1).is_err());
        let full = pack_of(&[(&vec![1; PACK - 100], None), (&vec![2; 70_000], None)]);
        assert!(encode(full.0, &full.1, false, 1).is_err());
    }

    /// Segments only where a file's segments are: aligned, of the length
    /// left there, of a file larger than one.
    #[test]
    fn segments_out_of_place_are_refused() {
        let segment = |offset: u64, len: u64, size: u64, unread: bool| {
            let body = Body::Segment(Segment {
                file: 3,
                offset,
                len: len as u32,
                size,
                mtime: MTIME,
                check: if unread {
                    Check::Unread("gone".into())
                } else {
                    Check::Hash([0; 32])
                },
            });
            let payload = vec![0; len as usize];
            let header = postcard::to_stdvec(&Header {
                codec: Codec::Raw,
                raw_len: len as u32,
                body,
            })
            .unwrap();
            let mut block = (header.len() as u32).to_le_bytes().to_vec();
            block.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            block.extend_from_slice(&header);
            block.extend_from_slice(&payload);
            decode(&block)
        };
        let size = 2 * SEGMENT + 10;
        assert!(segment(0, SEGMENT, size, false).is_ok());
        assert!(segment(2 * SEGMENT, 10, size, false).is_ok());
        assert!(segment(SEGMENT, 0, size, true).is_ok());
        assert!(segment(SEGMENT, SEGMENT, size, true).is_err());
        assert!(segment(2 * SEGMENT, 9, size, false).is_err());
        assert!(segment(SEGMENT, SEGMENT - 1, size, false).is_err());
        assert!(segment(1024, SEGMENT, size, false).is_err());
        assert!(segment(3 * SEGMENT, 10, size, false).is_err());
        assert!(segment(0, SEGMENT, SEGMENT, false).is_err());
        assert!(segment(0, 0, size, false).is_err());
    }

    /// Whatever bytes come, decoding a block and checking its parts never
    /// panics.
    #[test]
    fn any_bytes_are_refused_or_decoded() {
        let mut random = Random::new(23);
        let (body, payload) = pack_of(&[(b"some bytes", None), (b"more bytes", None)]);
        let good = encode(body, &payload, true, 1).unwrap();
        for _ in 0..20_000 {
            let mut bytes = good.clone();
            for _ in 0..1 + random.below(4) {
                let at = random.below(bytes.len() as u64) as usize;
                bytes[at] = random.next() as u8;
            }
            bytes.truncate(random.below(bytes.len() as u64 + 1) as usize);
            if let Ok(block) = decode(&bytes) {
                drop(block.parts());
            }
            let length = random.below(64) as usize;
            let noise = random.bytes(length);
            if let Ok(block) = decode(&noise) {
                drop(block.parts());
            }
        }
    }
}
