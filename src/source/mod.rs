//! The Source peer: scans its tree, sends the manifest, and sends the files
//! the Destination says it needs.
//!
//! One thread owns what the Source keeps of what to send, and decides what
//! to do next; it does no I/O ([`owner`]). Around it, executors carry out
//! what it says, and tell it what they did: the scan writes the manifest, a
//! batch at a time once the owner says it may; readers open and read the
//! files, and segments of large ones ([`read`]); compressors make blocks of
//! them, and a block sender writes those on the data's pipe, beside the
//! manifest's. The main thread reads the Destination's answers, and tells the
//! owner its needs; a thread reads control.

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
use crate::mux::Mux;
use crate::names::{self, Database};
use crate::pool::{self, Profiled};
use crate::profile::{self, State};
use crate::protocol::{self, Answer, BlocksEnd, Control, FileKey, Needs, PacketType};
use crate::report::{Count, Reporter, Stage};
use crate::scan::{self, Batch, Sink};
use crate::stage::{self, Stages};
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
    /// Where to report what goes wrong: to the client, on the server's
    /// control connection; or nowhere.
    pub report: Reporter,
    /// How many names of a directory the scan stats at a time; 0 for
    /// [`scan::BATCH`].
    pub batch_entries: usize,
    /// How many needed files may wait to be sent before the scan waits; 0
    /// for [`BACKLOG`].
    pub backlog_files: usize,
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
/// What the Source reads from the Destination: its answers, and control.
pub struct Reading {
    pub answers: Box<dyn Read + Send>,
    pub control: Box<dyn Read + Send>,
}

/// Runs the Source over one stream of the Destination's packets, writing
/// its own on `writer`, as the peers sent them before they had pipes: for
/// the tests. Each packet goes on the pipe its kind does, as it is read,
/// and what the Source writes goes on as it writes it, a few packets
/// waiting at most, so that a slow writer holds the scan back as a slow
/// connection does; each record of the manifest as a packet of its own.
pub fn run<W: Write + Send>(
    reader: &mut (impl Read + Send),
    writer: &mut W,
    options: &Options,
) -> Result<Summary> {
    let pipe = || crossbeam_channel::unbounded::<io::Result<Vec<u8>>>();
    let ((answers, answers_in), (control, control_in)) = (pipe(), pipe());
    let (out, written) = crossbeam_channel::bounded::<Vec<u8>>(16);
    let (manifest, manifest_in) = crossbeam_channel::bounded::<io::Result<Vec<u8>>>(16);
    let mux = Mux::for_source(out.clone(), manifest, out.clone(), None, Vec::new());
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut records = io::BufReader::new(crate::mux::Piped::new(manifest_in));
            while let Ok(Some(record)) = protocol::read_record::<Manifest>(&mut records) {
                let Ok(packet) = protocol::encode(&record) else {
                    return;
                };
                if out.send(packet).is_err() {
                    return;
                }
            }
        });
        scope.spawn(move || {
            loop {
                let packet = match protocol::receive(reader) {
                    Ok(Some(packet)) => packet,
                    Ok(None) => return,
                    Err(error) => {
                        let _ = answers.send(Err(io::Error::other(format!("{error:#}"))));
                        return;
                    }
                };
                // An answer goes as a record; anything else as a packet,
                // on control if it is the Destination's failure.
                let mut bytes = Vec::new();
                if let Ok(answer) = packet.decode::<Answer>() {
                    let _ = protocol::put_record(&mut bytes, &answer);
                    let _ = answers.send(Ok(bytes));
                    continue;
                }
                let failed = packet.kind == PacketType::Control
                    && matches!(packet.decode::<Control>(), Ok(Control::Error { .. }));
                let pipe = if failed { &control } else { &answers };
                let kind = packet.kind;
                let payload = packet.into_payload();
                bytes.push(kind as u8);
                bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&payload);
                let _ = pipe.send(Ok(bytes));
            }
        });
        scope.spawn(move || {
            // Once the writer fails, what comes is let go of: no one waits.
            let mut failed = false;
            for packet in written {
                failed = failed || writer.write_all(&packet).is_err();
            }
        });
        let reading = Reading {
            answers: Box::new(crate::mux::Piped::new(answers_in)),
            control: Box::new(crate::mux::Piped::new(control_in)),
        };
        run_on_pipes(reading, mux, options)
    })
}

/// Runs the Source on the pipes it reads, the Destination's, writing its own
/// on `mux`. A dry run sends nothing but the manifest. Owners' names are
/// sent unless `numeric_ids`. The Source reports only the paths it cannot
/// read; the Destination reports the rest.
pub fn run_on_pipes(reading: Reading, mux: Mux, options: &Options) -> Result<Summary> {
    let Options {
        root,
        dry_run,
        numeric_ids,
        ref report,
        batch_entries,
        backlog_files,
        ref avoid,
        one_file_system,
    } = *options;
    let Reading {
        mut answers,
        mut control,
    } = reading;
    let limits = Limits {
        backlog: if backlog_files == 0 {
            BACKLOG
        } else {
            backlog_files
        },
    };
    // The root is opened once, for the scan and the readers alike, and each
    // directory under it from there.
    let dirs = &DirCache::new(root, dirs::KEPT);
    // The first failure of any stage stops them all, the scan with them, and
    // is the one told.
    let stopping = Stop::new(Some(mux.clone()));
    let stop = &stopping;
    let reporter = report.clone();
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
        // The Destination's failure, which is the cause, stops the Source.
        // Closed as control ends, which the answers wait for once they end.
        let (control_open, control_end) = crossbeam_channel::bounded::<()>(0);
        stages.spawn("control", move |stop| {
            let _open = control_open;
            receive_control(&mut control, stop)
        });
        let scanner = {
            let mux = mux.clone();
            let reporter = reporter.clone();
            let events = events.clone();
            stages.spawn("manifest", move |stop| {
                let mut sink = ManifestSink {
                    mux: &mux,
                    events,
                    problems: 0,
                    reporter,
                    numeric_ids,
                    named_users: HashSet::new(),
                    named_groups: HashSet::new(),
                    stop,
                };
                // The Destination waits for the rest of the manifest, and
                // this side for its needs: a scan that fails stops both at
                // once, said as it fails.
                let bounds = scan::Bounds {
                    avoid,
                    one_file_system,
                };
                scan::scan_in_batches(root, dirs, stop, &mut sink, batch_entries, bounds)?;
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
                        // A segment's file is open: its path is not at hand.
                        let _on = match &job {
                            read::Job::Open { path, .. } => Some(profile::on(path)),
                            read::Job::Segment { .. } => None,
                        };
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
            // of their own, which numbers each as it sends it, and says how
            // many there were once the owner hands it the end. It has room
            // for as many as the owner hands it, and the end, so the owner
            // never waits for it.
            if let Some(gauge) = mux.data_gauge() {
                reporter.watch(Stage::Network, &gauge);
            }
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
                encode_block,
            );
            let mux = mux.clone();
            let sent = events.clone();
            let sending = stages.spawn("block sender", move |stop| {
                let mut blocks = 0;
                for block in compressed {
                    // Kept before the blocks are let go of, which the owner
                    // hands over.
                    match stop.failing(block)? {
                        Some(mut block) => {
                            block::number(&mut block, blocks);
                            mux.block(&block)?;
                            blocks += 1;
                            let _ = sent.send(Event::Sent);
                        }
                        // With the messages, after the hashes and what the
                        // Source could not read, which the owner said
                        // before it handed over the end.
                        None => mux.message(&BlocksEnd { blocks })?,
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

        let mut summary = Summary::default();
        // A stage on this thread, whose failure, or panic, is kept before
        // the scan is let go of: once the answers end, it is either done or
        // stopping.
        let received = stage::run("answers", || {
            receive(&mut answers, &events, dry_run, &mut summary, &control_end)
        });
        if let Err(error) = received {
            stop.fail(error);
        }
        // Done, or stopping, it needs nothing more from the Destination: what
        // reads control returns.
        mux.stop_reading();
        // In a dry run, the owner ends once nothing more can tell it
        // anything.
        drop(events);
        let problems = scanner.join();
        let owned = owner.join();
        stop.close();
        reporter.close();
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

/// Encodes a block, its files' bytes put one after another first, but for
/// one alone, as a segment is; or passes the end on, as `None`. It is not
/// compressed: the transport compresses the data, where the connection is.
fn encode_block(block: ToCompress) -> Result<Option<Vec<u8>>> {
    let Some((body, mut bytes)) = block else {
        return Ok(None);
    };
    let payload = match bytes.len() {
        1 => bytes.pop().expect("one"),
        _ => bytes.concat(),
    };
    block::encode(body, &payload, false, 0).map(Some)
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
                    // directory need not wait for. Said among the data, it
                    // goes before the blocks' end, handed over after: a file
                    // needed and neither sent nor said so by then fails there
                    // as never sent.
                    self.mux.data_message(&Manifest::Problem {
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
/// The needs go to the owner, the files needed one after another, as they
/// came at once, together.
fn receive(
    reader: &mut impl Read,
    events: &Sender<Event>,
    dry_run: bool,
    summary: &mut Summary,
    control_end: &Receiver<()>,
) -> Result<()> {
    // The owner stops only as the Source does, which says why, or once the
    // blocks have ended, when it needs nothing more: either way, what it can
    // no longer take is dropped.
    let tell = |needs: Needs| {
        let _ = events.send(Event::Needs(needs));
    };
    let mut reader = io::BufReader::with_capacity(64 << 10, reader);
    let mut files = Vec::new();
    loop {
        // Told before it waits for more, and before any other answer.
        if !files.is_empty() && reader.buffer().is_empty() {
            tell(Needs::Files(std::mem::take(&mut files)));
        }
        let answer = match profile::wait(State::ReadConnection, || {
            protocol::read_record::<Answer>(&mut reader)
        }) {
            Ok(Some(answer)) => answer,
            // Ended, or cut short: the Destination's failure, if it said
            // one, is the cause, which it said on control before its pipes
            // ended, and control ends with them, its end going first.
            ended => {
                let _ = profile::wait(State::ReadConnection, || control_end.recv());
                ended?;
                bail!("the connection closed before the transfer finished");
            }
        };
        if let Answer::File(id) = answer {
            summary.needed += 1;
            files.push(id);
            continue;
        }
        if !files.is_empty() {
            tell(Needs::Files(std::mem::take(&mut files)));
        }
        match answer {
            Answer::File(_) => unreachable!("taken above"),
            Answer::Attributes(_) => summary.needed += 1,
            Answer::Compared { first, last } => tell(Needs::Compared { first, last }),
            Answer::NeedsEnd if dry_run => return Ok(()),
            Answer::NeedsEnd => tell(Needs::End),
            // The Destination reports what failed, and its warnings, itself.
            Answer::Failed { .. } | Answer::NotDeleted { .. } => summary.failed += 1,
            Answer::Done => return Ok(()),
        }
    }
}

/// Reads the Destination's control pipe, until the Source stops reading it,
/// which has nothing on it but its end, or the Destination's failure: that
/// is the cause, and stops the Source.
fn receive_control(reader: &mut impl Read, stop: &Stop) -> Result<()> {
    let Some(packet) = profile::wait(State::ReadConnection, || protocol::receive(reader))? else {
        return Ok(());
    };
    match packet.decode::<Control>()? {
        // The Destination's failure is the cause, and it knows of it.
        Control::Error { message } => {
            stop.peer_failed(anyhow::anyhow!("the Destination failed: {message}"));
            Err(Stopped.into())
        }
        other => bail!("unexpected {other:?} from the Destination"),
    }
}

/// How many needed files may wait to be read before the scan waits.
///
/// Both sides keep track of each needed file until it is sent, and the
/// manifest can run far ahead of the data: the scan waits while more than
/// this many files wait to be read, and, as it writes, while the manifest's
/// window is used up, which the Destination gives back as it reads, so that
/// what each keeps stays bounded, by those, by what the Destination holds
/// to compare, and by a directory's own, whose needs come at once. Sending
/// files needs no more of the manifest, and the Destination reads it as it
/// goes, so the scan always goes on; once the files are all sent, or the
/// Source stops, it waits no more, and if it stopped, the scan stops too.
const BACKLOG: usize = 100_000;

/// Writes the scan's manifest, each batch's records at once, once the owner
/// has kept its entries.
struct ManifestSink<'a> {
    events: Sender<Event>,
    problems: u64,
    reporter: Reporter,
    numeric_ids: bool,
    /// IDs whose names have been sent, or looked up and found to have none.
    named_users: HashSet<u32>,
    named_groups: HashSet<u32>,
    /// Once the Source stops, nothing more of the manifest is sent: the scan
    /// stops with it.
    stop: &'a Stop,
    /// Where its records go, on the manifest's pipe, which the transport
    /// compresses where the connection is.
    mux: &'a Mux,
}

impl ManifestSink<'_> {
    /// The names of the IDs in `entries` not named before, to send before
    /// them.
    fn new_names(&mut self, entries: &[manifest::Entry]) -> Vec<Manifest> {
        if self.numeric_ids {
            return Vec::new();
        }
        let mut users: Vec<u32> = entries.iter().map(|entry| entry.uid).collect();
        let mut groups: Vec<u32> = entries.iter().map(|entry| entry.gid).collect();
        users.retain(|uid| self.named_users.insert(*uid));
        groups.retain(|gid| self.named_groups.insert(*gid));
        if users.is_empty() && groups.is_empty() {
            return Vec::new();
        }
        let _names = profile::enter(State::Names);
        let users = names::names(Database::Users, &users)
            .into_iter()
            .map(|(id, name)| Manifest::User { id, name });
        let groups = names::names(Database::Groups, &groups)
            .into_iter()
            .map(|(id, name)| Manifest::Group { id, name });
        users.chain(groups).collect()
    }

    /// Writes `records`, at once, in their order, waiting for room.
    fn write(&mut self, records: impl IntoIterator<Item = Manifest>) -> Result<()> {
        let mut bytes = Vec::new();
        for record in records {
            protocol::put_record(&mut bytes, &record)?;
        }
        self.mux.manifest(&bytes).context("write the manifest")
    }

    /// Hands the owner the entries of a batch, and waits until it says the
    /// batch may go: the scan may be faster than the connection, or than the
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
    fn root(&mut self, entry: manifest::Entry) -> Result<()> {
        self.wait_for_room(
            KnownPart::of(b"", std::slice::from_ref(&entry)),
            Wait::Never,
        )?;
        self.stop.check()?;
        let mut records = self.new_names(std::slice::from_ref(&entry));
        records.push(Manifest::Root(entry));
        self.write(records)
    }

    fn entries(&mut self, batch: Batch) -> Result<()> {
        // The Destination takes no more than the manifest's window allows,
        // which the writing waits for; the batch, before that, for the
        // files to be read.
        self.wait_for_room(KnownPart::of(batch.path, &batch.entries), Wait::Backlog)?;
        self.stop.check()?;
        let mut records = self.new_names(&batch.entries);
        // Each batch goes out as soon as it is read, so the Destination
        // compares while the scan goes on.
        records.extend(batch.into_records());
        self.write(records)
    }

    fn unlistable(&mut self, dir: manifest::FileId) -> Result<()> {
        self.stop.check()?;
        self.write([Manifest::Unlistable { dir }])
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
        self.write([Manifest::Problem {
            path: path.to_vec(),
            file: None,
        }])
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

    fn end(&mut self) -> Result<()> {
        self.stop.check()?;
        self.write([Manifest::End])
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
            report: Reporter::none(),
            batch_entries: 0,
            backlog_files: 0,
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
                    if matches!(packet.decode::<Manifest>(), Ok(Manifest::In { .. })) {
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
    /// and the hashes sent of those, once the blocks have ended, numbered
    /// from 0 in the order sent, and said how many.
    fn sent_files(written: &[u8]) -> Option<(Files, Vec<FileHash>)> {
        let mut reader = written;
        let mut assembler = block::Assembler::default();
        let mut files: Vec<(FileId, std::collections::BTreeMap<u64, Vec<u8>>)> = Vec::new();
        let mut hashes = Vec::new();
        let mut numbers = Vec::new();
        let mut ended = None;
        while let Some(packet) = protocol::receive(&mut reader).ok()? {
            match packet.kind {
                PacketType::Data => {
                    for encoded in assembler.push(&packet.into_payload()).ok()? {
                        numbers.push(block::sequence(&encoded));
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
                PacketType::BlocksEnd => ended = Some(packet.decode::<BlocksEnd>().ok()?.blocks),
                _ => {}
            }
        }
        let files = files
            .into_iter()
            .map(|(id, pieces)| (id, pieces.into_values().flatten().collect()))
            .collect();
        // Numbered in the order sent, from 0, as many as it says it sent.
        let sent = numbers.len() as u64;
        (ended == Some(sent) && numbers.into_iter().eq(0..sent)).then_some((files, hashes))
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
                        protocol::encode(&Answer::File(1)).unwrap(),
                        protocol::encode(&Answer::NeedsEnd).unwrap(),
                    ]
                    .concat();
                } else if !self.done {
                    wait_for(&|written| sent_files(written).is_some());
                    self.answers = protocol::encode(&Answer::Done).unwrap();
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
