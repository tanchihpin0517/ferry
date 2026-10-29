//! What the data's pipe carries of the files the Destination wants: a
//! record each, [`Data`]. A file of at most [`SEGMENT`] bytes goes whole;
//! a larger one in segments, [`SEGMENT`] bytes from a multiple of it, or
//! what is left of the file there; each with what the Source made of its
//! bytes, or why it could not read them whole. After a file's last segment
//! goes its hash; a file the Source could not open is said so; and the
//! data ends with its end, as the manifest does.
//!
//! A whole file carries its BLAKE3 hash, and a segment the chaining value
//! of its part of its file's. BLAKE3 hashes a tree, of which a segment,
//! aligned and a power of two long, is a subtree: so a segment is checked
//! on its own, by [`chaining_value`], and its file's hash, the same as of
//! all its bytes at once, is merged from its segments' by [`root`], and
//! sent apart, once all are read.
//!
//! The records go one after another, in the order the Source writes them,
//! which the pipe keeps; the transport compresses them, where the
//! connection is: so none is numbered, nor counted. A pipe that ends
//! without the data's end was cut, as a Source that failed ends its pipes.

use std::io::{BufRead, Read as _};
use std::ops::{Deref, Range};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use blake3::hazmat::{
    ChainingValue, HasherExt, Mode, left_subtree_len, merge_subtrees_non_root, merge_subtrees_root,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::manifest::{FileId, Timestamp};

/// The size of a segment: a power of two, as BLAKE3's tree needs, and the
/// largest file that goes whole.
///
/// Measured, as blocks then, on crate sources, a build's output, `/usr`, a
/// tar of sources and large files of model weights, numbers and zip, each
/// compressed alone: 2 MiB sent as much as a stream of files did, 8 MiB 0.2
/// to 1.1% less at zstd 1; but a local copy of a 4 GiB file on tmpfs took
/// 3.3 s with 8 MiB, against 2.9 s, and a segment's worth of memory is held
/// by each thread that reads, checks or writes one.
pub const SEGMENT: u64 = 2 * 1024 * 1024;

/// The most a record of the data holds, encoded: a segment's bytes, and
/// room for a long reason a file could not be read, or its path.
pub const MOST: usize = SEGMENT as usize + 64 * 1024;

/// What the Destination checks bytes against, or why there are none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Check {
    /// A whole file's BLAKE3 hash, or a segment's chaining value.
    Hash([u8; 32]),
    /// Why the Source could not read them whole: the file shrank, or a read
    /// failed. None are sent, and the Destination fails the file.
    Unread(String),
}

/// A record of the data's pipe, its bytes as `B` holds them: as the Source
/// writes them, [`Bytes`]; as the Destination reads them, where they are
/// in the buffer it read the record into; or of their own, [`ByteBuf`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Data<B> {
    /// A file of at most [`SEGMENT`] bytes, whole: its bytes, as the Source
    /// read them, its size as it found it when it opened it, and their
    /// hash; or why it could not read them whole, and none.
    File {
        id: FileId,
        mtime: Timestamp,
        check: Check,
        bytes: B,
    },
    /// A segment of a file larger than [`SEGMENT`]: where in the file it
    /// begins, a multiple of [`SEGMENT`]; the file's size and mtime as the
    /// Source found them when it opened it, what it gains after left for
    /// the next run, which finds its mtime changed; its bytes, [`SEGMENT`]
    /// or what is left of the file from the offset, and their chaining
    /// value; or why it could not read them whole, and none.
    Segment {
        id: FileId,
        offset: u64,
        size: u64,
        mtime: Timestamp,
        check: Check,
        bytes: B,
    },
    /// The hash of a file sent in segments, merged from theirs, after its
    /// last.
    Hash { file: FileId, hash: [u8; 32] },
    /// A file needed that the Source could not open, at `path`: it never
    /// comes.
    Unopened { file: FileId, path: Vec<u8> },
    /// The end of the data: the Source sent all it was asked for.
    End,
}

impl<B> Data<B> {
    /// The same record, its bytes as `holding` makes them.
    pub fn map<C>(self, holding: impl FnOnce(B) -> C) -> Data<C> {
        match self {
            Data::File {
                id,
                mtime,
                check,
                bytes,
            } => Data::File {
                id,
                mtime,
                check,
                bytes: holding(bytes),
            },
            Data::Segment {
                id,
                offset,
                size,
                mtime,
                check,
                bytes,
            } => Data::Segment {
                id,
                offset,
                size,
                mtime,
                check,
                bytes: holding(bytes),
            },
            Data::Hash { file, hash } => Data::Hash { file, hash },
            Data::Unopened { file, path } => Data::Unopened { file, path },
            Data::End => Data::End,
        }
    }

    /// Refuses a record of a file's bytes, `len` of them, that a Source
    /// would not write: a whole file larger than [`SEGMENT`], or bytes of
    /// one it could not read; a segment out of its file, or of other than
    /// its length.
    pub fn holds(&self, len: usize) -> Result<()> {
        match self {
            Data::File { check, .. } => {
                if len as u64 > SEGMENT {
                    bail!("a whole file of {len} bytes");
                }
                if len > 0 && matches!(check, Check::Unread(_)) {
                    bail!("an unread file of {len} bytes");
                }
            }
            Data::Segment {
                offset,
                size,
                check,
                ..
            } => {
                if *size <= SEGMENT || offset % SEGMENT != 0 || offset >= size {
                    bail!("a segment at {offset} of a file of {size} bytes");
                }
                let expected = match check {
                    Check::Hash(_) => SEGMENT.min(size - offset),
                    Check::Unread(_) => 0,
                };
                if len as u64 != expected {
                    bail!("a segment of {len} bytes, not {expected}");
                }
            }
            Data::Hash { .. } | Data::Unopened { .. } | Data::End => {}
        }
        Ok(())
    }
}

impl Data<Vec<u8>> {
    /// Encodes it as a record of the data, its bytes copied once: into a
    /// buffer, from the place returned.
    pub fn encode(&self) -> Result<(Vec<u8>, usize)> {
        crate::inject::fail_if("data-encode")?;
        let written = match self {
            Data::File {
                id,
                mtime,
                check,
                bytes,
            } => Data::File {
                id: *id,
                mtime: *mtime,
                check: check.clone(),
                bytes: Bytes(bytes),
            },
            Data::Segment {
                id,
                offset,
                size,
                mtime,
                check,
                bytes,
            } => Data::Segment {
                id: *id,
                offset: *offset,
                size: *size,
                mtime: *mtime,
                check: check.clone(),
                bytes: Bytes(bytes),
            },
            Data::Hash { file, hash } => Data::Hash {
                file: *file,
                hash: *hash,
            },
            Data::Unopened { file, path } => Data::Unopened {
                file: *file,
                path: path.clone(),
            },
            Data::End => Data::End,
        };
        crate::protocol::encode_record(&written)
    }
}

/// A record's bytes as the Source writes them: as bytes, at once.
pub struct Bytes<'a>(pub &'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

/// A record's bytes of their own: as the tests and the fuzzer hold them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteBuf(pub Vec<u8>);

impl Serialize for ByteBuf {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByteBuf {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: &[u8] = Deserialize::deserialize(deserializer)?;
        Ok(ByteBuf(bytes.to_vec()))
    }
}

/// A record of the data as read: the buffer it was read into, and the
/// record, its bytes where they are in it.
pub type Read = (Vec<u8>, Data<Range<usize>>);

/// Reads the next record of the data's pipe into a buffer of its own, read
/// once; none once the pipe ends between records. One cut short, too long,
/// that does not decode whole, or that a Source would not write, is an
/// error.
pub fn read(reader: &mut impl BufRead) -> Result<Option<Read>> {
    let Some(length) = crate::protocol::read_length(reader)? else {
        return Ok(None);
    };
    let mut buffer = Vec::with_capacity(length);
    reader
        .take(length as u64)
        .read_to_end(&mut buffer)
        .context("read a record of the data")?;
    if buffer.len() < length {
        return Err(
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
                .context("a record cut short"),
        );
    }
    let start = buffer.as_ptr() as usize;
    let (record, rest) =
        postcard::take_from_bytes::<Data<&[u8]>>(&buffer).context("decode a record of the data")?;
    ensure!(
        rest.is_empty(),
        "a record with {} bytes past it",
        rest.len()
    );
    let record = record.map(|bytes| {
        let at = if bytes.is_empty() {
            0
        } else {
            bytes.as_ptr() as usize - start
        };
        at..at + bytes.len()
    });
    if let Data::File { bytes, .. } | Data::Segment { bytes, .. } = &record {
        record.holds(bytes.len())?;
    }
    Ok(Some((buffer, record)))
}

/// What a record of a file's bytes, read into `buffer`, holds, checked
/// against what the Source made of them: the part of its file to write;
/// none of another record. `released` runs once nothing holds the buffer:
/// once the part's bytes are written, or failed; at once, where it holds
/// none.
pub fn part(
    buffer: Vec<u8>,
    record: Data<Range<usize>>,
    released: impl FnOnce() + Send + Sync + 'static,
) -> Option<Part> {
    let unread = |why: &str| format!("the Source could not read it whole: {why}");
    let mismatch = || "the content's hash does not match the Source's".to_owned();
    let payload = Arc::new(Payload {
        bytes: buffer,
        released: Some(Box::new(released)),
    });
    let shared = |range: Range<usize>| Shared {
        payload: Arc::clone(&payload),
        range,
    };
    match record {
        Data::File {
            id,
            mtime,
            check,
            bytes,
        } => {
            let size = bytes.len() as u64;
            let bytes = match check {
                Check::Unread(why) => Err(unread(&why)),
                Check::Hash(hash) if blake3::hash(&payload.bytes[bytes.clone()]) == hash => {
                    Ok(shared(bytes))
                }
                Check::Hash(_) => Err(mismatch()),
            };
            Some(Part {
                file: id,
                offset: 0,
                size,
                mtime,
                bytes,
                chaining_value: None,
            })
        }
        Data::Segment {
            id,
            offset,
            size,
            mtime,
            check,
            bytes,
        } => {
            let (bytes, value) = match check {
                Check::Unread(why) => (Err(unread(&why)), None),
                Check::Hash(value)
                    if chaining_value(offset, &payload.bytes[bytes.clone()]) == value =>
                {
                    (Ok(shared(bytes)), Some(value))
                }
                Check::Hash(_) => (Err(mismatch()), None),
            };
            Some(Part {
                file: id,
                offset,
                size,
                mtime,
                bytes,
                chaining_value: value,
            })
        }
        Data::Hash { .. } | Data::Unopened { .. } | Data::End => None,
    }
}

/// Bytes of a record of the data, where they are in the buffer it was read
/// into, which is let go of once nothing holds it.
#[derive(Clone)]
pub struct Shared {
    payload: Arc<Payload>,
    range: Range<usize>,
}

/// A record's buffer, and what is done once it is let go of.
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
    /// Bytes of their own, as a record's would be: for a test of what takes
    /// them, not of records.
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

/// How much of what the Destination holds a record of `raw_len` bytes of a
/// file takes: those bytes, which it holds until they are written, but no
/// less than 4 KiB, for the rest of it and its handling.
pub fn cost(raw_len: usize) -> u64 {
    raw_len.max(4096) as u64
}

/// What a record holds of one file, checked.
#[derive(Debug, PartialEq, Eq)]
pub struct Part {
    pub file: FileId,
    /// Where in the file its bytes go.
    pub offset: u64,
    /// The file's size and mtime as the Source found them when it opened
    /// it: a whole file's size is its bytes', none where it could not be
    /// read.
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

/// How many segments a file of `size` bytes, larger than [`SEGMENT`], is
/// sent in.
pub fn segments(size: u64) -> u64 {
    size.div_ceil(SEGMENT)
}

/// The chaining value of a file's bytes from `offset`, a multiple of
/// [`SEGMENT`]: [`SEGMENT`] of them, or the rest of the file, and at least
/// one, as [`Data::holds`] makes sure of a segment.
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

    /// A record of a whole file of `bytes`, or one the Source could not
    /// read, as it writes it, read back as the Destination reads it.
    fn read_back(record: &Data<ByteBuf>) -> Result<Option<super::Read>> {
        let encoded = crate::protocol::encode_record(record)?;
        read(&mut std::io::BufReader::new(&encoded.0[encoded.1..]))
    }

    fn file(id: FileId, bytes: &[u8], check: Check) -> Data<ByteBuf> {
        Data::File {
            id,
            mtime: MTIME,
            check,
            bytes: ByteBuf(bytes.to_vec()),
        }
    }

    fn segment_of(file: &[u8], offset: u64) -> Data<ByteBuf> {
        let end = (offset + SEGMENT).min(file.len() as u64);
        let bytes = file[offset as usize..end as usize].to_vec();
        Data::Segment {
            id: 7,
            offset,
            size: file.len() as u64,
            mtime: MTIME,
            check: Check::Hash(chaining_value(offset, &bytes)),
            bytes: ByteBuf(bytes),
        }
    }

    /// Any record comes back as it was written, its bytes where they are
    /// in the buffer read, checked as the Source made them: the parts of
    /// files and segments, the hash and what could not be opened as they
    /// are.
    #[test]
    fn any_record_comes_back_and_its_bytes_are_checked() {
        let mut random = Random::new(13);
        let large = random.bytes(2 * SEGMENT as usize + 12345);
        for turn in 0..70 {
            let record = match turn % 7 {
                0 => {
                    let length = random.below(SEGMENT + 1) as usize;
                    let bytes = random.bytes(length);
                    let hash = *blake3::hash(&bytes).as_bytes();
                    file(3, &bytes, Check::Hash(hash))
                }
                1 => file(4, b"", Check::Unread("it shrank".to_owned())),
                2 | 3 => segment_of(&large, random.below(3) * SEGMENT),
                4 => Data::Hash {
                    file: 7,
                    hash: [9; 32],
                },
                5 => Data::End,
                _ => Data::Unopened {
                    file: 8,
                    path: b"a/b".to_vec(),
                },
            };
            let (buffer, read) = read_back(&record).unwrap().unwrap();
            let bytes = |record: &Data<ByteBuf>| match record {
                Data::File { bytes, .. } | Data::Segment { bytes, .. } => Some(bytes.0.clone()),
                _ => None,
            };
            let expected = bytes(&record);
            let back = read.clone().map(|range| ByteBuf(buffer[range].to_vec()));
            assert_eq!(back, record);
            match part(buffer, read, || {}) {
                Some(part) => {
                    let unread = matches!(
                        record,
                        Data::File {
                            check: Check::Unread(_),
                            ..
                        }
                    );
                    if unread {
                        assert!(part.bytes.is_err());
                    } else {
                        assert_eq!(part.bytes.as_deref().ok(), expected.as_deref());
                    }
                }
                None => assert!(expected.is_none()),
            }
        }
    }

    /// Bytes other than the Source read, in a file or a segment, are found.
    #[test]
    fn bytes_not_as_read_are_found() {
        let record = file(3, b"other", Check::Hash(*blake3::hash(b"bytes").as_bytes()));
        let (buffer, read) = read_back(&record).unwrap().unwrap();
        let checked = part(buffer, read, || {}).unwrap();
        assert!(checked.bytes.unwrap_err().contains("does not match"));
        let large = Random::new(2).bytes(SEGMENT as usize + 10);
        let mut record = segment_of(&large, 0);
        if let Data::Segment { bytes, .. } = &mut record {
            bytes.0[5] ^= 1;
        }
        let (buffer, read) = read_back(&record).unwrap().unwrap();
        assert!(part(buffer, read, || {}).unwrap().bytes.is_err());
    }

    /// A record a Source would not write is refused: a whole file larger
    /// than a segment, bytes of a file it could not read, a segment out of
    /// place or of another length; and any bytes are refused or read,
    /// never a panic.
    #[test]
    fn records_not_as_a_source_writes_them_are_refused() {
        let large = vec![1u8; SEGMENT as usize + 1];
        let too_large = file(1, &large, Check::Hash([0; 32]));
        assert!(read_back(&too_large).is_err());
        let unread_bytes = file(1, b"x", Check::Unread("gone".to_owned()));
        assert!(read_back(&unread_bytes).is_err());
        let refused = |offset: u64, size: u64, len: usize| {
            read_back(&Data::Segment {
                id: 1,
                offset,
                size,
                mtime: MTIME,
                check: Check::Hash([0; 32]),
                bytes: ByteBuf(vec![0; len]),
            })
            .is_err()
        };
        assert!(refused(1, 3 * SEGMENT, SEGMENT as usize));
        assert!(refused(0, SEGMENT, SEGMENT as usize));
        assert!(refused(3 * SEGMENT, 3 * SEGMENT, 0));
        assert!(refused(0, 3 * SEGMENT, 5));
        assert!(!refused(SEGMENT, SEGMENT + 9, 9));
        let mut random = Random::new(17);
        for _ in 0..2000 {
            let length = random.below(64) as usize;
            let bytes = random.bytes(length);
            let _ = read(&mut std::io::BufReader::new(bytes.as_slice()));
        }
    }

    /// What is done once a record's buffer is let go of runs once its part
    /// has, and at once where it holds none: a file not as read.
    #[test]
    fn a_record_is_released_once_its_part_lets_go() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let released = Arc::new(AtomicU64::new(0));
        let releasing = |record: &Data<ByteBuf>| {
            let released = Arc::clone(&released);
            let (buffer, read) = read_back(record).unwrap().unwrap();
            part(buffer, read, move || {
                released.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap()
        };
        let good = releasing(&file(
            1,
            b"first",
            Check::Hash(*blake3::hash(b"first").as_bytes()),
        ));
        assert_eq!(released.load(Ordering::Relaxed), 0);
        assert_eq!(good.bytes.as_deref(), Ok(&b"first"[..]));
        drop(good);
        assert_eq!(released.load(Ordering::Relaxed), 1);
        let gone = releasing(&file(2, b"", Check::Unread("it shrank".to_owned())));
        assert_eq!(released.load(Ordering::Relaxed), 2);
        drop(gone);
        assert_eq!(released.load(Ordering::Relaxed), 2);
        assert_eq!(cost(0), 4096);
        assert_eq!(cost(SEGMENT as usize), SEGMENT);
    }
}
