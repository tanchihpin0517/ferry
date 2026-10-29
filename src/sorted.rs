//! What a large directory holds, within a bound on memory, past which it
//! goes to temporary files, nameless from the start.
//!
//! - [`Sorter`]: a directory's names, in any order, given back sorted by
//!   bytes. Those of a directory too large for the bound are sorted a run at
//!   a time, each written to disk, and the runs merged as the names are
//!   read: they come in the order, and with the bytes, they would have
//!   sorted in memory. A directory within the bound never touches the disk.
//! - [`Keyed`]: records whose keys come in order, each with a value, looked
//!   up by key, in any order, or read back in order from the first.
//!
//! On disk, both are blocks: records front-coded, each as how many bytes its
//! key shares with the one before, the rest of it, and its value, at most
//! [`BLOCK`] bytes of them, each block compressed alone as a zstd frame, at
//! level 1. What is held of a file in memory is where each block is, and its
//! first key, so that a key is found by reading one block. See
//! [Scan](../docs/design/pipeline.md#1-scan).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::FileType;

use crate::fsys::{self, Listed};

/// How many bytes of names, with what each costs beside its bytes, a
/// directory being listed holds before it sorts them into a run; and of any
/// [`Keyed`]'s records, before they go to disk.
pub const HELD: usize = 64 << 20;

/// What a name, or a record, costs in memory beside its bytes: its `Vec`s,
/// and what their allocations take.
const OVERHEAD: usize = std::mem::size_of::<Listed>() + 16;

/// How many runs are merged at once, at most: so as many files are open,
/// each with a block's bytes in memory, one decompressor for them all.
/// Merging 128 runs of random names held 7.2 MiB, 32 runs 1.6 MiB. More
/// runs than that, of 64 MiB each, are merged in levels.
pub const FAN_IN: usize = 128;

/// The most bytes of records in a block, but for a single record larger
/// than that, which is a block alone.
const BLOCK: usize = 64 << 10;

/// A block's header on disk: its length compressed, and its records'.
const BLOCK_HEADER: usize = 8;

/// The most bytes a key, or a value, read back may have: more is a file
/// gone wrong.
const RECORD_MOST: usize = 64 << 20;

/// How much is held before a run is written, and how many runs are merged
/// at once: [`HELD`], or what `FERRY_TEST_NAMES_HELD` says, and [`FAN_IN`];
/// or a test's.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub held: usize,
    pub fan_in: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            held: crate::inject::names_held().unwrap_or(HELD),
            fan_in: FAN_IN,
        }
    }
}

/// When, and where, what is held goes to disk: past `limits`, in `temp`.
#[derive(Clone, Debug)]
pub struct Spill {
    pub limits: Limits,
    pub temp: PathBuf,
}

impl Default for Spill {
    /// The [`Limits`]' own, in [`temp_dir`].
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            temp: temp_dir(),
        }
    }
}

/// The names of the directory open as `directory`, without `.` and `..`,
/// sorted by bytes, those of one too large for `limits` sorted on disk, in
/// `temp`.
pub fn list(directory: impl std::os::fd::AsFd, limits: Limits, temp: &Path) -> io::Result<Sorted> {
    let mut sorter = Sorter::new(limits, temp);
    fsys::list_entries(directory, |name, file_type| sorter.push(name, file_type))?;
    sorter.finish()
}

/// Where what is held goes to disk: `std::env::temp_dir()`, which is
/// `TMPDIR` where it is set, and `/tmp` otherwise, as GNU `sort` does.
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// A failure to write, or read back, what is held in the temporary
/// directory: one the transfer cannot go on from, as holding it in memory
/// instead would break the bound.
#[derive(Debug)]
pub struct TempFailed {
    temp: PathBuf,
    error: io::Error,
}

impl fmt::Display for TempFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot keep what a large directory holds in {}: {}; TMPDIR sets where it is kept",
            self.temp.display(),
            self.error
        )
    }
}

impl std::error::Error for TempFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Whether `error` is a [`TempFailed`]: the temporary directory's, not the
/// listed directory's.
pub fn is_temp_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<TempFailed>())
}

/// `error`, met in `temp`, as a [`TempFailed`] of the same kind.
fn temp_failed(temp: &Path, error: io::Error) -> io::Error {
    if is_temp_failure(&error) {
        return error;
    }
    io::Error::new(
        error.kind(),
        TempFailed {
            temp: temp.to_owned(),
            error,
        },
    )
}

/// Takes a directory's names, in any order, and gives them back sorted.
pub struct Sorter {
    limits: Limits,
    temp: PathBuf,
    /// The names held, not yet in a run, and what they cost.
    held: Vec<Listed>,
    cost: usize,
    /// The runs written so far.
    runs: Vec<Run>,
}

impl Sorter {
    pub fn new(limits: Limits, temp: &Path) -> Self {
        Self {
            limits,
            temp: temp.to_owned(),
            held: Vec::new(),
            cost: 0,
            runs: Vec::new(),
        }
    }

    /// Takes one name; once what is held reaches the bound, sorts it into a
    /// run.
    pub fn push(&mut self, name: &[u8], file_type: FileType) -> io::Result<()> {
        self.held.push(Listed {
            name: name.to_vec(),
            file_type,
        });
        self.cost += name.len() + OVERHEAD;
        if self.cost >= self.limits.held {
            self.spill()?;
        }
        Ok(())
    }

    /// Sorts what is held into a run; then, while as many runs of one level
    /// as are merged at once are there, merges them into one of the next.
    fn spill(&mut self) -> io::Result<()> {
        // As many as the next run will hold, about: so that growing it does
        // not take twice the room.
        let room = Vec::with_capacity(self.held.len());
        let mut held = std::mem::replace(&mut self.held, room);
        held.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        self.cost = 0;
        let run = Run::write(&self.temp, 0, held.into_iter().map(Ok))
            .map_err(|error| temp_failed(&self.temp, error))?;
        self.runs.push(run);
        while let Some(level) = self.full_level() {
            let (merged, kept): (Vec<Run>, Vec<Run>) = std::mem::take(&mut self.runs)
                .into_iter()
                .partition(|run| run.level == level);
            self.runs = kept;
            let run = self.merge_into_run(merged, level + 1)?;
            self.runs.push(run);
        }
        Ok(())
    }

    /// A level with as many runs as are merged at once, if any.
    fn full_level(&self) -> Option<u32> {
        let mut counts = std::collections::BTreeMap::new();
        for run in &self.runs {
            *counts.entry(run.level).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .find(|&(_, count)| count >= self.limits.fan_in)
            .map(|(level, _)| level)
    }

    /// Merges `runs` into one run at `level`, letting go of them.
    fn merge_into_run(&self, runs: Vec<Run>, level: u32) -> io::Result<Run> {
        let temp = &self.temp;
        let sources = runs
            .into_iter()
            .map(|run| Source::Run(Cursor::new(run.blocks)))
            .collect();
        let merge = Merge::new(temp, sources)?;
        Run::write(temp, level, merge).map_err(|error| temp_failed(temp, error))
    }

    /// The names taken, sorted: in memory, where no run was written; else
    /// merged from the runs and what is held, at most as many at once as
    /// [`Limits::fan_in`] says.
    pub fn finish(mut self) -> io::Result<Sorted> {
        let mut held = std::mem::take(&mut self.held);
        held.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        if self.runs.is_empty() {
            return Ok(Sorted(Inner::Memory(held.into_iter())));
        }
        // What is held is one of the sources: so as many runs at most as
        // are merged at once, less one, are left to merge with it.
        while self.runs.len() >= self.limits.fan_in.max(2) {
            self.runs.sort_by_key(|run| run.level);
            let rest = self.runs.split_off(self.limits.fan_in.max(2));
            let first = std::mem::replace(&mut self.runs, rest);
            let level = first.iter().map(|run| run.level).max().unwrap_or(0) + 1;
            let run = self.merge_into_run(first, level)?;
            self.runs.push(run);
        }
        let mut sources: Vec<Source> = std::mem::take(&mut self.runs)
            .into_iter()
            .map(|run| Source::Run(Cursor::new(run.blocks)))
            .collect();
        sources.push(Source::Memory(held.into_iter()));
        Ok(Sorted(Inner::Merge(Merge::new(&self.temp, sources)?)))
    }
}

/// A directory's names, sorted by bytes, each as it comes; a run that cannot
/// be read back is a [`TempFailed`].
pub struct Sorted(Inner);

enum Inner {
    Memory(std::vec::IntoIter<Listed>),
    Merge(Merge),
}

impl Sorted {
    /// Whether its names were sorted on disk.
    pub fn on_disk(&self) -> bool {
        matches!(self.0, Inner::Merge(_))
    }
}

impl Iterator for Sorted {
    type Item = io::Result<Listed>;

    fn next(&mut self) -> Option<io::Result<Listed>> {
        match &mut self.0 {
            Inner::Memory(names) => names.next().map(Ok),
            Inner::Merge(merge) => merge.next(),
        }
    }
}

/// A run on disk: names and their types, sorted; its level, how many merges
/// made it.
struct Run {
    blocks: Blocks,
    level: u32,
}

impl Run {
    /// Writes `names`, sorted, as a run at `level`, in a new nameless file
    /// in `temp`.
    fn write(
        temp: &Path,
        level: u32,
        names: impl IntoIterator<Item = io::Result<Listed>>,
    ) -> io::Result<Run> {
        let mut writer = Writer::new(Blocks::new(temp)?)?;
        for listed in names {
            let Listed { name, file_type } = listed?;
            writer.push(&name, &[type_byte(file_type)])?;
        }
        Ok(Run {
            blocks: writer.finish()?,
            level,
        })
    }
}

/// Where a merge takes names from: a run, or what was held in memory, sorted.
enum Source {
    Run(Cursor),
    Memory(std::vec::IntoIter<Listed>),
}

impl Source {
    fn next(&mut self, decompressor: &mut Decompressor) -> io::Result<Option<Listed>> {
        match self {
            Source::Run(run) => run
                .next(decompressor)?
                .map(|(name, value)| {
                    let file_type = value
                        .first()
                        .copied()
                        .ok_or_else(|| gone_wrong("a run's name of no type"))
                        .and_then(from_type_byte)?;
                    Ok(Listed { name, file_type })
                })
                .transpose(),
            Source::Memory(names) => Ok(names.next()),
        }
    }
}

/// The next name of each source, the least first; ties, which a directory
/// has none of, by source.
struct Merge {
    temp: PathBuf,
    sources: Vec<Source>,
    heads: BinaryHeap<Reverse<(Vec<u8>, usize, u8)>>,
    /// One for every source: they read a block at a time, one at a time.
    decompressor: Decompressor,
    failed: bool,
}

impl Merge {
    fn new(temp: &Path, mut sources: Vec<Source>) -> io::Result<Merge> {
        let mut decompressor = Decompressor::default();
        let mut heads = BinaryHeap::with_capacity(sources.len());
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(Listed { name, file_type }) = source
                .next(&mut decompressor)
                .map_err(|error| temp_failed(temp, error))?
            {
                heads.push(Reverse((name, index, type_byte(file_type))));
            }
        }
        Ok(Merge {
            temp: temp.to_owned(),
            sources,
            heads,
            decompressor,
            failed: false,
        })
    }
}

impl Iterator for Merge {
    type Item = io::Result<Listed>;

    fn next(&mut self) -> Option<io::Result<Listed>> {
        if self.failed {
            return None;
        }
        let Reverse((name, index, file_type)) = self.heads.pop()?;
        match self.sources[index].next(&mut self.decompressor) {
            Ok(Some(Listed {
                name: next,
                file_type: next_type,
            })) => self
                .heads
                .push(Reverse((next, index, type_byte(next_type)))),
            Ok(None) => {}
            Err(error) => {
                self.failed = true;
                return Some(Err(temp_failed(&self.temp, error)));
            }
        }
        let file_type = from_type_byte(file_type).expect("a type it made");
        Some(Ok(Listed { name, file_type }))
    }
}

/// A [`Keyed`]'s record: its key and value.
type Record = (Vec<u8>, Vec<u8>);

/// Records whose keys come in increasing order, each with a value: held in
/// memory up to the bound, then written to disk in blocks, a file for all
/// of them, so that what is held stays within the bound however many come.
/// A record is found by its key, in any order, reading one block; or read
/// back in order, from the first, as it was pushed.
pub struct Keyed {
    limits: Limits,
    temp: PathBuf,
    /// The records on disk, the first ones; none until the bound was first
    /// reached.
    written: Option<Writer>,
    /// How many records are on disk.
    on_disk: usize,
    /// The records after those, in memory, and what they cost.
    held: Vec<(Vec<u8>, Vec<u8>)>,
    cost: usize,
    /// The block last looked in, and where in it the last look stopped.
    looked: Option<Looked>,
    /// Where reading in order is: the next record's number, and, while it
    /// is on disk, the block it is in, and where in it.
    next: usize,
    reading: Option<Cursor>,
    decompressor: Decompressor,
}

impl Keyed {
    pub fn new(spill: &Spill) -> Self {
        Self {
            limits: spill.limits,
            temp: spill.temp.clone(),
            written: None,
            on_disk: 0,
            held: Vec::new(),
            cost: 0,
            looked: None,
            next: 0,
            reading: None,
            decompressor: Decompressor::default(),
        }
    }

    /// How many records were pushed.
    pub fn len(&self) -> usize {
        self.on_disk + self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether some of its records are on disk.
    pub fn on_disk(&self) -> bool {
        self.on_disk > 0
    }

    /// Takes a record, whose key is greater than any before it; once what is
    /// held reaches the bound, writes it to disk.
    pub fn push(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        debug_assert!(
            self.held
                .last()
                .is_none_or(|(last, _)| last.as_slice() < key),
            "keys in order"
        );
        self.held.push((key.to_vec(), value.to_vec()));
        self.cost += key.len() + value.len() + OVERHEAD;
        if self.cost >= self.limits.held {
            self.spill()
                .map_err(|error| temp_failed(&self.temp, error))?;
        }
        Ok(())
    }

    /// Writes what is held to disk, after what is there, in whole blocks.
    fn spill(&mut self) -> io::Result<()> {
        let writer = match &mut self.written {
            Some(writer) => writer,
            None => self.written.insert(Writer::new(Blocks::new(&self.temp)?)?),
        };
        for (key, value) in self.held.drain(..) {
            writer.push(&key, &value)?;
            self.on_disk += 1;
        }
        writer.flush()?;
        self.cost = 0;
        Ok(())
    }

    /// The value of the record whose key is `key`, if any.
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        Ok(self.find(key)?.map(|(_, value)| value))
    }

    /// The record whose key is `key`, if any: its number, from 0 as they
    /// were pushed, and its value.
    pub fn find(&mut self, key: &[u8]) -> io::Result<Option<(usize, Vec<u8>)>> {
        Ok(self
            .numbered_floor(key)?
            .filter(|(_, (found, _))| found.as_slice() == key)
            .map(|(number, (_, value))| (number, value)))
    }

    /// The record with the greatest key not past `key`, if any.
    pub fn floor(&mut self, key: &[u8]) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self.numbered_floor(key)?.map(|(_, record)| record))
    }

    /// As [`Keyed::floor`], with the record's number.
    fn numbered_floor(&mut self, key: &[u8]) -> io::Result<Option<(usize, Record)>> {
        if let Some(first) = self.held.first()
            && key >= first.0.as_slice()
        {
            let at = self
                .held
                .partition_point(|(held, _)| held.as_slice() <= key);
            return Ok(Some((self.on_disk + at - 1, self.held[at - 1].clone())));
        }
        let Some(written) = &self.written else {
            return Ok(None);
        };
        // The last block whose first key is not past it.
        let at = written
            .blocks
            .index
            .partition_point(|block| block.first.as_slice() <= key);
        if at == 0 {
            return Ok(None);
        }
        let at = at - 1;
        if self.looked.as_ref().is_none_or(|looked| looked.block != at) {
            let raw = written
                .blocks
                .read(at, self.decompressor.get()?)
                .map_err(|error| temp_failed(&self.temp, error))?;
            self.looked = Some(Looked {
                block: at,
                raw,
                after: 0,
                floor: None,
            });
        }
        let looked = self.looked.as_mut().expect("looked");
        // Looked for in order, as they mostly are, a block is read once:
        // from where the last look stopped, unless that is past the key.
        if looked
            .floor
            .as_ref()
            .is_some_and(|(_, (found, _))| found.as_slice() > key)
        {
            looked.after = 0;
            looked.floor = None;
        }
        let (previous, mut number) = match &looked.floor {
            Some((number, (found, _))) => (found.clone(), number + 1),
            None => (Vec::new(), written.blocks.index[at].number),
        };
        let mut records = Records::after(&looked.raw[looked.after..], previous);
        while let Some(record) = records.next() {
            let record = record.map_err(|error| temp_failed(&self.temp, error))?;
            if record.0.as_slice() > key {
                break;
            }
            looked.after = looked.raw.len() - records.raw.len();
            looked.floor = Some((number, record));
            number += 1;
        }
        Ok(looked.floor.clone())
    }

    /// The next record in order, from the first: its key and value.
    pub fn next_record(&mut self) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let record = self.peek()?;
        if record.is_some() {
            self.next += 1;
            if let Some(reading) = &mut self.reading {
                reading.skip();
            }
        }
        Ok(record)
    }

    /// The next record in order, left to be read next.
    pub fn peek(&mut self) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.next >= self.on_disk {
            self.reading = None;
            return Ok(self.held.get(self.next - self.on_disk).cloned());
        }
        let written = self.written.as_ref().expect("on disk");
        // Records pushed since may have gone to disk: where it is reading is
        // found again, once.
        if self
            .reading
            .as_ref()
            .is_none_or(|reading| reading.number != self.next)
        {
            let index = &written.blocks.index;
            let at = index.partition_point(|block| block.number <= self.next) - 1;
            let mut reading = Cursor::at(at, index[at].number);
            while reading.number < self.next {
                reading
                    .read(&written.blocks, &mut self.decompressor)
                    .map_err(|error| temp_failed(&self.temp, error))?;
            }
            self.reading = Some(reading);
        }
        let reading = self.reading.as_mut().expect("reading");
        reading
            .peek(&written.blocks, &mut self.decompressor)
            .map_err(|error| temp_failed(&self.temp, error))
    }
}

/// A block of a [`Keyed`]'s looked in, decompressed, by its number; where
/// the last look in it stopped, after the greatest record not past its key,
/// which it keeps, with its number.
struct Looked {
    block: usize,
    raw: Vec<u8>,
    after: usize,
    floor: Option<(usize, Record)>,
}

/// A nameless file of blocks, and where each is: what is held of it in
/// memory.
struct Blocks {
    file: File,
    /// Where the next block goes.
    end: u64,
    index: Vec<BlockAt>,
}

/// Where a block is in its file, how long it is, compressed and not, and
/// its first record's number and key.
struct BlockAt {
    offset: u64,
    length: u32,
    raw: u32,
    number: usize,
    first: Vec<u8>,
}

impl Blocks {
    fn new(temp: &Path) -> io::Result<Blocks> {
        Ok(Blocks {
            file: unnamed(temp)?,
            end: 0,
            index: Vec::new(),
        })
    }

    /// The records of the block numbered `at`, decompressed.
    fn read(&self, at: usize, decompressor: &mut zstd::bulk::Decompressor) -> io::Result<Vec<u8>> {
        let block = &self.index[at];
        let mut compressed = vec![0; block.length as usize];
        self.file
            .read_exact_at(&mut compressed, block.offset + BLOCK_HEADER as u64)?;
        let raw = decompressor.decompress(&compressed, block.raw as usize)?;
        if raw.len() != block.raw as usize {
            return Err(gone_wrong("a block that does not read back"));
        }
        Ok(raw)
    }
}

/// Writes records to a file of blocks, a block once it is full.
struct Writer {
    blocks: Blocks,
    compressor: zstd::bulk::Compressor<'static>,
    /// The block filling: its records, front-coded, the first one's number
    /// and key, and the key before.
    raw: Vec<u8>,
    first: Option<(usize, Vec<u8>)>,
    previous: Vec<u8>,
    /// How many records were pushed.
    count: usize,
}

impl Writer {
    fn new(blocks: Blocks) -> io::Result<Writer> {
        Ok(Writer {
            blocks,
            compressor: zstd::bulk::Compressor::new(1)?,
            raw: Vec::with_capacity(BLOCK),
            first: None,
            previous: Vec::new(),
            count: 0,
        })
    }

    fn push(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        // A block starts with a key whole, so that it reads alone.
        let shared = if self.first.is_none() {
            self.first = Some((self.count, key.to_vec()));
            0
        } else {
            self.previous
                .iter()
                .zip(key)
                .take_while(|(a, b)| a == b)
                .count()
        };
        put_varint(&mut self.raw, shared as u64);
        put_varint(&mut self.raw, (key.len() - shared) as u64);
        self.raw.extend_from_slice(&key[shared..]);
        put_varint(&mut self.raw, value.len() as u64);
        self.raw.extend_from_slice(value);
        self.previous.clear();
        self.previous.extend_from_slice(key);
        self.count += 1;
        if self.raw.len() >= BLOCK {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes the block filling, if any.
    fn flush(&mut self) -> io::Result<()> {
        let Some((number, first)) = self.first.take() else {
            return Ok(());
        };
        let compressed = self.compressor.compress(&self.raw)?;
        let mut block = Vec::with_capacity(BLOCK_HEADER + compressed.len());
        block.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        block.extend_from_slice(&(self.raw.len() as u32).to_le_bytes());
        block.extend_from_slice(&compressed);
        self.blocks.file.write_all_at(&block, self.blocks.end)?;
        self.blocks.index.push(BlockAt {
            offset: self.blocks.end,
            length: compressed.len() as u32,
            raw: self.raw.len() as u32,
            number,
            first,
        });
        self.blocks.end += block.len() as u64;
        self.raw.clear();
        Ok(())
    }

    fn finish(mut self) -> io::Result<Blocks> {
        self.flush()?;
        Ok(self.blocks)
    }
}

/// A zstd decompressor, made once one is needed.
#[derive(Default)]
struct Decompressor(Option<zstd::bulk::Decompressor<'static>>);

impl Decompressor {
    fn get(&mut self) -> io::Result<&mut zstd::bulk::Decompressor<'static>> {
        if self.0.is_none() {
            self.0 = Some(zstd::bulk::Decompressor::new()?);
        }
        Ok(self.0.as_mut().expect("made"))
    }
}

/// Where reading a file of blocks in order is: the block, its records'
/// bytes, as decompressed, where in them, and the key before; and the next
/// record's number.
struct Cursor {
    /// The run it reads, for a merge's source; a [`Keyed`]'s is its own.
    blocks: Option<Blocks>,
    at: usize,
    raw: Vec<u8>,
    position: usize,
    previous: Vec<u8>,
    number: usize,
    peeked: Option<(Vec<u8>, Vec<u8>)>,
}

impl Cursor {
    /// Reading `blocks`, from the first.
    fn new(blocks: Blocks) -> Self {
        Self {
            blocks: Some(blocks),
            ..Self::at(0, 0)
        }
    }

    /// Reading from the block numbered `at`, whose first record is
    /// numbered `number`.
    fn at(at: usize, number: usize) -> Self {
        Self {
            blocks: None,
            at,
            raw: Vec::new(),
            position: 0,
            previous: Vec::new(),
            number,
            peeked: None,
        }
    }

    /// The next record of the run it reads.
    fn next(&mut self, decompressor: &mut Decompressor) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let blocks = self.blocks.take().expect("a run");
        let record = self.read(&blocks, decompressor);
        self.blocks = Some(blocks);
        record
    }

    /// The next record of `blocks`, read.
    fn read(
        &mut self,
        blocks: &Blocks,
        decompressor: &mut Decompressor,
    ) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let record = self.peek(blocks, decompressor)?;
        if record.is_some() {
            self.skip();
        }
        Ok(record)
    }

    /// The next record of `blocks`, left to be read next.
    fn peek(
        &mut self,
        blocks: &Blocks,
        decompressor: &mut Decompressor,
    ) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        if let Some(peeked) = &self.peeked {
            return Ok(Some(peeked.clone()));
        }
        if self.position >= self.raw.len() {
            if self.at >= blocks.index.len() {
                return Ok(None);
            }
            // One block's bytes at a time: those before are let go of.
            self.raw = blocks.read(self.at, decompressor.get()?)?;
            self.position = 0;
            self.previous.clear();
            self.at += 1;
        }
        let mut records = Records {
            raw: &self.raw[self.position..],
            previous: std::mem::take(&mut self.previous),
        };
        let record = records.decode();
        self.position = self.raw.len() - records.raw.len();
        self.previous = records.previous;
        let record = record?;
        self.peeked = Some(record.clone());
        Ok(Some(record))
    }

    /// Goes past the record peeked.
    fn skip(&mut self) {
        if self.peeked.take().is_some() {
            self.number += 1;
        }
    }
}

/// The records of a block, decoded, in order.
struct Records<'a> {
    raw: &'a [u8],
    previous: Vec<u8>,
}

impl<'a> Records<'a> {
    /// The records in `raw`, which come after the one whose key is
    /// `previous`.
    fn after(raw: &'a [u8], previous: Vec<u8>) -> Self {
        Self { raw, previous }
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        if length > self.raw.len() {
            return Err(gone_wrong("a block cut short"));
        }
        let (taken, rest) = self.raw.split_at(length);
        self.raw = rest;
        Ok(taken)
    }

    fn decode(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let shared = take_varint(&mut self.raw)? as usize;
        let rest = take_varint(&mut self.raw)? as usize;
        if shared > self.previous.len() || shared + rest > RECORD_MOST {
            return Err(gone_wrong("a block that does not read back"));
        }
        let mut key = self.previous[..shared].to_vec();
        key.extend_from_slice(self.take(rest)?);
        let length = take_varint(&mut self.raw)? as usize;
        if length > RECORD_MOST {
            return Err(gone_wrong("a block that does not read back"));
        }
        let value = self.take(length)?.to_vec();
        self.previous.clone_from(&key);
        Ok((key, value))
    }
}

impl Iterator for Records<'_> {
    type Item = io::Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.raw.is_empty() {
            return None;
        }
        let record = self.decode();
        if record.is_err() {
            self.raw = &[];
        }
        Some(record)
    }
}

fn gone_wrong(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// A new file in `temp`, to read and write, with no name: made so on Linux,
/// with `O_TMPFILE`, where the filesystem can; else made with a name no one
/// else has, and unlinked at once. Either way, it is gone once closed,
/// however the process ends.
fn unnamed(temp: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{Mode, OFlags};
        use rustix::io::Errno;
        let flags = OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC;
        match rustix::fs::openat(rustix::fs::CWD, temp, flags, Mode::from_raw_mode(0o600)) {
            Ok(fd) => return Ok(File::from(fd)),
            // A filesystem that cannot, as NFS; a kernel that knows no
            // `O_TMPFILE` takes it for `O_DIRECTORY`.
            Err(Errno::OPNOTSUPP | Errno::ISDIR | Errno::INVAL) => {}
            Err(error) => return Err(error.into()),
        }
    }
    named_then_unlinked(temp)
}

/// A new file in `temp`, made with a name of its own, refusing one there,
/// or a symlink, and unlinked at once.
fn named_then_unlinked(temp: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    static MADE: AtomicU64 = AtomicU64::new(0);
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::RDWR | OFlags::CLOEXEC;
    let mut tries = 0;
    loop {
        let name = temp.join(format!(
            ".ferry-sort-{}-{}-{}",
            std::process::id(),
            MADE.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos())
                .unwrap_or(0)
        ));
        match rustix::fs::openat(rustix::fs::CWD, &name, flags, Mode::from_raw_mode(0o600)) {
            Ok(fd) => {
                rustix::fs::unlink(&name)?;
                return Ok(File::from(fd));
            }
            Err(rustix::io::Errno::EXIST) if tries < 100 => tries += 1,
            Err(error) => return Err(error.into()),
        }
    }
}

fn put_varint(raw: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            raw.push(byte);
            return;
        }
        raw.push(byte | 0x80);
    }
}

fn take_varint(raw: &mut &[u8]) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let Some((&byte, rest)) = raw.split_first() else {
            return Err(gone_wrong("a block cut short"));
        };
        *raw = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(gone_wrong("a block's number of more than ten bytes"))
}

fn type_byte(file_type: FileType) -> u8 {
    match file_type {
        FileType::RegularFile => 0,
        FileType::Directory => 1,
        FileType::Symlink => 2,
        FileType::Fifo => 3,
        FileType::Socket => 4,
        FileType::CharacterDevice => 5,
        FileType::BlockDevice => 6,
        FileType::Unknown => 7,
    }
}

fn from_type_byte(byte: u8) -> io::Result<FileType> {
    Ok(match byte {
        0 => FileType::RegularFile,
        1 => FileType::Directory,
        2 => FileType::Symlink,
        3 => FileType::Fifo,
        4 => FileType::Socket,
        5 => FileType::CharacterDevice,
        6 => FileType::BlockDevice,
        7 => FileType::Unknown,
        _ => return Err(gone_wrong("a run's name of no type")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};

    /// A random name: of the bytes a name may have, some sharing a long
    /// prefix, some not UTF-8.
    fn names(count: usize, seed: u64) -> Vec<(Vec<u8>, FileType)> {
        let mut state = seed;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let types = [
            FileType::RegularFile,
            FileType::Directory,
            FileType::Symlink,
            FileType::Unknown,
        ];
        let mut seen = std::collections::HashSet::new();
        let mut made = Vec::new();
        while made.len() < count {
            let mut name = match random() % 3 {
                0 => b"a-rather-long-prefix-that-many-names-share/".to_vec(),
                1 => vec![0xff, 0xfe],
                _ => Vec::new(),
            };
            for _ in 0..1 + random() % 20 {
                // Any byte but NUL and `/`.
                let byte = match (random() % 255) as u8 + 1 {
                    b'/' => b'_',
                    byte => byte,
                };
                name.push(byte);
            }
            if name.contains(&b'/') {
                name.retain(|&byte| byte != b'/');
            }
            if seen.insert(name.clone()) {
                made.push((name, types[(random() % 4) as usize]));
            }
        }
        made
    }

    fn sorted_in_memory(names: &[(Vec<u8>, FileType)]) -> Vec<(Vec<u8>, FileType)> {
        let mut names = names.to_vec();
        names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        names
    }

    fn sorted_by(limits: Limits, temp: &Path, names: &[(Vec<u8>, FileType)]) -> Sorted {
        let mut sorter = Sorter::new(limits, temp);
        for (name, file_type) in names {
            sorter.push(name, *file_type).unwrap();
        }
        sorter.finish().unwrap()
    }

    fn collected(sorted: Sorted) -> Vec<(Vec<u8>, FileType)> {
        sorted
            .map(|listed| {
                let listed = listed.unwrap();
                (listed.name, listed.file_type)
            })
            .collect()
    }

    fn files_in(path: &Path) -> usize {
        std::fs::read_dir(path).unwrap().count()
    }

    #[test]
    fn names_come_sorted_as_in_memory_whether_merged_once_or_in_levels() {
        let temp = tempfile::tempdir().unwrap();
        let names = names(5_000, 7);
        let expected = sorted_in_memory(&names);
        // In memory alone; a run every hundred names or so, merged at once;
        // and three at a time, so in levels, a merge's runs merged again.
        for limits in [
            Limits::default(),
            Limits {
                held: 100 * (OVERHEAD + 12),
                fan_in: 128,
            },
            Limits {
                held: 50 * (OVERHEAD + 12),
                fan_in: 3,
            },
        ] {
            let sorted = sorted_by(limits, temp.path(), &names);
            assert_eq!(sorted.on_disk(), limits.held < HELD, "{limits:?}");
            assert_eq!(collected(sorted), expected, "{limits:?}");
        }
        assert_eq!(files_in(temp.path()), 0);
    }

    #[test]
    fn no_names_and_one_name_come_as_they_are() {
        let temp = tempfile::tempdir().unwrap();
        let small = Limits { held: 1, fan_in: 2 };
        assert!(collected(sorted_by(small, temp.path(), &[])).is_empty());
        let one = vec![(b"only".to_vec(), FileType::Fifo)];
        assert_eq!(collected(sorted_by(small, temp.path(), &one)), one);
    }

    #[test]
    fn nothing_is_left_in_the_temporary_directory_by_a_sort_stopped_half_way() {
        let temp = tempfile::tempdir().unwrap();
        let names = names(2_000, 11);
        let limits = Limits {
            held: 100 * (OVERHEAD + 12),
            fan_in: 4,
        };
        let mut sorted = sorted_by(limits, temp.path(), &names);
        assert!(sorted.on_disk());
        for _ in 0..500 {
            sorted.next().unwrap().unwrap();
        }
        // The runs have no names, while being read, as after.
        assert_eq!(files_in(temp.path()), 0);
        drop(sorted);
        assert_eq!(files_in(temp.path()), 0);
    }

    #[test]
    fn a_file_made_with_a_name_is_unlinked_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let mut file = named_then_unlinked(temp.path()).unwrap();
        assert_eq!(files_in(temp.path()), 0);
        file.write_all(b"still there").unwrap();
        file.rewind().unwrap();
        let mut read = String::new();
        file.read_to_string(&mut read).unwrap();
        assert_eq!(read, "still there");
    }

    #[test]
    fn a_temporary_directory_that_cannot_be_written_fails_naming_it_and_tmpdir() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut sorter = Sorter::new(Limits { held: 1, fan_in: 2 }, &missing);
        let error = sorter.push(b"a", FileType::RegularFile).unwrap_err();
        assert!(is_temp_failure(&error), "{error}");
        let said = error.to_string();
        assert!(said.contains(&missing.display().to_string()), "{said}");
        assert!(said.contains("TMPDIR"), "{said}");
        // An error of the listed directory's is not one.
        assert!(!is_temp_failure(&io::Error::from(io::ErrorKind::NotFound)));
    }

    #[test]
    fn a_directory_is_listed_sorted_through_the_disk_as_in_memory() {
        let temp = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        for index in 0..300 {
            std::fs::write(tree.path().join(format!("file-{index:04}")), b"").unwrap();
        }
        std::fs::create_dir(tree.path().join("a-directory")).unwrap();
        let directory = File::open(tree.path()).unwrap();
        let in_memory: Vec<_> = fsys::read_entries(&directory)
            .unwrap()
            .into_iter()
            .map(|listed| (listed.name, listed.file_type))
            .collect();
        let limits = Limits {
            held: 40 * (OVERHEAD + 9),
            fan_in: 3,
        };
        let sorted = list(&directory, limits, temp.path()).unwrap();
        assert!(sorted.on_disk());
        assert_eq!(collected(sorted), in_memory);
        assert_eq!(files_in(temp.path()), 0);
    }

    fn spill(held: usize, temp: &Path) -> Spill {
        Spill {
            limits: Limits { held, fan_in: 4 },
            temp: temp.to_owned(),
        }
    }

    /// Keys in order, with values of all sizes, one larger than a block.
    fn records(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        names(count, 3)
            .into_iter()
            .map(|(name, _)| name)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let value = match index % 7 {
                    0 => Vec::new(),
                    6 if index == 6 => vec![0xab; BLOCK + 100],
                    other => vec![other as u8; other * 13],
                };
                (name, value)
            })
            .collect()
    }

    #[test]
    fn records_kept_are_found_by_key_in_any_order_and_read_back_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let records = records(4_000);
        // In memory alone, and on disk past a bound reached many times.
        for held in [HELD, 3_000] {
            let mut keyed = Keyed::new(&spill(held, temp.path()));
            for (key, value) in &records {
                keyed.push(key, value).unwrap();
            }
            assert_eq!(keyed.len(), records.len());
            assert_eq!(keyed.on_disk(), held < HELD);
            // Every key, from the last back, and every other from the first:
            // so in an order other than theirs.
            for (key, value) in records.iter().rev().chain(records.iter().step_by(2)) {
                assert_eq!(keyed.get(key).unwrap().as_ref(), Some(value), "{held}");
            }
            // Keys not there: before the first, between two, after the last.
            for key in [&b""[..], b"\x00", b"\xff\xff\xff\xff\xff\xff\xff\xff"] {
                if records.iter().all(|(had, _)| had.as_slice() != key) {
                    assert_eq!(keyed.get(key).unwrap(), None);
                }
            }
            let between = [records[10].0.clone(), vec![0]].concat();
            assert_eq!(keyed.get(&between).unwrap(), None);
            // In order, from the first, peeking as it goes.
            let mut read = Vec::new();
            while let Some(peeked) = keyed.peek().unwrap() {
                let record = keyed.next_record().unwrap().unwrap();
                assert_eq!(peeked, record);
                read.push(record);
            }
            assert_eq!(read, records);
            assert_eq!(keyed.next_record().unwrap(), None);
        }
        assert_eq!(files_in(temp.path()), 0);
    }

    #[test]
    fn the_record_at_or_before_a_key_is_found_on_disk_and_in_memory() {
        let temp = tempfile::tempdir().unwrap();
        let mut keyed = Keyed::new(&spill(300, temp.path()));
        for number in (0u64..1_000).step_by(10) {
            keyed
                .push(&number.to_be_bytes(), &number.to_le_bytes())
                .unwrap();
        }
        assert!(keyed.on_disk());
        for wanted in [0u64, 1, 9, 10, 15, 500, 501, 989, 990, 991, 5_000] {
            let found = keyed.floor(&wanted.to_be_bytes()).unwrap().unwrap();
            let expected = wanted.min(990) / 10 * 10;
            assert_eq!(found.0, expected.to_be_bytes(), "{wanted}");
            assert_eq!(found.1, expected.to_le_bytes(), "{wanted}");
        }
        // Found by its key alone, with its number, on disk and held.
        let mut keyed = Keyed::new(&spill(300, temp.path()));
        let mut pushed = 0u64;
        while !keyed.on_disk() || keyed.held.len() < 2 {
            keyed
                .push(&(pushed * 10).to_be_bytes(), &pushed.to_le_bytes())
                .unwrap();
            pushed += 1;
        }
        for wanted in (0..pushed * 10 + 5).rev() {
            let found = keyed.find(&wanted.to_be_bytes()).unwrap();
            let number = wanted / 10;
            let expected = (wanted % 10 == 0 && number < pushed)
                .then(|| (number as usize, number.to_le_bytes().to_vec()));
            assert_eq!(found, expected, "{wanted}");
        }
        let mut empty = Keyed::new(&spill(300, temp.path()));
        assert_eq!(empty.floor(b"x").unwrap(), None);
        empty.push(b"m", b"").unwrap();
        assert_eq!(empty.floor(b"a").unwrap(), None);
    }

    /// Looked for in order, back, again, or anywhere, across blocks, a
    /// record is found as it was pushed.
    #[test]
    fn records_are_found_in_any_order_across_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let mut keyed = Keyed::new(&spill(50_000, temp.path()));
        let count = 30_000u64;
        for number in 0..count {
            let value = vec![number as u8; (number % 7) as usize];
            keyed.push(&(number * 3).to_be_bytes(), &value).unwrap();
        }
        let index = &keyed.written.as_ref().unwrap().blocks.index;
        assert!(index.len() > 2 && index[1].number - index[0].number > 100);
        let mut wanted: Vec<u64> = (0..count * 3 + 2).collect();
        wanted.extend((0..count * 3).rev().step_by(50));
        wanted.extend([7, 7, 6, 6, 90_000, 0, 89_997, 3]);
        let mut state = 1u64;
        wanted.extend((0..1_000).map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 33) % (count * 3 + 2)
        }));
        for wanted in wanted {
            let number = (wanted / 3).min(count - 1);
            let expected = (number as usize, vec![number as u8; (number % 7) as usize]);
            let found = keyed
                .numbered_floor(&wanted.to_be_bytes())
                .unwrap()
                .unwrap();
            assert_eq!((found.0, found.1.1), expected, "{wanted}");
            let found = keyed.find(&wanted.to_be_bytes()).unwrap();
            assert_eq!(
                found,
                (wanted % 3 == 0 && wanted < count * 3).then_some(expected),
                "{wanted}"
            );
        }
    }

    #[test]
    fn records_read_in_order_while_more_are_pushed() {
        let temp = tempfile::tempdir().unwrap();
        let records = records(1_000);
        let mut keyed = Keyed::new(&spill(2_000, temp.path()));
        let mut read = Vec::new();
        for (index, (key, value)) in records.iter().enumerate() {
            keyed.push(key, value).unwrap();
            if index % 3 == 0 {
                read.push(keyed.next_record().unwrap().unwrap());
            }
        }
        while let Some(record) = keyed.next_record().unwrap() {
            read.push(record);
        }
        assert_eq!(read, records);
        assert!(keyed.on_disk());
        drop(keyed);
        assert_eq!(files_in(temp.path()), 0);
    }

    #[test]
    fn records_that_cannot_go_to_disk_fail_naming_it_and_tmpdir() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut keyed = Keyed::new(&spill(1, &missing));
        let error = keyed.push(b"a", b"value").unwrap_err();
        assert!(is_temp_failure(&error), "{error}");
        assert!(error.to_string().contains("TMPDIR"), "{error}");
    }
}
