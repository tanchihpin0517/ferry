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
//! all its bytes at once, is merged from its segments' by [`root`], and
//! sent apart, once all are read.
//!
//! The blocks of a transfer go one after another in data packets, which cut
//! them wherever they like: an [`Assembler`] puts them back together, and
//! [`END`] follows the last.

use std::ops::{Deref, Range};
use std::sync::Arc;

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
/// alone: blocks of 2 MiB send as much as the frames before them, 2 MiB of
/// a stream of files, did. Blocks of 8 MiB send 0.2 to 1.1% less at zstd 1,
/// 1.5 to 9% less at zstd 3, and 7% less at zstd 9 on the tar; but a local
/// copy of a 4 GiB file on tmpfs took 3.3 s with them, against 2.9 s, and a
/// block's worth of memory is held by each thread that reads, compresses,
/// decodes or writes one.
pub const PACK: usize = 2 * 1024 * 1024;

/// The size of a segment: a power of two, as BLAKE3's tree needs, and the
/// largest file a pack takes. The same as a pack's, for the same reasons.
pub const SEGMENT: u64 = 2 * 1024 * 1024;

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
    crate::inject::fail_if("block-encode")?;
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
    /// The payload, from `start`: the block as it came, where it was sent
    /// as it is, so that it is not copied again.
    buffer: Vec<u8>,
    start: usize,
}

impl Block {
    pub fn payload(&self) -> &[u8] {
        &self.buffer[self.start..]
    }
}

/// Decodes a block, refusing one that [`encode`] would not have made.
pub fn decode(block: Vec<u8>) -> Result<Block> {
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
    let start = PREFIX + header_len;
    let bytes = &block[start..];
    let (buffer, start) = match header.codec {
        Codec::Raw if bytes.len() == raw_len => (block, start),
        Codec::Zstd if bytes.len() < raw_len => {
            let payload = zstd::bulk::decompress(bytes, raw_len).context("decompress a block")?;
            if payload.len() != raw_len {
                bail!("a payload of {} bytes, not {raw_len}", payload.len());
            }
            (payload, 0)
        }
        codec => bail!(
            "a payload of {} bytes, {codec:?}, of {raw_len}",
            bytes.len()
        ),
    };
    Ok(Block {
        body: header.body,
        buffer,
        start,
    })
}

/// Bytes of a block's payload: the files of a pack share their pack's,
/// which is let go of once none of them holds it.
#[derive(Clone)]
pub struct Shared {
    payload: Arc<Payload>,
    range: Range<usize>,
}

/// A block's payload, and what is done once it is let go of.
struct Payload {
    bytes: Vec<u8>,
    released: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        if let Some(released) = self.released.take() {
            released();
        }
    }
}

impl Deref for Shared {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.payload.bytes[self.range.clone()]
    }
}

#[cfg(test)]
impl Shared {
    /// Bytes of their own, as a block's would be: for a test of what takes
    /// them, not of blocks.
    pub fn of(bytes: Vec<u8>) -> Self {
        let range = 0..bytes.len();
        Self {
            payload: Arc::new(Payload {
                bytes,
                released: None,
            }),
            range,
        }
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "{} bytes", self.len())
    }
}

impl PartialEq for Shared {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Shared {}

/// How much of the data credit a block of a payload of `raw_len` bytes
/// takes: those bytes as decoded, which is what the Destination holds of
/// it until it is written, but no less than 4 KiB, for its header and its
/// handling.
pub fn cost(raw_len: usize) -> u64 {
    raw_len.max(4096) as u64
}

/// What a block holds of one file.
#[derive(Debug, PartialEq, Eq)]
pub struct Part {
    pub file: FileId,
    /// Where in the file its bytes go.
    pub offset: u64,
    /// The file's size and mtime as the Source found them when it opened
    /// it: a pack's file's size is its length in the pack, none where it
    /// could not be read.
    pub size: u64,
    pub mtime: Timestamp,
    /// Its bytes, if they are those the Source read; else why not.
    pub bytes: Result<Shared, String>,
    /// A segment's chaining value, once its bytes are found to match it,
    /// for [`root`] to merge with its file's others.
    pub chaining_value: Option<[u8; 32]>,
}

impl Part {
    /// Whether it is a segment of a file, rather than a whole one.
    pub fn is_segment(&self) -> bool {
        self.size > SEGMENT
    }
}

impl Block {
    /// What the block holds of each file, in order, each checked against
    /// what the Source made of its bytes.
    pub fn into_parts(self) -> Vec<Part> {
        self.into_parts_then(None)
    }

    /// As [`Block::into_parts`], and `released` runs once every part has
    /// let go of the block's bytes, as the files written, or failed, do.
    pub fn into_parts_releasing(
        self,
        released: impl FnOnce() + Send + Sync + 'static,
    ) -> Vec<Part> {
        self.into_parts_then(Some(Box::new(released)))
    }

    fn into_parts_then(self, released: Option<Box<dyn FnOnce() + Send + Sync>>) -> Vec<Part> {
        let unread = |why: &str| format!("the Source could not read it whole: {why}");
        let mismatch = || "the content's hash does not match the Source's".to_owned();
        let start = self.start;
        let payload = Arc::new(Payload {
            bytes: self.buffer,
            released,
        });
        let shared = |range: Range<usize>| Shared {
            payload: Arc::clone(&payload),
            range,
        };
        match self.body {
            Body::Pack { files } => {
                let mut start = start;
                files
                    .into_iter()
                    .map(|file| {
                        let range = start..start + file.len as usize;
                        start = range.end;
                        let bytes = match file.check {
                            Check::Unread(why) => Err(unread(&why)),
                            Check::Hash(hash)
                                if blake3::hash(&payload.bytes[range.clone()]) == hash =>
                            {
                                Ok(shared(range))
                            }
                            Check::Hash(_) => Err(mismatch()),
                        };
                        Part {
                            file: file.file,
                            offset: 0,
                            size: u64::from(file.len),
                            mtime: file.mtime,
                            bytes,
                            chaining_value: None,
                        }
                    })
                    .collect()
            }
            Body::Segment(segment) => {
                let (bytes, value) = match segment.check {
                    Check::Unread(why) => (Err(unread(&why)), None),
                    Check::Hash(value)
                        if chaining_value(segment.offset, &payload.bytes[start..]) == value =>
                    {
                        (Ok(shared(start..payload.bytes.len())), Some(value))
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

/// What follows the last block of a transfer: a prefix of no header and no
/// payload, which no block has.
pub const END: [u8; PREFIX] = [0; PREFIX];

/// What an [`Assembler`] puts back together.
#[derive(Debug, PartialEq, Eq)]
pub enum Assembled {
    /// A block, still encoded, for any thread to decode.
    Block(Vec<u8>),
    /// [`END`]: the Source sent every block it will.
    End,
}

/// Puts blocks back together from the pieces data packets carry: each block
/// in a buffer of its own, of its size once its prefix says it, so that
/// every byte is copied once, and a block holds no more room than it needs.
#[derive(Default)]
pub struct Assembler {
    /// The block being put together, and how long it is whole, once its
    /// prefix is in.
    block: Vec<u8>,
    whole: Option<usize>,
}

impl Assembler {
    /// Takes the next piece, and returns every block it completes, still
    /// encoded, so that they can be decoded in parallel, and the end, if it
    /// comes.
    pub fn push(&mut self, mut piece: &[u8]) -> Result<Vec<Assembled>> {
        let mut assembled = Vec::new();
        while !piece.is_empty() {
            let whole = match self.whole {
                Some(whole) => whole,
                None => {
                    let take = (PREFIX - self.block.len()).min(piece.len());
                    self.block.extend_from_slice(&piece[..take]);
                    piece = &piece[take..];
                    if self.block.len() < PREFIX {
                        break;
                    }
                    if self.block == END {
                        self.block.clear();
                        assembled.push(Assembled::End);
                        continue;
                    }
                    let header_len =
                        u32::from_le_bytes(self.block[..4].try_into().expect("four bytes"))
                            as usize;
                    let payload_len =
                        u32::from_le_bytes(self.block[4..8].try_into().expect("four bytes"))
                            as usize;
                    if header_len == 0 || header_len + payload_len > MOST {
                        bail!(
                            "a block with a header of {header_len} bytes and a payload of {payload_len}"
                        );
                    }
                    self.block.reserve_exact(header_len + payload_len);
                    *self.whole.insert(PREFIX + header_len + payload_len)
                }
            };
            let take = (whole - self.block.len()).min(piece.len());
            self.block.extend_from_slice(&piece[..take]);
            piece = &piece[take..];
            if self.block.len() == whole {
                assembled.push(Assembled::Block(std::mem::replace(
                    &mut self.block,
                    Vec::with_capacity(PREFIX),
                )));
                self.whole = None;
            }
        }
        Ok(assembled)
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
    let mut hasher = SegmentHasher::new(offset);
    hasher.update(bytes);
    hasher.finish()
}

/// Makes a segment's chaining value as its bytes come, a piece at a time,
/// while each is fresh in the cache.
pub struct SegmentHasher(blake3::Hasher);

impl SegmentHasher {
    /// For the segment at `offset`, as [`chaining_value`] takes it.
    pub fn new(offset: u64) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.set_input_offset(offset);
        Self(hasher)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The chaining value of every byte given; at least one was.
    pub fn finish(&self) -> [u8; 32] {
        self.0.finalize_non_root()
    }
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
            let decoded = decode(block).unwrap();
            assert_eq!(decoded.body, body, "{turn}");
            assert_eq!(decoded.payload(), payload, "{turn}");
            let mut start = 0;
            for part in decoded.into_parts() {
                match part.bytes {
                    Ok(bytes) => {
                        assert_eq!(&*bytes, &payload[start..start + bytes.len()]);
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
        assert_eq!(decode(block).unwrap().payload(), text);
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
        assert_eq!(decode(block).unwrap().payload(), noise);
    }

    /// A file whose bytes are not those the Source hashed fails alone; a
    /// segment's, the segment.
    #[test]
    fn bytes_not_as_read_are_found() {
        let (body, mut payload) = pack_of(&[(b"first", None), (b"second", None), (b"", None)]);
        payload[7] ^= 1;
        let block = decode(encode(body, &payload, true, 1).unwrap()).unwrap();
        let parts = block.into_parts();
        assert_eq!(parts[0].bytes.as_deref(), Ok(&b"first"[..]));
        assert_eq!(
            parts[1].bytes,
            Err("the content's hash does not match the Source's".to_owned())
        );
        assert_eq!(parts[2].bytes.as_deref(), Ok(&b""[..]));
        assert!(!parts[0].is_segment());

        let file = Random::new(19).bytes(SEGMENT as usize + 100);
        let (body, mut payload) = segment_of(&file, SEGMENT);
        let good = decode(encode(body.clone(), &payload, false, 1).unwrap()).unwrap();
        let Body::Segment(Segment {
            check: Check::Hash(value),
            ..
        }) = body
        else {
            unreachable!()
        };
        let good = good.into_parts();
        assert_eq!(good[0].chaining_value, Some(value));
        assert_eq!(good[0].offset, SEGMENT);
        assert!(good[0].is_segment());
        payload[99] ^= 1;
        let bad = decode(encode(body, &payload, false, 1).unwrap()).unwrap();
        let bad = bad.into_parts();
        assert!(bad[0].bytes.is_err());
        assert_eq!(bad[0].chaining_value, None);
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
        let block = decode(encode(moved, &payload, true, 1).unwrap()).unwrap();
        assert!(block.into_parts()[0].bytes.is_err());
    }

    /// Bodies that do not hold their payload, and blocks cut, padded or
    /// otherwise not as encoded, are refused, never decoded.
    #[test]
    fn blocks_not_as_encoded_are_refused() {
        let (body, payload) = pack_of(&[(b"some bytes", None), (b"more", None)]);
        let good = encode(body.clone(), &payload, false, 1).unwrap();
        assert!(decode(good.clone()).is_ok());
        assert!(decode(good[..good.len() - 1].to_vec()).is_err());
        assert!(decode([good.as_slice(), &[0]].concat()).is_err());
        assert!(decode(good[..7].to_vec()).is_err());

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
        assert!(decode(block(&header(Codec::Raw, 14, &body), &payload)).is_ok());
        // A header followed by a byte it does not hold.
        let padded = [header(Codec::Raw, 14, &body), vec![0]].concat();
        assert!(decode(block(&padded, &payload)).is_err());
        // Files that do not add up to the payload.
        assert!(decode(block(&header(Codec::Raw, 13, &body), &payload[..13])).is_err());
        // A compressed payload that does not shrink, or decodes to more.
        assert!(decode(block(&header(Codec::Zstd, 14, &body), &payload)).is_err());
        let squeezed = zstd::bulk::compress(&[b'a'; 1000], 1).unwrap();
        let thousand = pack_of(&[(&[b'a'; 1000], None)]).0;
        assert!(decode(block(&header(Codec::Zstd, 1000, &thousand), &squeezed)).is_ok());
        let hundred = pack_of(&[(&[b'a'; 100], None)]).0;
        assert!(decode(block(&header(Codec::Zstd, 100, &hundred), &squeezed)).is_err());
        // A pack of nothing, of an unread file with bytes, of a file larger
        // than a segment, and one too large.
        assert!(decode(block(&header(Codec::Raw, 0, &pack_of(&[]).0), &[])).is_err());
        let unread = Body::Pack {
            files: vec![PackFile {
                file: 1,
                len: 4,
                mtime: MTIME,
                check: Check::Unread("gone".into()),
            }],
        };
        assert!(decode(block(&header(Codec::Raw, 4, &unread), b"abcd")).is_err());
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
            decode(block)
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
            if let Ok(block) = decode(bytes) {
                drop(block.into_parts());
            }
            let length = random.below(64) as usize;
            let noise = random.bytes(length);
            if let Ok(block) = decode(noise) {
                drop(block.into_parts());
            }
        }
    }

    /// However a stream of blocks is cut, by a byte, a few, or not at all,
    /// each block comes whole, in a buffer of its own size, and the end
    /// after them.
    #[test]
    fn blocks_cut_anywhere_are_put_back_together() {
        let mut random = Random::new(29);
        let blocks: Vec<Vec<u8>> = [0, 10, 70_000, 300_000]
            .into_iter()
            .map(|length| {
                let bytes = random.bytes(length);
                let (body, payload) = pack_of(&[(&bytes, None)]);
                encode(body, &payload, length > 50_000, 1).unwrap()
            })
            .collect();
        let stream: Vec<u8> = blocks.concat().into_iter().chain(END).collect();
        let expected: Vec<Assembled> = blocks
            .iter()
            .cloned()
            .map(Assembled::Block)
            .chain([Assembled::End])
            .collect();
        for size in [1, 7, 8, 9, 4096, stream.len()] {
            let mut assembler = Assembler::default();
            let mut assembled = Vec::new();
            for piece in stream.chunks(size) {
                for item in assembler.push(piece).unwrap() {
                    if let Assembled::Block(block) = &item {
                        assert!(block.capacity() <= block.len() + 16, "{size}");
                    }
                    assembled.push(item);
                }
            }
            assert_eq!(assembled, expected, "{size}");
        }
    }

    /// A prefix of no header, or of more than a block holds, is refused.
    #[test]
    fn prefixes_out_of_bounds_are_refused() {
        let prefix =
            |header: u32, payload: u32| [header.to_le_bytes(), payload.to_le_bytes()].concat();
        assert!(Assembler::default().push(&prefix(0, 5)).is_err());
        assert!(Assembler::default().push(&prefix(MOST as u32, 1)).is_err());
        assert!(
            Assembler::default()
                .push(&prefix(10, MOST as u32 - 10))
                .is_ok()
        );
    }

    /// What is done once a block's bytes are let go of runs once the last
    /// part holding them has, and at once where none does: a pack of files
    /// none of which came as read.
    #[test]
    fn a_block_is_released_once_its_last_part_lets_go() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let released = Arc::new(AtomicU64::new(0));
        let releasing = |block: Vec<u8>| {
            let released = Arc::clone(&released);
            decode(block).unwrap().into_parts_releasing(move || {
                released.fetch_add(1, Ordering::Relaxed);
            })
        };
        let (body, payload) = pack_of(&[(b"first", None), (b"second", None)]);
        let mut parts = releasing(encode(body, &payload, false, 1).unwrap());
        let second = parts.pop().unwrap();
        drop(parts);
        assert_eq!(released.load(Ordering::Relaxed), 0);
        assert_eq!(second.bytes.as_deref(), Ok(&b"second"[..]));
        drop(second);
        assert_eq!(released.load(Ordering::Relaxed), 1);

        let (body, payload) = pack_of(&[(b"gone", Some("it shrank"))]);
        let parts = releasing(encode(body, &payload, false, 1).unwrap());
        assert_eq!(released.load(Ordering::Relaxed), 2);
        drop(parts);
        assert_eq!(released.load(Ordering::Relaxed), 2);
        assert_eq!(cost(0), 4096);
        assert_eq!(cost(PACK), PACK as u64);
    }
}
