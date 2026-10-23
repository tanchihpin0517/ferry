//! The Source peer: scans its tree, sends the manifest, and sends the files
//! the Destination says it needs.
//!
//! Three things run at once: the scan sends the manifest, the main thread
//! reads the Destination's answers, and a sender thread sends the needed
//! files, in blocks. They share the connection's writer a packet at a time.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::sync::{Condvar, Mutex};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::block::{self, Body, Check, PackFile, Segment};
use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::manifest::{self, FileId, Kind, Manifest, Timestamp};
use crate::mux::{self, Mux};
use crate::names::{self, Database};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::{
    self, Control, FileHash, FileKey, ManifestEncoder, Needs, PacketType, Results,
};
use crate::report::{Count, Reporter, Stage};
use crate::scan::{self, Sink};
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

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
    /// those it needs: the parts all of whose entries are among them go. A
    /// part is never empty, so its last entry is the one before its end.
    fn compared_through(&mut self, id: FileId) {
        while self.parts.front().is_some_and(|part| part.end() - 1 <= id) {
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
    /// Compress the blocks of files.
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
    /// How many entries sent may wait to be compared before the scan waits;
    /// see [`crate::protocol::in_flight_limit`].
    pub in_flight_entries: u32,
    /// What the scan must never reach: the Destination's root, when the
    /// client found it on the same kernel. Reaching it stops the transfer.
    pub avoid: Vec<FileKey>,
    /// Go into no directory on another filesystem than the root's: `-x`.
    pub one_file_system: bool,
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

/// Checks that the Source's root, a directory, a single file, or a symlink,
/// exists. It is checked while preparing, so that a missing source fails
/// before anything changes.
pub fn check_root(root: &OsStr) -> Result<()> {
    let path = std::path::Path::new(root);
    // As the scan stats it: through a symlink to a directory.
    match fsys::stat_source_root(root) {
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
        in_flight_entries,
        ref avoid,
        one_file_system,
    } = *options;
    let known: Mutex<Known> = Mutex::new(Known::default());
    let known = &known;
    let backlog = &Backlog::new(
        if backlog_files == 0 {
            BACKLOG
        } else {
            backlog_files
        },
        crate::protocol::in_flight_limit(in_flight_entries),
    );
    // The root is opened once, for the scan and the readers alike, and each
    // directory under it from there.
    let dirs = &DirCache::new(root, dirs::KEPT);
    let (mux, outlet) = mux::channel_paced(if paced_bytes == 0 {
        mux::PACED_BYTES
    } else {
        paced_bytes
    });
    // The first failure of any stage stops them all, the scan with them, and
    // is the one told.
    let stopping = Stop::new(Some(mux.clone()));
    let stop = &stopping;
    let reporter = if report {
        Reporter::to_mux(mux.clone())
    } else {
        Reporter::none()
    };

    let (summary, problems, sent) = std::thread::scope(|scope| {
        let stages = Stages::new(scope, stop);
        let pump = stages.spawn("pump", move |_| mux::pump(writer, outlet));
        // Each stage has its own handle on the pump, which ends once every
        // handle is dropped.
        let scanner = {
            let mux = mux.clone();
            let reporter = reporter.clone();
            stages.spawn("manifest", move |stop| {
                let mut sink = ManifestSink {
                    mux: &mux,
                    known,
                    backlog,
                    continuing: false,
                    problems: 0,
                    reporter,
                    numeric_ids,
                    named_users: HashSet::new(),
                    named_groups: HashSet::new(),
                    stop,
                    stream: compress
                        .then(|| ManifestEncoder::new(compress_level))
                        .transpose()?,
                };
                // The Destination waits for the rest of the manifest, and
                // this side for its needs: a scan that fails stops both at
                // once, said as it fails.
                let bounds = scan::Bounds {
                    avoid,
                    one_file_system,
                };
                scan::scan_in_parts(root, dirs, stop, &mut sink, part_entries, bounds)?;
                Ok(sink.problems)
            })
        };
        let (jobs, sender) = if dry_run {
            (None, None)
        } else {
            let (jobs, queue) = crossbeam_channel::unbounded();
            // Blocks are compressed by a pool, and sent in order by a thread
            // of their own, the end last.
            reporter.watch(Stage::Network, &mux.data_gauge());
            let (blocks, compressed) = pool::gauged(
                &stages,
                pool::threads(8),
                COMPRESSING,
                Some(reporter.gauge(Stage::Compress, COMPRESSING)),
                Profiled {
                    role: "compress",
                    work: State::Compress,
                    full: State::ToCompress,
                },
                move |block: Option<(Body, Vec<u8>)>| match block {
                    Some((body, payload)) => {
                        block::encode(body, &payload, compress, compress_level)
                    }
                    None => Ok(block::END.to_vec()),
                },
            );
            let sending = {
                let mux = mux.clone();
                stages.spawn("block sender", move |stop| {
                    for block in compressed {
                        // Kept before the blocks are let go of, which the
                        // file sender hands over.
                        mux.data(&stop.failing(block)?)?;
                    }
                    // The blocks end with the file sender, or as the Source
                    // stops.
                    stop.check()
                })
            };
            let mux = mux.clone();
            let readers_jobs = jobs.clone();
            let reporter = reporter.clone();
            let sender = stages.spawn("file sender", move |stop| {
                let problems = send_files(
                    root,
                    dirs,
                    backlog,
                    &mux,
                    blocks,
                    readers_jobs,
                    queue,
                    &reporter,
                    stop,
                );
                // Nothing more is sent: the scan waits for it no more.
                backlog.close();
                let problems = stop.failing(problems)?;
                // The blocks end with this; the block sender then ends too,
                // once it has handed every block over.
                sending.join().ok_or(Stopped)?;
                Ok(problems)
            });
            (Some(jobs), Some(sender))
        };
        drop(mux);

        let mut summary = Summary::default();
        let received = receive(reader, known, jobs, backlog, &mut summary, stop);
        // Kept before the scan is let go of: once the answers end, it is
        // either done or stopping.
        if let Err(error) = received {
            stop.fail(error);
        }
        backlog.close();
        let problems = scanner.join();
        let sent = sender.and_then(|sender| sender.join());
        // With every handle dropped, the pump writes what is queued and ends.
        stop.close();
        reporter.close();
        pump.join();
        (summary, problems, sent)
    });
    stopping.result()?;
    let mut summary = summary;
    summary.problems = problems.unwrap_or(0) + sent.unwrap_or(0);
    summary.entries = known.lock().expect("not poisoned").len();
    Ok(summary)
}

/// Reads the Destination's answers until it is done: the needs, in a dry run,
/// or the needs, the results, and then its report that it wrote everything.
fn receive(
    reader: &mut impl Read,
    known: &Mutex<Known>,
    mut jobs: Option<Sender<Job>>,
    backlog: &Backlog,
    summary: &mut Summary,
    stop: &Stop,
) -> Result<()> {
    let dry_run = jobs.is_none();
    // The file sender stops only as the Source does, which says why, so a job
    // it can no longer take is dropped.
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
                Needs::Through(id) => {
                    known.lock().expect("not poisoned").compared_through(id);
                    backlog.through(id);
                }
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
                // The Destination's failure is the cause, and it knows of it.
                Control::Error { message } => {
                    stop.peer_failed(anyhow::anyhow!("the Destination failed: {message}"));
                    return Err(Stopped.into());
                }
                other => bail!("unexpected {other:?} from the Destination"),
            },
            other => bail!("unexpected {other:?} packet from the Destination"),
        }
    }
}

/// How many needed files may wait to be sent before the scan waits.
const BACKLOG: usize = 100_000;

/// The files the Destination needs and this side has not sent yet, and the
/// entries sent and not compared yet, whose needs are still to come. Both
/// sides keep track of each needed file until it is sent, and the manifest
/// can run far ahead of the data: the scan waits while more than
/// [`BACKLOG`] files wait to be sent, and, before a directory, while more
/// than [`crate::protocol::IN_FLIGHT_ENTRIES`] entries wait to be compared,
/// so that what each keeps stays bounded, by those and by a directory's own,
/// whose needs come at once. Sending files needs no more of the manifest,
/// and the Destination says how far it compared as it goes, so the scan
/// always goes on; once the files are all sent, or the Source stops, it
/// waits no more, and if it stopped, the scan stops too.
struct Backlog {
    state: Mutex<Pacing>,
    changed: Condvar,
    /// At most how many files wait to be sent before the scan does.
    limit: usize,
    /// At most how many entries sent wait to be compared before the scan
    /// does, at a directory's start.
    in_flight_limit: FileId,
    gauge: profile::Gauge,
    in_flight_gauge: profile::Gauge,
}

#[derive(Default)]
struct Pacing {
    /// Files needed and not sent yet.
    waiting: usize,
    /// The last entry the manifest has sent.
    sent: FileId,
    /// The last entry the Destination said it compared.
    through: FileId,
    /// Nothing more is waited for.
    closed: bool,
}

impl Pacing {
    fn in_flight(&self) -> FileId {
        self.sent.saturating_sub(self.through)
    }
}

impl Backlog {
    fn new(limit: usize, in_flight_limit: FileId) -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            limit,
            in_flight_limit,
            gauge: profile::Gauge::new("needed files", limit),
            in_flight_gauge: profile::Gauge::new(
                "entries in flight",
                usize::try_from(in_flight_limit).unwrap_or(usize::MAX),
            ),
        }
    }

    fn needed(&self) {
        let mut state = self.state.lock().expect("not poisoned");
        state.waiting += 1;
        self.gauge.set(state.waiting);
    }

    fn sent(&self) {
        let mut state = self.state.lock().expect("not poisoned");
        state.waiting = state.waiting.saturating_sub(1);
        self.gauge.set(state.waiting);
        self.changed.notify_all();
    }

    /// The manifest has sent every entry up to `id`.
    fn sent_through(&self, id: FileId) {
        let mut state = self.state.lock().expect("not poisoned");
        state.sent = state.sent.max(id);
        self.in_flight_gauge
            .set(usize::try_from(state.in_flight()).unwrap_or(usize::MAX));
    }

    /// The Destination has compared every entry up to `id`.
    fn through(&self, id: FileId) {
        let mut state = self.state.lock().expect("not poisoned");
        state.through = state.through.max(id);
        self.in_flight_gauge
            .set(usize::try_from(state.in_flight()).unwrap_or(usize::MAX));
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state.lock().expect("not poisoned").closed = true;
        self.changed.notify_all();
    }

    /// Waits until at most its limit of files wait to be sent, and, before
    /// a directory's first part, `starts_directory`, of entries wait to be
    /// compared; or there is nothing more to wait for. Not within a
    /// directory: the Destination says how far it compared only once a
    /// directory's last part is, so one larger than the limit would wait for
    /// itself.
    fn wait_for_room(&self, starts_directory: bool) {
        let busy = |state: &Pacing| {
            !state.closed
                && (state.waiting > self.limit
                    || (starts_directory && state.in_flight() > self.in_flight_limit))
        };
        let state = self.state.lock().expect("not poisoned");
        if !busy(&state) {
            return;
        }
        let why = if state.waiting > self.limit {
            State::Backlog
        } else {
            State::InFlight
        };
        let waited = profile::wait(why, || self.changed.wait_while(state, |state| busy(state)));
        drop(waited.expect("not poisoned"));
    }
}

/// Sends the scan's manifest, and remembers each entry by ID.
struct ManifestSink<'a> {
    mux: &'a Mux,
    known: &'a Mutex<Known>,
    backlog: &'a Backlog,
    /// The last directory part sent has more to come: the next is its.
    continuing: bool,
    problems: u64,
    reporter: Reporter,
    numeric_ids: bool,
    /// IDs whose names have been sent, or looked up and found to have none.
    named_users: HashSet<u32>,
    named_groups: HashSet<u32>,
    /// Once the Source stops, nothing more of the manifest is sent: the scan
    /// stops with it.
    stop: &'a Stop,
    /// Where the data is compressed, the manifest is too, as one stream.
    stream: Option<ManifestEncoder>,
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

    /// Sends `message`, in its order, waiting for room: in the manifest's
    /// stream, where it is compressed.
    fn send(&mut self, message: &Manifest) -> Result<()> {
        match &mut self.stream {
            Some(stream) => {
                let packet = profile::wait(State::Compress, || stream.encode(message))?;
                self.mux.packet_paced(packet)
            }
            None => self.mux.message_paced(message),
        }
    }
}

impl Sink for ManifestSink<'_> {
    fn manifest(&mut self, message: Manifest) -> Result<()> {
        if matches!(message, Manifest::Directory { .. }) {
            self.backlog.wait_for_room(!self.continuing);
        }
        self.stop.check()?;
        let names = match &message {
            Manifest::Root(entry) => self.new_names(std::slice::from_ref(entry)),
            Manifest::Directory { entries, .. } => self.new_names(entries),
            _ => None,
        };
        // The scan may be faster than the connection: the manifest waits for
        // room rather than piling up.
        if let Some(names) = names {
            self.send(&names)?;
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
        self.send(&message)?;
        if let Manifest::Directory { entries, more, .. } = &message {
            if let Some(last) = entries.last() {
                self.backlog.sent_through(last.id);
            }
            self.continuing = *more;
        }
        Ok(())
    }

    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()> {
        self.stop.check()?;
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

    fn vanished(&mut self, path: &[u8]) -> Result<()> {
        self.stop.check()?;
        self.reporter.say(format!(
            "warning: {} vanished before it could be read",
            String::from_utf8_lossy(&display(path, false))
        ));
        self.reporter.add(Count::Vanished, 1);
        Ok(())
    }
}

/// What the file sender does next.
enum Job {
    /// A file to send, and its path.
    Send(FileId, Vec<u8>),
    /// The needs are over: the blocks end once every file is sent.
    End,
    /// A reader has a file ready.
    Ready(Ready),
}

/// A file a reader has prepared.
enum Ready {
    /// A small file, read whole and hashed, or as much of it as could be,
    /// ready for a pack.
    Small { entry: PackFile, bytes: Vec<u8> },
    /// A larger file, open, for the sender to read itself.
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

/// Files up to this size are read whole by the readers, ahead of the
/// sender.
const SMALL: u64 = 1024 * 1024;

/// How many files may be with the readers, or read and waiting, at once.
const IN_FLIGHT: usize = 64;

/// How many blocks may be with the compressors, waiting, being compressed,
/// or compressed and not sent.
const COMPRESSING: usize = 16;

/// Sends each needed file, in blocks, and ends them once the needs end and
/// every file is sent. A file that cannot be opened is reported and skipped.
/// Returns how many could not be opened.
///
/// A pool of readers opens the files, and reads small ones whole, ahead of
/// the sender, in parallel; files go into packs in whatever order they are
/// ready, since each carries its ID. A larger file is read by this thread, a
/// segment at a time, so it is never held in memory whole.
#[allow(clippy::too_many_arguments)]
fn send_files(
    root: &OsStr,
    dirs: &DirCache,
    needed: &Backlog,
    mux: &Mux,
    blocks: pool::Submitter<Option<(Body, Vec<u8>)>>,
    jobs: Sender<Job>,
    queue: Receiver<Job>,
    reporter: &Reporter,
    stop: &Stop,
) -> Result<u64> {
    let mut packer = Packer::new(blocks);
    let mut problems = 0;
    let readers = pool::threads(16);
    let (reads, wanted) = crossbeam_channel::bounded::<(FileId, Vec<u8>)>(IN_FLIGHT);
    // Each reader takes the next file as it is free. One waits on the channel
    // at a time, the rest on the lock: a receiver of each one's own would
    // have all of them spin each time it is empty, as it often is with small
    // files, which they read faster than they come.
    let wanted = &Mutex::new(wanted);
    std::thread::scope(|scope| -> Result<u64> {
        // Dropped as this ends, however it does, which ends the readers.
        let reads = reads;
        let stages = Stages::new(scope, stop);
        for _ in 0..readers {
            let jobs = jobs.clone();
            stages.spawn("reader", move |stop| {
                while let Ok((id, path)) =
                    profile::wait(State::Work, || wanted.lock().expect("not poisoned").recv())
                {
                    let ready =
                        profile::wait(State::ReadFile, || prepare(root, dirs, id, path, stop));
                    if jobs.send(Job::Ready(ready)).is_err() {
                        break;
                    }
                }
                Ok(())
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
                // A reader that failed has stopped the Source: then this waits
                // for room no more.
                profile::wait(State::ToReaders, || {
                    crossbeam_channel::select! {
                        send(reads, (id, path)) -> sent => sent.map_err(|_| Stopped),
                        recv(stop.signal()) -> _ => Err(Stopped),
                    }
                })?;
                in_flight += 1;
            }
            reading.set(in_flight);
            if ending && in_flight == 0 && backlog.is_empty() {
                break;
            }
            // With nothing more to send right now, the pack goes out
            // unfilled, so that the last files of a run, or of a slow scan,
            // never wait.
            // Nothing more is sent once the Source has stopped: the blocks
            // are not ended, as though they were whole.
            stop.check()?;
            let job = match queue.try_recv() {
                Ok(job) => job,
                Err(_) => {
                    packer.seal()?;
                    let next = profile::wait(State::Work, || {
                        crossbeam_channel::select! {
                            recv(queue) -> job => Ok(job.ok()),
                            recv(stop.signal()) -> _ => Err(Stopped),
                        }
                    });
                    match next.map_err(anyhow::Error::from)? {
                        Some(job) => job,
                        None => break,
                    }
                }
            };
            match job {
                Job::Send(id, path) => backlog.push_back((id, path)),
                Job::End => ending = true,
                Job::Ready(ready) => {
                    in_flight -= 1;
                    // Sent, or never to be: the scan may go on.
                    needed.sent();
                    match ready {
                        Ready::Small { entry, bytes } => packer.add(entry, bytes)?,
                        Ready::Large {
                            id,
                            file,
                            size,
                            mtime,
                        } => {
                            profile::wait(State::ReadFile, || {
                                send_large(&file, id, size, mtime, &mut packer, mux, stop)
                            })?;
                        }
                        Ready::Unreadable { id, path, error } => {
                            // One the Source stopped reading is not one it
                            // could not read.
                            stop.check()?;
                            problems += 1;
                            reporter.say(format!(
                                "cannot read {}: {error}",
                                String::from_utf8_lossy(&display(&path, false))
                            ));
                            reporter.add(Count::Problems, 1);
                            // The Destination must know too: it may be the one
                            // reporting, and the file never comes, which its
                            // directory need not wait for. Sent as a message,
                            // it goes before the blocks' end, written after:
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
        packer.end()?;
        Ok(problems)
    })
}

/// Opens a file for sending: a small one is read whole, and hashed, ready
/// for a pack; a larger one is left open for the sender.
fn prepare(root: &OsStr, dirs: &DirCache, id: FileId, path: Vec<u8>, stop: &Stop) -> Ready {
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
    match read_whole(&file, id, size, mtime, stop) {
        Ok((entry, bytes)) => Ready::Small { entry, bytes },
        // Reading errors mark the file: this is the Source stopping, which
        // the file sender sees before it would report it.
        Err(error) => Ready::Unreadable {
            id,
            path,
            error: io::Error::other(error),
        },
    }
}

/// What was read of a file: its bytes, or, if they could not be read
/// whole, why, and none of them.
struct Content {
    bytes: Vec<u8>,
    unread: Option<String>,
}

/// Reads `len` bytes of `file` from `offset`: as many as it had there when
/// it was opened, as rsync does, what it gained since being left for the
/// next run, which finds its mtime changed. Each piece is hashed as it is
/// read, while it is fresh in the cache. Of one that shrank, or whose
/// reading failed, nothing is sent but why, which fails it on the
/// Destination: nothing is sent again.
fn read_part(
    file: &File,
    offset: u64,
    len: usize,
    stop: &Stop,
    mut hash: impl FnMut(&[u8]),
) -> Result<Content> {
    let mut bytes = vec![0; len];
    let mut done = 0;
    let unread = |why: String| Content {
        bytes: Vec::new(),
        unread: Some(why),
    };
    while done < len {
        // A large file stops being read as the Source stops.
        stop.check()?;
        let wanted = READ_CHUNK.min(len - done);
        match file.read_at(&mut bytes[done..done + wanted], offset + done as u64) {
            Ok(0) => return Ok(unread("it shrank while it was read".to_owned())),
            Ok(count) => {
                hash(&bytes[done..done + count]);
                done += count;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Ok(unread(format!("reading it failed: {error}"))),
        }
    }
    Ok(Content {
        bytes,
        unread: None,
    })
}

/// Reads a file of `size` bytes whole, or as much of it as it can, for a
/// pack.
fn read_whole(
    file: &File,
    id: FileId,
    size: u64,
    mtime: Timestamp,
    stop: &Stop,
) -> Result<(PackFile, Vec<u8>)> {
    let mut hasher = blake3::Hasher::new();
    let content = read_part(file, 0, size as usize, stop, |bytes| {
        hasher.update(bytes);
    })?;
    Ok(packed(id, mtime, content, *hasher.finalize().as_bytes()))
}

/// A file read whole, or as much of it as could be, as it goes in a pack:
/// with its hash, or why it could not be read whole, and none of its bytes.
fn packed(id: FileId, mtime: Timestamp, content: Content, hash: [u8; 32]) -> (PackFile, Vec<u8>) {
    let (len, check) = match content.unread {
        None => (content.bytes.len() as u32, Check::Hash(hash)),
        Some(why) => (0, Check::Unread(why)),
    };
    let entry = PackFile {
        file: id,
        len,
        mtime,
        check,
    };
    (entry, content.bytes)
}

/// Sends a file too large for the readers: whole, in a pack, if it fits in
/// one, or else a segment at a time, each with its chaining value, and then
/// its hash, merged from theirs. Of one that cannot be read whole, nothing
/// more is sent once a segment says so, which fails it on the Destination.
fn send_large(
    file: &File,
    id: FileId,
    size: u64,
    mtime: Timestamp,
    packer: &mut Packer,
    mux: &Mux,
    stop: &Stop,
) -> Result<()> {
    if size <= block::SEGMENT {
        let (entry, bytes) = read_whole(file, id, size, mtime, stop)?;
        return packer.add(entry, bytes);
    }
    let mut values = Vec::new();
    for index in 0..block::segments(size) {
        let offset = index * block::SEGMENT;
        let len = block::SEGMENT.min(size - offset);
        let mut hasher = block::SegmentHasher::new(offset);
        let content = read_part(file, offset, len as usize, stop, |bytes| {
            hasher.update(bytes)
        })?;
        let (len, check) = match content.unread {
            None => {
                let value = hasher.finish();
                values.push(value);
                (len as u32, Check::Hash(value))
            }
            Some(why) => (0, Check::Unread(why)),
        };
        let whole = matches!(check, Check::Hash(_));
        let segment = Segment {
            file: id,
            offset,
            len,
            size,
            mtime,
            check,
        };
        packer.segment(segment, content.bytes)?;
        if !whole {
            return Ok(());
        }
    }
    mux.message(&FileHash {
        file: id,
        hash: block::root(&values, size),
    })
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

/// The pack being filled, and where it goes once sealed, as each segment
/// does, to be compressed and sent, in order, the end last.
struct Packer {
    blocks: pool::Submitter<Option<(Body, Vec<u8>)>>,
    files: Vec<PackFile>,
    payload: Vec<u8>,
    /// Its files' bytes and their entries in its header, at most.
    filled: usize,
}

impl Packer {
    fn new(blocks: pool::Submitter<Option<(Body, Vec<u8>)>>) -> Self {
        Self {
            blocks,
            files: Vec::new(),
            payload: Vec::new(),
            filled: 0,
        }
    }

    /// Puts a file in the pack, sealing the pack first if the file would
    /// take it past [`block::PACK`], and after if the file fills it.
    fn add(&mut self, entry: PackFile, bytes: Vec<u8>) -> Result<()> {
        let cost = bytes.len() + entry_size(&entry);
        if !self.files.is_empty() && self.filled + cost > block::PACK {
            self.seal()?;
        }
        if self.files.is_empty() {
            self.payload = Vec::with_capacity(block::PACK.max(bytes.len()));
        }
        self.payload.extend_from_slice(&bytes);
        self.files.push(entry);
        self.filled += cost;
        if self.filled >= block::PACK {
            self.seal()?;
        }
        Ok(())
    }

    /// Hands the pack over, if it holds anything.
    fn seal(&mut self) -> Result<()> {
        if self.files.is_empty() {
            return Ok(());
        }
        let files = std::mem::take(&mut self.files);
        let payload = std::mem::take(&mut self.payload);
        self.filled = 0;
        self.blocks.submit(Some((Body::Pack { files }, payload)))
    }

    /// Hands a segment over, apart from any pack.
    fn segment(&mut self, segment: Segment, bytes: Vec<u8>) -> Result<()> {
        self.blocks.submit(Some((Body::Segment(segment), bytes)))
    }

    /// Hands the last pack over, and then the end.
    fn end(mut self) -> Result<()> {
        self.seal()?;
        self.blocks.submit(None)
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
    use crate::manifest::Timestamp;
    use crate::stop::Reported;
    use std::sync::Arc;

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

        // Through the last ID there could be, as a Destination gone wrong may
        // say: all go, nothing overflows.
        let mut known = Known::default();
        known.record(b"", &[entry(0, b"", Kind::Directory)]);
        known.record(b"", &[entry(1, b"f", Kind::File)]);
        known.compared_through(FileId::MAX);
        assert!(known.parts.is_empty());
    }

    /// Over its limit, the scan waits for files to be sent; once they are,
    /// or once the transfer is over, it goes on.
    #[test]
    fn the_scan_waits_while_too_many_files_wait_to_be_sent() {
        use std::time::Duration;
        let backlog = Backlog::new(2, 100);
        for _ in 0..4 {
            backlog.needed();
        }
        // Not over: no wait.
        let at_limit = Backlog::new(4, 100);
        for _ in 0..4 {
            at_limit.needed();
        }
        at_limit.wait_for_room(true);

        std::thread::scope(|scope| {
            let (done, finished) = crossbeam_channel::unbounded();
            let backlog = &backlog;
            let waiting = scope.spawn(move || {
                backlog.wait_for_room(true);
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
        let over = Backlog::new(1, 100);
        for _ in 0..3 {
            over.needed();
        }
        std::thread::scope(|scope| {
            let (done, finished) = crossbeam_channel::unbounded();
            let over = &over;
            scope.spawn(move || {
                over.wait_for_room(true);
                done.send(()).unwrap();
            });
            over.close();
            finished.recv_timeout(Duration::from_secs(10)).unwrap();
        });
    }

    /// Before a directory, the scan waits while more entries sent than its
    /// limit are not compared; within one, it does not, as the Destination
    /// says how far it compared only at a directory's end. Told how far the
    /// Destination is, or that the transfer is over, it goes on.
    #[test]
    fn the_scan_waits_while_too_many_entries_wait_to_be_compared() {
        use std::time::Duration;
        let backlog = Backlog::new(100, 10);
        backlog.sent_through(10);
        backlog.wait_for_room(true);
        backlog.sent_through(25);
        // Within a directory: no wait.
        backlog.wait_for_room(false);
        let waits = |release: &dyn Fn()| {
            std::thread::scope(|scope| {
                let (done, finished) = crossbeam_channel::unbounded();
                let backlog = &backlog;
                scope.spawn(move || {
                    backlog.wait_for_room(true);
                    done.send(()).unwrap();
                });
                std::thread::sleep(Duration::from_millis(50));
                assert!(finished.try_recv().is_err(), "it did not wait");
                release();
                finished.recv_timeout(Duration::from_secs(10)).unwrap();
            });
        };
        // Compared up to 15 leaves 10: room.
        waits(&|| backlog.through(15));
        backlog.sent_through(40);
        waits(&|| backlog.close());
    }

    /// As rsync does: a file is read up to the size it had when opened; one
    /// that grew since is cut there, and of one that shrank nothing goes
    /// but why: it is not padded, as each block says what it holds.
    #[test]
    fn a_file_is_read_up_to_the_size_it_had_when_opened() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        std::fs::write(&path, b"hello world").unwrap();
        let mtime = Timestamp {
            seconds: 1,
            nanoseconds: 0,
        };
        let read = |size: u64| {
            let file = File::open(&path).unwrap();
            read_whole(&file, 1, size, mtime, &Stop::new(None)).unwrap()
        };
        let hash = |bytes: &[u8]| Check::Hash(*blake3::hash(bytes).as_bytes());
        // Grown since: only what it had.
        let (entry, bytes) = read(5);
        assert_eq!(
            (entry.len, entry.check, bytes),
            (5, hash(b"hello"), b"hello".to_vec())
        );
        let (entry, bytes) = read(11);
        assert_eq!(entry.check, hash(b"hello world"));
        assert_eq!(bytes, b"hello world");
        // Shrunk since: none of it, and why.
        let (entry, bytes) = read(16);
        assert_eq!(
            (entry.len, entry.check, bytes),
            (
                0,
                Check::Unread("it shrank while it was read".to_owned()),
                Vec::new()
            )
        );
        // A segment, from where it begins.
        let file = File::open(&path).unwrap();
        let mut read = Vec::new();
        let content = read_part(&file, 6, 5, &Stop::new(None), |bytes| {
            read.extend_from_slice(bytes)
        })
        .unwrap();
        assert_eq!((content.bytes, content.unread), (b"world".to_vec(), None));
        assert_eq!(read, b"world");
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

        let (done, prepared) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let _ = done.send(prepare(
                &root,
                &DirCache::new(&root, dirs::KEPT),
                1,
                b"f".to_vec(),
                &Stop::new(None),
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
            &Stop::new(None),
        ) {
            Ready::Unreadable { error, .. } => {
                assert_eq!(error.to_string(), "it is no longer a regular file");
            }
            Ready::Small { .. } | Ready::Large { .. } => panic!("a directory was read"),
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
            &Stop::new(None),
        ) {
            Ready::Small { entry, bytes } => {
                assert_eq!((entry.len, bytes), (7, b"content".to_vec()));
            }
            _ => panic!("the root was not read"),
        }

        let fifo = temp.path().join("fifo");
        mkfifo(&fifo);
        let root = fifo.into_os_string();
        let ready =
            crate::test_support::within("opening a FIFO root", Duration::from_secs(5), move || {
                matches!(
                    prepare(
                        &root,
                        &DirCache::new(&root, dirs::KEPT),
                        0,
                        Vec::new(),
                        &Stop::new(None)
                    ),
                    Ready::Unreadable { .. }
                )
            });
        assert!(ready, "a FIFO root was read");
    }

    /// Writes slowly, as a connection may, and keeps what it was given.
    struct Slow(Vec<u8>);

    impl Write for Slow {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(1));
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The Source's options for `root`, as a test runs it.
    fn options(root: &OsStr) -> Options<'_> {
        Options {
            root,
            dry_run: true,
            numeric_ids: true,
            compress: false,
            compress_level: 3,
            report: false,
            part_entries: 0,
            backlog_files: 0,
            paced_bytes: 4096,
            in_flight_entries: 0,
            avoid: Vec::new(),
            one_file_system: false,
        }
    }

    /// The control messages, and how many directories of the manifest, that
    /// `written` holds.
    fn sent(written: &[u8]) -> (Vec<Control>, usize) {
        let mut reader = written;
        let (mut controls, mut directories) = (Vec::new(), 0);
        while let Some(packet) = protocol::receive(&mut reader).unwrap() {
            match packet.kind {
                PacketType::Control => controls.push(packet.decode::<Control>().unwrap()),
                PacketType::Manifest => {
                    if matches!(packet.decode::<Manifest>(), Ok(Manifest::Directory { .. })) {
                        directories += 1;
                    }
                }
                _ => {}
            }
        }
        (controls, directories)
    }

    /// The Destination failing stops the scan at once, rather than once the
    /// whole tree is scanned and sent: here, of 2,000 directories, over a
    /// slow connection.
    #[test]
    fn the_destination_failing_stops_the_scan() {
        crate::test_support::within(
            "the destination failing stops the scan",
            crate::test_support::PATIENCE,
            move || {
                let temp = tempfile::tempdir().unwrap();
                for index in 0..2000 {
                    std::fs::create_dir(temp.path().join(format!("d{index:04}"))).unwrap();
                }
                let answers = protocol::encode(&Control::Error {
                    message: "x".into(),
                })
                .unwrap();
                let mut written = Slow(Vec::new());
                let started = std::time::Instant::now();
                let error = run(
                    &mut answers.as_slice(),
                    &mut written,
                    &options(temp.path().as_os_str()),
                )
                .unwrap_err();
                assert!(started.elapsed() < std::time::Duration::from_secs(2));
                assert_eq!(format!("{error:#}"), "the Destination failed: x");
                // The Destination knows: it is not told back.
                assert!(error.is::<Reported>());
                let (controls, directories) = sent(&written.0);
                assert_eq!(controls, []);
                assert!(directories < 200, "{directories} directories sent");
            },
        );
    }

    /// What a Source writes, kept where its Destination can see it.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A Destination that sends nothing, and closes once the Source has
    /// told it of a failure, as one does.
    struct ClosesOnError(Shared);

    impl Read for ClosesOnError {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            let started = std::time::Instant::now();
            while sent(&self.0.0.lock().unwrap()).0.is_empty() {
                assert!(started.elapsed() < std::time::Duration::from_secs(20));
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(0)
        }
    }

    /// A scan that fails says so once, as it fails, and the server does not
    /// say so again.
    #[test]
    fn a_scan_that_fails_says_so_once() {
        let temp = tempfile::tempdir().unwrap();
        let stat = fsys::stat_root(temp.path().as_os_str()).unwrap();
        let mut options = options(temp.path().as_os_str());
        options.avoid = vec![fsys::key(&stat)];
        let written = Shared::default();
        let mut reader = ClosesOnError(written.clone());
        let error = run(&mut reader, &mut written.clone(), &options).unwrap_err();
        assert!(error.is::<Reported>());
        let written = written.0.lock().unwrap().clone();
        let (controls, _) = sent(&written);
        assert!(
            matches!(&controls[..], [Control::Error { message }]
                if message == "the Destination's root is the Source's root"),
            "{controls:?}"
        );
    }

    /// Files by ID, with their content.
    type Files = Vec<(FileId, Vec<u8>)>;

    /// The files in `written`'s blocks, those in segments put together, and
    /// the hashes sent of those, once the blocks have ended.
    fn sent_files(written: &[u8]) -> Option<(Files, Vec<FileHash>)> {
        let mut reader = written;
        let mut assembler = block::Assembler::default();
        let mut files: Files = Vec::new();
        let mut hashes = Vec::new();
        let mut ended = false;
        while let Some(packet) = protocol::receive(&mut reader).ok()? {
            match packet.kind {
                PacketType::Data => {
                    for assembled in assembler.push(&packet.into_payload()).ok()? {
                        let block::Assembled::Block(encoded) = assembled else {
                            ended = true;
                            continue;
                        };
                        for part in block::decode(encoded).ok()?.into_parts() {
                            let bytes = part.bytes.ok()?;
                            if part.offset == 0 {
                                files.push((part.file, bytes.to_vec()));
                                continue;
                            }
                            // A segment after the one before it.
                            let (_, content) =
                                files.iter_mut().rev().find(|(id, _)| *id == part.file)?;
                            if content.len() as u64 != part.offset {
                                return None;
                            }
                            content.extend_from_slice(&bytes);
                        }
                    }
                }
                PacketType::FileHash => hashes.push(packet.decode::<FileHash>().ok()?),
                _ => {}
            }
        }
        ended.then_some((files, hashes))
    }

    /// A Destination that asks for the file with ID 1 once the manifest is
    /// sent, doing `then` first, and is done once the stream ends.
    struct AsksOnceScanned {
        written: Shared,
        then: Option<Box<dyn FnOnce() + Send>>,
        answers: Vec<u8>,
        done: bool,
    }

    impl Read for AsksOnceScanned {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let wait_for = |done: &dyn Fn(&[u8]) -> bool| {
                let started = std::time::Instant::now();
                while !done(&self.written.0.lock().unwrap()) {
                    assert!(started.elapsed() < std::time::Duration::from_secs(20));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            };
            if self.answers.is_empty() {
                if let Some(then) = self.then.take() {
                    wait_for(&|written| {
                        let mut reader = written;
                        std::iter::from_fn(|| protocol::receive(&mut reader).ok().flatten()).any(
                            |packet| {
                                packet.kind == PacketType::Manifest
                                    && matches!(packet.decode::<Manifest>(), Ok(Manifest::End))
                            },
                        )
                    });
                    then();
                    self.answers = [
                        protocol::encode(&Needs::Files(vec![1])).unwrap(),
                        protocol::encode(&Needs::End).unwrap(),
                    ]
                    .concat();
                } else if !self.done {
                    wait_for(&|written| sent_files(written).is_some());
                    self.answers = protocol::encode(&Control::Done).unwrap();
                    self.done = true;
                } else {
                    return Ok(0);
                }
            }
            let count = out.len().min(self.answers.len());
            out[..count].copy_from_slice(&self.answers[..count]);
            self.answers.drain(..count);
            Ok(count)
        }
    }

    /// A root named by a symlink that is switched to another tree once the
    /// manifest is sent is read from the tree it named when the transfer
    /// began: each file is opened from the root opened once, never by the
    /// root's path again.
    #[test]
    fn a_root_switched_after_the_scan_is_read_from_the_tree_it_was() {
        let temp = tempfile::tempdir().unwrap();
        for (tree, content) in [("release-42", "old"), ("release-43", "new")] {
            std::fs::create_dir(temp.path().join(tree)).unwrap();
            std::fs::write(temp.path().join(tree).join("f"), content).unwrap();
        }
        let current = temp.path().join("current");
        std::os::unix::fs::symlink(temp.path().join("release-42"), &current).unwrap();
        let switched = current.clone();
        let next = temp.path().join("release-43");
        let written = Shared::default();
        let mut reader = AsksOnceScanned {
            written: written.clone(),
            then: Some(Box::new(move || {
                std::fs::remove_file(&switched).unwrap();
                std::os::unix::fs::symlink(&next, &switched).unwrap();
            })),
            answers: Vec::new(),
            done: false,
        };
        let mut options = options(current.as_os_str());
        options.dry_run = false;
        run(&mut reader, &mut written.clone(), &options).unwrap();
        let written = written.0.lock().unwrap();
        assert_eq!(
            sent_files(&written).unwrap(),
            (vec![(1, b"old".to_vec())], Vec::new())
        );
    }

    /// A file larger than a segment goes a segment at a time, each where it
    /// goes, and then its hash, the same as of all its bytes at once.
    #[test]
    fn a_large_file_goes_in_segments_then_its_hash() {
        let temp = tempfile::tempdir().unwrap();
        let content = crate::test_support::Random::new(31).bytes(block::SEGMENT as usize + 1000);
        std::fs::write(temp.path().join("f"), &content).unwrap();
        let written = Shared::default();
        let mut reader = AsksOnceScanned {
            written: written.clone(),
            then: Some(Box::new(|| {})),
            answers: Vec::new(),
            done: false,
        };
        let mut options = options(temp.path().as_os_str());
        options.dry_run = false;
        run(&mut reader, &mut written.clone(), &options).unwrap();
        let written = written.0.lock().unwrap();
        let (files, hashes) = sent_files(&written).unwrap();
        assert_eq!(files, [(1, content.clone())]);
        assert_eq!(
            hashes,
            [FileHash {
                file: 1,
                hash: *blake3::hash(&content).as_bytes()
            }]
        );
    }
}
