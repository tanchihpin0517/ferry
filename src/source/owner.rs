//! The Source's owner: all the Source keeps of what to send, changed only by
//! the events it is told, one at a time, by one thread. What it decides is
//! said as commands, which the readers and the data's sender carry out. It does no I/O, and waits on nothing, so that what it does is
//! tested on events made up.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::sync::Arc;

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::block::{self, Check, Data};
use crate::manifest::{FileId, Timestamp};
use crate::profile;

use super::read::{Content, Job};

/// How many files are read at once, whatever their size: one by each
/// reader. Small files read sixteen at once came at most of the speed of
/// sixty-four at once, from a disk, an SSD, NFS and WekaFS; eight at 60% to
/// 100% of sixteen's, four at 34% to 70%, from the first three. Large ones
/// came about as fast four, eight or sixteen at once.
pub(super) const READERS: usize = 16;

/// How many files may be with the readers at once, read or waiting to be,
/// each with one job at a time: a small one read whole, a large one a
/// segment at a time. Twice the readers, so that each has its next job
/// ready, rather than wait for the owner to be told of its last. A job is
/// at most a segment, so 64 MiB at most.
pub(super) const READING: usize = 2 * READERS;

/// How many records of the data may be with its sender, not yet written on
/// the data's pipe. The data credit once let 64 MiB of packs wait there; so
/// many buffers, allocated and let go of, made a copy of a large file on one
/// machine a tenth slower, its page faults twice as many.
pub(super) const SENDING: usize = 16;

/// How far ahead of the Destination the scan may go.
pub(super) struct Limits {
    /// How many needed files may wait to be read.
    pub(super) backlog: usize,
}

/// When a batch of the manifest may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wait {
    /// At once: the root.
    Never,
    /// Once few enough needed files wait to be read: a directory's batch.
    Backlog,
}

/// What the owner tells the scan of its part.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Room {
    /// It may not go yet, for this reason, which the scan waits in.
    Wait(profile::State),
    /// It may go.
    Go,
}

/// A part of the manifest the scan has ready: when it may go, and where to
/// say so. The scan keeps its entries once it may, before it writes it.
pub(super) struct Part {
    pub(super) wait: Wait,
    pub(super) room: Sender<Room>,
}

/// What a reader did.
pub(super) enum Read {
    /// A file of at most a segment read whole, with its hash; or why it
    /// could not be, and none of its bytes.
    Whole {
        id: FileId,
        mtime: Timestamp,
        check: Check,
        bytes: Vec<u8>,
    },
    /// A file larger than a segment, open, to be read a segment at a time.
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
    /// Files the Destination needs, each with its path, in the order
    /// needed: only files, as only their content is sent.
    Needed(Vec<(FileId, Vec<u8>)>),
    /// The Destination's needs have ended.
    NeedsEnd,
    /// A reader is done.
    Read(Read),
    /// A record of the data is written on its pipe.
    Sent,
}

/// What the owner has done.
pub(super) enum Command {
    /// Tell the scan of its part.
    Room(Sender<Room>, Room),
    /// Hand a reader a job.
    Read(Job),
    /// Send a record of the data.
    Send(Data<Vec<u8>>),
    /// Report a file that could not be opened: the Destination is told
    /// among the data.
    Unreadable { path: Vec<u8>, error: io::Error },
    /// Every file needed is sent: end the data.
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

/// A part the scan waits to send, and what it was told of it.
struct Parked {
    part: Part,
    told: Option<profile::State>,
}

/// The Source's owner; see the module.
pub(super) struct Owner {
    limits: Limits,
    /// Files are sent: not in a dry run.
    sending: bool,
    /// Files needed and not opened yet, in the order needed, whatever their
    /// size.
    waiting: VecDeque<(FileId, Vec<u8>)>,
    /// How many files are with the readers, at most [`READING`]: a small one
    /// until it is read, a large one from when it is handed out to be opened
    /// until its last segment is read. Each has at most one job with them.
    reading: usize,
    /// Files being read a segment at a time, in the order opened.
    large: VecDeque<Large>,
    /// Files needed and not read yet, or not opened, for the scan to wait
    /// on.
    needed: usize,
    parked: Option<Parked>,
    /// The records of the data waiting for room with its sender.
    held: VecDeque<Data<Vec<u8>>>,
    /// How many records the sender has, not yet written.
    sending_records: usize,
    /// The Destination's needs have ended.
    ended: bool,
    /// The data has ended.
    finished: bool,
}

impl Owner {
    pub(super) fn new(limits: Limits, sending: bool) -> Self {
        Self {
            limits,
            sending,
            waiting: VecDeque::new(),
            reading: 0,
            large: VecDeque::new(),
            needed: 0,
            parked: None,
            held: VecDeque::new(),
            sending_records: 0,
            ended: false,
            finished: false,
        }
    }

    /// Whether the data has ended: there is nothing more to do.
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
            Event::Needed(needed) => {
                if self.sending {
                    self.needed += needed.len();
                    self.waiting.extend(needed);
                }
            }
            Event::NeedsEnd => self.ended = true,
            Event::Read(read) => self.read(read, &mut commands),
            Event::Sent => {
                self.sending_records -= 1;
                self.release_held(&mut commands);
            }
        }
        self.dispatch(&mut commands);
        self.finish(&mut commands);
        self.release(&mut commands);
        Ok(commands)
    }

    fn read(&mut self, read: Read, commands: &mut Vec<Command>) {
        match read {
            Read::Whole {
                id,
                mtime,
                check,
                bytes,
            } => {
                self.reading -= 1;
                self.needed -= 1;
                let record = Data::File {
                    id,
                    mtime,
                    check,
                    bytes,
                };
                self.give(record, commands);
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
                    let (bytes, check) = match content.unread {
                        None => {
                            file.values[(offset / block::SEGMENT) as usize] = Some(value);
                            file.sent += 1;
                            (content.bytes, Check::Hash(value))
                        }
                        Some(why) => {
                            file.failed = true;
                            (Vec::new(), Check::Unread(why))
                        }
                    };
                    let segment = Data::Segment {
                        id,
                        offset,
                        size: file.size,
                        mtime: file.mtime,
                        check,
                        bytes,
                    };
                    let hash = (file.sent == file.segments()).then(|| {
                        let values: Vec<[u8; 32]> = file
                            .values
                            .iter()
                            .map(|value| value.expect("every segment read"))
                            .collect();
                        Data::Hash {
                            file: id,
                            hash: block::root(&values, file.size),
                        }
                    });
                    sent = Some((segment, hash));
                }
                if file.done() {
                    self.large.remove(index);
                    self.reading -= 1;
                }
                if let Some((segment, hash)) = sent {
                    self.give(segment, commands);
                    if let Some(hash) = hash {
                        self.give(hash, commands);
                    }
                }
            }
            Read::Unreadable { id, path, error } => {
                self.reading -= 1;
                self.needed -= 1;
                // The Destination is told among the data, in its turn.
                let record = Data::Unopened {
                    file: id,
                    path: path.clone(),
                };
                commands.push(Command::Unreadable { path, error });
                self.give(record, commands);
            }
        }
    }

    /// Hands a record of the data over, if its sender has room, or keeps it
    /// until it does.
    fn give(&mut self, record: Data<Vec<u8>>, commands: &mut Vec<Command>) {
        self.held.push_back(record);
        self.release_held(commands);
    }

    /// The records kept go, in order, as far as the sender's room allows:
    /// it is made as each is written, which waits while the data's pipe can
    /// take no more.
    fn release_held(&mut self, commands: &mut Vec<Command>) {
        while self.sending_records < SENDING
            && let Some(record) = self.held.pop_front()
        {
            self.sending_records += 1;
            commands.push(Command::Send(record));
        }
    }

    /// Hands the readers the next jobs: up to [`READING`] files are with
    /// them at once, whatever their size, each with one job at a time, so
    /// that each is read in order, as the system reads ahead in any file
    /// read so: from a spinning disk, one file's segments read at once by
    /// several readers came at two thirds of its speed or less. The next
    /// segment of each large file open goes first, once the one before it
    /// is read, as the file was needed before those not opened yet; then
    /// the files needed, in the order needed. None while records wait for
    /// the sender: what they read would only wait too, in memory.
    fn dispatch(&mut self, commands: &mut Vec<Command>) {
        if !self.held.is_empty() {
            return;
        }
        for file in &mut self.large {
            if file.reading == 0 && !file.failed && file.next < file.segments() {
                let offset = file.next * block::SEGMENT;
                let len = block::SEGMENT.min(file.size - offset) as usize;
                file.next += 1;
                file.reading += 1;
                commands.push(Command::Read(Job::Segment {
                    id: file.id,
                    file: Arc::clone(&file.file),
                    offset,
                    len,
                }));
            }
        }
        while self.reading < READING
            && let Some((id, path)) = self.waiting.pop_front()
        {
            self.reading += 1;
            commands.push(Command::Read(Job::Open { id, path }));
        }
    }

    /// Once the needs have ended and every file needed is sent, or said it
    /// could not be read, the data ends.
    fn finish(&mut self, commands: &mut Vec<Command>) {
        let all_read = self.waiting.is_empty() && self.reading == 0;
        if self.sending && self.ended && all_read && !self.finished && self.held.is_empty() {
            commands.push(Command::End);
            self.finished = true;
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
        let why = match parked.part.wait {
            // Nothing more is waited for once the data has ended.
            _ if self.finished => None,
            Wait::Never => None,
            Wait::Backlog if backlog => Some(profile::State::Backlog),
            Wait::Backlog => None,
        };
        if let Some(why) = why {
            if parked.told != Some(why) {
                parked.told = Some(why);
                commands.push(Command::Room(parked.part.room.clone(), Room::Wait(why)));
            }
            return;
        }
        let Parked { part, .. } = self.parked.take().expect("parked");
        commands.push(Command::Room(part.room, Room::Go));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{self, Kind};
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

    /// A file entry of `size` bytes.
    fn sized(id: FileId, name: &[u8], size: u64) -> manifest::Entry {
        manifest::Entry {
            size,
            ..entry(id, name, Kind::File)
        }
    }

    const MTIME: Timestamp = Timestamp {
        seconds: 1,
        nanoseconds: 2,
    };

    /// An owner of `backlog`, sending files.
    fn owner(backlog: usize) -> Owner {
        Owner::new(Limits { backlog }, true)
    }

    /// A part of `entries` in `path`, waiting as `wait` says, and where the
    /// scan hears of it.
    fn part(path: &[u8], entries: &[manifest::Entry], wait: Wait) -> (Event, Receiver<Room>) {
        let (room, heard) = crossbeam_channel::unbounded();
        let _ = (path, entries);
        let part = Part { wait, room };
        (Event::Part(part), heard)
    }

    /// The files `ids` needed, each with its path, as what reads the answers
    /// finds them.
    fn needed(ids: impl IntoIterator<Item = FileId>) -> Event {
        Event::Needed(
            ids.into_iter()
                .map(|id| (id, format!("f{id:03}").into_bytes()))
                .collect(),
        )
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

    /// The records of files' bytes `commands` sends: a whole file's ID, or a
    /// segment's ID and offset, and whether it is said unread.
    fn sent(commands: &[Command]) -> Vec<(FileId, Option<u64>, bool)> {
        commands
            .iter()
            .filter_map(|command| match command {
                Command::Send(Data::File { id, check, .. }) => {
                    Some((*id, None, matches!(check, Check::Unread(_))))
                }
                Command::Send(Data::Segment {
                    id, offset, check, ..
                }) => Some((*id, Some(*offset), matches!(check, Check::Unread(_)))),
                _ => None,
            })
            .collect()
    }

    /// The hashes `commands` sends, of files in segments.
    fn hashes(commands: &[Command]) -> Vec<(FileId, [u8; 32])> {
        commands
            .iter()
            .filter_map(|command| match command {
                Command::Send(Data::Hash { file, hash }) => Some((*file, *hash)),
                _ => None,
            })
            .collect()
    }

    fn small(id: FileId, bytes: &[u8]) -> Event {
        Event::Read(Read::Whole {
            id,
            mtime: MTIME,
            check: Check::Hash(*blake3::hash(bytes).as_bytes()),
            bytes: bytes.to_vec(),
        })
    }

    /// An owner that has sent the root and a part of `files` files, IDs 1 to
    /// `files`, at the root, as the scan found them empty.
    fn with_files(owner: &mut Owner, files: u64) {
        with_sizes(owner, &vec![0; files as usize]);
    }

    /// An owner that has sent the root and a part of files of `sizes`, IDs
    /// from 1, at the root.
    fn with_sizes(owner: &mut Owner, sizes: &[u64]) {
        let (root, _) = part(b"", &[entry(0, b"", Kind::Directory)], Wait::Never);
        assert_eq!(told(owner.on(root).unwrap()), [Room::Go]);
        let entries: Vec<manifest::Entry> = (1..)
            .zip(sizes)
            .map(|(id, &size)| sized(id, format!("f{id:03}").as_bytes(), size))
            .collect();
        let (files, _) = part(b"", &entries, Wait::Backlog);
        assert_eq!(told(owner.on(files).unwrap()), [Room::Go]);
    }

    /// Files are read once needed, in the order needed, and each sent as it
    /// is read; the data ends once the needs have and every file is sent.
    #[test]
    fn files_needed_are_read_in_order_and_sent_as_read() {
        let mut owner = owner(100);
        with_files(&mut owner, 3);
        let commands = owner.on(needed(vec![2, 1, 3])).unwrap();
        assert_eq!(
            jobs(&commands),
            [(2, u64::MAX), (1, u64::MAX), (3, u64::MAX)]
        );
        assert_eq!(
            sent(&owner.on(small(2, b"two")).unwrap()),
            [(2, None, false)]
        );
        assert_eq!(
            sent(&owner.on(small(1, b"one")).unwrap()),
            [(1, None, false)]
        );
        let commands = owner.on(Event::NeedsEnd).unwrap();
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, Command::End))
        );
        let commands = owner.on(small(3, b"three")).unwrap();
        assert_eq!(sent(&commands), [(3, None, false)]);
        assert!(matches!(commands.last(), Some(Command::End)));
        assert!(owner.is_finished());
    }

    /// The scan waits while more needed files than its limit wait to be
    /// read, and goes on once they are.
    #[test]
    fn the_scan_waits_while_too_many_files_wait_to_be_read() {
        let mut owner = owner(2);
        with_files(&mut owner, 4);
        owner.on(needed(vec![1, 2, 3, 4])).unwrap();
        let (next, _) = part(b"", &[entry(5, b"g", Kind::File)], Wait::Backlog);
        assert_eq!(
            told(owner.on(next).unwrap()),
            [Room::Wait(profile::State::Backlog)]
        );
        assert!(told(owner.on(small(1, b"1")).unwrap()).is_empty());
        assert_eq!(told(owner.on(small(2, b"2")).unwrap()), [Room::Go]);
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

    /// A large file is read a segment at a time, in order, each sent as it
    /// is read, and then its hash, merged from theirs.
    #[test]
    fn a_large_file_goes_a_segment_at_a_time_in_order_then_its_hash() {
        let mut owner = owner(100);
        let content = crate::test_support::Random::new(5).bytes(2 * block::SEGMENT as usize + 9);
        let size = content.len() as u64;
        with_sizes(&mut owner, &[size]);
        let commands = owner.on(needed(vec![1])).unwrap();
        assert_eq!(jobs(&commands), [(1, u64::MAX)]);
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        let s = block::SEGMENT;
        assert_eq!(jobs(&owner.on(opened).unwrap()), [(1, 0)]);
        let piece =
            |index: u64| &content[(index * s) as usize..((index + 1) * s).min(size) as usize];
        owner.on(Event::NeedsEnd).unwrap();
        // The next segment once the one before it is read.
        for index in [0, 1] {
            let commands = owner.on(segment(1, index * s, piece(index), None)).unwrap();
            assert_eq!(sent(&commands), [(1, Some(index * s), false)]);
            assert_eq!(jobs(&commands), [(1, (index + 1) * s)]);
            assert!(hashes(&commands).is_empty());
        }
        let commands = owner.on(segment(1, 2 * s, piece(2), None)).unwrap();
        assert_eq!(sent(&commands), [(1, Some(2 * s), false)]);
        assert_eq!(hashes(&commands), [(1, *blake3::hash(&content).as_bytes())]);
        assert!(matches!(commands.last(), Some(Command::End)));
    }

    /// Of a large file a segment of which cannot be read whole, that segment
    /// says so, and nothing more of it is read, nor sent, nor its hash.
    #[test]
    fn a_segment_not_read_whole_ends_its_file() {
        let mut owner = owner(100);
        let s = block::SEGMENT;
        let size = 40 * s;
        with_sizes(&mut owner, &[size]);
        owner.on(needed(vec![1])).unwrap();
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        assert_eq!(jobs(&owner.on(opened).unwrap()), [(1, 0)]);
        let commands = owner.on(segment(1, 0, &vec![1; s as usize], None)).unwrap();
        assert_eq!(jobs(&commands), [(1, s)]);
        owner.on(Event::NeedsEnd).unwrap();
        let commands = owner.on(segment(1, s, b"x", Some("it shrank"))).unwrap();
        assert_eq!(sent(&commands), [(1, Some(s), true)]);
        assert!(jobs(&commands).is_empty());
        assert!(hashes(&commands).is_empty());
        // It is let go of, and the data ends.
        assert!(matches!(commands.last(), Some(Command::End)));
        assert!(owner.is_finished());
    }

    /// Up to [`READING`] files are with the readers at once, whatever their
    /// size: small ones, each read whole, and large ones, each a segment at
    /// a time.
    #[test]
    fn up_to_reading_files_are_with_the_readers_whatever_their_size() {
        let mut small = owner(1000);
        with_files(&mut small, 100);
        let commands = small.on(needed(1..=100)).unwrap();
        assert_eq!(jobs(&commands).len(), READING);

        let size = 3 * block::SEGMENT + 1;
        let mut large = owner(1000);
        with_sizes(&mut large, &vec![size; 40]);
        let commands = large.on(needed(1..=40)).unwrap();
        assert_eq!(jobs(&commands).len(), READING);
        // Each opened: its first segment, and no file more, as each is
        // still being read.
        for id in 1..=READING as FileId {
            let commands = large
                .on(Event::Read(Read::Large {
                    id,
                    file: open_file(size),
                    size,
                    mtime: MTIME,
                }))
                .unwrap();
            assert_eq!(jobs(&commands), [(id, 0)]);
        }
    }

    /// Large files and small ones needed mixed, against readers that finish
    /// each job in the order handed out: no more than [`READING`] files are
    /// with the readers at once, whatever their size; each has at most one
    /// job with them, so that a large file's segments are read one at a
    /// time, in order; files are opened in the order needed; several large
    /// files are read at once; and every file is sent.
    #[test]
    fn files_are_read_in_the_order_needed_each_a_job_at_a_time() {
        /// Tells `owner` of `event`, and has each record it hands over sent at
        /// once; the jobs it hands out go to `with_readers`, and to `handed`
        /// as (ID, offset), an open at offset `u64::MAX`.
        fn tell(
            owner: &mut Owner,
            event: Event,
            with_readers: &mut VecDeque<Job>,
            handed: &mut Vec<(FileId, u64)>,
        ) {
            let mut events = vec![event];
            while let Some(event) = events.pop() {
                for command in owner.on(event).unwrap() {
                    match command {
                        Command::Read(job) => {
                            handed.push(match &job {
                                Job::Open { id, .. } => (*id, u64::MAX),
                                Job::Segment { id, offset, .. } => (*id, *offset),
                            });
                            with_readers.push_back(job);
                        }
                        Command::Send(..) => events.push(Event::Sent),
                        _ => {}
                    }
                }
            }
        }
        let s = block::SEGMENT;
        let sizes: Vec<u64> = (0..90)
            .map(|index| if index % 3 == 0 { 3 * s + 1 } else { 100 })
            .collect();
        let is_large = |id: FileId| sizes[id as usize - 1] > s;
        let mut owner = owner(1000);
        with_sizes(&mut owner, &sizes);
        let (mut with_readers, mut handed) = (VecDeque::new(), Vec::new());
        tell(
            &mut owner,
            needed(1..=sizes.len() as FileId),
            &mut with_readers,
            &mut handed,
        );
        tell(&mut owner, Event::NeedsEnd, &mut with_readers, &mut handed);
        let mut most_large = 0;
        while let Some(job) = with_readers.pop_front() {
            assert!(owner.reading <= READING, "{} files read", owner.reading);
            assert!(
                with_readers.len() < READING,
                "{} jobs",
                with_readers.len() + 1
            );
            most_large = most_large.max(owner.large.len());
            let event = match job {
                Job::Open { id, .. } if is_large(id) => {
                    let size = sizes[id as usize - 1];
                    Event::Read(Read::Large {
                        id,
                        file: open_file(size),
                        size,
                        mtime: MTIME,
                    })
                }
                Job::Open { id, .. } => small(id, &[1; 100]),
                Job::Segment {
                    id, offset, len, ..
                } => {
                    let another = with_readers.iter().any(
                        |job| matches!(job, Job::Segment { id: of, .. } | Job::Open { id: of, .. } if *of == id),
                    );
                    assert!(!another, "two jobs of {id} with the readers");
                    segment(id, offset, &vec![1; len], None)
                }
            };
            tell(&mut owner, event, &mut with_readers, &mut handed);
        }
        assert!(owner.is_finished());
        assert!(most_large > 1, "{most_large} large files read at once");
        // Every file opened once, in the order needed.
        let opened: Vec<FileId> = handed
            .iter()
            .filter(|&&(_, offset)| offset == u64::MAX)
            .map(|&(id, _)| id)
            .collect();
        assert_eq!(opened, (1..=sizes.len() as FileId).collect::<Vec<_>>());
        // Each large file's segments, in order.
        for id in (1..=sizes.len() as FileId).filter(|&id| is_large(id)) {
            let offsets: Vec<u64> = handed
                .iter()
                .filter(|&&(of, offset)| of == id && offset != u64::MAX)
                .map(|&(_, offset)| offset)
                .collect();
            assert_eq!(
                offsets,
                (0..4).map(|index| index * s).collect::<Vec<_>>(),
                "{id}"
            );
        }
    }

    /// A file that cannot be opened is reported, and the Source goes on.
    #[test]
    fn a_file_that_cannot_be_opened_is_reported() {
        let mut owner = owner(100);
        with_files(&mut owner, 1);
        owner.on(needed(vec![1])).unwrap();
        owner.on(Event::NeedsEnd).unwrap();
        let commands = owner
            .on(Event::Read(Read::Unreadable {
                id: 1,
                path: b"f001".to_vec(),
                error: io::Error::other("gone"),
            }))
            .unwrap();
        assert!(matches!(
            &commands[..],
            [
                Command::Unreadable { .. },
                Command::Send(Data::Unopened { file: 1, .. }),
                Command::End
            ]
        ));
    }

    /// A dry run reads nothing, but checks what the Destination needs.
    #[test]
    fn a_dry_run_reads_nothing() {
        let mut owner = Owner::new(Limits { backlog: 1 }, false);
        with_files(&mut owner, 3);
        let commands = owner.on(needed(vec![1, 2, 3])).unwrap();
        assert!(jobs(&commands).is_empty());
        let (next, _) = part(b"", &[entry(4, b"g", Kind::File)], Wait::Backlog);
        assert_eq!(told(owner.on(next).unwrap()), [Room::Go]);
    }

    /// An owner whose data's sender has no room: what it hands it waits for
    /// a record to be written.
    fn sender_full() -> Owner {
        let mut owner = owner(1000);
        owner.sending_records = SENDING;
        owner
    }

    /// With the sender full, a record waits, and nothing more is read
    /// meanwhile; room made, the record goes, and reading goes on.
    #[test]
    fn a_record_waits_for_the_sender_and_reading_with_it() {
        let mut owner = sender_full();
        with_files(&mut owner, 3);
        let commands = owner.on(needed(vec![1, 2])).unwrap();
        assert_eq!(jobs(&commands).len(), 2);
        assert!(sent(&owner.on(small(1, b"one")).unwrap()).is_empty());
        let commands = owner.on(needed(vec![3])).unwrap();
        assert!(jobs(&commands).is_empty(), "read while a record waits");
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(sent(&commands), [(1, None, false)]);
        assert_eq!(jobs(&commands), [(3, u64::MAX)]);
    }

    /// No more records than the sender has room for are handed to it: the
    /// next waits until one is written, and reading with it.
    #[test]
    fn records_wait_for_room_with_the_sender() {
        let mut owner = owner(1000);
        let files = SENDING as u64 + 2;
        with_files(&mut owner, files);
        owner.on(needed(1..=files)).unwrap();
        let mut handed = 0;
        for id in 1..=files {
            handed += sent(&owner.on(small(id, b"x")).unwrap()).len();
        }
        assert_eq!(handed, SENDING);
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(sent(&commands), [(SENDING as u64 + 1, None, false)]);
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(sent(&commands), [(SENDING as u64 + 2, None, false)]);
    }

    /// The data ends only once the last record has gone, which waits for
    /// the sender as any other does.
    #[test]
    fn the_data_ends_once_the_last_record_has_gone() {
        let mut owner = sender_full();
        with_files(&mut owner, 1);
        owner.on(needed(vec![1])).unwrap();
        owner.on(Event::NeedsEnd).unwrap();
        let commands = owner.on(small(1, b"one")).unwrap();
        assert!(sent(&commands).is_empty());
        assert!(!commands.iter().any(|c| matches!(c, Command::End)));
        assert!(!owner.is_finished());
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(sent(&commands), [(1, None, false)]);
        assert!(matches!(commands.last(), Some(Command::End)));
        assert!(owner.is_finished());
    }

    /// A segment waits for the sender as a whole file does, and its file's
    /// hash after it, in their order.
    #[test]
    fn segments_and_their_hash_wait_for_the_sender_in_order() {
        let mut owner = owner(1000);
        owner.sending_records = SENDING - 1;
        with_files(&mut owner, 1);
        owner.on(needed(vec![1])).unwrap();
        let content = crate::test_support::Random::new(7).bytes(block::SEGMENT as usize + 10);
        let size = content.len() as u64;
        let opened = Event::Read(Read::Large {
            id: 1,
            file: open_file(size),
            size,
            mtime: MTIME,
        });
        assert_eq!(jobs(&owner.on(opened).unwrap()), [(1, 0)]);
        let s = block::SEGMENT as usize;
        let commands = owner.on(segment(1, 0, &content[..s], None)).unwrap();
        assert_eq!(sent(&commands), [(1, Some(0), false)]);
        assert_eq!(jobs(&commands), [(1, block::SEGMENT)]);
        // No room left for the last, nor its hash: they wait.
        let commands = owner
            .on(segment(1, block::SEGMENT, &content[s..], None))
            .unwrap();
        assert!(sent(&commands).is_empty() && hashes(&commands).is_empty());
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(sent(&commands), [(1, Some(block::SEGMENT), false)]);
        let commands = owner.on(Event::Sent).unwrap();
        assert_eq!(hashes(&commands).len(), 1);
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
        let mut owner = owner(usize::MAX);
        let mut took = Duration::ZERO;
        // Each record is sent at once, as the data's sender says.
        let mut timed = |owner: &mut Owner, event: Event| {
            let started = Instant::now();
            let mut commands = owner.on(event).unwrap();
            loop {
                let records = commands
                    .iter()
                    .filter(|command| matches!(command, Command::Send(..)))
                    .count();
                if records == 0 {
                    break;
                }
                commands = Vec::new();
                for _ in 0..records {
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
            let (part, _) = part(&path, &entries, Wait::Backlog);
            timed(&mut owner, part);
            let ids: Vec<FileId> = (next..next + 100).collect();
            timed(&mut owner, needed(ids));
            let reads: Vec<Event> = (next..next + 100)
                .map(|id| {
                    Event::Read(Read::Whole {
                        id,
                        mtime: MTIME,
                        check: Check::Hash([0; 32]),
                        bytes: vec![0; 4096],
                    })
                })
                .collect();
            for read in reads {
                timed(&mut owner, read);
            }
            next += 100;
        }
        let files = DIRECTORIES * 100;
        println!(
            "{files} files in {took:.2?} of the owner's time: {:.0} files a second",
            files as f64 / took.as_secs_f64()
        );
    }
}
