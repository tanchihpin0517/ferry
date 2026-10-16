//! The Source peer: scans its tree, sends the manifest, and sends the files
//! the Destination says it needs.
//!
//! Three things run at once: the scan sends the manifest, the main thread
//! reads the Destination's answers, and a sender thread streams the needed
//! files. They share the connection's writer a packet at a time.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::frame;
use crate::fsys;
use crate::manifest::{self, FileId, Kind, Manifest, Timestamp};
use crate::mux::{self, Mux};
use crate::names::{self, Database};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::{self, Control, Needs, PacketType, Results};
use crate::record::{self, Header, Trailer};
use crate::report::{Count, Reporter, Stage};
use crate::scan::{self, Sink};

/// How many bytes a file is read in at a time.
const READ_CHUNK: usize = 256 * 1024;

/// What a run found, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    pub entries: u64,
    pub needed: u64,
    /// Paths on the Source that could not be read.
    pub problems: u64,
    /// Entries the Destination could not make match.
    pub failed: u64,
}

/// Each entry the Source has sent in the manifest, by ID: whether it is a
/// file, and its path, to answer the Destination's needs. A tree may hold
/// millions of entries, so paths are kept compactly: each directory's path
/// once, and every name in one buffer.
#[derive(Default)]
struct Known {
    directories: Vec<Vec<u8>>,
    names: Vec<u8>,
    entries: Vec<KnownEntry>,
}

struct KnownEntry {
    /// Its directory, in `directories`.
    directory: u32,
    /// Where its name ends in `names`; it starts where the one before ends.
    name_end: u64,
    is_file: bool,
}

impl Known {
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Records `entries` of the directory at `path`, in ID order. The root
    /// is the one entry of the directory `` with the name ``.
    fn record(&mut self, path: &[u8], entries: &[manifest::Entry]) {
        // A directory's parts come one after another.
        if self.directories.last().is_none_or(|last| last != path) {
            self.directories.push(path.to_vec());
        }
        let directory =
            u32::try_from(self.directories.len() - 1).expect("fewer than 2^32 directories");
        for entry in entries {
            debug_assert_eq!(
                entry.id as usize,
                self.entries.len(),
                "IDs follow manifest order"
            );
            self.names.extend_from_slice(&entry.name);
            self.entries.push(KnownEntry {
                directory,
                name_end: self.names.len() as u64,
                is_file: entry.kind == Kind::File,
            });
        }
    }

    /// Whether the entry `id` is a file, if there is one.
    fn is_file(&self, id: FileId) -> Option<bool> {
        self.entries.get(id as usize).map(|entry| entry.is_file)
    }

    /// The path of the entry `id`, if there is one.
    fn path(&self, id: FileId) -> Option<Vec<u8>> {
        let index = id as usize;
        let entry = self.entries.get(index)?;
        let start = match index {
            0 => 0,
            _ => self.entries[index - 1].name_end as usize,
        };
        let name = &self.names[start..entry.name_end as usize];
        Some(manifest::join(
            &self.directories[entry.directory as usize],
            name,
        ))
    }
}

pub struct Options<'a> {
    /// The Source's root.
    pub root: &'a OsStr,
    pub dry_run: bool,
    /// Send no owners' names.
    pub numeric_ids: bool,
    /// Compress the record stream.
    pub compress: bool,
    /// The zstd level to compress at.
    pub compress_level: i32,
    /// Report what goes wrong, to the client, in report packets.
    pub report: bool,
}

/// A path as the client prints it: `.` for the root, `/` after a directory.
pub fn display(path: &[u8], is_directory: bool) -> Vec<u8> {
    let mut shown = if path.is_empty() {
        b".".to_vec()
    } else {
        path.to_vec()
    };
    if is_directory && !path.is_empty() {
        shown.push(b'/');
    }
    shown
}

/// Checks that the Source's root, a directory or a single file, exists. It is
/// checked while preparing, so that a missing source fails before anything
/// changes.
pub fn check_root(root: &OsStr) -> Result<()> {
    let path = std::path::Path::new(root);
    // As the scan stats it: through a symlink.
    match fsys::stat_root(root) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("the source {} does not exist", path.display())
        }
        Err(error) => {
            Err(error).with_context(|| format!("cannot read the source {}", path.display()))
        }
    }
}

/// Runs the Source over a connection. A dry run sends nothing but the
/// manifest. Owners' names are sent unless `numeric_ids`. The Source reports
/// only the paths it cannot read; the Destination reports the rest.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
) -> Result<Summary> {
    let Options {
        root,
        dry_run,
        numeric_ids,
        compress,
        compress_level,
        report,
    } = *options;
    let known: Mutex<Known> = Mutex::new(Known::default());
    let known = &known;

    std::thread::scope(|scope| -> Result<Summary> {
        let (mux, outlet) = mux::channel();
        let pump = scope.spawn(move || mux::pump(writer, outlet));
        let reporter = if report {
            Reporter::to_mux(mux.clone())
        } else {
            Reporter::none()
        };
        // Each thread has its own handle on the pump, which ends once every
        // handle is dropped.
        let scanner = {
            let mux = mux.clone();
            let reporter = reporter.clone();
            scope.spawn(move || -> Result<u64> {
                let _thread = profile::thread("manifest");
                let mut sink = ManifestSink {
                    mux: &mux,
                    known,
                    problems: 0,
                    reporter,
                    numeric_ids,
                    named_users: HashSet::new(),
                    named_groups: HashSet::new(),
                };
                // The Destination waits for the rest of the manifest, and
                // this side for its needs: a scan that fails says so at once,
                // so that both stop, rather than leaving them waiting.
                if let Err(error) = scan::scan(root, &mut sink) {
                    let _ = mux.message(&Control::Error {
                        message: format!("{error:#}"),
                    });
                    return Err(error);
                }
                Ok(sink.problems)
            })
        };
        let (jobs, sender) = if dry_run {
            (None, None)
        } else {
            let (jobs, queue) = mpsc::channel();
            // Frames are compressed by a pool, and sent in order by a thread
            // of their own.
            reporter.watch(Stage::Network, &mux.data_gauge());
            let (frames, compressed) = pool::gauged(
                scope,
                pool::threads(8),
                16,
                Some(reporter.gauge(Stage::Compress, 16)),
                Profiled {
                    role: "compress",
                    work: State::Compress,
                    full: State::ToCompress,
                },
                move |raw: Vec<u8>| frame::encode(&raw, compress, compress_level),
            );
            let sending = {
                let mux = mux.clone();
                scope.spawn(move || -> Result<()> {
                    let _thread = profile::thread("frame sender");
                    for frame in compressed {
                        mux.data(&frame?)?;
                    }
                    Ok(())
                })
            };
            let mux = mux.clone();
            let readers_jobs = jobs.clone();
            let reporter = reporter.clone();
            let sender = scope.spawn(move || -> Result<u64> {
                let _thread = profile::thread("file sender");
                let problems =
                    send_files(root, known, &mux, frames, readers_jobs, queue, &reporter);
                // The frames end with the sender; the sending thread then ends
                // too, once it has handed every frame over.
                let sent = sending.join().expect("the frame sender panicked");
                let problems = problems?;
                sent?;
                Ok(problems)
            });
            (Some(jobs), Some(sender))
        };
        drop(mux);

        let mut summary = Summary::default();
        let stop = jobs.clone();
        let received = receive(reader, known, jobs, &mut summary);
        if let Some(stop) = stop {
            let _ = stop.send(Job::Stop);
        }
        let problems = scanner.join().expect("the scan panicked");
        let sent = sender.map(|sender| sender.join().expect("the file sender panicked"));
        reporter.close();
        // With every handle dropped, the pump writes what is queued and ends.
        let pumped = pump.join().expect("the pump panicked");
        // A failed scan is why the rest stopped.
        let problems = problems?;
        received?;
        pumped?;
        summary.problems = problems + sent.transpose()?.unwrap_or(0);
        summary.entries = known.lock().expect("not poisoned").len() as u64;
        Ok(summary)
    })
}

/// Reads the Destination's answers until it is done: the needs, in a dry run,
/// or the needs, the results, and then its report that it wrote everything.
fn receive(
    reader: &mut impl Read,
    known: &Mutex<Known>,
    mut jobs: Option<Sender<Job>>,
    summary: &mut Summary,
) -> Result<()> {
    let dry_run = jobs.is_none();
    // The sender stops only on an error, which it reports itself, so a job it
    // can no longer take is dropped.
    let give = |jobs: &Option<Sender<Job>>, job: Job| {
        if let Some(jobs) = jobs {
            let _ = jobs.send(job);
        }
    };
    let _thread = profile::thread("answers");
    loop {
        let Some(packet) = profile::wait(State::ReadConnection, || protocol::receive(reader))?
        else {
            bail!("the connection closed before the transfer finished");
        };
        match packet.kind {
            PacketType::Needs => match packet.decode::<Needs>()? {
                Needs::Files(needed) => {
                    for id in needed {
                        let is_file = known
                            .lock()
                            .expect("not poisoned")
                            .is_file(id)
                            .with_context(|| format!("the Destination needs unknown ID {id}"))?;
                        summary.needed += 1;
                        // Only files' content is sent; the Destination makes
                        // the rest itself.
                        if is_file {
                            give(&jobs, Job::Send(id));
                        }
                    }
                }
                Needs::Attributes(changed) => summary.needed += changed.len() as u64,
                Needs::Deletes(_) => {}
                Needs::End if dry_run => return Ok(()),
                Needs::End => give(&jobs, Job::PassEnd),
            },
            // The Destination reports what failed, and its warnings, itself.
            PacketType::Results => match packet.decode::<Results>()? {
                Results::Failed { .. } | Results::NotDeleted { .. } => summary.failed += 1,
                Results::Resend(again) => {
                    give(&jobs, Job::Resend(again));
                    // The stream ends after these, so the sender needs nothing
                    // more.
                    jobs = None;
                }
            },
            PacketType::Control => match packet.decode::<Control>()? {
                Control::Done => return Ok(()),
                Control::Error { message } => bail!("the Destination failed: {message}"),
                other => bail!("unexpected {other:?} from the Destination"),
            },
            other => bail!("unexpected {other:?} packet from the Destination"),
        }
    }
}

/// Sends the scan's manifest, and remembers each entry by ID.
struct ManifestSink<'a> {
    mux: &'a Mux,
    known: &'a Mutex<Known>,
    problems: u64,
    reporter: Reporter,
    numeric_ids: bool,
    /// IDs whose names have been sent, or looked up and found to have none.
    named_users: HashSet<u32>,
    named_groups: HashSet<u32>,
}

impl ManifestSink<'_> {
    /// The names of the IDs in `entries` not named before, to send before
    /// them.
    fn new_names(&mut self, entries: &[manifest::Entry]) -> Option<Manifest> {
        if self.numeric_ids {
            return None;
        }
        let mut users: Vec<u32> = entries.iter().map(|entry| entry.uid).collect();
        let mut groups: Vec<u32> = entries.iter().map(|entry| entry.gid).collect();
        users.retain(|uid| self.named_users.insert(*uid));
        groups.retain(|gid| self.named_groups.insert(*gid));
        if users.is_empty() && groups.is_empty() {
            return None;
        }
        let _names = profile::enter(State::Names);
        let users: Vec<(u32, String)> = names::names(Database::Users, &users).into_iter().collect();
        let groups: Vec<(u32, String)> = names::names(Database::Groups, &groups)
            .into_iter()
            .collect();
        (!users.is_empty() || !groups.is_empty()).then_some(Manifest::Names { users, groups })
    }
}

impl Sink for ManifestSink<'_> {
    fn manifest(&mut self, message: Manifest) -> Result<()> {
        let names = match &message {
            Manifest::Root(entry) => self.new_names(std::slice::from_ref(entry)),
            Manifest::Directory { entries, .. } => self.new_names(entries),
            _ => None,
        };
        // The scan may be faster than the connection: the manifest waits for
        // room rather than piling up.
        if let Some(names) = names {
            self.mux.message_paced(&names)?;
        }
        match &message {
            Manifest::Root(entry) => self
                .known
                .lock()
                .expect("not poisoned")
                .record(b"", std::slice::from_ref(entry)),
            Manifest::Directory { path, entries, .. } => self
                .known
                .lock()
                .expect("not poisoned")
                .record(path, entries),
            Manifest::Names { .. } | Manifest::Problem { .. } | Manifest::End => {}
        }
        // Each directory goes out as soon as it is read, so the Destination
        // compares while the scan goes on.
        self.mux.message_paced(&message)?;
        Ok(())
    }

    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()> {
        self.problems += 1;
        self.reporter.say(format!(
            "cannot read {}: {error}",
            String::from_utf8_lossy(&display(path, false))
        ));
        self.reporter.add(Count::Problems, 1);
        // The Destination must know, so as not to take what is missing for
        // deleted.
        let problem = Manifest::Problem {
            path: path.to_vec(),
        };
        self.mux.message(&problem)?;
        Ok(())
    }
}

/// What the file sender does next.
enum Job {
    Send(FileId),
    /// The needs are over: end the first pass.
    PassEnd,
    /// The files to send again; the stream ends after them.
    Resend(Vec<FileId>),
    /// A reader has a file ready.
    Ready(Ready),
    /// The transfer is over, or failed: stop at once. The readers keep the
    /// queue open, so its closing cannot say so.
    Stop,
}

/// A file a reader has prepared.
enum Ready {
    /// A small file, read whole: its record, ready to go.
    Record(Vec<u8>),
    /// A large file, open, for the sender to stream itself.
    Large {
        id: FileId,
        file: File,
        size: u64,
        mtime: Timestamp,
    },
    /// A file that could not be opened.
    Unreadable { path: Vec<u8>, error: io::Error },
}

/// Files up to this size are read whole by the readers, ahead of the stream.
const SMALL: u64 = 1024 * 1024;

/// How many files may be with the readers, or read and waiting, at once.
const IN_FLIGHT: usize = 64;

/// Streams the record of each needed file; ends the first pass when the needs
/// end; sends again the files the Destination asks for; and ends the stream.
/// A file that cannot be read is reported and skipped. Returns how many could
/// not be read.
///
/// A pool of readers opens the files, and reads small ones whole, ahead of
/// the stream, in parallel; records go into the stream in whatever order they
/// are ready, since each carries its ID. A large file is streamed by this
/// thread, so it is never held in memory whole.
#[allow(clippy::too_many_arguments)]
fn send_files(
    root: &OsStr,
    known: &Mutex<Known>,
    mux: &Mux,
    frames: pool::Submitter<Vec<u8>>,
    jobs: Sender<Job>,
    queue: Receiver<Job>,
    reporter: &Reporter,
) -> Result<u64> {
    let mut stream = DataWriter {
        frames,
        buffer: Vec::with_capacity(frame::FRAME),
    };
    let mut problems = 0;
    std::thread::scope(|scope| -> Result<u64> {
        let readers = pool::threads(16);
        let (reads, wanted) = mpsc::sync_channel::<(FileId, Vec<u8>)>(IN_FLIGHT);
        let wanted = Arc::new(Mutex::new(wanted));
        for _ in 0..readers {
            let wanted = Arc::clone(&wanted);
            let jobs = jobs.clone();
            scope.spawn(move || {
                let _thread = profile::thread("reader");
                loop {
                    let next =
                        profile::wait(State::Work, || wanted.lock().expect("not poisoned").recv());
                    let Ok((id, path)) = next else {
                        return;
                    };
                    let ready = profile::wait(State::ReadFile, || prepare(root, id, path));
                    if jobs.send(Job::Ready(ready)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(jobs);

        let mut backlog: VecDeque<FileId> = VecDeque::new();
        let mut in_flight = 0;
        let reading = reporter.gauge(Stage::Read, IN_FLIGHT);
        let mut pass_ending = false;
        let mut last_pass = false;
        loop {
            while in_flight < IN_FLIGHT
                && let Some(id) = backlog.pop_front()
            {
                let path = known
                    .lock()
                    .expect("not poisoned")
                    .path(id)
                    .with_context(|| format!("the Destination needs unknown ID {id}"))?;
                profile::wait(State::ToReaders, || reads.send((id, path)))
                    .map_err(|_| anyhow::anyhow!("the readers stopped"))?;
                in_flight += 1;
            }
            reading.set(in_flight);
            let settled = in_flight == 0 && backlog.is_empty();
            if pass_ending && settled {
                record::write_header(&mut stream, &Header::PassEnd)?;
                // The Destination answers only once it has read this.
                stream.finish()?;
                pass_ending = false;
            }
            if last_pass && settled {
                break;
            }
            // With nothing more to send right now, the frame goes out
            // unfilled, so that the last files of a run, or of a slow scan,
            // never wait.
            let job = match queue.try_recv() {
                Ok(job) => job,
                Err(_) => {
                    stream.finish()?;
                    match profile::wait(State::Work, || queue.recv()) {
                        Ok(job) => job,
                        Err(_) => break,
                    }
                }
            };
            match job {
                Job::Send(id) => backlog.push_back(id),
                Job::PassEnd => pass_ending = true,
                Job::Resend(ids) => {
                    backlog.extend(ids);
                    last_pass = true;
                }
                Job::Stop => break,
                Job::Ready(ready) => {
                    in_flight -= 1;
                    match ready {
                        Ready::Record(record) => {
                            stream.write_all(&record)?;
                        }
                        Ready::Large {
                            id,
                            file,
                            size,
                            mtime,
                        } => {
                            profile::wait(State::ReadFile, || {
                                write_record(file, id, size, mtime, &mut stream)
                            })?;
                        }
                        Ready::Unreadable { path, error } => {
                            problems += 1;
                            reporter.say(format!(
                                "cannot read {}: {error}",
                                String::from_utf8_lossy(&display(&path, false))
                            ));
                            reporter.add(Count::Problems, 1);
                            // The Destination must know too: it may be the one
                            // reporting, and `sync` must not delete.
                            mux.message(&Manifest::Problem { path })?;
                        }
                    }
                }
            }
        }
        record::write_header(&mut stream, &Header::End)?;
        stream.finish()?;
        Ok(problems)
    })
}

/// Opens a file for the stream: a small one is read whole into its record; a
/// large one is left open for the sender.
fn prepare(root: &OsStr, id: FileId, path: Vec<u8>) -> Ready {
    let opened = open_source_file(root, &path).and_then(|file| {
        let metadata = file.metadata()?;
        Ok((file, metadata.len(), modified(&metadata)))
    });
    let (file, size, mtime) = match opened {
        Ok(opened) => opened,
        Err(error) => return Ready::Unreadable { path, error },
    };
    if size > SMALL {
        return Ready::Large {
            id,
            file,
            size,
            mtime,
        };
    }
    let mut record = Vec::with_capacity(size as usize + 128);
    match write_record(file, id, size, mtime, &mut record) {
        Ok(()) => Ready::Record(record),
        // Writing to memory does not fail; reading errors mark the record.
        Err(error) => Ready::Unreadable {
            path,
            error: io::Error::other(error),
        },
    }
}

/// Writes one file's record, at the size and mtime the file had when it was
/// opened. A file that changes while it is read is still sent at that size,
/// and marked changed, as is one whose reading fails partway.
fn write_record(
    mut file: File,
    id: FileId,
    size: u64,
    mtime: Timestamp,
    stream: &mut impl Write,
) -> Result<()> {
    record::write_header(stream, &Header::File { id, size, mtime })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; READ_CHUNK.min(size.max(1) as usize)];
    let mut remaining = size;
    let mut changed = false;
    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let count = file.read(&mut buffer[..wanted]).unwrap_or_else(|_| {
            changed = true;
            0
        });
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        stream.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    // A file that shrank is padded, so the record keeps its size.
    changed |= remaining > 0;
    while remaining > 0 {
        let count = buffer.len().min(remaining as usize);
        buffer[..count].fill(0);
        hasher.update(&buffer[..count]);
        stream.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    // A file that grew, or whose size or mtime changed while it was read,
    // changed too.
    let mut probe = [0; 1];
    changed |= file.read(&mut probe).map_or(true, |count| count > 0);
    changed |= file.metadata().map_or(true, |closed| {
        closed.len() != size || modified(&closed) != mtime
    });
    let trailer = Trailer {
        hash: *hasher.finalize().as_bytes(),
        changed,
    };
    record::write_trailer(stream, &trailer)
}

fn modified(metadata: &std::fs::Metadata) -> Timestamp {
    use std::os::unix::fs::MetadataExt;
    Timestamp {
        seconds: metadata.mtime(),
        nanoseconds: metadata.mtime_nsec() as u32,
    }
}

/// Opens a file of the tree by its path relative to the root, never following
/// a symlink below the root. The empty path is the root itself.
fn open_source_file(root: &OsStr, path: &[u8]) -> io::Result<File> {
    if path.is_empty() {
        return File::open(root);
    }
    let (directory, name) = match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&b""[..], path),
    };
    let directory = fsys::open_path(fsys::open_root(root)?, directory)?;
    fsys::open_file(directory, name)
}

/// Writes the record stream as data packets, a packet's worth at a time.
struct DataWriter {
    /// Where frames go to be compressed and sent.
    frames: pool::Submitter<Vec<u8>>,
    /// The frame being filled.
    buffer: Vec<u8>,
}

impl DataWriter {
    /// Sends the frame being filled, if it holds anything.
    fn send(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let frame = std::mem::replace(&mut self.buffer, Vec::with_capacity(frame::FRAME));
        self.frames.submit(frame)
    }

    /// Sends whatever is buffered, ending the stream's last packet.
    fn finish(&mut self) -> Result<()> {
        self.send()
    }
}

impl Write for DataWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(frame::FRAME - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        if self.buffer.len() == frame::FRAME {
            self.send().map_err(io::Error::other)?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send().map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Timestamp;

    fn entry(id: FileId, name: &[u8], kind: Kind) -> manifest::Entry {
        manifest::Entry {
            id,
            name: name.to_vec(),
            kind,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }

    #[test]
    fn known_entries_give_back_their_paths() {
        let mut known = Known::default();
        known.record(b"", &[entry(0, b"", Kind::Directory)]);
        known.record(
            b"",
            &[entry(1, b"a", Kind::Directory), entry(2, b"f", Kind::File)],
        );
        // One directory in two parts.
        known.record(b"a", &[entry(3, b"x", Kind::File)]);
        known.record(b"a", &[entry(4, b"y", Kind::Fifo)]);
        known.record(b"a/z", &[entry(5, b"deep", Kind::File)]);

        assert_eq!(known.len(), 6);
        assert_eq!(
            known.directories,
            [b"".to_vec(), b"a".to_vec(), b"a/z".to_vec()]
        );
        let paths: Vec<Vec<u8>> = (0..6).map(|id| known.path(id).unwrap()).collect();
        assert_eq!(
            paths,
            [&b""[..], b"a", b"f", b"a/x", b"a/y", b"a/z/deep"].map(<[u8]>::to_vec)
        );
        let files: Vec<bool> = (0..6).map(|id| known.is_file(id).unwrap()).collect();
        assert_eq!(files, [false, false, true, true, false, true]);
        assert_eq!(known.path(6), None);
        assert_eq!(known.is_file(6), None);
    }
}
