//! A directory's names, sorted by bytes, within a bound on memory. Those of
//! a directory too large for it are sorted a run at a time, each written to
//! a temporary file, nameless from the start, and the runs merged as the
//! names are read: the names come in the order, and with the bytes, they
//! would have sorted in memory. A directory within the bound never touches
//! the disk. See the
//! [large-directories draft](../docs/drafts/large-directories.md).
//!
//! A run is the names front-coded, each as how many bytes it shares with the
//! one before, the rest of it, and its type, then compressed as one zstd
//! stream, at level 1.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::FileType;

use crate::fsys::{self, Listed};

/// How many bytes of names, with what each costs beside its bytes, a
/// directory being listed holds before it sorts them into a run.
pub const HELD: usize = 64 << 20;

/// What a name costs in memory beside its bytes: its `Listed`, and what its
/// allocation takes.
const OVERHEAD: usize = std::mem::size_of::<Listed>() + 16;

/// How many runs are merged at once, at most: so as many files are open,
/// each read through a [`BUFFER`], beside what decompressing it holds.
/// Merging 128 runs held 28 MiB, about 220 KiB a run, its window as
/// [`WINDOW_LOG`] says, and 45 MiB with zstd's own at level 1, 2^19; so 32,
/// about 7 MiB. More runs than that, of 64 MiB each, are merged in levels.
pub const FAN_IN: usize = 32;

/// What each run is read through.
const BUFFER: usize = 64 << 10;

/// How far back a run's compression looks, as a power of two: what reading
/// a run holds beside its buffer. Runs of random names took as many bytes
/// with zstd's own window, 2^19, about half their names'.
const WINDOW_LOG: u32 = 16;

/// The most bytes a name read back from a run may have: more is a run
/// gone wrong.
const NAME_MOST: usize = 64 << 10;

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

/// When, and where, names go to disk: past `limits`, in `temp`.
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

/// Where the runs are written: `std::env::temp_dir()`, which is `TMPDIR`
/// where it is set, and `/tmp` otherwise, as GNU `sort` does.
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// A failure to write, or read back, a run in the temporary directory: one
/// the transfer cannot go on from, as sorting the names in memory instead
/// would break the bound.
#[derive(Debug)]
pub struct TempFailed {
    temp: PathBuf,
    error: io::Error,
}

impl fmt::Display for TempFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot sort a large directory's names in {}: {}; TMPDIR sets where they are sorted",
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
            .map(|run| run.read(temp).map(Source::Run))
            .collect::<io::Result<Vec<_>>>()?;
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
        let temp = &self.temp;
        let mut sources = std::mem::take(&mut self.runs)
            .into_iter()
            .map(|run| run.read(temp).map(Source::Run))
            .collect::<io::Result<Vec<_>>>()?;
        sources.push(Source::Memory(held.into_iter()));
        Ok(Sorted(Inner::Merge(Merge::new(temp, sources)?)))
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

/// A run on disk: its file, nameless, read from its start; its level, how
/// many merges made it; and how many names it has.
struct Run {
    file: File,
    level: u32,
    names: u64,
}

impl Run {
    /// Writes `names`, sorted, as a run at `level`, in a new nameless file
    /// in `temp`.
    fn write(
        temp: &Path,
        level: u32,
        names: impl IntoIterator<Item = io::Result<Listed>>,
    ) -> io::Result<Run> {
        let file = unnamed(temp)?;
        let mut encoder = zstd::stream::write::Encoder::new(BufWriter::new(file), 1)?;
        encoder.window_log(WINDOW_LOG)?;
        let mut previous: Vec<u8> = Vec::new();
        let mut count = 0;
        for listed in names {
            let Listed { name, file_type } = listed?;
            let shared = previous
                .iter()
                .zip(&name)
                .take_while(|(a, b)| a == b)
                .count();
            write_varint(&mut encoder, shared as u64)?;
            write_varint(&mut encoder, (name.len() - shared) as u64)?;
            encoder.write_all(&name[shared..])?;
            encoder.write_all(&[type_byte(file_type)])?;
            previous = name;
            count += 1;
        }
        let mut file = encoder
            .finish()?
            .into_inner()
            .map_err(|error| error.into_error())?;
        file.rewind()?;
        Ok(Run {
            file,
            level,
            names: count,
        })
    }

    /// Reads the run back, from its start.
    fn read(self, temp: &Path) -> io::Result<RunReader> {
        let reader = BufReader::with_capacity(BUFFER, self.file);
        let mut decoder = zstd::stream::read::Decoder::with_buffer(reader)
            .map_err(|error| temp_failed(temp, error))?;
        decoder
            .window_log_max(WINDOW_LOG)
            .map_err(|error| temp_failed(temp, error))?;
        Ok(RunReader {
            decoder,
            previous: Vec::new(),
            left: self.names,
        })
    }
}

/// A run being read, a name at a time.
struct RunReader {
    decoder: zstd::stream::read::Decoder<'static, BufReader<File>>,
    previous: Vec<u8>,
    left: u64,
}

impl RunReader {
    fn next(&mut self) -> io::Result<Option<Listed>> {
        if self.left == 0 {
            return Ok(None);
        }
        let shared = read_varint(&mut self.decoder)? as usize;
        let rest = read_varint(&mut self.decoder)? as usize;
        if shared > self.previous.len() || shared + rest > NAME_MOST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a run of names that does not read back",
            ));
        }
        let mut name = self.previous[..shared].to_vec();
        name.resize(shared + rest, 0);
        self.decoder.read_exact(&mut name[shared..])?;
        let mut file_type = [0];
        self.decoder.read_exact(&mut file_type)?;
        let file_type = from_type_byte(file_type[0])?;
        self.previous.clone_from(&name);
        self.left -= 1;
        Ok(Some(Listed { name, file_type }))
    }
}

/// Where a merge takes names from: a run, or what was held in memory, sorted.
enum Source {
    Run(RunReader),
    Memory(std::vec::IntoIter<Listed>),
}

impl Source {
    fn next(&mut self) -> io::Result<Option<Listed>> {
        match self {
            Source::Run(run) => run.next(),
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
    failed: bool,
}

impl Merge {
    fn new(temp: &Path, mut sources: Vec<Source>) -> io::Result<Merge> {
        let mut heads = BinaryHeap::with_capacity(sources.len());
        for (index, source) in sources.iter_mut().enumerate() {
            if let Some(Listed { name, file_type }) =
                source.next().map_err(|error| temp_failed(temp, error))?
            {
                heads.push(Reverse((name, index, type_byte(file_type))));
            }
        }
        Ok(Merge {
            temp: temp.to_owned(),
            sources,
            heads,
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
        match self.sources[index].next() {
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

/// A new file in `temp`, to read and write, with no name: made so on Linux,
/// with `O_TMPFILE`, where the filesystem can; else made with a name no one
/// else has, and unlinked at once. Either way, it is gone once closed,
/// however the process ends.
fn unnamed(temp: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    #[cfg(target_os = "linux")]
    {
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

fn write_varint(writer: &mut impl Write, mut value: u64) -> io::Result<()> {
    let mut bytes = [0u8; 10];
    let mut length = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes[length] = byte;
            length += 1;
            break;
        }
        bytes[length] = byte | 0x80;
        length += 1;
    }
    writer.write_all(&bytes[..length])
}

fn read_varint(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "a run's number of more than ten bytes",
    ))
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
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a run's name of no type",
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
