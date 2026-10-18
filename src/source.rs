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
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};

use crate::dirs::{self, DirCache};
use crate::frame;
use crate::fsys;
use crate::manifest::{self, FileId, Kind, Manifest, Timestamp};
use crate::mux::{self, Mux};
use crate::names::{self, Database};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::{self, Control, FileKey, Needs, PacketType, Results};
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

/// Each entry the Source has sent in the manifest and the Destination has
/// not compared yet, by ID: whether it is a file, and its path, to answer
/// the Destination's needs. Kept a part of the manifest at a time, each with
/// its directory's path once and its names in one buffer, and let go a part
/// at a time, as the Destination says it has compared through it: what is
/// kept is what is on its way, not the whole tree.
#[derive(Default)]
struct Known {
    parts: VecDeque<KnownPart>,
    /// How many entries were ever recorded.
    recorded: u64,
}

struct KnownPart {
    /// The ID of its first entry; the others follow it.
    first: FileId,
    directory: Vec<u8>,
    names: Vec<u8>,
    /// For each entry, where its name ends in `names`, and whether it is a
    /// file.
    entries: Vec<(u32, bool)>,
}

impl KnownPart {
    /// The ID after its last entry's.
    fn end(&self) -> FileId {
        self.first + self.entries.len() as FileId
    }
}

impl Known {
    fn len(&self) -> u64 {
        self.recorded
    }

    /// Records `entries` of the directory at `path`, in ID order. The root
    /// is the one entry of the directory `` with the name ``.
    fn record(&mut self, path: &[u8], entries: &[manifest::Entry]) {
        let Some(first) = entries.first() else {
            return;
        };
        debug_assert!(
            self.parts.back().is_none_or(|last| last.end() == first.id),
            "IDs follow manifest order"
        );
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
        self.recorded += entries.len() as u64;
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
    /// those it needs: the parts all of whose entries are among them go.
    fn compared_through(&mut self, id: FileId) {
        while self.parts.front().is_some_and(|part| part.end() <= id + 1) {
            self.parts.pop_front();
        }
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
    /// At most how many entries of a directory go in a manifest part; 0 for
    /// [`scan::PART`].
    pub part_entries: usize,
    /// How many needed files may wait to be sent before the scan waits; 0
    /// for [`BACKLOG`].
    pub backlog_files: usize,
    /// How many bytes of messages may wait to be written before the manifest
    /// waits for room; 0 for [`mux::PACED_BYTES`].
    pub paced_bytes: usize,
    /// What the scan must never reach: the Destination's root, when the
    /// client found it on the same kernel. Reaching it stops the transfer.
    pub avoid: Vec<FileKey>,
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
        part_entries,
        backlog_files,
        paced_bytes,
        ref avoid,
    } = *options;
    let known: Mutex<Known> = Mutex::new(Known::default());
    let known = &known;
    let backlog = &Backlog::new(if backlog_files == 0 {
        BACKLOG
    } else {
        backlog_files
    });
    // The root is opened once, for the scan and the readers alike, and each
    // directory under it from there.
    let dirs = &DirCache::new(root, dirs::KEPT);

    std::thread::scope(|scope| -> Result<Summary> {
        let (mux, outlet) = mux::channel_paced(if paced_bytes == 0 {
            mux::PACED_BYTES
        } else {
            paced_bytes
        });
        let pump = scope.spawn(move || {
            let _thread = profile::thread("pump");
            mux::pump(writer, outlet)
        });
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
                    backlog,
                    problems: 0,
                    reporter,
                    numeric_ids,
                    named_users: HashSet::new(),
                    named_groups: HashSet::new(),
                };
                // The Destination waits for the rest of the manifest, and
                // this side for its needs: a scan that fails says so at once,
                // so that both stop, rather than leaving them waiting.
                if let Err(error) = scan::scan_in_parts(root, dirs, &mut sink, part_entries, avoid)
                {
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
                let problems = send_files(
                    root,
                    dirs,
                    backlog,
                    &mux,
                    frames,
                    readers_jobs,
                    queue,
                    &reporter,
                );
                // Nothing more is sent: the scan waits for it no more.
                backlog.close();
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
        let received = receive(reader, known, jobs, backlog, &mut summary);
        backlog.close();
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
        summary.entries = known.lock().expect("not poisoned").len();
        Ok(summary)
    })
}

/// Reads the Destination's answers until it is done: the needs, in a dry run,
/// or the needs, the results, and then its report that it wrote everything.
fn receive(
    reader: &mut impl Read,
    known: &Mutex<Known>,
    mut jobs: Option<Sender<Job>>,
    backlog: &Backlog,
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
                        let unknown = || format!("the Destination needs unknown ID {id}");
                        let known = known.lock().expect("not poisoned");
                        let is_file = known.is_file(id).with_context(unknown)?;
                        summary.needed += 1;
                        // Only files' content is sent; the Destination makes
                        // the rest itself. Its path goes with it, so that the
                        // entry need not be kept until it is sent.
                        if is_file && jobs.is_some() {
                            let path = known.path(id).with_context(unknown)?;
                            drop(known);
                            backlog.needed();
                            give(&jobs, Job::Send(id, path));
                        }
                    }
                }
                Needs::Attributes(changed) => summary.needed += changed.len() as u64,
                Needs::Deletes(_) => {}
                Needs::Through(id) => known.lock().expect("not poisoned").compared_through(id),
                Needs::End if dry_run => return Ok(()),
                Needs::End => {
                    give(&jobs, Job::End);
                    // The stream ends once these are sent, so the sender needs
                    // nothing more.
                    jobs = None;
                }
            },
            // The Destination reports what failed, and its warnings, itself.
            PacketType::Results => match packet.decode::<Results>()? {
                Results::Failed { .. } | Results::NotDeleted { .. } => summary.failed += 1,
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

/// How many needed files may wait to be sent before the scan waits.
const BACKLOG: usize = 100_000;

/// The files the Destination needs and this side has not sent yet. Both
/// sides keep track of each until it is sent, and the manifest can run far
/// ahead of the data: the scan waits while there are more than [`BACKLOG`],
/// so that what each keeps stays bounded, by that and by a directory's own,
/// whose needs come at once. Sending them needs no more of the manifest, so
/// the scan always goes on; once the transfer is over, or fails, it waits
/// no more.
struct Backlog {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
    /// At most how many wait before the scan does.
    limit: usize,
    gauge: profile::Gauge,
}

impl Backlog {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new((0, false)),
            changed: Condvar::new(),
            limit,
            gauge: profile::Gauge::new("needed files", limit),
        }
    }

    fn needed(&self) {
        let mut state = self.state.lock().expect("not poisoned");
        state.0 += 1;
        self.gauge.set(state.0);
    }

    fn sent(&self) {
        let mut state = self.state.lock().expect("not poisoned");
        state.0 = state.0.saturating_sub(1);
        self.gauge.set(state.0);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state.lock().expect("not poisoned").1 = true;
        self.changed.notify_all();
    }

    /// Waits until at most its limit of files wait to be sent, or there is
    /// nothing more to wait for.
    fn wait_for_room(&self) {
        let state = self.state.lock().expect("not poisoned");
        if state.0 <= self.limit || state.1 {
            return;
        }
        let waited = profile::wait(State::Backlog, || {
            self.changed
                .wait_while(state, |(waiting, closed)| *waiting > self.limit && !*closed)
        });
        drop(waited.expect("not poisoned"));
    }
}

/// Sends the scan's manifest, and remembers each entry by ID.
struct ManifestSink<'a> {
    mux: &'a Mux,
    known: &'a Mutex<Known>,
    backlog: &'a Backlog,
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
        if matches!(message, Manifest::Directory { .. }) {
            self.backlog.wait_for_room();
        }
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
            file: None,
        };
        self.mux.message(&problem)?;
        Ok(())
    }
}

/// What the file sender does next.
enum Job {
    /// A file to send, and its path.
    Send(FileId, Vec<u8>),
    /// The needs are over: the stream ends once every file is sent.
    End,
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
    Unreadable {
        id: FileId,
        path: Vec<u8>,
        error: io::Error,
    },
}

/// Files up to this size are read whole by the readers, ahead of the stream.
const SMALL: u64 = 1024 * 1024;

/// How many files may be with the readers, or read and waiting, at once.
const IN_FLIGHT: usize = 64;

/// Streams the record of each needed file, and ends the stream once the needs
/// end and every file is sent. A file that cannot be opened is reported and
/// skipped. Returns how many could not be opened.
///
/// A pool of readers opens the files, and reads small ones whole, ahead of
/// the stream, in parallel; records go into the stream in whatever order they
/// are ready, since each carries its ID. A large file is streamed by this
/// thread, so it is never held in memory whole.
#[allow(clippy::too_many_arguments)]
fn send_files(
    root: &OsStr,
    dirs: &DirCache,
    needed: &Backlog,
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
                    let ready = profile::wait(State::ReadFile, || prepare(root, dirs, id, path));
                    if jobs.send(Job::Ready(ready)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(jobs);

        let mut backlog: VecDeque<(FileId, Vec<u8>)> = VecDeque::new();
        let mut in_flight = 0;
        let reading = reporter.gauge(Stage::Read, IN_FLIGHT);
        let mut ending = false;
        loop {
            while in_flight < IN_FLIGHT
                && let Some((id, path)) = backlog.pop_front()
            {
                profile::wait(State::ToReaders, || reads.send((id, path)))
                    .map_err(|_| anyhow::anyhow!("the readers stopped"))?;
                in_flight += 1;
            }
            reading.set(in_flight);
            if ending && in_flight == 0 && backlog.is_empty() {
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
                Job::Send(id, path) => backlog.push_back((id, path)),
                Job::End => ending = true,
                Job::Stop => break,
                Job::Ready(ready) => {
                    in_flight -= 1;
                    // Sent, or never to be: the scan may go on.
                    needed.sent();
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
                        Ready::Unreadable { id, path, error } => {
                            problems += 1;
                            reporter.say(format!(
                                "cannot read {}: {error}",
                                String::from_utf8_lossy(&display(&path, false))
                            ));
                            reporter.add(Count::Problems, 1);
                            // The Destination must know too: it may be the one
                            // reporting, and the file never comes, which its
                            // directory need not wait for. Sent as a message,
                            // it goes before the stream's end, written after:
                            // a file needed and neither sent nor said so by
                            // then fails there as never sent.
                            mux.message(&Manifest::Problem {
                                path,
                                file: Some(id),
                            })?;
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
fn prepare(root: &OsStr, dirs: &DirCache, id: FileId, path: Vec<u8>) -> Ready {
    let (file, stat) = match open_source_file(root, dirs, &path) {
        Ok(opened) => opened,
        Err(error) => return Ready::Unreadable { id, path, error },
    };
    #[allow(clippy::unnecessary_cast)]
    let (size, mtime) = (stat.st_size as u64, fsys::mtime(&stat));
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
            id,
            path,
            error: io::Error::other(error),
        },
    }
}

/// Writes one file's record, at the size and mtime the file had when it was
/// opened, as rsync does: what it gains after is not read, and is left for
/// the next run, which finds its mtime changed. One that shrinks, or whose
/// reading fails partway, is padded to its size, to keep the stream in step,
/// and marked unread, for the Destination to discard. Nothing is sent again.
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
    let mut unread = None;
    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let count = match file.read(&mut buffer[..wanted]) {
            Ok(0) => {
                unread = Some("it shrank while it was read".to_owned());
                break;
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                unread = Some(format!("reading it failed: {error}"));
                break;
            }
        };
        hasher.update(&buffer[..count]);
        stream.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    while remaining > 0 {
        let count = buffer.len().min(remaining as usize);
        buffer[..count].fill(0);
        hasher.update(&buffer[..count]);
        stream.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    let trailer = Trailer {
        hash: *hasher.finalize().as_bytes(),
        unread,
    };
    record::write_trailer(stream, &trailer)
}

/// Opens a regular file of the tree by its path relative to the root, never
/// following a symlink below the root, and returns it with its status as
/// opened. The empty path is the root itself; any other is opened in its
/// directory from `dirs`, opened once from the root.
fn open_source_file(
    root: &OsStr,
    dirs: &DirCache,
    path: &[u8],
) -> io::Result<(File, rustix::fs::Stat)> {
    if path.is_empty() {
        return fsys::open_regular_root(root);
    }
    let (directory, name) = match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&b""[..], path),
    };
    fsys::open_regular_file(&*dirs.open(directory)?, name)
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
    }

    /// Over its limit, the scan waits for files to be sent; once they are,
    /// or once the transfer is over, it goes on.
    #[test]
    fn the_scan_waits_while_too_many_files_wait_to_be_sent() {
        use std::time::Duration;
        let backlog = Backlog::new(2);
        for _ in 0..4 {
            backlog.needed();
        }
        // Not over: no wait.
        let at_limit = Backlog::new(4);
        for _ in 0..4 {
            at_limit.needed();
        }
        at_limit.wait_for_room();

        std::thread::scope(|scope| {
            let (done, finished) = mpsc::channel();
            let backlog = &backlog;
            let waiting = scope.spawn(move || {
                backlog.wait_for_room();
                done.send(()).unwrap();
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(finished.try_recv().is_err(), "it did not wait");
            backlog.sent();
            backlog.sent();
            finished.recv_timeout(Duration::from_secs(10)).unwrap();
            waiting.join().unwrap();
        });

        // Over, but the transfer is over: no more waiting.
        let over = Backlog::new(1);
        for _ in 0..3 {
            over.needed();
        }
        std::thread::scope(|scope| {
            let (done, finished) = mpsc::channel();
            let over = &over;
            scope.spawn(move || {
                over.wait_for_room();
                done.send(()).unwrap();
            });
            over.close();
            finished.recv_timeout(Duration::from_secs(10)).unwrap();
        });
    }

    /// As rsync does: a file is read up to the size it had when opened; one
    /// that grew since is cut there, and one that shrank is padded and
    /// marked unread.
    #[test]
    fn a_file_is_read_up_to_the_size_it_had_when_opened() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        std::fs::write(&path, b"hello world").unwrap();
        let mtime = Timestamp {
            seconds: 1,
            nanoseconds: 0,
        };
        let record = |size: u64| {
            let mut stream = Vec::new();
            write_record(File::open(&path).unwrap(), 1, size, mtime, &mut stream).unwrap();
            let mut reader = stream.as_slice();
            assert_eq!(
                record::read_header(&mut reader).unwrap(),
                Header::File { id: 1, size, mtime }
            );
            let mut content = vec![0; size as usize];
            reader.read_exact(&mut content).unwrap();
            let trailer = record::read_trailer(&mut reader).unwrap();
            assert!(reader.is_empty());
            assert_eq!(trailer.hash, *blake3::hash(&content).as_bytes());
            (content, trailer.unread)
        };
        // Grown since: only what it had.
        assert_eq!(record(5), (b"hello".to_vec(), None));
        assert_eq!(record(11), (b"hello world".to_vec(), None));
        // Shrunk since: padded, and marked.
        let (content, unread) = record(16);
        assert_eq!(content, b"hello world\0\0\0\0\0");
        assert_eq!(unread.as_deref(), Some("it shrank while it was read"));
    }

    fn mkfifo(path: &std::path::Path) {
        let directory = fsys::open_root(path.parent().unwrap().as_os_str()).unwrap();
        let name = path.file_name().unwrap().as_encoded_bytes();
        fsys::make_special(directory, name, &Kind::Fifo, 0o644).unwrap();
    }

    /// A file the scan found, made a FIFO before it is read, is reported as
    /// unreadable at once, rather than waited on for ever for a writer.
    #[test]
    fn a_file_made_a_fifo_since_the_scan_is_unreadable_rather_than_waited_on() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().as_os_str().to_owned();
        let fifo = temp.path().join("f");
        mkfifo(&fifo);

        let (done, prepared) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(prepare(
                &root,
                &DirCache::new(&root, dirs::KEPT),
                1,
                b"f".to_vec(),
            ));
        });
        let ready = match prepared.recv_timeout(Duration::from_secs(5)) {
            Ok(ready) => ready,
            Err(_) => {
                // Let the stuck reader go, so the test fails rather than hangs.
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo);
                panic!("opening a FIFO waited for a writer");
            }
        };
        match ready {
            Ready::Unreadable { id, path, error } => {
                assert_eq!((id, path.as_slice()), (1, &b"f"[..]));
                assert_eq!(error.to_string(), "it is no longer a regular file");
            }
            _ => panic!("a FIFO was read"),
        }
    }

    /// A file made a directory since the scan is reported as unreadable, not
    /// sent as its size in zeros.
    #[test]
    fn a_file_made_a_directory_since_the_scan_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("d")).unwrap();
        match prepare(
            temp.path().as_os_str(),
            &DirCache::new(temp.path().as_os_str(), dirs::KEPT),
            1,
            b"d".to_vec(),
        ) {
            Ready::Unreadable { error, .. } => {
                assert_eq!(error.to_string(), "it is no longer a regular file");
            }
            Ready::Record(_) | Ready::Large { .. } => panic!("a directory was read"),
        }
    }

    /// A root that is one file is opened as the files below a root are, but
    /// through a symlink the user named.
    #[test]
    fn a_root_that_is_a_file_is_read_through_a_symlink_but_not_a_fifo() {
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, b"content").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        match prepare(
            link.as_os_str(),
            &DirCache::new(link.as_os_str(), dirs::KEPT),
            0,
            Vec::new(),
        ) {
            Ready::Record(record) => {
                let mut reader = record.as_slice();
                let Header::File { size, .. } = record::read_header(&mut reader).unwrap() else {
                    panic!("not a file's record");
                };
                assert_eq!(size, 7);
            }
            _ => panic!("the root was not read"),
        }

        let fifo = temp.path().join("fifo");
        mkfifo(&fifo);
        let root = fifo.into_os_string();
        let ready =
            crate::test_support::within("opening a FIFO root", Duration::from_secs(5), move || {
                matches!(
                    prepare(&root, &DirCache::new(&root, dirs::KEPT), 0, Vec::new()),
                    Ready::Unreadable { .. }
                )
            });
        assert!(ready, "a FIFO root was read");
    }
}
