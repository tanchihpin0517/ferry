//! The Source's owner: all the Source keeps of what to send, changed only by
//! the events it is told, one at a time, by one thread. What it decides is
//! said as commands, which the readers, the compressors and the connection
//! carry out. It does no I/O, and waits on nothing, so that what it does is
//! tested on events made up.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;

use crate::block::{self, Body, Check, PackFile, Segment};
use crate::manifest::{self, FileId, Kind, Timestamp};
use crate::profile;
use crate::protocol::{FileHash, Needs};

use super::read::{Content, Job};

/// How many files and segments may be with the readers at once, read or
/// being read: each is at most a segment, so 64 MiB at most.
pub(super) const READING: usize = 32;

/// How many blocks may be with the compressors, or compressed and not yet
/// handed to the connection's writer. The credit alone would let 64 MiB of
/// them wait there; so many buffers, allocated and let go of, made a copy
/// of a large file on one machine a tenth slower, its page faults twice as
/// many.
pub(super) const COMPRESSING: usize = 16;

/// Each entry the Source has sent in the manifest and the Destination has
/// not compared yet, by ID: whether it is a file, and its path, to answer
/// the Destination's needs. Kept a part of the manifest at a time, each with
/// its directory's path once and its names in one buffer, and let go a part
/// at a time, as the Destination says it has compared through it: what is
/// kept is what is on its way, not the whole tree.
#[derive(Default)]
pub(super) struct Known {
    parts: VecDeque<KnownPart>,
    /// How many entries were ever recorded.
    recorded: u64,
}

/// A part of the manifest, as it is kept.
pub(super) struct KnownPart {
    /// The ID of its first entry; the others follow it.
    first: FileId,
    directory: Vec<u8>,
    names: Vec<u8>,
    /// For each entry, where its name ends in `names`, and whether it is a
    /// file.
    entries: Vec<(u32, bool)>,
}

impl KnownPart {
    /// The `entries` of the directory at `path`, in ID order, as they are
    /// kept; none of none. The root is the one entry of the directory `` with
    /// the name ``.
    pub(super) fn of(path: &[u8], entries: &[manifest::Entry]) -> Option<KnownPart> {
        let first = entries.first()?;
        let mut part = KnownPart {
            first: first.id,
            directory: path.to_vec(),
            names: Vec::new(),
            entries: Vec::with_capacity(entries.len()),
        };
        for entry in entries {
            part.names.extend_from_slice(&entry.name);
            let end = u32::try_from(part.names.len()).expect("a part's names under 4 GiB");
            part.entries.push((end, entry.kind == Kind::File));
        }
        Some(part)
    }

    /// The ID after its last entry's.
    fn end(&self) -> FileId {
        self.first + self.entries.len() as FileId
    }
}

impl Known {
    pub(super) fn len(&self) -> u64 {
        self.recorded
    }

    /// Records a part, whose IDs follow the last recorded's.
    fn record(&mut self, part: KnownPart) {
        debug_assert!(
            self.parts
                .back()
                .is_none_or(|last| last.end() == part.first),
            "IDs follow manifest order"
        );
        self.recorded += part.entries.len() as u64;
        self.parts.push_back(part);
    }

    /// The part the entry `id` is in, and where in it, if it is kept.
    fn find(&self, id: FileId) -> Option<(&KnownPart, usize)> {
        let index = self.parts.partition_point(|part| part.end() <= id);
        let part = self.parts.get(index)?;
        let offset = usize::try_from(id.checked_sub(part.first)?).ok()?;
        (offset < part.entries.len()).then_some((part, offset))
    }

    /// Whether the entry `id` is a file, if it is kept.
    fn is_file(&self, id: FileId) -> Option<bool> {
        self.find(id).map(|(part, offset)| part.entries[offset].1)
    }

    /// The path of the entry `id`, if it is kept.
    fn path(&self, id: FileId) -> Option<Vec<u8>> {
        let (part, offset) = self.find(id)?;
        let start = match offset {
            0 => 0,
            _ => part.entries[offset - 1].0 as usize,
        };
        let name = &part.names[start..part.entries[offset].0 as usize];
        Some(manifest::join(&part.directory, name))
    }

    /// The Destination has compared every entry up to `id`, and asked for
    /// those it needs: the parts all of whose entries are among them go. A
    /// part is never empty, so its last entry is the one before its end.
    fn compared_through(&mut self, id: FileId) {
        while self.parts.front().is_some_and(|part| part.end() - 1 <= id) {
            self.parts.pop_front();
        }
    }
}

/// How far ahead of the Destination the scan may go.
pub(super) struct Limits {
    /// How many needed files may wait to be read.
    pub(super) backlog: usize,
    /// How many entries sent may wait to be compared, before a directory.
    pub(super) in_flight: FileId,
}

/// When a part of the manifest may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wait {
    /// At once: the root.
    Never,
    /// Once few enough needed files wait to be read: a directory's later
    /// part. The Destination says how far it compared only once a
    /// directory's last part is, so one larger than the limit would wait for
    /// itself.
    Backlog,
    /// That, and once few enough entries sent wait to be compared: a
    /// directory's first part.
    InFlight,
}

/// What the owner tells the scan of its part.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Room {
    /// It may not go yet, for this reason, which the scan waits in.
    Wait(profile::State),
    /// It may go.
    Go,
}

/// A part of the manifest the scan has ready: what of it to keep, if it
/// holds anything, when it may go, and where to say so.
pub(super) struct Part {
    pub(super) known: Option<KnownPart>,
    pub(super) wait: Wait,
    pub(super) room: Sender<Room>,
}

/// What a reader did.
pub(super) enum Read {
    /// A file read whole, or as much of it as could be, for a pack.
    Small { entry: PackFile, bytes: Vec<u8> },
    /// A file too large for a pack, open, to be read a segment at a time.
    Large {
        id: FileId,
        file: Arc<File>,
        size: u64,
        mtime: Timestamp,
    },
    /// A segment read, or as much of it as could be, with its chaining
    /// value.
    Segment {
        id: FileId,
        offset: u64,
        content: Content,
        value: [u8; 32],
    },
    /// A file that could not be opened.
    Unreadable {
        id: FileId,
        path: Vec<u8>,
        error: io::Error,
    },
}

/// What the owner is told.
pub(super) enum Event {
    /// The scan has a part of the manifest ready.
    Part(Part),
    /// The Destination's needs.
    Needs(Needs),
    /// A reader is done.
    Read(Read),
    /// The Destination gives more of the data credit.
    Credit(u64),
    /// A block is handed to the connection's writer.
    Sent,
    /// Nothing more is to be told right now: the pack being filled goes
    /// unfilled, so that the last files of a run, or of a slow scan, never
    /// wait.
    Idle,
}

/// What the owner has done.
pub(super) enum Command {
    /// Tell the scan of its part.
    Room(Sender<Room>, Room),
    /// Hand a reader a job.
    Read(Job),
    /// Compress a block and send it: its body, and the bytes of its files,
    /// or its segment's, which the compressor puts one after another.
    Block(Body, Vec<Vec<u8>>),
    /// Report a file that could not be opened, and tell the Destination.
    Unreadable {
        id: FileId,
        path: Vec<u8>,
        error: io::Error,
    },
    /// Send the hash of a file sent in segments.
    Hash(FileHash),
    /// Every file needed is sent: end the blocks.
    End,
}

/// A file being read a segment at a time.
struct Large {
    id: FileId,
    file: Arc<File>,
    size: u64,
    mtime: Timestamp,
    /// The next segment to read.
    next: u64,
    /// How many of its segments the readers have.
    reading: usize,
    /// Each segment's chaining value, once it is read.
    values: Vec<Option<[u8; 32]>>,
    /// How many of its segments are sent.
    sent: u64,
    /// A segment could not be read whole: nothing more of it is read, nor
    /// its hash sent.
    failed: bool,
}

impl Large {
    fn segments(&self) -> u64 {
        block::segments(self.size)
    }

    /// Nothing more of it is to be read, nor sent.
    fn done(&self) -> bool {
        (self.failed || self.sent == self.segments()) && self.reading == 0
    }
}

/// The pack being filled: its files, and the readers' buffers of their
/// bytes, handed on as they are.
#[derive(Default)]
struct Pack {
    files: Vec<PackFile>,
    bytes: Vec<Vec<u8>>,
    /// Its files' bytes and their entries in its header, at most.
    filled: usize,
}

/// A part the scan waits to send, and what it was told of it.
struct Parked {
    part: Part,
    told: Option<profile::State>,
}

/// The Source's owner; see the module.
pub(super) struct Owner {
    known: Known,
    limits: Limits,
    /// Files are sent: not in a dry run.
    sending: bool,
    /// Files needed and not opened yet, in the order needed.
    waiting: VecDeque<(FileId, Vec<u8>)>,
    /// How many jobs the readers have, and of them segments.
    reading: usize,
    segments: usize,
    /// Files being read a segment at a time, in the order opened.
    large: VecDeque<Large>,
    /// Files needed and not read yet, or not opened, for the scan to wait
    /// on.
    needed: usize,
    /// The last entry the manifest has sent, and the last the Destination
    /// said it compared.
    sent: FileId,
    through: FileId,
    parked: Option<Parked>,
    pack: Pack,
    /// How much of the data credit is left, and the blocks waiting for more:
    /// what is sent and not let go of by the Destination never exceeds what
    /// it gave.
    credit: u64,
    held: VecDeque<(Body, Vec<Vec<u8>>, u64)>,
    /// How many blocks the compressors have, or have compressed and not
    /// handed on.
    compressing: usize,
    /// The Destination's needs have ended.
    ended: bool,
    /// The blocks have ended.
    finished: bool,
}

impl Owner {
    pub(super) fn new(limits: Limits, sending: bool) -> Self {
        Self {
            known: Known::default(),
            limits,
            sending,
            waiting: VecDeque::new(),
            reading: 0,
            segments: 0,
            large: VecDeque::new(),
            needed: 0,
            sent: 0,
            through: 0,
            parked: None,
            pack: Pack::default(),
            credit: 0,
            held: VecDeque::new(),
            compressing: 0,
            ended: false,
            finished: false,
        }
    }

    /// How many entries the manifest has sent.
    pub(super) fn entries(&self) -> u64 {
        self.known.len()
    }

    /// Whether the blocks have ended: there is nothing more to do.
    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Takes an event, and says what to do about it.
    pub(super) fn on(&mut self, event: Event) -> Result<Vec<Command>> {
        let mut commands = Vec::new();
        match event {
            Event::Part(part) => {
                debug_assert!(self.parked.is_none(), "one part at a time");
                self.parked = Some(Parked { part, told: None });
            }
            Event::Needs(needs) => self.needs(needs)?,
            Event::Read(read) => self.read(read, &mut commands),
            Event::Credit(bytes) => {
                self.credit = self.credit.saturating_add(bytes);
                self.release_held(&mut commands);
            }
            Event::Sent => {
                self.compressing -= 1;
                self.release_held(&mut commands);
            }
            Event::Idle => self.seal(&mut commands),
        }
        self.dispatch(&mut commands);
        self.finish(&mut commands);
        self.release(&mut commands);
        Ok(commands)
    }

    fn needs(&mut self, needs: Needs) -> Result<()> {
        match needs {
            Needs::Files(needed) => {
                for id in needed {
                    let unknown = || format!("the Destination needs unknown ID {id}");
                    let is_file = self.known.is_file(id).with_context(unknown)?;
                    // Only files' content is sent; the Destination makes the
                    // rest itself. Its path goes with it, so that the entry
                    // need not be kept until it is sent.
                    if is_file && self.sending {
                        let path = self.known.path(id).with_context(unknown)?;
                        self.waiting.push_back((id, path));
                        self.needed += 1;
                    }
                }
            }
            Needs::Attributes(_) => {}
            Needs::Through(id) => {
                self.known.compared_through(id);
                self.through = self.through.max(id);
            }
            Needs::End => self.ended = true,
        }
        Ok(())
    }

    fn read(&mut self, read: Read, commands: &mut Vec<Command>) {
        self.reading -= 1;
        match read {
            Read::Small { entry, bytes } => {
                self.needed -= 1;
                self.add(entry, bytes, commands);
            }
            Read::Large {
                id,
                file,
                size,
                mtime,
            } => {
                // Opened, and sent from here on: the scan may go on.
                self.needed -= 1;
                let segments = block::segments(size) as usize;
                self.large.push_back(Large {
                    id,
                    file,
                    size,
                    mtime,
                    next: 0,
                    reading: 0,
                    values: vec![None; segments],
                    sent: 0,
                    failed: false,
                });
            }
            Read::Segment {
                id,
                offset,
                content,
                value,
            } => {
                self.segments -= 1;
                let index = self
                    .large
                    .iter()
                    .position(|file| file.id == id)
                    .expect("a segment of a file being read");
                let file = &mut self.large[index];
                file.reading -= 1;
                // What comes of a file that failed is dropped.
                let mut sent = None;
                if !file.failed {
                    let (len, check) = match content.unread {
                        None => {
                            file.values[(offset / block::SEGMENT) as usize] = Some(value);
                            file.sent += 1;
                            (content.bytes.len() as u32, Check::Hash(value))
                        }
                        Some(why) => {
                            file.failed = true;
                            (0, Check::Unread(why))
                        }
                    };
                    let segment = Segment {
                        file: id,
                        offset,
                        len,
                        size: file.size,
                        mtime: file.mtime,
                        check,
                    };
                    let hash = (file.sent == file.segments()).then(|| {
                        let values: Vec<[u8; 32]> = file
                            .values
                            .iter()
                            .map(|value| value.expect("every segment read"))
                            .collect();
                        FileHash {
                            file: id,
                            hash: block::root(&values, file.size),
                        }
                    });
                    sent = Some((segment, hash));
                }
                if file.done() {
                    self.large.remove(index);
                }
                if let Some((segment, hash)) = sent {
                    self.give(Body::Segment(segment), vec![content.bytes], commands);
                    commands.extend(hash.map(Command::Hash));
                }
            }
            Read::Unreadable { id, path, error } => {
                self.needed -= 1;
                commands.push(Command::Unreadable { id, path, error });
            }
        }
    }

    /// Hands a block over, if the credit allows, and the compressors have
    /// room, or keeps it until they do.
    fn give(&mut self, body: Body, bytes: Vec<Vec<u8>>, commands: &mut Vec<Command>) {
        let cost = block::cost(bytes.iter().map(Vec::len).sum());
        self.held.push_back((body, bytes, cost));
        self.release_held(commands);
    }

    /// The blocks kept go, in order, as far as the credit and the
    /// compressors' room allow.
    fn release_held(&mut self, commands: &mut Vec<Command>) {
        while self.compressing < COMPRESSING
            && self
                .held
                .front()
                .is_some_and(|(_, _, cost)| *cost <= self.credit)
        {
            let (body, bytes, cost) = self.held.pop_front().expect("one waits");
            self.credit -= cost;
            self.compressing += 1;
            commands.push(Command::Block(body, bytes));
        }
    }

    /// Hands the readers the next jobs, as long as they may have more:
    /// segments of files open, but no more than half the readers' while
    /// files wait to be opened, so that a large file never holds back the
    /// small ones behind it; and the files needed, in the order needed.
    /// None while blocks wait for credit: what they read would only wait
    /// too, in memory.
    fn dispatch(&mut self, commands: &mut Vec<Command>) {
        while self.reading < READING && self.held.is_empty() {
            let segments_may = self.waiting.is_empty() || self.segments < READING / 2;
            let large = segments_may
                .then(|| {
                    self.large
                        .iter_mut()
                        .find(|file| !file.failed && file.next < file.segments())
                })
                .flatten();
            if let Some(file) = large {
                let offset = file.next * block::SEGMENT;
                let len = block::SEGMENT.min(file.size - offset) as usize;
                file.next += 1;
                file.reading += 1;
                self.segments += 1;
                self.reading += 1;
                commands.push(Command::Read(Job::Segment {
                    id: file.id,
                    file: Arc::clone(&file.file),
                    offset,
                    len,
                }));
            } else if let Some((id, path)) = self.waiting.pop_front() {
                self.reading += 1;
                commands.push(Command::Read(Job::Open { id, path }));
            } else {
                break;
            }
        }
    }

    /// Puts a file in the pack, sealing the pack first if the file would
    /// take it past [`block::PACK`], and after if the file fills it.
    fn add(&mut self, entry: PackFile, bytes: Vec<u8>, commands: &mut Vec<Command>) {
        let cost = bytes.len() + entry_size(&entry);
        if !self.pack.files.is_empty() && self.pack.filled + cost > block::PACK {
            self.seal(commands);
        }
        self.pack.bytes.push(bytes);
        self.pack.files.push(entry);
        self.pack.filled += cost;
        if self.pack.filled >= block::PACK {
            self.seal(commands);
        }
    }

    /// Hands the pack over, if it holds anything.
    fn seal(&mut self, commands: &mut Vec<Command>) {
        if self.pack.files.is_empty() {
            return;
        }
        let Pack { files, bytes, .. } = std::mem::take(&mut self.pack);
        self.give(Body::Pack { files }, bytes, commands);
    }

    /// Once the needs have ended and every file needed is sent, or said it
    /// could not be read, the last pack goes, and the end.
    fn finish(&mut self, commands: &mut Vec<Command>) {
        let all_read = self.waiting.is_empty() && self.reading == 0 && self.large.is_empty();
        if self.sending && self.ended && all_read && !self.finished {
            self.seal(commands);
            if self.held.is_empty() {
                commands.push(Command::End);
                self.finished = true;
            }
        }
    }

    /// Lets the scan send its part, once it may: its entries are kept from
    /// then on, before the Destination can need any. Otherwise tells it why
    /// it waits.
    fn release(&mut self, commands: &mut Vec<Command>) {
        let Some(parked) = &mut self.parked else {
            return;
        };
        let backlog = self.needed > self.limits.backlog;
        let in_flight = self.sent.saturating_sub(self.through) > self.limits.in_flight;
        let why = match parked.part.wait {
            // Nothing more is waited for once the blocks have ended.
            _ if self.finished => None,
            Wait::Never => None,
            Wait::Backlog | Wait::InFlight if backlog => Some(profile::State::Backlog),
            Wait::InFlight if in_flight => Some(profile::State::InFlight),
            Wait::Backlog | Wait::InFlight => None,
        };
        if let Some(why) = why {
            if parked.told != Some(why) {
                parked.told = Some(why);
                commands.push(Command::Room(parked.part.room.clone(), Room::Wait(why)));
            }
            return;
        }
        let Parked { part, .. } = self.parked.take().expect("parked");
        if let Some(known) = part.known {
            self.sent = self.sent.max(known.end() - 1);
            self.known.record(known);
        }
        commands.push(Command::Room(part.room, Room::Go));
    }
}

/// The most room an entry takes in its pack's header: its file's ID, length
/// and mtime, and its hash, or why it could not be read.
fn entry_size(entry: &PackFile) -> usize {
    match &entry.check {
        Check::Hash(_) => 64,
        Check::Unread(why) => 64 + why.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;

    fn entry(id: FileId, name: &[u8], kind: Kind) -> manifest::Entry {
        manifest::Entry {
            id,
            name: name.to_vec(),
            kind,
            size: 0,
            mtime: MTIME,
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }

    const MTIME: Timestamp = Timestamp {
        seconds: 1,
        nanoseconds: 2,
    };

    #[test]
    fn known_entries_give_back_their_paths() {
        let mut known = Known::default();
        let mut record = |path: &[u8], entries: &[manifest::Entry]| {
            known.record(KnownPart::of(path, entries).unwrap())
        };
        record(b"", &[entry(0, b"", Kind::Directory)]);
        record(
            b"",
            &[entry(1, b"a", Kind::Directory), entry(2, b"f", Kind::File)],
        );
        // One directory in two parts.
        record(b"a", &[entry(3, b"x", Kind::File)]);
        record(b"a", &[entry(4, b"y", Kind::Fifo)]);
        record(b"a/z", &[entry(5, b"deep", Kind::File)]);

        assert_eq!(known.len(), 6);
        let paths: Vec<Vec<u8>> = (0..6).map(|id| known.path(id).unwrap()).collect();
        assert_eq!(
            paths,
            [&b""[..], b"a", b"f", b"a/x", b"a/y", b"a/z/deep"].map(<[u8]>::to_vec)
        );
        let files: Vec<bool> = (0..6).map(|id| known.is_file(id).unwrap()).collect();
        assert_eq!(files, [false, false, true, true, false, true]);
        assert_eq!(known.path(6), None);
        assert_eq!(known.is_file(6), None);

        // Compared through 3: the parts all of whose entries are, go; the
        // part of 3 alone goes too, and those after stay.
        known.compared_through(3);
        assert_eq!(known.parts.len(), 2);
        for id in 0..4 {
            assert_eq!(known.path(id), None, "{id}");
        }
        assert_eq!(known.path(4).unwrap(), b"a/y");
        assert_eq!(known.path(5).unwrap(), b"a/z/deep");
        // Through the middle of a part keeps it.
        known.compared_through(4);
        assert_eq!(known.path(5).unwrap(), b"a/z/deep");
        known.compared_through(5);
        assert!(known.parts.is_empty());
        assert_eq!(known.len(), 6);

        // Through the last ID there could be, as a Destination gone wrong may
        // say: all go, nothing overflows.
        let mut known = Known::default();
        known.record(KnownPart::of(b"", &[entry(0, b"", Kind::Directory)]).unwrap());
        known.record(KnownPart::of(b"", &[entry(1, b"f", Kind::File)]).unwrap());
        known.compared_through(FileId::MAX);
        assert!(known.parts.is_empty());
    }

    /// An owner of `limits`, sending files, with credit enough for any
    /// test.
    fn owner(backlog: usize, in_flight: FileId) -> Owner {
        let mut owner = Owner::new(Limits { backlog, in_flight }, true);
        assert!(owner.on(Event::Credit(1 << 40)).unwrap().is_empty());
        owner
    }

    /// A part of `entries` in `path`, waiting as `wait` says, and where the
    /// scan hears of it.
    fn part(path: &[u8], entries: &[manifest::Entry], wait: Wait) -> (Event, Receiver<Room>) {
        let (room, heard) = crossbeam_channel::unbounded();
        let part = Part {
            known: KnownPart::of(path, entries),
            wait,
            room,
        };
        (Event::Part(part), heard)
    }

    /// What the scan was told by `commands`.
    fn told(commands: Vec<Command>) -> Vec<Room> {
        commands
            .into_iter()
            .filter_map(|command| match command {
                Command::Room(room, said) => {
                    drop(room);
                    Some(said)
                }
                _ => None,
            })
            .collect()
    }

    /// The jobs handed to the readers by `commands`, as (ID, offset), an
    /// open at offset `u64::MAX`.
    fn jobs(commands: &[Command]) -> Vec<(FileId, u64)> {
        commands
            .iter()
            .filter_map(|command| match command {
                Command::Read(Job::Open { id, .. }) => Some((*id, u64::MAX)),
                Command::Read(Job::Segment { id, offset, .. }) => Some((*id, *offset)),
                _ => None,
            })
            .collect()
    }

    /// The blocks `commands` hands over: a pack's IDs, or a segment's ID and
    /// offset, and whether it is said unread.
    fn blocks(commands: &[Command]) -> Vec<(Vec<FileId>, Option<u64>, bool)> {
        commands
            .iter()
            .filter_map(|command| match command {
                Command::Block(Body::Pack { files }, _) => Some((
                    files.iter().map(|file| file.file).collect(),
                    None,
                    files
                        .iter()
                        .any(|file| matches!(file.check, Check::Unread(_))),
                )),
                Command::Block(Body::Segment(segment), _) => Some((
                    vec![segment.file],
                    Some(segment.offset),
                    matches!(segment.check, Check::Unread(_)),
                )),
                _ => None,
            })
            .collect()
    }

    fn small(id: FileId, bytes: &[u8]) -> Event {
        Event::Read(Read::Small {
            entry: PackFile {
                file: id,
                len: bytes.len() as u32,
                mtime: MTIME,
                check: Check::Hash(*blake3::hash(bytes).as_bytes()),
            },
            bytes: bytes.to_vec(),
        })
    }

    /// An owner that has sent the root and a part of `files` files, IDs 1 to
    /// `files`, at the root.
    fn with_files(owner: &mut Owner, files: u64) {
        let (root, _) = part(b"", &[entry(0, b"", Kind::Directory)], Wait::Never);
        assert_eq!(told(owner.on(root).unwrap()), [Room::Go]);
        let entries: Vec<manifest::Entry> = (1..=files)
            .map(|id| entry(id, format!("f{id:03}").as_bytes(), Kind::File))
            .collect();
        let (files, _) = part(b"", &entries, Wait::InFlight);
        assert_eq!(told(owner.on(files).unwrap()), [Room::Go]);
    }

    /// Files are read once needed, in the order needed, and go in a pack,
    /// sealed once nothing more is to be told; the blocks end once the
    /// needs have and every file is sent.
    #[test]
    fn files_needed_are_read_in_order_and_packed() {
        let mut owner = owner(100, 1000);
        with_files(&mut owner, 3);
        let commands = owner.on(Event::Needs(Needs::Files(vec![2, 1, 3]))).unwrap();
        assert_eq!(
            jobs(&commands),
            [(2, u64::MAX), (1, u64::MAX), (3, u64::MAX)]
        );
        assert!(blocks(&owner.on(small(2, b"two")).unwrap()).is_empty());
        assert!(blocks(&owner.on(small(1, b"one")).unwrap()).is_empty());
        assert_eq!(
            blocks(&owner.on(Event::Idle).unwrap()),
            [(vec![2, 1], None, false)]
        );
        let commands = owner.on(Event::Needs(Needs::End)).unwrap();
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, Command::End))
        );
        let commands = owner.on(small(3, b"three")).unwrap();
        assert_eq!(blocks(&commands), [(vec![3], None, false)]);
        assert!(matches!(commands.last(), Some(Command::End)));
        assert!(owner.is_finished());
    }

    /// A pack is sealed before a file that would take it past its size, and
    /// once a file fills it.
    #[test]
    fn a_pack_holds_no_more_than_its_size() {
        let mut owner = owner(100, 1000);
        with_files(&mut owner, 3);
        owner.on(Event::Needs(Needs::Files(vec![1, 2, 3]))).unwrap();
        let half = vec![7; block::PACK / 2];
        assert!(blocks(&owner.on(small(1, &half)).unwrap()).is_empty());
        // Half and its entry, and half again, would not fit.
        assert_eq!(
            blocks(&owner.on(small(2, &half)).unwrap()),
            [(vec![1], None, false)]
        );
        let full = vec![7; block::PACK];
        assert_eq!(
            blocks(&owner.on(small(3, &full)).unwrap()),
            [(vec![2], None, false), (vec![3], None, false)]
        );
    }

    /// The scan waits while more needed files than its limit wait to be
    /// read, and goes on once they are.
    #[test]
    fn the_scan_waits_while_too_many_files_wait_to_be_read() {
        let mut owner = owner(2, 1000);
        with_files(&mut owner, 4);
        owner
            .on(Event::Needs(Needs::Files(vec![1, 2, 3, 4])))
            .unwrap();
        let (next, _) = part(b"", &[entry(5, b"g", Kind::File)], Wait::Backlog);
        assert_eq!(
            told(owner.on(next).unwrap()),
            [Room::Wait(profile::State::Backlog)]
        );
        assert!(told(owner.on(small(1, b"1")).unwrap()).is_empty());
        assert_eq!(told(owner.on(small(2, b"2")).unwrap()), [Room::Go]);
        // Its entries are kept once it goes.
        assert!(owner.on(Event::Needs(Needs::Files(vec![5]))).is_ok());
    }

    /// Before a directory, the scan waits while more entries sent than its
    /// limit are not compared; within one, it does not, as the Destination
    /// says how far it compared only at a directory's end.
    #[test]
    fn the_scan_waits_before_a_directory_while_too_many_entries_wait_to_be_compared() {
        let mut owner = owner(100, 3);
        with_files(&mut owner, 4);
        let (within, _) = part(b"", &[entry(5, b"g", Kind::File)], Wait::Backlog);
        assert_eq!(told(owner.on(within).unwrap()), [Room::Go]);
        let (next, _) = part(b"d", &[entry(6, b"h", Kind::File)], Wait::InFlight);
        assert_eq!(
            told(owner.on(next).unwrap()),
            [Room::Wait(profile::State::InFlight)]
        );
        // Compared up to 2 leaves 3: room.
        let commands = owner.on(Event::Needs(Needs::Through(2))).unwrap();
        assert_eq!(told(commands), [Room::Go]);
    }

    /// The Destination needing what was never sent, as only one gone wrong
    /// does, fails the Source.
    #[test]
    fn an_unknown_id_is_refused() {
        let mut owner = owner(100, 1000);
        with_files(&mut owner, 1);
        let error = owner.on(Event::Needs(Needs::Files(vec![9]))).err().unwrap();
        assert_eq!(error.to_string(), "the Destination needs unknown ID 9");
    }

    /// A temporary file of `size` bytes, open, to stand for one being read.
    fn open_file(size: u64) -> Arc<File> {
        let file = tempfile::tempfile().unwrap();
        file.set_len(size).unwrap();
        Arc::new(file)
    }

    fn segment(id: FileId, offset: u64, bytes: &[u8], unread: Option<&str>) -> Event {
        Event::Read(Read::Segment {
            id,
            offset,
            content: Content {
                bytes: if unread.is_some() {
                    Vec::new()
                } else {
                    bytes.to_vec()
                },
                unread: unread.map(str::to_owned),
            },
            value: block::chaining_value(offset, bytes),
        })
    }

    /// A large file is read a segment at a time, by any reader, each sent
    /// as it is read, in whatever order, and then its hash, merged from
    /// theirs.
    #[test]
    fn a_large_file_goes_a_segment_at_a_time_then_its_hash() {
        let mut owner = owner(100, 1000);
        with_files(&mut owner, 1);
        owner.on(Event::Needs(Needs::Files(vec![1]))).unwrap();
        let content = crate::test_support::Random::new(5).bytes(2 * block::SEGMENT as usize + 9);
        let size = content.len() as u64;
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        let s = block::SEGMENT;
        assert_eq!(
            jobs(&owner.on(opened).unwrap()),
            [(1, 0), (1, s), (1, 2 * s)]
        );
        let piece =
            |index: u64| &content[(index * s) as usize..((index + 1) * s).min(size) as usize];
        owner.on(Event::Needs(Needs::End)).unwrap();
        for index in [2, 0] {
            let commands = owner.on(segment(1, index * s, piece(index), None)).unwrap();
            assert_eq!(blocks(&commands), [(vec![1], Some(index * s), false)]);
            assert!(!commands.iter().any(|c| matches!(c, Command::Hash(_))));
        }
        let commands = owner.on(segment(1, s, piece(1), None)).unwrap();
        assert_eq!(blocks(&commands), [(vec![1], Some(s), false)]);
        let hashes: Vec<&FileHash> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Hash(hash) => Some(hash),
                _ => None,
            })
            .collect();
        assert_eq!(
            hashes,
            [&FileHash {
                file: 1,
                hash: *blake3::hash(&content).as_bytes()
            }]
        );
        assert!(matches!(commands.last(), Some(Command::End)));
    }

    /// Of a large file a segment of which cannot be read whole, that segment
    /// says so, and nothing more is read, nor sent, nor its hash; the
    /// segments already with the readers are dropped as they come.
    #[test]
    fn a_segment_not_read_whole_ends_its_file() {
        let mut owner = owner(100, 1000);
        with_files(&mut owner, 2);
        owner.on(Event::Needs(Needs::Files(vec![1]))).unwrap();
        let s = block::SEGMENT;
        let size = 40 * s;
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        assert_eq!(jobs(&owner.on(opened).unwrap()).len(), READING);
        let commands = owner.on(segment(1, 0, b"x", Some("it shrank"))).unwrap();
        assert_eq!(blocks(&commands), [(vec![1], Some(0), true)]);
        // No segment of it more for the reader that is free.
        assert!(jobs(&commands).is_empty());
        let commands = owner.on(segment(1, s, &vec![1; s as usize], None)).unwrap();
        assert!(blocks(&commands).is_empty());
        // Once every reader has given its segment back, it is let go of, and
        // the blocks end.
        owner.on(Event::Needs(Needs::End)).unwrap();
        let mut ended = false;
        for index in 2..READING as u64 {
            let commands = owner
                .on(segment(1, index * s, &vec![1; s as usize], None))
                .unwrap();
            assert!(blocks(&commands).is_empty());
            assert!(!commands.iter().any(|c| matches!(c, Command::Hash(_))));
            ended = commands.iter().any(|c| matches!(c, Command::End));
        }
        assert!(ended && owner.is_finished());
    }

    /// While files wait to be opened, a large file's segments take no more
    /// than half the readers, so that it never holds back the small files
    /// behind it.
    #[test]
    fn small_files_are_read_beside_a_large_one() {
        let mut owner = owner(1000, 1000);
        with_files(&mut owner, 100);
        let needed: Vec<FileId> = (1..=100).collect();
        let commands = owner.on(Event::Needs(Needs::Files(needed))).unwrap();
        assert_eq!(jobs(&commands).len(), READING);
        let size = 100 * block::SEGMENT;
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        // The one reader free takes a segment.
        assert_eq!(jobs(&owner.on(opened).unwrap()), [(1, 0)]);
        let mut segments = 1;
        for id in 2..=40 {
            let commands = owner.on(small(id, b"s")).unwrap();
            for (job, _) in jobs(&commands) {
                if job == 1 {
                    segments += 1;
                }
            }
            assert!(segments <= READING / 2, "{segments} segments");
        }
        assert_eq!(segments, READING / 2);
    }

    /// A file that cannot be opened is reported, and the Source goes on.
    #[test]
    fn a_file_that_cannot_be_opened_is_reported() {
        let mut owner = owner(100, 1000);
        with_files(&mut owner, 1);
        owner.on(Event::Needs(Needs::Files(vec![1]))).unwrap();
        owner.on(Event::Needs(Needs::End)).unwrap();
        let commands = owner
            .on(Event::Read(Read::Unreadable {
                id: 1,
                path: b"f001".to_vec(),
                error: io::Error::other("gone"),
            }))
            .unwrap();
        assert!(matches!(
            &commands[..],
            [Command::Unreadable { id: 1, .. }, Command::End]
        ));
    }

    /// A dry run reads nothing, but checks what the Destination needs.
    #[test]
    fn a_dry_run_reads_nothing() {
        let mut owner = Owner::new(
            Limits {
                backlog: 1,
                in_flight: 1000,
            },
            false,
        );
        with_files(&mut owner, 3);
        let commands = owner.on(Event::Needs(Needs::Files(vec![1, 2, 3]))).unwrap();
        assert!(jobs(&commands).is_empty());
        let (next, _) = part(b"", &[entry(4, b"g", Kind::File)], Wait::Backlog);
        assert_eq!(told(owner.on(next).unwrap()), [Room::Go]);
        assert!(owner.on(Event::Needs(Needs::Files(vec![9]))).is_err());
    }

    /// An owner with no credit yet, as one starts.
    fn without_credit() -> Owner {
        Owner::new(
            Limits {
                backlog: 1000,
                in_flight: 1000,
            },
            true,
        )
    }

    /// With no credit, a block waits, and nothing more is read meanwhile;
    /// credit given, the block goes, and reading goes on.
    #[test]
    fn a_block_waits_for_credit_and_reading_with_it() {
        let mut owner = without_credit();
        with_files(&mut owner, 3);
        let commands = owner.on(Event::Needs(Needs::Files(vec![1, 2]))).unwrap();
        assert_eq!(jobs(&commands).len(), 2);
        owner.on(small(1, b"one")).unwrap();
        assert!(blocks(&owner.on(Event::Idle).unwrap()).is_empty());
        let commands = owner.on(Event::Needs(Needs::Files(vec![3]))).unwrap();
        assert!(jobs(&commands).is_empty(), "read while a block waits");
        let commands = owner.on(Event::Credit(block::cost(3))).unwrap();
        assert_eq!(blocks(&commands), [(vec![1], None, false)]);
        assert_eq!(jobs(&commands), [(3, u64::MAX)]);
    }

    /// Blocks wait in order: the first, if more than the credit, holds back
    /// the rest; each goes as the credit given back covers it.
    #[test]
    fn blocks_go_in_order_as_the_credit_covers_them() {
        let mut owner = without_credit();
        with_files(&mut owner, 2);
        owner.on(Event::Needs(Needs::Files(vec![1, 2]))).unwrap();
        let large = vec![1; 10_000];
        owner.on(small(1, &large)).unwrap();
        owner.on(Event::Idle).unwrap();
        owner.on(small(2, b"two")).unwrap();
        owner.on(Event::Idle).unwrap();
        // Enough for the second, not the first: neither goes.
        assert!(blocks(&owner.on(Event::Credit(block::cost(3))).unwrap()).is_empty());
        let commands = owner.on(Event::Credit(block::cost(10_000))).unwrap();
        assert_eq!(
            blocks(&commands),
            [(vec![1], None, false), (vec![2], None, false)]
        );
    }

    /// No more blocks than the compressors have room for are handed to
    /// them: the next waits until one is sent, and reading with it.
    #[test]
    fn blocks_wait_for_room_with_the_compressors() {
        let mut owner = owner(1000, 1000);
        let files = COMPRESSING as u64 + 2;
        with_files(&mut owner, files);
        owner
            .on(Event::Needs(Needs::Files((1..=files).collect())))
            .unwrap();
        let mut handed = 0;
        for id in 1..=files {
            owner.on(small(id, b"x")).unwrap();
            handed += blocks(&owner.on(Event::Idle).unwrap()).len();
        }
        assert_eq!(handed, COMPRESSING);
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(
            blocks(&commands),
            [(vec![COMPRESSING as u64 + 1], None, false)]
        );
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(
            blocks(&commands),
            [(vec![COMPRESSING as u64 + 2], None, false)]
        );
    }

    /// The blocks end only once the last block has gone, which waits for the
    /// credit as any other does.
    #[test]
    fn the_blocks_end_once_the_last_has_credit() {
        let mut owner = without_credit();
        with_files(&mut owner, 1);
        owner.on(Event::Needs(Needs::Files(vec![1]))).unwrap();
        owner.on(Event::Needs(Needs::End)).unwrap();
        let commands = owner.on(small(1, b"one")).unwrap();
        assert!(blocks(&commands).is_empty());
        assert!(!commands.iter().any(|c| matches!(c, Command::End)));
        assert!(!owner.is_finished());
        let commands = owner.on(Event::Credit(block::cost(3))).unwrap();
        assert_eq!(blocks(&commands), [(vec![1], None, false)]);
        assert!(matches!(commands.last(), Some(Command::End)));
        assert!(owner.is_finished());
    }

    /// A segment takes its share of the credit too; its file's hash goes as
    /// its last segment is read, sent or not, as the Destination keeps a
    /// hash until the segments come.
    #[test]
    fn segments_take_credit_as_packs_do() {
        let mut owner = without_credit();
        owner
            .on(Event::Credit(block::cost(block::SEGMENT as usize)))
            .unwrap();
        with_files(&mut owner, 1);
        owner.on(Event::Needs(Needs::Files(vec![1]))).unwrap();
        let content = crate::test_support::Random::new(7).bytes(block::SEGMENT as usize + 10);
        let size = content.len() as u64;
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        assert_eq!(jobs(&owner.on(opened).unwrap()).len(), 2);
        let s = block::SEGMENT as usize;
        let commands = owner.on(segment(1, 0, &content[..s], None)).unwrap();
        assert_eq!(blocks(&commands), [(vec![1], Some(0), false)]);
        // No credit left for the last: it waits.
        let commands = owner
            .on(segment(1, block::SEGMENT, &content[s..], None))
            .unwrap();
        assert!(blocks(&commands).is_empty());
        assert!(commands.iter().any(|c| matches!(c, Command::Hash(_))));
        let commands = owner.on(Event::Credit(block::cost(10))).unwrap();
        assert_eq!(blocks(&commands), [(vec![1], Some(block::SEGMENT), false)]);
    }

    /// How many small files a second the owner takes, alone, on events made
    /// up: each part of a tree of directories of 100 files of 4 KiB sent,
    /// its files needed, each read, and packed. Only the owner's time is
    /// counted. A measurement, not a check: `cargo test --release --lib
    /// owner_rate -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn owner_rate() {
        use std::time::{Duration, Instant};
        const DIRECTORIES: u64 = 10_000;
        let mut owner = owner(usize::MAX, FileId::MAX);
        let mut took = Duration::ZERO;
        // Each block is sent at once, as the block sender says.
        let mut timed = |owner: &mut Owner, event: Event| {
            let started = Instant::now();
            let mut commands = owner.on(event).unwrap();
            loop {
                let blocks = commands
                    .iter()
                    .filter(|command| matches!(command, Command::Block(..)))
                    .count();
                if blocks == 0 {
                    break;
                }
                commands = Vec::new();
                for _ in 0..blocks {
                    commands.extend(owner.on(Event::Sent).unwrap());
                }
            }
            took += started.elapsed();
        };
        let (root, _) = part(b"", &[entry(0, b"", Kind::Directory)], Wait::Never);
        timed(&mut owner, root);
        let mut next = 1;
        for directory in 0..DIRECTORIES {
            let entries: Vec<manifest::Entry> = (0..100)
                .map(|index| entry(next + index, format!("f{index:03}").as_bytes(), Kind::File))
                .collect();
            let path = format!("d{directory:05}").into_bytes();
            let (part, _) = part(&path, &entries, Wait::InFlight);
            timed(&mut owner, part);
            let ids: Vec<FileId> = (next..next + 100).collect();
            timed(&mut owner, Event::Needs(Needs::Files(ids)));
            let reads: Vec<Event> = (next..next + 100)
                .map(|id| {
                    Event::Read(Read::Small {
                        entry: PackFile {
                            file: id,
                            len: 4096,
                            mtime: MTIME,
                            check: Check::Hash([0; 32]),
                        },
                        bytes: vec![0; 4096],
                    })
                })
                .collect();
            for read in reads {
                timed(&mut owner, read);
            }
            timed(&mut owner, Event::Needs(Needs::Through(next + 99)));
            next += 100;
        }
        let files = DIRECTORIES * 100;
        println!(
            "{files} files in {took:.2?} of the owner's time: {:.0} files a second",
            files as f64 / took.as_secs_f64()
        );
    }
}
