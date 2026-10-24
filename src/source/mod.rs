//! The Source peer: scans its tree, sends the manifest, and sends the files
//! the Destination says it needs.
//!
//! One thread owns what the Source keeps of what to send, and decides what
//! to do next; it does no I/O ([`owner`]). Around it, executors carry out
//! what it says, and tell it what they did: the scan sends the manifest, a
//! part at a time once the owner says it may; readers open and read the
//! files, and segments of large ones ([`read`]); compressors make blocks of
//! them, and a block sender hands those to the connection's writer, a
//! packet at a time, beside the manifest. The main thread reads the
//! Destination's answers, and tells the owner its needs.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::block::{self, Body};
use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::manifest::{self, Manifest};
use crate::mux::{self, Mux};
use crate::names::{self, Database};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::{
    self, Control, Credit, FileKey, ManifestEncoder, Needs, PacketType, Results,
};
use crate::report::{Count, Reporter, Stage};
use crate::scan::{self, Sink};
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

mod owner;
mod read;

use owner::{
    COMPRESSING, Command, Event, KnownPart, Limits, Owner, Part, READERS, READING, Room, Wait,
};

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
    let limits = Limits {
        backlog: if backlog_files == 0 {
            BACKLOG
        } else {
            backlog_files
        },
        in_flight: crate::protocol::in_flight_limit(in_flight_entries),
    };
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
    // What the owner is told, in the order it was told: a part the scan
    // keeps before it sends it, and so before the Destination can need
    // anything of it.
    let (events, told) = crossbeam_channel::unbounded::<Event>();
    // The readers take the next job as each is free. One waits on the
    // channel at a time, the rest on the lock: a receiver of each one's own
    // would have all of them spin each time it is empty, as it often is with
    // small files, which they read faster than they come.
    let (jobs, queue) = crossbeam_channel::bounded::<read::Job>(READING);
    let queue = &Mutex::new(queue);

    let (summary, problems, owned) = std::thread::scope(|scope| {
        let stages = Stages::new(scope, stop);
        let pump = stages.spawn("pump", move |_| mux::pump(writer, outlet));
        // Each stage has its own handle on the pump, which ends once every
        // handle is dropped.
        let scanner = {
            let mux = mux.clone();
            let reporter = reporter.clone();
            let events = events.clone();
            stages.spawn("manifest", move |stop| {
                let mut sink = ManifestSink {
                    mux: &mux,
                    events,
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
        // In a dry run, the owner only keeps the manifest's entries, and
        // paces the scan.
        let (blocks, sending) = if dry_run {
            drop(jobs);
            (None, None)
        } else {
            for _ in 0..pool::threads(READERS) {
                let events = events.clone();
                stages.spawn("reader", move |stop| {
                    while let Ok(job) =
                        profile::wait(State::Work, || queue.lock().expect("not poisoned").recv())
                    {
                        let read =
                            profile::wait(State::ReadFile, || read::read(job, root, dirs, stop));
                        if events.send(Event::Read(read)).is_err() {
                            break;
                        }
                    }
                    Ok(())
                });
            }
            // Blocks are compressed by a pool, and sent in order by a thread
            // of their own, the end last. It has room for as many as the
            // owner hands it, and the end, so the owner never waits for it.
            reporter.watch(Stage::Network, &mux.data_gauge());
            let room = COMPRESSING + 1;
            let (blocks, compressed) = pool::gauged(
                &stages,
                pool::threads(8),
                room,
                Some(reporter.gauge(Stage::Compress, 16)),
                Profiled {
                    role: "compress",
                    work: State::Compress,
                    full: State::ToCompress,
                },
                move |block: ToCompress| compress_block(block, compress, compress_level),
            );
            let mux = mux.clone();
            let sent = events.clone();
            let sending = stages.spawn("block sender", move |stop| {
                for block in compressed {
                    // Kept before the blocks are let go of, which the owner
                    // hands over.
                    let block = stop.failing(block)?;
                    mux.data(&block)?;
                    // The owner, once done, needs to know no more.
                    if block != block::END {
                        let _ = sent.send(Event::Sent);
                    }
                }
                // The blocks end with the owner, or as the Source stops.
                stop.check()
            });
            (Some(blocks), Some((sending, jobs)))
        };
        let owner = {
            let mux = mux.clone();
            let reporter = reporter.clone();
            stages.spawn("owner", move |stop| {
                let (sending, jobs) = match sending {
                    Some((sending, jobs)) => (Some(sending), Some(jobs)),
                    None => (None, None),
                };
                let mut owner = Owner::new(limits, !dry_run);
                let mut acting = Acting {
                    mux: &mux,
                    reporter: &reporter,
                    stop,
                    jobs,
                    blocks,
                    unreadable: 0,
                };
                let driven = drive(&told, &mut owner, &mut acting, stop);
                let unreadable = acting.unreadable;
                // The blocks end with this; the block sender then ends too,
                // once it has handed every block over.
                drop(acting);
                stop.failing(driven)?;
                if let Some(sending) = sending {
                    sending.join().ok_or(Stopped)?;
                }
                Ok((owner.entries(), unreadable))
            })
        };
        drop(mux);

        let mut summary = Summary::default();
        let received = receive(reader, &events, dry_run, &mut summary, stop);
        // Kept before the scan is let go of: once the answers end, it is
        // either done or stopping.
        if let Err(error) = received {
            stop.fail(error);
        }
        // In a dry run, the owner ends once nothing more can tell it
        // anything.
        drop(events);
        let problems = scanner.join();
        let owned = owner.join();
        // With every handle dropped, the pump writes what is queued and ends.
        stop.close();
        reporter.close();
        pump.join();
        (summary, problems, owned)
    });
    stopping.result()?;
    let (entries, unreadable) = owned.unwrap_or_default();
    let mut summary = summary;
    summary.problems = problems.unwrap_or(0) + unreadable;
    summary.entries = entries;
    Ok(summary)
}

/// What the compressors are handed: a block's body, and the bytes of its
/// files, or its segment's, or the end.
type ToCompress = Option<(Body, Vec<Vec<u8>>)>;

/// Compresses a block, its files' bytes put one after another first, but
/// for one alone, as a segment is; or the end.
fn compress_block(block: ToCompress, compress: bool, level: i32) -> Result<Vec<u8>> {
    let Some((body, mut bytes)) = block else {
        return Ok(block::END.to_vec());
    };
    let payload = match bytes.len() {
        1 => bytes.pop().expect("one"),
        _ => bytes.concat(),
    };
    block::encode(body, &payload, compress, level)
}

/// Runs the owner: takes each event as it comes, and carries out what it
/// says, until the blocks have ended, or, in a dry run, until nothing more
/// comes.
fn drive(
    told: &Receiver<Event>,
    owner: &mut Owner,
    acting: &mut Acting,
    stop: &Stop,
) -> Result<()> {
    loop {
        let event = match told.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Disconnected) => return Ok(()),
            Err(TryRecvError::Empty) => {
                acting.carry_out(owner.on(Event::Idle)?)?;
                if owner.is_finished() {
                    return Ok(());
                }
                let next = profile::wait(State::Work, || {
                    crossbeam_channel::select! {
                        recv(told) -> event => Ok(event.ok()),
                        recv(stop.signal()) -> _ => Err(Stopped),
                    }
                })?;
                match next {
                    Some(event) => event,
                    None => return Ok(()),
                }
            }
        };
        // Nothing more is done once the Source has stopped: the blocks are
        // not ended, as though they were whole.
        stop.check()?;
        acting.carry_out(owner.on(event)?)?;
        if owner.is_finished() {
            return Ok(());
        }
    }
}

/// Carries out what the owner says.
struct Acting<'a> {
    mux: &'a Mux,
    reporter: &'a Reporter,
    stop: &'a Stop,
    /// Where the readers take their jobs; none in a dry run.
    jobs: Option<Sender<read::Job>>,
    /// Where blocks go to be compressed and sent, in order, the end last;
    /// none in a dry run.
    blocks: Option<pool::Submitter<ToCompress>>,
    /// How many files could not be opened.
    unreadable: u64,
}

impl Acting<'_> {
    fn carry_out(&mut self, commands: Vec<Command>) -> Result<()> {
        for command in commands {
            match command {
                // A scan gone, stopping, hears no more.
                Command::Room(room, said) => {
                    let _ = room.send(said);
                }
                // The owner hands the readers no more than they have room
                // for, so this never waits.
                Command::Read(job) => {
                    let jobs = self.jobs.as_ref().expect("readers when sending");
                    jobs.send(job).map_err(|_| Stopped)?;
                }
                Command::Block(body, bytes) => {
                    let blocks = self.blocks.as_mut().expect("compressors when sending");
                    blocks.submit(Some((body, bytes)))?;
                }
                Command::Unreadable { id, path, error } => {
                    // One the Source stopped reading is not one it could not
                    // read.
                    self.stop.check()?;
                    self.unreadable += 1;
                    self.reporter.say(format!(
                        "cannot read {}: {error}",
                        String::from_utf8_lossy(&display(&path, false))
                    ));
                    self.reporter.add(Count::Problems, 1);
                    // The Destination must know too: it may be the one
                    // reporting, and the file never comes, which its
                    // directory need not wait for. Sent as a message, it
                    // goes before the blocks' end, handed over after: a file
                    // needed and neither sent nor said so by then fails there
                    // as never sent.
                    self.mux.message(&Manifest::Problem {
                        path,
                        file: Some(id),
                    })?;
                }
                Command::Hash(hash) => self.mux.message(&hash)?,
                Command::End => {
                    let blocks = self.blocks.as_mut().expect("compressors when sending");
                    blocks.submit(None)?;
                }
            }
        }
        Ok(())
    }
}

/// Reads the Destination's answers until it is done: the needs, in a dry run,
/// or the needs, the results, and then its report that it wrote everything.
/// The needs go to the owner.
fn receive(
    reader: &mut impl Read,
    events: &Sender<Event>,
    dry_run: bool,
    summary: &mut Summary,
    stop: &Stop,
) -> Result<()> {
    // The owner stops only as the Source does, which says why, or once the
    // blocks have ended, when it needs nothing more: either way, what it can
    // no longer take is dropped.
    let tell = |needs: Needs| {
        let _ = events.send(Event::Needs(needs));
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
                    summary.needed += needed.len() as u64;
                    tell(Needs::Files(needed));
                }
                Needs::Attributes(changed) => summary.needed += changed.len() as u64,
                Needs::Through(id) => tell(Needs::Through(id)),
                Needs::End if dry_run => return Ok(()),
                Needs::End => tell(Needs::End),
            },
            // The Destination reports what failed, and its warnings, itself.
            PacketType::Results => match packet.decode::<Results>()? {
                Results::Failed { .. } | Results::NotDeleted { .. } => summary.failed += 1,
            },
            PacketType::Credit => {
                let Credit { bytes } = packet.decode()?;
                let _ = events.send(Event::Credit(bytes));
            }
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

/// How many needed files may wait to be read before the scan waits.
///
/// Both sides keep track of each needed file until it is sent, and the
/// manifest can run far ahead of the data: the scan waits while more than
/// this many files wait to be read, and, before a directory, while more
/// than [`crate::protocol::IN_FLIGHT_ENTRIES`] entries wait to be compared,
/// so that what each keeps stays bounded, by those and by a directory's
/// own, whose needs come at once. Sending files needs no more of the
/// manifest, and the Destination says how far it compared as it goes, so
/// the scan always goes on; once the files are all sent, or the Source
/// stops, it waits no more, and if it stopped, the scan stops too.
const BACKLOG: usize = 100_000;

/// Sends the scan's manifest, each part once the owner has kept its entries.
struct ManifestSink<'a> {
    mux: &'a Mux,
    events: Sender<Event>,
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
impl ManifestSink<'_> {
    /// Hands the owner the entries of a part, and waits until it says the
    /// part may go: the scan may be faster than the connection, or than the
    /// Destination, and waits rather than piling up.
    fn wait_for_room(&mut self, known: Option<KnownPart>, wait: Wait) -> Result<()> {
        let (room, heard) = crossbeam_channel::unbounded();
        let part = Part { known, wait, room };
        self.events.send(Event::Part(part)).map_err(|_| Stopped)?;
        let mut waiting = State::ToOwner;
        loop {
            let said = profile::wait(waiting, || {
                crossbeam_channel::select! {
                    recv(heard) -> said => said.map_err(|_| Stopped),
                    recv(self.stop.signal()) -> _ => Err(Stopped),
                }
            })?;
            match said {
                Room::Wait(why) => waiting = why,
                Room::Go => return Ok(()),
            }
        }
    }
}

impl Sink for ManifestSink<'_> {
    fn manifest(&mut self, message: Manifest) -> Result<()> {
        let wait = match &message {
            Manifest::Root(_) => Some(Wait::Never),
            Manifest::Directory { .. } if self.continuing => Some(Wait::Backlog),
            Manifest::Directory { .. } => Some(Wait::InFlight),
            Manifest::Names { .. } | Manifest::Problem { .. } | Manifest::End => None,
        };
        if let Some(wait) = wait {
            let known = match &message {
                Manifest::Root(entry) => KnownPart::of(b"", std::slice::from_ref(entry)),
                Manifest::Directory { path, entries, .. } => KnownPart::of(path, entries),
                _ => None,
            };
            self.wait_for_room(known, wait)?;
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
        // Each directory goes out as soon as it is read, so the Destination
        // compares while the scan goes on.
        self.send(&message)?;
        if let Manifest::Directory { more, .. } = &message {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::FileId;
    use crate::protocol::FileHash;
    use crate::stop::Reported;
    use std::sync::Arc;

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

    /// The files in `written`'s blocks, in the order their first bytes came,
    /// those in segments put together whatever order the segments came in,
    /// and the hashes sent of those, once the blocks have ended.
    fn sent_files(written: &[u8]) -> Option<(Files, Vec<FileHash>)> {
        let mut reader = written;
        let mut assembler = block::Assembler::default();
        let mut files: Vec<(FileId, std::collections::BTreeMap<u64, Vec<u8>>)> = Vec::new();
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
                            let bytes = part.bytes.ok()?.to_vec();
                            match files.iter_mut().find(|(id, _)| *id == part.file) {
                                Some((_, pieces)) => {
                                    pieces.insert(part.offset, bytes);
                                }
                                None => files.push((part.file, [(part.offset, bytes)].into())),
                            }
                        }
                    }
                }
                PacketType::FileHash => hashes.push(packet.decode::<FileHash>().ok()?),
                _ => {}
            }
        }
        let files = files
            .into_iter()
            .map(|(id, pieces)| (id, pieces.into_values().flatten().collect()))
            .collect();
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
                    // As a Destination gives its credit, and then needs.
                    let credit = Credit {
                        bytes: crate::protocol::DATA_CREDIT,
                    };
                    self.answers = [
                        protocol::encode(&credit).unwrap(),
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
