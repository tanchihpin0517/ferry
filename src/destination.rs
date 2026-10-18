//! The Destination peer: compares the manifest with its own tree, says what it
//! needs, and makes its tree match.
//!
//! The main thread reads packets: it compares each manifest message as it
//! arrives, creates the directories, symlinks, and special files it needs,
//! sets attributes that differ, and hands data packets to a writer thread,
//! which writes the files in the record stream. When the stream ends, the
//! writer thread sets the directories' attributes and mtimes, deepest first,
//! since writing into a directory changes its mtime.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use rustix::fs::FileType;
use rustix::io::Errno;

use crate::compare::{Comparer, Extras, Prepared, Tree, Verdict};
use crate::frame;
use crate::fsys;
use crate::interrupt;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::mux::{self, Mux};
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::progress;
use crate::protocol::{self, Control, FileKey, Needs, PacketType, Results};
use crate::record::{self, ChannelReader, Header};
use crate::report::{Count, Reporter, Stage, TRANSFERRING_EVERY, Transfer};
use crate::scan;
use crate::source::display;

pub struct Options {
    /// The Destination's root, as path bytes.
    pub root: Vec<u8>,
    pub dry_run: bool,
    pub modify_window: i64,
    pub numeric_ids: bool,
    pub fsync: bool,
    /// Delete what the Source does not have.
    pub delete: bool,
    /// Report what is found and what goes wrong, to the client, in report
    /// packets.
    pub report: bool,
    /// The Source's root, then each directory above it, when the client
    /// found them on the same kernel: none of them is deleted, nor the
    /// Source's root gone into. Reaching one stops the transfer.
    pub protect: Vec<FileKey>,
}

/// What went wrong, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    /// Entries that could not be made to match, or deleted.
    pub failed: u64,
    /// Paths the Source could not read.
    pub problems: u64,
}

/// Reports a line for standard error.
fn tell(reporter: &Reporter, message: std::fmt::Arguments) {
    reporter.say(message.to_string());
}

/// What the main thread and the writer thread share.
struct State {
    ownership: Arc<Ownership>,
    /// Files whose content is coming, by ID.
    targets: HashMap<FileId, Target>,
    /// The directories announced and not finished yet, by path; the root's
    /// is empty. See [`Open`].
    open: HashMap<Vec<u8>, Open>,
    /// The directories whose parts were applied, as the manifest went into
    /// them, depth first: each one's parent is below it.
    stack: Vec<Vec<u8>>,
    /// Where a directory done goes to be finished, in the order it is done.
    finishing: Option<mpsc::Sender<(Vec<u8>, Entry)>>,
    warnings: Warnings,
    /// Whether to delete what the Source does not have.
    delete: bool,
    /// What is never deleted; see [`Options::protect`].
    protect: Arc<Vec<FileKey>>,
    /// Why the transfer is to stop: something to be deleted was one to
    /// protect.
    refused: Option<String>,
    /// Entries that could not be made to match, or deleted.
    failures: u64,
    /// Paths the Source could not read.
    source_problems: u64,
    /// The root, as the comparison opened it, once: what is under it is
    /// reached from it, never from its path again, which may have come to
    /// name another since, as a symlink switched or a disk mounted.
    root_directory: Option<Arc<OwnedFd>>,
    reporter: Reporter,
}

impl State {
    fn new(delete: bool) -> Self {
        Self {
            ownership: Arc::new(Ownership::here()),
            targets: HashMap::new(),
            open: HashMap::new(),
            stack: Vec::new(),
            finishing: None,
            warnings: Warnings::default(),
            delete,
            protect: Arc::new(Vec::new()),
            refused: None,
            failures: 0,
            source_problems: 0,
            root_directory: None,
            reporter: Reporter::none(),
        }
    }
}

/// How many manifest entries apart the Source is told it need keep them no
/// more.
const THROUGH_EVERY: FileId = 4096;

/// How many decompressed frames, of up to [`frame::FRAME`] each, may wait for
/// the parser: 16 MiB. Beyond, decompressing waits, and so reading the
/// connection, as for a disk slower than the network.
const DECODED: usize = 8;

/// A directory announced, by its parent's entries or as the root, and not
/// finished yet. Its attributes and mtime are set last, once nothing more is
/// made in it, nor under it: when the manifest has left it, and all under
/// it, and every file in it is written, and every directory in it done.
/// That is as soon as it can be: its mtime changes only with what is made or
/// removed in it, and it can be one that cannot be gone into after.
struct Open {
    entry: Entry,
    /// It, or something in it, changed: done, it is finished, not just let go.
    finish: bool,
    /// The manifest has left it, and all under it.
    left: bool,
    /// The files in it needed and not settled yet.
    files: usize,
    /// The directories in it not done yet.
    children: usize,
    /// Of those, the names whose parts have not come: once it is left, none
    /// will, as for one the Source could not list.
    unseen: HashSet<Vec<u8>>,
}

/// The directory a path is in, and its name there; none for the root.
fn parent_and_name(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() {
        return None;
    }
    Some(match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&b""[..], path),
    })
}

/// Whether `path` is `directory`, or under it.
fn is_under(path: &[u8], directory: &[u8]) -> bool {
    directory.is_empty()
        || path == directory
        || (path.starts_with(directory) && path.get(directory.len()) == Some(&b'/'))
}

impl State {
    /// A directory announced: by its parent's entry, or the root.
    fn announce(&mut self, path: Vec<u8>, entry: Entry) {
        if let Some((parent, name)) = parent_and_name(&path)
            && let Some(parent) = self.open.get_mut(parent)
        {
            parent.children += 1;
            parent.unseen.insert(name.to_vec());
        }
        self.open.insert(
            path,
            Open {
                entry,
                finish: false,
                left: false,
                files: 0,
                children: 0,
                unseen: HashSet::new(),
            },
        );
    }

    /// The directory at `path` is to be finished, once done.
    fn mark_to_finish(&mut self, path: &[u8]) {
        if let Some(open) = self.open.get_mut(path) {
            open.finish = true;
        }
    }

    /// A file in `directory` is needed.
    fn file_needed(&mut self, directory: &[u8]) {
        if let Some(open) = self.open.get_mut(directory) {
            open.files += 1;
        }
    }

    /// A file in `directory` is settled: written, or failed.
    fn file_settled(&mut self, directory: &[u8]) {
        if let Some(open) = self.open.get_mut(directory) {
            open.files = open.files.saturating_sub(1);
        }
        self.done_if_so(directory.to_vec());
    }

    /// The parts of the directory at `path` are applied, as the manifest
    /// goes into it: what it has left behind, it has left.
    fn enter(&mut self, path: &[u8]) {
        while let Some(top) = self.stack.last() {
            if is_under(path, top) {
                break;
            }
            let top = self.stack.pop().expect("one is there");
            self.leave(top);
        }
        if let Some((parent, name)) = parent_and_name(path)
            && let Some(parent) = self.open.get_mut(parent)
        {
            parent.unseen.remove(name);
        }
        self.stack.push(path.to_vec());
    }

    /// The manifest is over: it has left every directory.
    fn leave_all(&mut self) {
        while let Some(top) = self.stack.pop() {
            self.leave(top);
        }
    }

    /// The manifest has left the directory at `path`, and all under it.
    fn leave(&mut self, path: Vec<u8>) {
        let unseen = match self.open.get_mut(&path) {
            Some(open) => std::mem::take(&mut open.unseen),
            None => return,
        };
        for name in unseen {
            let child = manifest::join(&path, &name);
            if let Some(open) = self.open.get_mut(&child) {
                open.left = true;
            }
            self.done_if_so(child);
        }
        if let Some(open) = self.open.get_mut(&path) {
            open.left = true;
        }
        self.done_if_so(path);
    }

    /// If the directory at `path` is done, lets it go, to be finished if it
    /// is to be, and so on up, as each one's last undone directory is.
    fn done_if_so(&mut self, mut path: Vec<u8>) {
        loop {
            let done = self
                .open
                .get(&path)
                .is_some_and(|open| open.left && open.files == 0 && open.children == 0);
            if !done {
                return;
            }
            let open = self.open.remove(&path).expect("there");
            if open.finish
                && let Some(finishing) = &self.finishing
            {
                let _ = finishing.send((path.clone(), open.entry));
            }
            let Some((parent, _)) = parent_and_name(&path) else {
                return;
            };
            let parent = parent.to_vec();
            if let Some(open) = self.open.get_mut(&parent) {
                open.children = open.children.saturating_sub(1);
            }
            path = parent;
        }
    }
}

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
#[derive(Clone)]
struct Target {
    directory: Vec<u8>,
    entry: Entry,
    /// The root itself, which is a file: said so, rather than taken from a
    /// name, which a manifest sets.
    is_root: bool,
}

/// Runs the Destination over a connection. A dry run reports each path it
/// lists, in manifest order. The Destination reports the counts and what it
/// cannot do; the Source reports the paths it cannot read.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
) -> Result<Summary> {
    let mut state = State::new(options.delete);
    state.protect = Arc::new(options.protect.clone());
    let state = Mutex::new(state);
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));
    let state = &state;
    // The reading failed: what the writer then fails at follows from it.
    let reading_failed = &AtomicBool::new(false);

    std::thread::scope(|scope| -> Result<()> {
        let (mux, outlet) = mux::channel();
        let pump = scope.spawn(move || mux::pump(writer, outlet));
        let reporter = if options.report {
            Reporter::to_mux(mux.clone())
        } else {
            Reporter::none()
        };
        state.lock().expect("not poisoned").reporter = reporter.clone();
        let (data, stream) = mpsc::sync_channel::<Vec<u8>>(DECODED);
        let files = (!options.dry_run).then(|| {
            let mux = mux.clone();
            let reporter = reporter.clone();
            // Directories are finished as they are done, by a thread of
            // their own, in that order.
            let (finishing, done) = mpsc::channel();
            state.lock().expect("not poisoned").finishing = Some(finishing);
            let finisher = scope.spawn(move || finish_as_done(root, state, done, options.fsync));
            scope.spawn(move || -> Result<()> {
                let _thread = profile::thread("parser");
                let written = write_files(
                    root,
                    state,
                    &mux,
                    ChannelReader::new(stream, profile::State::Stream),
                    options.fsync,
                );
                let result = match written {
                    Ok(()) => finish_directories(state, finisher),
                    // Nothing more is finished: the finisher ends, and says
                    // why, if it failed too.
                    Err(error) => {
                        state.lock().expect("not poisoned").finishing.take();
                        let _ = finisher.join();
                        Err(error)
                    }
                }
                .and_then(|()| make_durable(root, state, options.fsync));
                for message in state.lock().expect("not poisoned").warnings.messages() {
                    tell(&reporter, format_args!("warning: {message}"));
                }
                match &result {
                    // In order after the results, which must arrive first.
                    Ok(()) => mux.message(&Control::Done)?,
                    // A writer that failed because the reading did, its stream
                    // ending early, is not the cause: the reading's failure is
                    // returned, for the server to report instead.
                    Err(_) if reading_failed.load(Ordering::SeqCst) => {}
                    Err(error) => mux.message(&Control::Error {
                        message: format!("{error:#}"),
                    })?,
                }
                result
            })
        });

        // Frames are decompressed by a pool, and fed to the file writer in
        // order by a thread of their own.
        let (frames, decoded) = pool::gauged(
            scope,
            pool::threads(8),
            16,
            Some(reporter.gauge(Stage::Decompress, 16)),
            Profiled {
                role: "decompress",
                work: profile::State::Decompress,
                full: profile::State::ToDecompress,
            },
            |encoded: Vec<u8>| frame::decode(&encoded),
        );
        let feeding = scope.spawn(move || -> Result<()> {
            let _thread = profile::thread("feeder");
            for bytes in decoded {
                let bytes = bytes?;
                if profile::wait(profile::State::ToParser, || data.send(bytes)).is_err() {
                    // The writer stopped, and reports why itself.
                    break;
                }
            }
            Ok(())
        });
        let mut frames = frames;
        let read = read_packets(
            reader,
            &mux,
            &comparer,
            root,
            state,
            options,
            &reporter,
            &mut frames,
        );
        // Known before the stream ends, which closing the frames does.
        if read.is_err() {
            reading_failed.store(true, Ordering::SeqCst);
        }
        drop(frames);
        drop(mux);
        let fed = feeding.join().expect("the frame feeder panicked");
        let written = files.map(|files| files.join().expect("the file writer panicked"));
        reporter.close();
        // With every handle dropped, the pump writes what is queued and ends.
        let pumped = pump.join().expect("the pump panicked");
        read?;
        fed?;
        written.transpose()?;
        pumped?;
        Ok(())
    })?;
    let state = state.lock().expect("not poisoned");
    Ok(Summary {
        failed: state.failures,
        problems: state.source_problems,
    })
}

#[allow(clippy::too_many_arguments)]
fn read_packets(
    reader: &mut impl Read,
    mux: &Mux,
    comparer: &Comparer,
    root: &Path,
    state: &Mutex<State>,
    options: &Options,
    reporter: &Reporter,
    frames: &mut pool::Submitter<Vec<u8>>,
) -> Result<()> {
    // The Destination's own tree is scanned as the manifest comes, by a
    // thread of its own, one directory at a time; a pool stats the entries
    // of each part, and the results are applied here, in the manifest's order.
    std::thread::scope(|scope| -> Result<()> {
        // The results are taken here, which also feeds the comparisons:
        // their room is not bounded, or this would wait on itself. They are
        // taken as each packet is.
        let (mut comparisons, mut compared) = pool::ordered_unbounded(
            scope,
            scan::STATS,
            64,
            Profiled {
                role: "stat",
                work: profile::State::Compare,
                full: profile::State::ToStat,
            },
            |Comparison {
                 path,
                 entries,
                 last,
                 ownership,
                 prepared,
             }: Comparison| {
                let directory = prepared.directory.as_ref().ok().cloned().flatten();
                let let_in = prepared.let_in;
                let refused = prepared.refused;
                let verdicts =
                    comparer.part(prepared.directory, &entries, &prepared.present, &ownership);
                Compared {
                    path,
                    entries,
                    last,
                    directory,
                    verdicts,
                    extras: prepared.extras,
                    let_in,
                    refused,
                }
            },
        );
        let (parts_in, parts_out) = mpsc::sync_channel::<Part>(PARTS);
        let root_bytes = options.root.clone();
        scope.spawn(move || {
            let _thread = profile::thread("compare");
            // What the Source does not have is found before anything here
            // changes: each directory is listed before its parts are applied.
            let mut tree = Tree::new(
                root_bytes,
                options.delete,
                !options.dry_run,
                options.protect.first().copied(),
            );
            while let Ok(part) = profile::wait(profile::State::Work, || parts_out.recv()) {
                let prepared = tree.part(&part.path, &part.entries, part.more);
                let comparison = Comparison {
                    path: part.path,
                    entries: part.entries,
                    last: !part.more,
                    ownership: part.ownership,
                    prepared,
                };
                if comparisons.submit(comparison).is_err() {
                    return;
                }
            }
        });
        let mut parts_in = Some(parts_in);
        // The compared parts of a directory waiting for its last.
        let mut waiting: Vec<Compared> = Vec::new();
        // The last ID the Source was told it need keep no more.
        let mut through: FileId = 0;
        // Nothing of the manifest is acted on before it is checked.
        let mut check = manifest::Check::default();
        let invalid = "the Source sent an invalid manifest";
        // Dropping `frames` when this returns ends the record stream for the
        // writer.
        let mut assembler = frame::Assembler::default();
        let _thread = profile::thread("receive");
        loop {
            while let Some(done) = compared.try_next() {
                profile::wait(profile::State::Apply, || {
                    take_compared(
                        done,
                        &mut waiting,
                        &mut through,
                        mux,
                        root,
                        state,
                        options,
                        reporter,
                    )
                })?;
            }
            let Some(packet) =
                profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
            else {
                return Ok(());
            };
            match packet.kind {
                PacketType::Manifest => match packet.decode::<Manifest>()? {
                    Manifest::Names { users, groups } => {
                        if !options.numeric_ids {
                            let mut state = state.lock().expect("not poisoned");
                            Arc::make_mut(&mut state.ownership).learn(&users, &groups);
                        }
                    }
                    Manifest::Problem { path, file } => {
                        // The Source reports it; here it is only counted. What
                        // it could not read is in the manifest, as unreadable,
                        // so it is never taken for extra.
                        let _ = path;
                        state.lock().expect("not poisoned").source_problems += 1;
                        // A needed file it could not open never comes: it is
                        // settled, so its directory is finished without it.
                        // One not waited for, whatever the Source says, is
                        // nothing to settle.
                        if let Some(id) = file {
                            settled(state, id);
                        }
                    }
                    Manifest::End => {
                        check.end().context(invalid)?;
                        // Every comparison comes out before the needs end.
                        drop(parts_in.take());
                        for done in compared.by_ref() {
                            profile::wait(profile::State::Apply, || {
                                take_compared(
                                    done,
                                    &mut waiting,
                                    &mut through,
                                    mux,
                                    root,
                                    state,
                                    options,
                                    reporter,
                                )
                            })?;
                        }
                        if !options.dry_run {
                            state.lock().expect("not poisoned").leave_all();
                        }
                        mux.message(&Needs::End)?;
                    }
                    Manifest::Root(entry) => {
                        check.root(&entry).context(invalid)?;
                        refuse_another_kind(root, &entry)?;
                        let mut state = state.lock().expect("not poisoned");
                        let verdict = comparer
                            .root(&entry, &state.ownership)
                            .context("compare the destination's root")?;
                        if entry.kind == Kind::Directory && !options.dry_run {
                            state.announce(Vec::new(), entry.clone());
                        }
                        if verdict.is_listed() && options.dry_run {
                            reporter.listed(vec![b".".to_vec()]);
                        }
                        if !options.dry_run {
                            apply_root(root, &entry, verdict, &mut state)?;
                        }
                        drop(state);
                        reporter.add(Count::Entries, 1);
                        if verdict == Verdict::Content && entry.kind == Kind::File {
                            reporter.add(Count::Needed, 1);
                            reporter.add(Count::ToSend, entry.size);
                        }
                        let (files, attributes) = split(&[entry], &[verdict]);
                        send_needs(mux, files, attributes, Vec::new())?;
                    }
                    Manifest::Directory {
                        path,
                        entries,
                        more,
                    } => {
                        check.directory(&path, &entries, more).context(invalid)?;
                        reporter.add(Count::Entries, entries.len() as u64);
                        let ownership = Arc::clone(&state.lock().expect("not poisoned").ownership);
                        let part = Part {
                            path,
                            entries,
                            more,
                            ownership,
                        };
                        let sent = profile::wait(profile::State::ToCompare, || {
                            parts_in
                                .as_ref()
                                .map(|parts_in| parts_in.send(part).is_ok())
                        });
                        match sent {
                            None => bail!("a directory after the manifest's end"),
                            Some(false) => bail!("the comparison stopped"),
                            Some(true) => {}
                        }
                    }
                },
                PacketType::Control => match packet.decode::<Control>()? {
                    Control::Error { message } => bail!("the Source failed: {message}"),
                    other => bail!("unexpected {other:?} from the Source"),
                },
                PacketType::Data => {
                    let payload = packet.into_payload();
                    reporter.add(Count::Wire, payload.len() as u64);
                    for frame in assembler.push(&payload)? {
                        frames.submit(frame)?;
                    }
                }
                other => bail!("unexpected {other:?} packet from the Source"),
            }
        }
    })
}

/// How many parts may wait for the compare thread: a few, since the stat
/// pool's queue holds the rest.
const PARTS: usize = 4;

/// A directory of the manifest, or a part of one, for the compare thread.
struct Part {
    path: Vec<u8>,
    entries: Vec<Entry>,
    more: bool,
    ownership: Arc<Ownership>,
}

/// A part, as the compare thread prepared it, for a stat thread to compare.
struct Comparison {
    path: Vec<u8>,
    entries: Vec<Entry>,
    /// The directory's last part.
    last: bool,
    ownership: Arc<Ownership>,
    prepared: Prepared,
}

/// A directory of the manifest, compared by a worker.
struct Compared {
    path: Vec<u8>,
    entries: Vec<Entry>,
    /// The directory's last part.
    last: bool,
    /// The Destination's directory, as the comparison opened and listed it:
    /// what is deleted from it is deleted from that very directory, however
    /// its path has changed since. None if it was not there.
    directory: Option<Arc<OwnedFd>>,
    verdicts: io::Result<Vec<Verdict>>,
    /// For `sync`: what the Destination has that the Source does not.
    extras: Option<io::Result<Extras>>,
    /// Its permissions were changed to work in it: they are set again when
    /// it is finished.
    let_in: bool,
    /// It is the Source's root, which the comparison did not go into.
    refused: bool,
}

fn send_needs(
    mux: &Mux,
    files: Vec<FileId>,
    attributes: Vec<FileId>,
    deletes: Vec<(Vec<u8>, bool)>,
) -> Result<()> {
    if !files.is_empty() {
        mux.message(&Needs::Files(files))?;
    }
    if !attributes.is_empty() {
        mux.message(&Needs::Attributes(attributes))?;
    }
    if !deletes.is_empty() {
        mux.message(&Needs::Deletes(deletes))?;
    }
    Ok(())
}

/// Takes a part as it is compared. A directory's parts wait for its last,
/// as rsync's do: then, for `sync`, what it has extra is dealt with first,
/// and each part after, so that nothing made for it is ever taken for
/// extra, as it would be on a filesystem that takes two names for one, such
/// as one that ignores case. `copy` deletes nothing, and goes the same way.
#[allow(clippy::too_many_arguments)]
fn take_compared(
    done: Compared,
    waiting: &mut Vec<Compared>,
    through: &mut FileId,
    mux: &Mux,
    root: &Path,
    state: &Mutex<State>,
    options: &Options,
    reporter: &Reporter,
) -> Result<()> {
    // Gone into, the Destination would write into the Source as it is read,
    // and `sync` delete from it.
    if done.refused {
        bail!(
            "the Source's root is inside the Destination, at {}",
            String::from_utf8_lossy(&display(&done.path, true))
        );
    }
    if done.path.is_empty()
        && let Some(directory) = &done.directory
    {
        state
            .lock()
            .expect("not poisoned")
            .root_directory
            .get_or_insert_with(|| Arc::clone(directory));
    }
    let last = done.last;
    waiting.push(done);
    if !last {
        return Ok(());
    }
    // Only a directory's last part comes with its extras, and only for `sync`.
    let deletes = match waiting.last_mut().and_then(|done| done.extras.take()) {
        Some(extras) => {
            let path = waiting[0].path.clone();
            let directory = waiting.last().and_then(|done| done.directory.clone());
            apply_extras(
                &path,
                directory.as_deref(),
                extras,
                waiting,
                mux,
                state,
                options,
                reporter,
            )?
        }
        None => Vec::new(),
    };
    if !options.dry_run
        && let Some(done) = waiting.first()
    {
        let mut state = state.lock().expect("not poisoned");
        state.enter(&done.path);
        if waiting.iter().any(|done| done.let_in) {
            state.mark_to_finish(&done.path);
        }
    }
    let last = waiting
        .iter()
        .rev()
        .find_map(|done| done.entries.last())
        .map(|entry| entry.id);
    for done in waiting.drain(..) {
        apply_compared(done, mux, root, state, options, reporter)?;
    }
    send_needs(mux, Vec::new(), Vec::new(), deletes)?;
    // Every entry up to its last is compared, and those needed asked for:
    // the Source may let them go, told now and then.
    if let Some(last) = last
        && last >= *through + THROUGH_EVERY
    {
        mux.message(&Needs::Through(last))?;
        *through = last;
    }
    Ok(())
}

/// Deals with what the directory at `path` has that the Source does not,
/// before anything of its parts: lists it, in a dry run, or deletes it.
/// Returns it, for the Source.
#[allow(clippy::too_many_arguments)]
fn apply_extras(
    path: &[u8],
    directory: Option<&OwnedFd>,
    extras: io::Result<Extras>,
    parts: &[Compared],
    mux: &Mux,
    state: &Mutex<State>,
    options: &Options,
    reporter: &Reporter,
) -> Result<Vec<(Vec<u8>, bool)>> {
    let extras = match extras {
        Ok(extras) => extras,
        Err(error) => {
            state.lock().expect("not poisoned").failures += 1;
            reporter.add(Count::Failed, 1);
            let message = format!("cannot list what it holds: {error}");
            tell(
                reporter,
                format_args!(
                    "cannot delete in {}: {message}",
                    String::from_utf8_lossy(&display(path, true))
                ),
            );
            mux.message(&Results::NotDeleted {
                path: path.to_vec(),
                message,
            })?;
            return Ok(Vec::new());
        }
    };
    // Extras come only from a listing, so only from a directory held.
    let Some(directory) = directory else {
        return Ok(Vec::new());
    };
    let extras = without_aliases_of_unreadable(directory, path, parts, extras, reporter);
    let deletes: Vec<(Vec<u8>, bool)> = extras
        .into_iter()
        .map(|(name, is_directory)| (manifest::join(path, &name), is_directory))
        .collect();
    if options.dry_run {
        reporter.listed(
            deletes
                .iter()
                .map(|(extra, is_directory)| {
                    let mut shown = b"deleting ".to_vec();
                    shown.extend(display(extra, *is_directory));
                    shown
                })
                .collect(),
        );
    } else {
        delete_extras(directory, path, &deletes, state, mux)?;
    }
    Ok(deletes)
}

/// The extras, but those that are what the Source could not read, under
/// another name the filesystem takes for the same, as one that ignores case
/// does: what it could not read is never deleted.
fn without_aliases_of_unreadable(
    directory: &OwnedFd,
    path: &[u8],
    parts: &[Compared],
    mut extras: Extras,
    reporter: &Reporter,
) -> Extras {
    let unreadable: Vec<&[u8]> = parts
        .iter()
        .flat_map(|done| &done.entries)
        .filter(|entry| entry.kind == Kind::Unreadable)
        .map(|entry| entry.name.as_slice())
        .collect();
    if unreadable.is_empty() || extras.is_empty() {
        return extras;
    }
    #[allow(clippy::unnecessary_cast)]
    let identity = |stat: &rustix::fs::Stat| (stat.st_dev as u64, stat.st_ino as u64);
    let same: HashSet<(u64, u64)> = unreadable
        .iter()
        .filter_map(|name| fsys::stat(directory, name).ok())
        .map(|stat| identity(&stat))
        .collect();
    extras.retain(|(name, _)| {
        let aliased = fsys::stat(directory, name).is_ok_and(|stat| same.contains(&identity(&stat)));
        if aliased {
            tell(
                reporter,
                format_args!(
                    "warning: not deleting {}: it is what the source could not read, under \
                     another name",
                    String::from_utf8_lossy(&display(&manifest::join(path, name), false))
                ),
            );
        }
        !aliased
    });
    extras
}

/// Lists, makes match, and asks for what a compared part needs; its
/// directory's extras are dealt with apart, by [`apply_extras`].
fn apply_compared(
    done: Compared,
    mux: &Mux,
    root: &Path,
    state: &Mutex<State>,
    options: &Options,
    reporter: &Reporter,
) -> Result<()> {
    let Compared {
        path,
        entries,
        directory,
        verdicts,
        ..
    } = done;
    // What a dry run lists, sent once the state is let go: listing may wait.
    let mut listed = Vec::new();
    let (files, attributes): (Vec<FileId>, Vec<FileId>) = {
        {
            let mut state = state.lock().expect("not poisoned");
            let verdicts = verdicts.with_context(|| {
                format!("compare {}", String::from_utf8_lossy(&display(&path, true)))
            })?;
            for (entry, verdict) in entries.iter().zip(&verdicts) {
                let child = manifest::join(&path, &entry.name);
                if verdict.is_listed() && options.dry_run {
                    // In `sync`, a directory something else replaces is
                    // deleted, with all it holds: said so, as a run would do.
                    if options.delete
                        && *verdict == Verdict::Content
                        && entry.kind != Kind::Directory
                        && let Some(directory) = &directory
                        && fsys::stat(&**directory, &entry.name)
                            .is_ok_and(|stat| fsys::file_type(&stat) == FileType::Directory)
                    {
                        let mut shown = b"deleting ".to_vec();
                        shown.extend(display(&child, true));
                        listed.push(shown);
                    }
                    listed.push(display(&child, entry.kind == Kind::Directory));
                }
                if entry.kind == Kind::Directory && !options.dry_run {
                    state.announce(child, entry.clone());
                }
            }
            let failures = if options.dry_run {
                Vec::new()
            } else {
                apply_directory(
                    root,
                    &path,
                    directory.as_deref(),
                    &entries,
                    &verdicts,
                    &mut state,
                )
            };
            if let Some(reason) = state.refused.take() {
                bail!(reason);
            }
            state.failures += failures.len() as u64;
            reporter.add(Count::Failed, failures.len() as u64);
            drop(state);
            reporter.listed(std::mem::take(&mut listed));
            for (id, message) in &failures {
                if let Some(entry) = entries.iter().find(|entry| entry.id == *id) {
                    let child = manifest::join(&path, &entry.name);
                    tell(
                        reporter,
                        format_args!(
                            "cannot copy {}: {message}",
                            String::from_utf8_lossy(&display(
                                &child,
                                entry.kind == Kind::Directory
                            ))
                        ),
                    );
                }
                let results = Results::Failed {
                    id: *id,
                    message: message.clone(),
                };
                mux.message(&results)?;
            }
            // What failed is not asked for: it has nowhere to go.
            let failed: HashSet<FileId> = failures.iter().map(|(id, _)| *id).collect();
            let (files, attributes) = split(&entries, &verdicts);
            (
                files
                    .into_iter()
                    .filter(|id| !failed.contains(id))
                    .collect(),
                attributes
                    .into_iter()
                    .filter(|id| !failed.contains(id))
                    .collect(),
            )
        }
    };
    let sizes: HashMap<FileId, u64> = entries
        .iter()
        .filter(|entry| entry.kind == Kind::File)
        .map(|entry| (entry.id, entry.size))
        .collect();
    let needed: Vec<u64> = files
        .iter()
        .filter_map(|id| sizes.get(id).copied())
        .collect();
    reporter.add(Count::Needed, needed.len() as u64);
    reporter.add(Count::ToSend, needed.iter().sum());
    send_needs(mux, files, attributes, Vec::new())
}

/// The IDs needing content, and those needing only attributes.
fn split(entries: &[Entry], verdicts: &[Verdict]) -> (Vec<FileId>, Vec<FileId>) {
    let mut files = Vec::new();
    let mut attributes = Vec::new();
    for (entry, verdict) in entries.iter().zip(verdicts) {
        match verdict {
            Verdict::Content => files.push(entry.id),
            Verdict::Attributes => attributes.push(entry.id),
            Verdict::Times | Verdict::Same => {}
        }
    }
    (files, attributes)
}

/// Refuses a root of another kind than the Source's, where the Destination
/// has one: a directory where the Source's is not one, or the reverse. One
/// would replace the other, deleting all of it, and it is most likely a
/// location mistyped; the root comes first, so nothing is changed yet.
fn refuse_another_kind(root: &Path, entry: &Entry) -> Result<()> {
    // As the user named it: a symlink to a directory is one.
    let Ok(metadata) = std::fs::metadata(root) else {
        // Missing, or the comparison says why.
        return Ok(());
    };
    match (metadata.is_dir(), entry.kind == Kind::Directory) {
        (true, false) => bail!(
            "{} is a directory, and the source is not: to copy the source into it, \
             name where it goes inside, such as {}/NAME",
            root.display(),
            root.display()
        ),
        (false, true) => bail!(
            "{} exists and is not a directory, and the source is one",
            root.display()
        ),
        _ => Ok(()),
    }
}

fn apply_root(root: &Path, entry: &Entry, verdict: Verdict, state: &mut State) -> Result<()> {
    match (verdict, &entry.kind) {
        (Verdict::Same, _) => Ok(()),
        (Verdict::Content, Kind::Directory) => {
            match std::fs::symlink_metadata(root) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => bail!("{} exists and is not a directory", root.display()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    std::fs::create_dir(root)
                        .with_context(|| format!("create {}", root.display()))?;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("inspect {}", root.display()));
                }
            }
            state.mark_to_finish(b"");
            Ok(())
        }
        (_, Kind::Directory) => {
            state.mark_to_finish(b"");
            Ok(())
        }
        (Verdict::Content, Kind::File) => {
            state.targets.insert(
                entry.id,
                Target {
                    directory: Vec::new(),
                    entry: entry.clone(),
                    is_root: true,
                },
            );
            Ok(())
        }
        _ => {
            let (directory, name) = root_parent(root)?;
            apply_entry(&directory, &name, b".", entry, verdict, state)
        }
    }
}

/// Makes each entry of the directory at `path` match, as far as the main
/// thread can: everything but files' content. Returns the entries that could
/// not be made to match, and why; the rest go on.
fn apply_directory(
    root: &Path,
    path: &[u8],
    listed: Option<&OwnedFd>,
    entries: &[Entry],
    verdicts: &[Verdict],
    state: &mut State,
) -> Vec<(FileId, String)> {
    if verdicts.iter().all(|&verdict| verdict == Verdict::Same) {
        return Vec::new();
    }
    // The directory as the comparison listed it; one it did not find, made
    // since by its parent, from the root.
    let opened = match listed {
        Some(listed) => rustix::io::dup(listed).map_err(io::Error::from),
        None => open_under(state, root, path),
    };
    let directory = match opened {
        Ok(directory) => directory,
        Err(error) => {
            // Nothing in it can be made: its parent could not be.
            return entries
                .iter()
                .zip(verdicts)
                .filter(|(_, verdict)| **verdict != Verdict::Same)
                .map(|(entry, _)| (entry.id, format!("cannot open its directory: {error}")))
                .collect();
        }
    };
    // Creating or replacing anything in a directory changes its mtime.
    if verdicts.contains(&Verdict::Content) {
        state.mark_to_finish(path);
    }
    let mut failures = Vec::new();
    for (entry, &verdict) in entries.iter().zip(verdicts) {
        let child = manifest::join(path, &entry.name);
        if entry.kind == Kind::Directory {
            if verdict == Verdict::Content
                && let Err(error) = make_directory(&directory, &entry.name, entry.mode)
            {
                failures.push((entry.id, format!("{error:#}")));
                continue;
            }
            if verdict != Verdict::Same {
                state.mark_to_finish(&child);
            }
            continue;
        }
        if verdict == Verdict::Content && entry.kind == Kind::File {
            state.targets.insert(
                entry.id,
                Target {
                    directory: path.to_vec(),
                    entry: entry.clone(),
                    is_root: false,
                },
            );
            state.file_needed(path);
            continue;
        }
        if let Err(error) = apply_entry(&directory, &entry.name, &child, entry, verdict, state) {
            if is_protected(&error) {
                state.refused.get_or_insert_with(|| {
                    format!(
                        "will not delete {} to make {}: {error:#}",
                        String::from_utf8_lossy(&display(&child, true)),
                        String::from_utf8_lossy(&display(&child, false)),
                    )
                });
            }
            failures.push((entry.id, format!("{error:#}")));
        }
    }
    failures
}

/// Makes a symlink or special file match, or sets a file's attributes. Files'
/// content and directories are handled elsewhere.
fn apply_entry(
    directory: impl AsFd,
    name: &[u8],
    shown: &[u8],
    entry: &Entry,
    verdict: Verdict,
    state: &mut State,
) -> Result<()> {
    match verdict {
        Verdict::Same => Ok(()),
        Verdict::Times => Ok(fsys::set_mtime_at(&directory, name, entry.mtime)?),
        Verdict::Attributes => {
            let stat = fsys::stat(&directory, name)?;
            #[allow(clippy::unnecessary_cast)]
            let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
            set_owner(&state.ownership, uid, gid, entry, |uid, gid| {
                fsys::set_owner_at(&directory, name, uid, gid)
            })?;
            if !entry.kind.is_symlink() {
                fsys::set_mode_at(&directory, name, entry.mode & 0o7777)?;
            }
            fsys::set_mtime_at(&directory, name, entry.mtime)?;
            Ok(())
        }
        Verdict::Content => {
            // Made under a temporary name, then renamed over whatever is there.
            let made = interrupt::create(&directory, || {
                loop {
                    let candidate = fsys::temporary_name(name);
                    let made = match &entry.kind {
                        Kind::Symlink { target } => {
                            fsys::make_symlink(&directory, &candidate, target)
                        }
                        kind => fsys::make_special(&directory, &candidate, kind, entry.mode),
                    };
                    match made {
                        Ok(()) => return Ok(((), candidate)),
                        Err(error) if Errno::from_io_error(&error) == Some(Errno::EXIST) => {
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
            });
            let ((), temporary, registered) = match made {
                Ok(made) => made,
                Err(error) if Errno::from_io_error(&error) == Some(Errno::PERM) => {
                    state.warnings.add(Warning::Special, shown);
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let finished = (|| -> Result<()> {
                let stat = fsys::stat(&directory, &temporary)?;
                #[allow(clippy::unnecessary_cast)]
                let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
                set_owner(&state.ownership, uid, gid, entry, |uid, gid| {
                    fsys::set_owner_at(&directory, &temporary, uid, gid)
                })?;
                if !entry.kind.is_symlink() {
                    fsys::set_mode_at(&directory, &temporary, entry.mode & 0o7777)?;
                }
                fsys::set_mtime_at(&directory, &temporary, entry.mtime)?;
                clear_the_way(&directory, name, state.delete, &state.protect)?;
                fsys::rename(&directory, &temporary, name)?;
                Ok(())
            })();
            if finished.is_err() {
                let _ = fsys::remove(&directory, &temporary, false);
            }
            drop(registered);
            finished
        }
    }
}

/// Gives an entry the Source's owner, mapped here, as far as this process may.
/// What it may not set, it leaves, without a word, as rsync does: a user who
/// is not root cannot keep another's files theirs, and keeping their own is
/// what they expect. `chown` takes the user and group to set, `None` leaving
/// one alone.
fn set_owner(
    ownership: &Ownership,
    current_uid: u32,
    current_gid: u32,
    entry: &Entry,
    chown: impl FnOnce(Option<u32>, Option<u32>) -> io::Result<()>,
) -> Result<()> {
    let uid = ownership.uid(entry.uid);
    let gid = ownership.gid(entry.gid);
    let set_uid = (uid != current_uid && ownership.can_set_user(uid, current_uid)).then_some(uid);
    let set_gid = (gid != current_gid && ownership.can_set_group(gid)).then_some(gid);
    if set_uid.is_some() || set_gid.is_some() {
        chown(set_uid, set_gid)?;
    }
    Ok(())
}

/// Creates a directory, replacing whatever else is there but a directory.
/// The directory is made writable by its owner, so its entries can be written;
/// its own permissions are set at the end.
fn make_directory(parent: impl AsFd, name: &[u8], mode: u32) -> Result<()> {
    match fsys::stat(&parent, name) {
        Ok(stat) if fsys::file_type(&stat) == FileType::Directory => return Ok(()),
        Ok(_) => fsys::remove(&parent, name, false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fsys::make_directory(&parent, name, (mode & 0o7777) | 0o700)?;
    Ok(())
}

/// What became of one record.
enum Written {
    /// Written, checked, and pushed to the drive, under its temporary name:
    /// ready to be renamed into place with its batch.
    Ready(Ready),
    Failed(anyhow::Error),
}

/// A file written under its temporary name, waiting for its batch.
struct Ready {
    directory: OwnedFd,
    name: Vec<u8>,
    temporary: Vec<u8>,
    /// Kept open, to set a barrier on its drive.
    file: File,
    /// Its drive, for one barrier per drive.
    device: u64,
    /// The root is this file: its directory is not one that is finished.
    is_root: bool,
    /// Unregistered when dropped: once renamed into place, or removed.
    _registered: interrupt::Registered,
}

/// A file handed to a writer: where it goes, and its record's bytes, which
/// the parser forwards as it reads them.
struct FileJob {
    id: FileId,
    target: Target,
    shown: Vec<u8>,
    size: u64,
    bytes: mpsc::Receiver<Vec<u8>>,
}

/// A file handed to a writer and not yet settled, for the dashboard.
struct Transferring {
    id: FileId,
    path: String,
    size: u64,
    /// Its bytes forwarded to its writer so far.
    done: Arc<AtomicU64>,
}

/// Reports the files being received and written every
/// [`TRANSFERRING_EVERY`] while there are any, until `stop` is dropped, and
/// then that none is, once some were reported.
fn report_transferring(
    transferring: &Mutex<Vec<Transferring>>,
    reporter: &Reporter,
    stop: &mpsc::Receiver<()>,
) {
    let mut reported = false;
    while let Err(mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(TRANSFERRING_EVERY) {
        let files: Vec<Transfer> = transferring
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|file| Transfer {
                id: file.id,
                path: file.path.clone(),
                size: file.size,
                done: file.done.load(Ordering::Relaxed).min(file.size),
            })
            .collect();
        let none = files.is_empty();
        if reported || !none {
            reporter.transferring(files);
        }
        reported = !none;
    }
    if reported {
        reporter.transferring(Vec::new());
    }
}

/// How many files are renamed together, after one barrier per drive.
const BATCH: usize = 256;

/// A file of a batch, and what to report of it.
struct Batched {
    id: FileId,
    shown: Vec<u8>,
    size: u64,
    ready: Ready,
}

/// Writes every file in the record stream, until it ends, reporting each file
/// that fails, among them those the Source could not read whole.
///
/// The stream is parsed in order, by this thread; the files are written by a
/// pool of writers, one file each, so that many are written at once. A
/// file's bytes go to its writer through a channel of their own, as they are
/// parsed, so a large file is never held in memory whole.
///
/// A writer leaves its file under its temporary name, pushed to the drive.
/// This thread renames them in batches: one barrier per drive, so that every
/// file of the batch reaches storage before any of their names, then the
/// renames, in parallel.
fn write_files(
    root: &Path,
    state: &Mutex<State>,
    mux: &Mux,
    mut stream: ChannelReader,
    fsync: bool,
) -> Result<()> {
    let writers = pool::threads(16);
    let reporter = state.lock().expect("not poisoned").reporter.clone();
    // The files handed to the writers and not yet settled, by when they
    // started, for the dashboard.
    let transferring: Mutex<Vec<Transferring>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| -> Result<()> {
        // Dropped when this returns, which stops the reports.
        let (stop, stopped) = mpsc::channel::<()>();
        if reporter.is_open() {
            let (transferring, reporter) = (&transferring, reporter.clone());
            scope.spawn(move || report_transferring(transferring, &reporter, &stopped));
        }
        let _stop = stop;
        let (jobs, queue) = mpsc::sync_channel::<FileJob>(writers);
        let queue = Arc::new(Mutex::new(queue));
        let (finished, outcomes) = mpsc::channel::<(FileId, Vec<u8>, u64, Written)>();
        for _ in 0..writers {
            let queue = Arc::clone(&queue);
            let finished = finished.clone();
            scope.spawn(move || {
                let _thread = profile::thread("writer");
                loop {
                    let job = profile::wait(profile::State::Work, || {
                        queue.lock().expect("not poisoned").recv()
                    });
                    let Ok(job) = job else {
                        return;
                    };
                    let mut bytes = ChannelReader::new(job.bytes, profile::State::Bytes);
                    let written = profile::wait(profile::State::WriteFile, || {
                        write_file(root, &job.target, job.size, &mut bytes, state, fsync)
                    })
                    .unwrap_or_else(Written::Failed);
                    if finished
                        .send((job.id, job.shown, job.size, written))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        drop(finished);

        let mut in_flight = 0usize;
        // Files handed to the writers: a full queue, and every writer busy.
        let writing = reporter.gauge(Stage::Write, writers * 2);
        let mut batch: Vec<Batched> = Vec::with_capacity(BATCH);
        let fail = |id: FileId, shown: &[u8], message: String| -> Result<()> {
            state.lock().expect("not poisoned").failures += 1;
            reporter.add(Count::Failed, 1);
            tell(
                &reporter,
                format_args!("cannot copy {}: {message}", String::from_utf8_lossy(shown)),
            );
            mux.message(&Results::Failed { id, message })
        };
        // Renames the batch into place, and reports each file.
        let flush = |batch: &mut Vec<Batched>| -> Result<()> {
            if batch.is_empty() {
                return Ok(());
            }
            let renamed = profile::wait(profile::State::Rename, || {
                rename_batch(std::mem::take(batch), state, fsync)
            })?;
            for (done, result) in renamed {
                settled(state, done.id);
                match result {
                    Ok(()) => {
                        reporter.add(Count::Files, 1);
                        reporter.add(Count::Bytes, done.size);
                    }
                    // Never deleted, and the transfer stops: the Source's
                    // root, or a directory above it, is where this file goes.
                    Err(error) if is_protected(&error) => {
                        return Err(error.context(format!(
                            "will not delete {}/ to make {}",
                            String::from_utf8_lossy(&done.shown),
                            String::from_utf8_lossy(&done.shown),
                        )));
                    }
                    Err(error) => fail(done.id, &done.shown, format!("{error:#}"))?,
                }
            }
            Ok(())
        };
        // What becomes of a file once its writer is done with it.
        let settle = |(id, shown, size, written): (FileId, Vec<u8>, u64, Written),
                      batch: &mut Vec<Batched>|
         -> Result<()> {
            transferring
                .lock()
                .expect("not poisoned")
                .retain(|file| file.id != id);
            match written {
                Written::Ready(ready) => {
                    batch.push(Batched {
                        id,
                        shown,
                        size,
                        ready,
                    });
                    if batch.len() >= BATCH {
                        flush(batch)?;
                    }
                    Ok(())
                }
                Written::Failed(error) => {
                    settled(state, id);
                    fail(id, &shown, format!("{error:#}"))
                }
            }
        };
        loop {
            while let Ok(outcome) = outcomes.try_recv() {
                in_flight -= 1;
                settle(outcome, &mut batch)?;
            }
            writing.set(in_flight);
            match record::read_header(&mut stream)? {
                Header::File { id, size, mtime } => {
                    let target = state
                        .lock()
                        .expect("not poisoned")
                        .targets
                        .get(&id)
                        .cloned()
                        .with_context(|| format!("the Source sent unneeded ID {id}"))?;
                    let mut entry = target.entry.clone();
                    entry.mtime = mtime;
                    let target = Target { entry, ..target };
                    let shown = display(
                        &manifest::join(&target.directory, &target.entry.name),
                        false,
                    );
                    let done = Arc::new(AtomicU64::new(0));
                    transferring
                        .lock()
                        .expect("not poisoned")
                        .push(Transferring {
                            id,
                            path: String::from_utf8_lossy(&shown).into_owned(),
                            size,
                            done: Arc::clone(&done),
                        });
                    let (forward, bytes) = mpsc::sync_channel(4);
                    let job = FileJob {
                        id,
                        target,
                        shown,
                        size,
                        bytes,
                    };
                    let sent = profile::wait(profile::State::ToWriters, || jobs.send(job).is_ok());
                    if !sent {
                        bail!("the file writers stopped");
                    }
                    in_flight += 1;
                    forward_record(&mut stream, size, &forward, &reporter, &done)?;
                }
                Header::End => {
                    drop(jobs);
                    while in_flight > 0 {
                        let outcome = profile::wait(profile::State::Outcome, || outcomes.recv())
                            .context("the file writers stopped")?;
                        in_flight -= 1;
                        settle(outcome, &mut batch)?;
                    }
                    flush(&mut batch)?;
                    return Ok(());
                }
            }
        }
    })
}

/// Renames a batch of files into place: one barrier per drive first, unless
/// told not to sync, then the renames, in parallel. Returns each file with
/// what became of its rename.
fn rename_batch(
    batch: Vec<Batched>,
    state: &Mutex<State>,
    fsync: bool,
) -> Result<Vec<(Batched, Result<()>)>> {
    if fsync {
        let mut barriered = HashSet::new();
        for done in &batch {
            if barriered.insert(done.ready.device) {
                fsys::barrier(&done.ready.file).context("order the batch's writes")?;
            }
        }
    }
    let (delete, protect) = {
        let state = state.lock().expect("not poisoned");
        (state.delete, Arc::clone(&state.protect))
    };
    let threads = pool::threads(8).min(batch.len()).max(1);
    let chunk = batch.len().div_ceil(threads);
    let mut batch = batch;
    let mut chunks = Vec::new();
    while !batch.is_empty() {
        let rest = batch.split_off(chunk.min(batch.len()));
        chunks.push(std::mem::replace(&mut batch, rest));
    }
    let renamed = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let protect = Arc::clone(&protect);
                scope.spawn(move || {
                    let _thread = profile::thread("rename");
                    chunk
                        .into_iter()
                        .map(|done| {
                            let result = rename_into_place(&done.ready, delete, &protect, fsync);
                            (done, result)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a renamer panicked"))
            .collect()
    });
    Ok(renamed)
}

/// Renames one file of a batch into place, or removes it if that fails.
fn rename_into_place(ready: &Ready, delete: bool, protect: &[FileKey], fsync: bool) -> Result<()> {
    let renamed = (|| -> Result<()> {
        clear_the_way(&ready.directory, &ready.name, delete, protect)?;
        fsys::rename(&ready.directory, &ready.temporary, &ready.name)?;
        // A root that is a file has no directory of its own that is
        // finished; its name is made durable here.
        if fsync && ready.is_root {
            fsys::fsync(&ready.directory)?;
        }
        Ok(())
    })();
    if renamed.is_err() {
        let _ = fsys::remove(&ready.directory, &ready.temporary, false);
    }
    renamed
}

/// Forwards one record's content and trailer from the stream to its writer.
/// A writer reads its record to the end whatever happens, so this never waits
/// on one that gave up; if one stopped altogether, the rest is read and
/// dropped, keeping the stream in step.
fn forward_record(
    stream: &mut impl Read,
    size: u64,
    forward: &mpsc::SyncSender<Vec<u8>>,
    reporter: &Reporter,
    done: &AtomicU64,
) -> Result<()> {
    let mut remaining = size;
    while remaining > 0 {
        let count = (256 * 1024).min(remaining as usize);
        let mut chunk = vec![0; count];
        stream
            .read_exact(&mut chunk)
            .context("read the record stream")?;
        let _ = profile::wait(profile::State::ToWriter, || forward.send(chunk));
        reporter.add(Count::Received, count as u64);
        done.fetch_add(count as u64, Ordering::Relaxed);
        remaining -= count as u64;
    }
    let trailer = record::read_trailer(stream)?;
    let mut bytes = Vec::new();
    record::write_trailer(&mut bytes, &trailer)?;
    let _ = profile::wait(profile::State::ToWriter, || forward.send(bytes));
    reporter.add(Count::Arrived, 1);
    Ok(())
}

/// Writes one file: into a temporary file beside it, checked against the
/// Source's hash, given its attributes, and pushed to the drive unless told
/// not to sync. It is left under its temporary name, for its batch to rename.
///
/// The record is read to its end whatever happens to the file, so that the
/// stream stays in step; only an error reading the stream itself is returned
/// as an error.
fn write_file(
    root: &Path,
    target: &Target,
    size: u64,
    stream: &mut impl Read,
    state: &Mutex<State>,
    fsync: bool,
) -> Result<Written> {
    let opened = target_directory(root, target, state).and_then(|(directory, name)| {
        let (file, temporary, registered) =
            interrupt::create(&directory, || fsys::create_temporary(&directory, &name))?;
        Ok((directory, name, file, temporary, registered))
    });
    let mut local_error: Option<anyhow::Error> = None;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut remaining = size;
    let (mut file, opened) = match opened {
        Ok((directory, name, file, temporary, registered)) => {
            (Some(file), Some((directory, name, temporary, registered)))
        }
        Err(error) => {
            local_error = Some(error);
            (None, None)
        }
    };
    while remaining > 0 {
        let count = buffer.len().min(remaining as usize);
        stream
            .read_exact(&mut buffer[..count])
            .context("read the record stream")?;
        hasher.update(&buffer[..count]);
        if let (Some(writing), None) = (&mut file, &local_error)
            && let Err(error) = writing.write_all(&buffer[..count])
        {
            local_error = Some(error.into());
        }
        remaining -= count as u64;
    }
    let trailer = record::read_trailer(stream)?;

    let Some((directory, name, temporary, registered)) = opened else {
        return Ok(Written::Failed(
            local_error.expect("set when nothing was opened"),
        ));
    };
    let file = file.expect("open when the directory is");
    let finished = (|| -> Result<Written> {
        if let Some(error) = local_error {
            return Ok(Written::Failed(error));
        }
        if let Some(why) = trailer.unread {
            return Ok(Written::Failed(anyhow::anyhow!(
                "the Source could not read it whole: {why}"
            )));
        }
        if *hasher.finalize().as_bytes() != trailer.hash {
            bail!("the content's hash does not match the Source's");
        }
        apply_file_attributes(&file, &target.entry, state)?;
        if fsync {
            fsys::fsync(&file)?;
        }
        let device = file.metadata()?.dev();
        Ok(Written::Ready(Ready {
            directory: directory.try_clone()?,
            name: name.clone(),
            temporary: temporary.clone(),
            file: file.try_clone()?,
            device,
            is_root: target.is_root,
            _registered: registered,
        }))
    })();
    let written = finished.unwrap_or_else(Written::Failed);
    if !matches!(written, Written::Ready(_)) {
        let _ = fsys::remove(&directory, &temporary, false);
    }
    Ok(written)
}

/// Owner first, since setting it clears setuid and setgid, then permissions,
/// then the mtime, last since nothing may write to the file after it.
fn apply_file_attributes(file: &File, entry: &Entry, state: &Mutex<State>) -> Result<()> {
    let metadata = file.metadata()?;
    let ownership = Arc::clone(&state.lock().expect("not poisoned").ownership);
    set_owner(
        &ownership,
        metadata.uid(),
        metadata.gid(),
        entry,
        |uid, gid| fsys::set_owner(file, uid, gid),
    )?;
    fsys::set_mode(file, entry.mode & 0o7777)?;
    fsys::set_mtime(file, entry.mtime)?;
    Ok(())
}

/// A file is settled, written or failed: its target goes, and its directory
/// has one fewer to wait for.
fn settled(state: &Mutex<State>, id: FileId) {
    let mut state = state.lock().expect("not poisoned");
    if let Some(target) = state.targets.remove(&id)
        && !target.is_root
    {
        state.file_settled(&target.directory);
    }
}

/// Finishes each directory as it is done, in that order: a directory's
/// after all under it.
fn finish_as_done(
    root: &Path,
    state: &Mutex<State>,
    done: mpsc::Receiver<(Vec<u8>, Entry)>,
    fsync: bool,
) -> Result<()> {
    let _thread = profile::thread("finisher");
    for (path, entry) in done {
        finish_directory(root, state, &path, &entry, fsync)?;
    }
    Ok(())
}

/// Finishes what is still open once everything is written, deepest first,
/// should anything be: every directory is finished as it is done, but one
/// with a file that never came, not even as one the Source could not open,
/// would otherwise never be. Then waits for every directory to be finished.
fn finish_directories(
    state: &Mutex<State>,
    finisher: std::thread::ScopedJoinHandle<'_, Result<()>>,
) -> Result<()> {
    {
        let mut state = state.lock().expect("not poisoned");
        state.leave_all();
        let mut left: Vec<(Vec<u8>, Entry)> = state
            .open
            .drain()
            .filter(|(_, open)| open.finish)
            .map(|(path, open)| (path, open.entry))
            .collect();
        left.sort_by_key(|(path, _)| std::cmp::Reverse(depth(path)));
        if let Some(finishing) = state.finishing.take() {
            for directory in left {
                let _ = finishing.send(directory);
            }
        }
    }
    finisher.join().expect("the finisher panicked")
}

/// Sets a directory's owner, permissions, and mtime, now that nothing more
/// is made in it nor under it, and makes it durable unless told not to.
fn finish_directory(
    root: &Path,
    state: &Mutex<State>,
    path: &[u8],
    entry: &Entry,
    fsync: bool,
) -> Result<()> {
    let (directory, ownership) = {
        let state = state.lock().expect("not poisoned");
        (open_under(&state, root, path), Arc::clone(&state.ownership))
    };
    let finished = (|| -> Result<()> {
        let directory = directory?;
        let stat = rustix::fs::fstat(&directory)?;
        #[allow(clippy::unnecessary_cast)]
        let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
        set_owner(&ownership, uid, gid, entry, |uid, gid| {
            fsys::set_owner(&directory, uid, gid)
        })?;
        fsys::set_mode(&directory, entry.mode & 0o7777)?;
        fsys::set_mtime(&directory, entry.mtime)?;
        if fsync {
            fsys::fsync(&directory)?;
        }
        Ok(())
    })();
    finished.with_context(|| format!("finish {}", String::from_utf8_lossy(&display(path, true))))
}

/// Makes everything this transfer wrote durable, once, at the end: what was
/// only ordered on the way reaches storage.
fn make_durable(root: &Path, state: &Mutex<State>, fsync: bool) -> Result<()> {
    if !fsync {
        return Ok(());
    }
    let held = state.lock().expect("not poisoned").root_directory.clone();
    let directory = match held {
        Some(directory) => rustix::io::dup(&*directory)?,
        // A root that is a file: its directory.
        None => root_parent(root)?.0,
    };
    fsys::sync_durable(&directory).context("make what was written durable")
}

fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        1 + path.iter().filter(|&&byte| byte == b'/').count()
    }
}

/// The directory at `path` under the root: from the root as the comparison
/// opened it, once there is one, never from its path again; before, as for
/// a root that is not a directory, from its path.
fn open_under(state: &State, root: &Path, path: &[u8]) -> io::Result<OwnedFd> {
    match &state.root_directory {
        Some(directory) => fsys::open_path(&**directory, path),
        None => fsys::open_path_from(root, path),
    }
}

/// The directory a target is written in, and its name there.
fn target_directory(
    root: &Path,
    target: &Target,
    state: &Mutex<State>,
) -> Result<(OwnedFd, Vec<u8>)> {
    if target.is_root {
        return root_parent(root);
    }
    let directory = open_under(
        &state.lock().expect("not poisoned"),
        root,
        &target.directory,
    )?;
    Ok((directory, target.entry.name.clone()))
}

/// The directory a root that is not a directory is in, and its name there.
fn root_parent(root: &Path) -> Result<(OwnedFd, Vec<u8>)> {
    let parent = match root.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = root
        .file_name()
        .with_context(|| format!("{} has no file name", root.display()))?;
    Ok((
        fsys::open_root(parent.as_os_str())?,
        name.as_bytes().to_vec(),
    ))
}

/// Whether an error is, at its root, a deletion refused because what was to
/// be deleted is to be protected; see [`fsys::Protected`].
fn is_protected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(fsys::is_protected)
    })
}

/// Removes a directory where something else is about to go. Without
/// deleting, only an empty one is removed: a directory with anything in it
/// stays, and nothing is written. `sync` removes it with all it holds, but
/// for what is among `protect`.
fn clear_the_way(
    directory: impl AsFd,
    name: &[u8],
    delete: bool,
    protect: &[FileKey],
) -> Result<()> {
    match fsys::stat(&directory, name) {
        Ok(stat) if fsys::file_type(&stat) == FileType::Directory => {
            if delete {
                fsys::remove_tree(&directory, name, protect)?;
            } else {
                fsys::remove(&directory, name, true)
                    .context("a directory that is not empty is where this belongs")?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Deletes, for `sync`, what a directory has that the Source does not, as
/// soon as its comparison finds it. What the Source listed but could not read
/// is in the manifest, as unreadable, so it is never among these.
fn delete_extras(
    held: &OwnedFd,
    directory: &[u8],
    extras: &[(Vec<u8>, bool)],
    state: &Mutex<State>,
    mux: &Mux,
) -> Result<()> {
    if extras.is_empty() {
        return Ok(());
    }
    let (reporter, protect) = {
        let state = state.lock().expect("not poisoned");
        (state.reporter.clone(), Arc::clone(&state.protect))
    };
    let mut deleted = false;
    for (path, is_directory) in extras {
        // Each is a name joined to the directory's path.
        let name = &path[if directory.is_empty() {
            0
        } else {
            directory.len() + 1
        }..];
        let result = fsys::remove_tree(held, name, &protect);
        match result {
            Ok(()) => deleted = true,
            Err(error) if fsys::is_protected(&error) => {
                bail!(
                    "will not delete {}: {error}",
                    String::from_utf8_lossy(&display(path, *is_directory))
                );
            }
            Err(error) => {
                state.lock().expect("not poisoned").failures += 1;
                reporter.add(Count::Failed, 1);
                tell(
                    &reporter,
                    format_args!(
                        "cannot delete {}: {error}",
                        String::from_utf8_lossy(&display(path, *is_directory))
                    ),
                );
                mux.message(&Results::NotDeleted {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            }
        }
    }
    // Deleting changes the directory's mtime, which is set again.
    if deleted {
        state
            .lock()
            .expect("not poisoned")
            .mark_to_finish(directory);
    }
    Ok(())
}

/// What the Destination could not do, by kind, with a few of the paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Warning {
    Special,
}

impl Warning {
    fn describe(self) -> &'static str {
        match self {
            Warning::Special => "were not created: creating sockets and devices needs privilege",
        }
    }
}

#[derive(Default)]
struct Warnings {
    kinds: BTreeMap<Warning, (u64, Vec<Vec<u8>>)>,
}

impl Warnings {
    const EXAMPLES: usize = 3;

    fn add(&mut self, warning: Warning, path: &[u8]) {
        let (count, examples) = self.kinds.entry(warning).or_default();
        *count += 1;
        if examples.len() < Self::EXAMPLES {
            examples.push(path.to_vec());
        }
    }

    fn messages(&self) -> Vec<String> {
        self.kinds
            .iter()
            .map(|(warning, (count, examples))| {
                let examples: Vec<String> = examples
                    .iter()
                    .map(|path| String::from_utf8_lossy(path).into_owned())
                    .collect();
                let more = if *count as usize > examples.len() {
                    ", …"
                } else {
                    ""
                };
                format!(
                    "{} {} ({}{more})",
                    progress::counted(*count, "entry", "entries"),
                    warning.describe(),
                    examples.join(", ")
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Timestamp;
    use crate::record::Trailer;

    fn entry(id: FileId, name: &[u8], kind: Kind, size: u64) -> Entry {
        Entry {
            id,
            name: name.to_vec(),
            kind: kind.clone(),
            size,
            mtime: Timestamp {
                seconds: 1_600_000_000,
                nanoseconds: 0,
            },
            mode: if kind == Kind::Directory {
                0o755
            } else {
                0o644
            },
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
        }
    }

    /// What a Source would send for one file, `hello`, read whole or, with
    /// why, not.
    fn packets(unread: Option<&str>) -> Vec<u8> {
        let mut packets = Vec::new();
        protocol::send(
            &mut packets,
            &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
        )
        .unwrap();
        let file = entry(1, b"f", Kind::File, 5);
        let directory = Manifest::Directory {
            path: Vec::new(),
            entries: vec![file.clone()],
            more: false,
        };
        protocol::send(&mut packets, &directory).unwrap();
        protocol::send(&mut packets, &Manifest::End).unwrap();
        let mut stream = Vec::new();
        let header = Header::File {
            id: 1,
            size: 5,
            mtime: file.mtime,
        };
        record::write_header(&mut stream, &header).unwrap();
        stream.extend_from_slice(b"hello");
        let trailer = Trailer {
            hash: *blake3::hash(b"hello").as_bytes(),
            unread: unread.map(str::to_owned),
        };
        record::write_trailer(&mut stream, &trailer).unwrap();
        record::write_header(&mut stream, &Header::End).unwrap();
        protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
        packets
    }

    /// Runs the Destination on `packets`, and returns what it sent back.
    fn run_on(root: &Path, packets: &[u8]) -> Vec<(PacketType, Vec<u8>)> {
        run_reporting(root, packets, false)
    }

    /// As [`run_on`], with reports or without.
    fn run_reporting(root: &Path, packets: &[u8], report: bool) -> Vec<(PacketType, Vec<u8>)> {
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run: false,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: false,
            report,
            protect: Vec::new(),
        };
        let mut output = Vec::new();
        run(&mut &packets[..], &mut output, &options).unwrap();
        let mut answers = Vec::new();
        let mut reader = output.as_slice();
        while let Some(packet) = protocol::receive(&mut reader).unwrap() {
            let kind = packet.kind;
            answers.push((kind, packet.into_payload()));
        }
        answers
    }

    fn decoded<M: protocol::Message>(answers: &[(PacketType, Vec<u8>)]) -> Vec<M> {
        answers
            .iter()
            .filter(|(kind, _)| *kind == M::TYPE)
            .map(|(_, payload)| postcard::from_bytes(payload).unwrap())
            .collect()
    }

    #[test]
    fn a_file_is_written() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let answers = run_on(&root, &packets(None));
        assert_eq!(decoded::<Results>(&answers), []);
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert_eq!(std::fs::read(root.join("f")).unwrap(), b"hello");
    }

    /// Whatever files are reported as they are written, none is once the
    /// stream ends.
    #[test]
    fn the_files_being_written_are_reported_until_none_is() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let answers = run_reporting(&root, &packets(None), true);
        let reported: Vec<Vec<Transfer>> = decoded::<crate::report::Report>(&answers)
            .into_iter()
            .filter_map(|report| match report {
                crate::report::Report::Transferring(files) => Some(files),
                _ => None,
            })
            .collect();
        for files in &reported[..reported.len().saturating_sub(1)] {
            for file in files {
                assert_eq!((file.id, file.path.as_str(), file.size), (1, "f", 5));
                assert!(file.done <= 5);
            }
        }
        assert!(reported.last().is_none_or(Vec::is_empty), "{reported:?}");
    }

    /// A directory in two parts, synced: only what neither part has is
    /// deleted, found once its last part is here.
    #[test]
    fn a_directory_in_parts_deletes_only_what_no_part_has() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir(&root).unwrap();
        for name in ["first", "second", "extra"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let mut packets = Vec::new();
        let send =
            |packets: &mut Vec<u8>, message: &Manifest| protocol::send(packets, message).unwrap();
        send(
            &mut packets,
            &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
        );
        send(
            &mut packets,
            &Manifest::Directory {
                path: Vec::new(),
                entries: vec![entry(1, b"first", Kind::File, 5)],
                more: true,
            },
        );
        send(
            &mut packets,
            &Manifest::Directory {
                path: Vec::new(),
                entries: vec![entry(2, b"second", Kind::File, 6)],
                more: false,
            },
        );
        send(&mut packets, &Manifest::End);
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: true,
            report: false,
            protect: Vec::new(),
        };
        let mut output = Vec::new();
        run(&mut &packets[..], &mut output, &options).unwrap();
        let mut reader = output.as_slice();
        let mut deletes = Vec::new();
        while let Some(packet) = protocol::receive(&mut reader).unwrap() {
            if packet.kind == PacketType::Needs
                && let Needs::Deletes(more) = packet.decode::<Needs>().unwrap()
            {
                deletes.extend(more);
            }
        }
        assert_eq!(deletes, [(b"extra".to_vec(), false)]);
    }

    /// What the Source listed but could not read is not deleted, nor
    /// anything under it; what it does not have, beside it, is.
    #[test]
    fn sync_deletes_beside_what_the_source_could_not_read_but_not_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir_all(root.join("locked/deeper")).unwrap();
        std::fs::write(root.join("locked/deeper/file"), "kept").unwrap();
        std::fs::write(root.join("extra"), "extra").unwrap();
        let mut packets = Vec::new();
        protocol::send(
            &mut packets,
            &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
        )
        .unwrap();
        protocol::send(
            &mut packets,
            &Manifest::Problem {
                path: b"locked".to_vec(),
                file: None,
            },
        )
        .unwrap();
        protocol::send(
            &mut packets,
            &Manifest::Directory {
                path: Vec::new(),
                entries: vec![entry(1, b"locked", Kind::Unreadable, 0)],
                more: false,
            },
        )
        .unwrap();
        protocol::send(&mut packets, &Manifest::End).unwrap();
        let mut stream = Vec::new();
        record::write_header(&mut stream, &Header::End).unwrap();
        protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run: false,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: true,
            report: false,
            protect: Vec::new(),
        };
        let mut output = Vec::new();
        let summary = run(&mut &packets[..], &mut output, &options).unwrap();
        assert_eq!(summary.problems, 1);
        assert_eq!(
            std::fs::read(root.join("locked/deeper/file")).unwrap(),
            b"kept"
        );
        assert!(!root.join("extra").exists());
    }

    /// An extra that is what the Source could not read, under another name
    /// the filesystem takes for the same, is kept: a hard link stands for a
    /// filesystem that ignores case. Another extra beside it is deleted.
    #[test]
    fn an_extra_that_is_what_the_source_could_not_read_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("locked"), "kept").unwrap();
        std::fs::hard_link(root.join("locked"), root.join("alias")).unwrap();
        std::fs::write(root.join("extra"), "extra").unwrap();
        let mut packets = Vec::new();
        protocol::send(
            &mut packets,
            &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
        )
        .unwrap();
        protocol::send(
            &mut packets,
            &Manifest::Directory {
                path: Vec::new(),
                entries: vec![entry(1, b"locked", Kind::Unreadable, 0)],
                more: false,
            },
        )
        .unwrap();
        protocol::send(&mut packets, &Manifest::End).unwrap();
        let mut stream = Vec::new();
        record::write_header(&mut stream, &Header::End).unwrap();
        protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run: false,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: true,
            report: false,
            protect: Vec::new(),
        };
        run(&mut &packets[..], &mut Vec::new(), &options).unwrap();
        assert_eq!(std::fs::read(root.join("alias")).unwrap(), b"kept");
        assert_eq!(std::fs::read(root.join("locked")).unwrap(), b"kept");
        assert!(!root.join("extra").exists());
    }

    /// Runs the Destination, syncing, on a manifest of `messages` and an
    /// empty record stream, beside a file outside its root and with one
    /// inside; checks that the run is refused as invalid, and that neither
    /// file, nor anything else, changed.
    fn refused(messages: &[Manifest]) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir_all(root.join("inside")).unwrap();
        std::fs::write(root.join("inside/keep"), "keep").unwrap();
        std::fs::write(root.join("keep"), "keep").unwrap();
        std::fs::write(temp.path().join("outside"), "outside").unwrap();
        let before = tree(temp.path());
        let mut packets = Vec::new();
        for message in messages {
            protocol::send(&mut packets, message).unwrap();
        }
        let mut stream = Vec::new();
        record::write_header(&mut stream, &Header::End).unwrap();
        protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
        for dry_run in [true, false] {
            let options = Options {
                root: root.as_os_str().as_bytes().to_vec(),
                dry_run,
                modify_window: 0,
                numeric_ids: true,
                fsync: false,
                delete: true,
                report: false,
                protect: Vec::new(),
            };
            let error = run(&mut &packets[..], &mut Vec::new(), &options)
                .expect_err("an invalid manifest was taken");
            assert!(
                format!("{error:#}").contains("invalid manifest"),
                "{error:#}"
            );
            assert_eq!(tree(temp.path()), before, "{messages:?}");
        }
    }

    /// Every path under `root`, with what each file holds.
    fn tree(root: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
        let mut found = Vec::new();
        let mut waiting = vec![root.to_path_buf()];
        while let Some(directory) = waiting.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                    waiting.push(path.clone());
                    found.push((path, None));
                } else {
                    let content = std::fs::read(&path).ok();
                    found.push((path, content));
                }
            }
        }
        found.sort();
        found
    }

    fn root_directory() -> Manifest {
        Manifest::Root(entry(0, b"", Kind::Directory, 0))
    }

    fn part(path: &[u8], entries: Vec<Entry>, more: bool) -> Manifest {
        Manifest::Directory {
            path: path.to_vec(),
            entries,
            more,
        }
    }

    /// The names of the Destination's own tree, "inside" and "keep", sent
    /// as they are, so that only what is tested is wrong.
    fn the_rest(first: FileId) -> Vec<Entry> {
        vec![
            entry(first, b"inside", Kind::Directory, 0),
            entry(first + 1, b"keep", Kind::File, 4),
        ]
    }

    #[test]
    fn a_manifest_with_a_name_that_is_not_one_is_refused() {
        for name in [
            &b".."[..],
            b".",
            b"",
            b"../outside",
            b"inside/keep",
            b"a\0b",
        ] {
            let mut entries = vec![entry(
                1,
                name,
                Kind::Symlink {
                    target: b"x".to_vec(),
                },
                0,
            )];
            entries.extend(the_rest(2));
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            for (id, entry) in entries.iter_mut().enumerate() {
                entry.id = id as FileId + 1;
            }
            refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
        }
    }

    #[test]
    fn a_manifest_with_a_directory_not_announced_is_refused() {
        for path in [&b".."[..], b"nowhere", b"keep", b"inside/../.."] {
            refused(&[
                root_directory(),
                part(b"", the_rest(1), false),
                part(path, vec![entry(3, b"x", Kind::File, 0)], false),
                Manifest::End,
            ]);
        }
        // Before the root.
        refused(&[part(b"", the_rest(1), false), Manifest::End]);
    }

    #[test]
    fn a_manifest_with_a_directory_sent_twice_is_refused() {
        refused(&[
            root_directory(),
            part(b"", the_rest(1), false),
            part(b"inside", vec![], false),
            part(b"inside", vec![], false),
            Manifest::End,
        ]);
        refused(&[
            root_directory(),
            part(b"", the_rest(1), false),
            part(b"", vec![], false),
            Manifest::End,
        ]);
    }

    #[test]
    fn a_manifest_out_of_order_is_refused() {
        // Within a part.
        let mut entries = the_rest(1);
        entries.reverse();
        refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
        // A name twice.
        let entries = vec![
            entry(1, b"keep", Kind::File, 4),
            entry(2, b"keep", Kind::File, 4),
        ];
        refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
        // Between parts.
        refused(&[
            root_directory(),
            part(b"", vec![entry(1, b"keep", Kind::File, 4)], true),
            part(b"", vec![entry(2, b"inside", Kind::Directory, 0)], false),
            Manifest::End,
        ]);
    }

    #[test]
    fn a_manifest_with_another_root_or_ids_out_of_turn_is_refused() {
        refused(&[root_directory(), root_directory(), Manifest::End]);
        refused(&[
            Manifest::Root(entry(0, b"named", Kind::Directory, 0)),
            Manifest::End,
        ]);
        refused(&[
            Manifest::Root(entry(5, b"", Kind::Directory, 0)),
            Manifest::End,
        ]);
        let mut entries = the_rest(1);
        entries[1].id = 7;
        refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
    }

    /// An extra is deleted from the directory the comparison listed, however
    /// its path has changed since: here, moved away, and another made where
    /// it was, whose own file of that name is kept.
    #[test]
    fn an_extra_is_deleted_from_the_directory_listed_not_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/extra"), "listed").unwrap();
        let listed = fsys::open_path_from(&root, b"d").unwrap();
        std::fs::rename(root.join("d"), root.join("moved")).unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/extra"), "another").unwrap();

        let state = Mutex::new(State::new(true));
        let (mux, _outlet) = mux::channel();
        delete_extras(&listed, b"d", &[(b"d/extra".to_vec(), false)], &state, &mux).unwrap();
        assert!(!root.join("moved/extra").exists());
        assert_eq!(std::fs::read(root.join("d/extra")).unwrap(), b"another");
    }

    /// An extra that cannot be deleted is counted as failed, and the rest
    /// are deleted all the same.
    #[test]
    fn an_extra_that_cannot_be_deleted_fails_and_the_rest_are_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("d/gone/deeper")).unwrap();
        std::fs::write(root.join("d/extra"), "x").unwrap();
        let listed = fsys::open_path_from(&root, b"d").unwrap();

        let state = Mutex::new(State::new(true));
        let (mux, _outlet) = mux::channel();
        // Gone already, as another may have deleted it: it cannot be.
        let extras = [
            (b"d/already-gone".to_vec(), false),
            (b"d/extra".to_vec(), false),
            (b"d/gone".to_vec(), true),
        ];
        delete_extras(&listed, b"d", &extras, &state, &mux).unwrap();
        assert_eq!(state.lock().unwrap().failures, 1);
        assert_eq!(
            std::fs::read_dir(root.join("d")).unwrap().count(),
            0,
            "the rest are deleted"
        );
    }

    /// Directories are done as the manifest leaves them, and all their files
    /// are settled, deepest first; one never listed, once its parent is
    /// left; one to be let go, without being finished. Nothing stays open.
    #[test]
    fn directories_are_finished_as_they_are_done_deepest_first() {
        let mut state = State::new(true);
        let (finishing, done) = mpsc::channel();
        state.finishing = Some(finishing);
        let directory = |id: FileId, name: &[u8]| entry(id, name, Kind::Directory, 0);
        let finished = |done: &mpsc::Receiver<(Vec<u8>, Entry)>| -> Vec<String> {
            done.try_iter()
                .map(|(path, _)| String::from_utf8(path).unwrap())
                .collect()
        };
        // The root, with a/ and b/ in it; a/ with a file and c/; b/ never
        // listed.
        state.announce(Vec::new(), directory(0, b""));
        state.mark_to_finish(b"");
        state.enter(b"");
        state.announce(b"a".to_vec(), directory(1, b"a"));
        state.announce(b"b".to_vec(), directory(2, b"b"));
        state.mark_to_finish(b"a");
        state.mark_to_finish(b"b");
        state.enter(b"a");
        state.file_needed(b"a");
        state.announce(b"a/c".to_vec(), directory(3, b"c"));
        state.enter(b"a/c");
        // Left, nothing under it, nothing to finish: let go.
        state.enter(b"a/d-not-announced");
        assert_eq!(finished(&done), Vec::<String>::new());
        assert!(!state.open.contains_key(b"a/c".as_slice()));
        // The manifest is over; a/ waits for its file.
        state.leave_all();
        assert_eq!(finished(&done), ["b"]);
        assert!(state.open.contains_key(b"a".as_slice()));
        assert!(state.open.contains_key(b"".as_slice()));
        state.file_settled(b"a");
        assert_eq!(finished(&done), ["a", ""]);
        assert!(
            state.open.is_empty(),
            "{:?}",
            state.open.keys().collect::<Vec<_>>()
        );
    }

    /// A directory whose needed file the Source could not open is finished
    /// once it says so, not at the end; an ID it does not wait for, or
    /// said again, is nothing.
    #[test]
    fn a_directory_is_finished_once_its_file_the_source_could_not_open_is_settled() {
        let mut state = State::new(true);
        let (finishing, done) = mpsc::channel();
        state.finishing = Some(finishing);
        let directory = |id: FileId, name: &[u8]| entry(id, name, Kind::Directory, 0);
        state.announce(Vec::new(), directory(0, b""));
        state.enter(b"");
        state.announce(b"a".to_vec(), directory(1, b"a"));
        state.announce(b"b".to_vec(), directory(2, b"b"));
        state.mark_to_finish(b"a");
        state.enter(b"a");
        state.file_needed(b"a");
        state.targets.insert(
            3,
            Target {
                directory: b"a".to_vec(),
                entry: entry(3, b"locked", Kind::File, 1),
                is_root: false,
            },
        );
        // The manifest has left a/, which waits for its file.
        state.enter(b"b");
        let state = Mutex::new(state);
        settled(&state, 99);
        assert!(done.try_recv().is_err());
        settled(&state, 3);
        let finished: Vec<Vec<u8>> = done.try_iter().map(|(path, _)| path).collect();
        assert_eq!(finished, [b"a".to_vec()]);
        settled(&state, 3);
        assert!(done.try_recv().is_err());
    }

    /// The Source fails part way, as a scan that fails does: the
    /// Destination stops with its reason, rather than waiting for the rest.
    #[test]
    fn a_source_that_fails_stops_the_destination() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let mut packets = Vec::new();
        protocol::send(
            &mut packets,
            &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
        )
        .unwrap();
        protocol::send(
            &mut packets,
            &Control::Error {
                message: "the scan failed".to_owned(),
            },
        )
        .unwrap();
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: false,
            report: false,
            protect: Vec::new(),
        };
        let mut output = Vec::new();
        let error = run(&mut &packets[..], &mut output, &options).unwrap_err();
        assert!(
            format!("{error:#}").contains("the Source failed: the scan failed"),
            "{error:#}"
        );
    }

    /// A file the Source could not read whole fails, and is not asked for
    /// again: the next run finds it changed.
    #[test]
    fn a_file_the_source_could_not_read_whole_fails_and_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let answers = run_on(&root, &packets(Some("it shrank while it was read")));
        let results = decoded::<Results>(&answers);
        assert!(
            matches!(
                &results[..],
                [Results::Failed { id: 1, message }]
                    if message == "the Source could not read it whole: it shrank while it was read"
            ),
            "{results:?}"
        );
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}
