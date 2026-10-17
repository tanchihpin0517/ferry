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
use crate::protocol::{self, Control, Needs, PacketType, Results};
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
    /// Every directory in the manifest, by path; the root's is empty.
    directories: HashMap<Vec<u8>, Entry>,
    /// Directories whose attributes and mtime are set at the end.
    finish: HashSet<Vec<u8>>,
    warnings: Warnings,
    /// Whether to delete what the Source does not have.
    delete: bool,
    /// Entries that could not be made to match, or deleted.
    failures: u64,
    /// Paths the Source could not read.
    source_problems: u64,
    reporter: Reporter,
}

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
#[derive(Clone)]
struct Target {
    directory: Vec<u8>,
    entry: Entry,
}

/// Runs the Destination over a connection. A dry run reports each path it
/// lists, in manifest order. The Destination reports the counts and what it
/// cannot do; the Source reports the paths it cannot read.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
) -> Result<Summary> {
    let state = Mutex::new(State {
        ownership: Arc::new(Ownership::here()),
        targets: HashMap::new(),
        directories: HashMap::new(),
        finish: HashSet::new(),
        warnings: Warnings::default(),
        delete: options.delete,
        failures: 0,
        source_problems: 0,
        reporter: Reporter::none(),
    });
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
        let (data, stream) = mpsc::channel::<Vec<u8>>();
        let files = (!options.dry_run).then(|| {
            let mux = mux.clone();
            let reporter = reporter.clone();
            scope.spawn(move || -> Result<()> {
                let _thread = profile::thread("parser");
                let result = write_files(
                    root,
                    state,
                    &mux,
                    ChannelReader::new(stream, profile::State::Stream),
                    options.fsync,
                )
                .and_then(|()| finish_directories(root, state, options.fsync))
                .and_then(|()| make_durable(root, options.fsync));
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
                if data.send(bytes?).is_err() {
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
        let (mut comparisons, mut compared) = pool::ordered(
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
                 ownership,
                 prepared,
             }: Comparison| {
                let verdicts =
                    comparer.part(prepared.directory, &entries, &prepared.present, &ownership);
                Compared {
                    path,
                    entries,
                    verdicts,
                    extras: prepared.extras,
                }
            },
        );
        let (parts_in, parts_out) = mpsc::sync_channel::<Part>(PARTS);
        let root_bytes = options.root.clone();
        scope.spawn(move || {
            let _thread = profile::thread("compare");
            // What the Source does not have is found before anything here
            // changes: each directory is listed before its parts are applied.
            let mut tree = Tree::new(root_bytes, options.delete);
            while let Ok(part) = profile::wait(profile::State::Work, || parts_out.recv()) {
                let prepared = tree.part(&part.path, &part.entries, part.more);
                let comparison = Comparison {
                    path: part.path,
                    entries: part.entries,
                    ownership: part.ownership,
                    prepared,
                };
                if comparisons.submit(comparison).is_err() {
                    return;
                }
            }
        });
        let mut parts_in = Some(parts_in);
        // The directory whose parts are coming.
        let mut parts: Option<Vec<u8>> = None;
        // Dropping `frames` when this returns ends the record stream for the
        // writer.
        let mut assembler = frame::Assembler::default();
        let _thread = profile::thread("receive");
        loop {
            while let Some(done) = compared.try_next() {
                profile::wait(profile::State::Apply, || {
                    apply_compared(done, mux, root, state, options, reporter)
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
                    Manifest::Problem { path } => {
                        // The Source reports it; here it is only counted. What
                        // it could not read is in the manifest, as unreadable,
                        // so it is never taken for extra.
                        let _ = path;
                        state.lock().expect("not poisoned").source_problems += 1;
                    }
                    Manifest::End => {
                        if let Some(parted) = &parts {
                            bail!(
                                "the manifest ended before the rest of {}",
                                String::from_utf8_lossy(parted)
                            );
                        }
                        // Every comparison comes out before the needs end.
                        drop(parts_in.take());
                        for done in compared.by_ref() {
                            profile::wait(profile::State::Apply, || {
                                apply_compared(done, mux, root, state, options, reporter)
                            })?;
                        }
                        mux.message(&Needs::End)?;
                    }
                    Manifest::Root(entry) => {
                        refuse_another_kind(root, &entry)?;
                        let mut state = state.lock().expect("not poisoned");
                        let verdict = comparer
                            .root(&entry, &state.ownership)
                            .context("compare the destination's root")?;
                        if entry.kind == Kind::Directory {
                            state.directories.insert(Vec::new(), entry.clone());
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
                        reporter.add(Count::Entries, entries.len() as u64);
                        if let Some(parted) = &parts
                            && *parted != path
                        {
                            bail!(
                                "the Source sent {} before the rest of {}",
                                String::from_utf8_lossy(&path),
                                String::from_utf8_lossy(parted)
                            );
                        }
                        parts = more.then(|| path.clone());
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
    ownership: Arc<Ownership>,
    prepared: Prepared,
}

/// A directory of the manifest, compared by a worker.
struct Compared {
    path: Vec<u8>,
    entries: Vec<Entry>,
    verdicts: io::Result<Vec<Verdict>>,
    /// For `sync`: what the Destination has that the Source does not.
    extras: Option<io::Result<Extras>>,
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

/// Lists, makes match, and asks for what a compared directory needs.
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
        verdicts,
        extras,
    } = done;
    // What a dry run lists, sent once the state is let go: listing may wait.
    let mut listed = Vec::new();
    let (files, attributes, deletes): (Vec<FileId>, Vec<FileId>, Extras) = {
        {
            let mut state = state.lock().expect("not poisoned");
            let verdicts = verdicts.with_context(|| {
                format!("compare {}", String::from_utf8_lossy(&display(&path, true)))
            })?;
            // The Destination's directory, for a dry run to look into.
            let mut opened: Option<Option<OwnedFd>> = None;
            for (entry, verdict) in entries.iter().zip(&verdicts) {
                let child = manifest::join(&path, &entry.name);
                if verdict.is_listed() && options.dry_run {
                    // In `sync`, a directory something else replaces is
                    // deleted, with all it holds: said so, as a run would do.
                    if options.delete
                        && *verdict == Verdict::Content
                        && entry.kind != Kind::Directory
                        && let Some(directory) =
                            opened.get_or_insert_with(|| fsys::open_path_from(root, &path).ok())
                        && fsys::stat(&*directory, &entry.name)
                            .is_ok_and(|stat| fsys::file_type(&stat) == FileType::Directory)
                    {
                        let mut shown = b"deleting ".to_vec();
                        shown.extend(display(&child, true));
                        listed.push(shown);
                    }
                    listed.push(display(&child, entry.kind == Kind::Directory));
                }
                if entry.kind == Kind::Directory {
                    state.directories.insert(child, entry.clone());
                }
            }
            let mut deletes = Vec::new();
            if let Some(extras) = extras {
                match extras {
                    Ok(extras) => {
                        for (name, is_directory) in extras {
                            let extra = manifest::join(&path, &name);
                            let mut shown = b"deleting ".to_vec();
                            shown.extend(display(&extra, is_directory));
                            if options.dry_run {
                                listed.push(shown);
                            }
                            deletes.push((extra, is_directory));
                        }
                    }
                    Err(error) => {
                        state.failures += 1;
                        reporter.add(Count::Failed, 1);
                        let message = format!("cannot list what it holds: {error}");
                        tell(
                            reporter,
                            format_args!(
                                "cannot delete in {}: {message}",
                                String::from_utf8_lossy(&display(&path, true))
                            ),
                        );
                        let results = Results::NotDeleted {
                            path: path.clone(),
                            message,
                        };
                        mux.message(&results)?;
                    }
                }
            }
            let failures = if options.dry_run {
                Vec::new()
            } else {
                apply_directory(root, &path, &entries, &verdicts, &mut state)
            };
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
                deletes,
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
    if !options.dry_run {
        delete_extras(root, &path, &deletes, state, mux)?;
    }
    send_needs(mux, files, attributes, deletes)
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
            state.finish.insert(Vec::new());
            Ok(())
        }
        (_, Kind::Directory) => {
            state.finish.insert(Vec::new());
            Ok(())
        }
        (Verdict::Content, Kind::File) => {
            state.targets.insert(
                entry.id,
                Target {
                    directory: Vec::new(),
                    entry: entry.clone(),
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
    entries: &[Entry],
    verdicts: &[Verdict],
    state: &mut State,
) -> Vec<(FileId, String)> {
    if verdicts.iter().all(|&verdict| verdict == Verdict::Same) {
        return Vec::new();
    }
    let directory = match fsys::open_path_from(root, path) {
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
        state.finish.insert(path.to_vec());
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
                state.finish.insert(child);
            }
            continue;
        }
        if verdict == Verdict::Content && entry.kind == Kind::File {
            state.targets.insert(
                entry.id,
                Target {
                    directory: path.to_vec(),
                    entry: entry.clone(),
                },
            );
            continue;
        }
        if let Err(error) = apply_entry(&directory, &entry.name, &child, entry, verdict, state) {
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
                clear_the_way(&directory, name, state.delete)?;
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
                state.lock().expect("not poisoned").targets.remove(&done.id);
                match result {
                    Ok(()) => {
                        reporter.add(Count::Files, 1);
                        reporter.add(Count::Bytes, done.size);
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
                    state.lock().expect("not poisoned").targets.remove(&id);
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
    let delete = state.lock().expect("not poisoned").delete;
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
                scope.spawn(move || {
                    let _thread = profile::thread("rename");
                    chunk
                        .into_iter()
                        .map(|done| {
                            let result = rename_into_place(&done.ready, delete, fsync);
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
fn rename_into_place(ready: &Ready, delete: bool, fsync: bool) -> Result<()> {
    let renamed = (|| -> Result<()> {
        clear_the_way(&ready.directory, &ready.name, delete)?;
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
    let opened = target_directory(root, target).and_then(|(directory, name)| {
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
            is_root: target.entry.name.is_empty(),
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

/// Sets each directory's attributes and mtime, deepest first, now that
/// nothing more is written into it, and makes it durable unless told not to:
/// every directory that something was created in is among them.
fn finish_directories(root: &Path, state: &Mutex<State>, fsync: bool) -> Result<()> {
    let mut state = state.lock().expect("not poisoned");
    let mut paths: Vec<Vec<u8>> = state.finish.drain().collect();
    paths.sort_by_key(|path| std::cmp::Reverse(depth(path)));
    for path in paths {
        let Some(entry) = state.directories.get(&path).cloned() else {
            continue;
        };
        let shown = display(&path, true);
        let finished = (|| -> Result<()> {
            let directory = fsys::open_path(fsys::open_root(root.as_os_str())?, &path)?;
            let stat = rustix::fs::fstat(&directory)?;
            #[allow(clippy::unnecessary_cast)]
            let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
            set_owner(&state.ownership, uid, gid, &entry, |uid, gid| {
                fsys::set_owner(&directory, uid, gid)
            })?;
            fsys::set_mode(&directory, entry.mode & 0o7777)?;
            fsys::set_mtime(&directory, entry.mtime)?;
            if fsync {
                fsys::fsync(&directory)?;
            }
            Ok(())
        })();
        finished.with_context(|| format!("finish {}", String::from_utf8_lossy(&shown)))?;
    }
    Ok(())
}

/// Makes everything this transfer wrote durable, once, at the end: what was
/// only ordered on the way reaches storage.
fn make_durable(root: &Path, fsync: bool) -> Result<()> {
    if !fsync {
        return Ok(());
    }
    let directory = match fsys::open_root(root.as_os_str()) {
        Ok(directory) => directory,
        // A root that is a file: its directory.
        Err(_) => root_parent(root)?.0,
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

/// The directory a target is written in, and its name there.
fn target_directory(root: &Path, target: &Target) -> Result<(OwnedFd, Vec<u8>)> {
    if target.entry.name.is_empty() {
        return root_parent(root);
    }
    let directory = fsys::open_path(fsys::open_root(root.as_os_str())?, &target.directory)?;
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

/// Removes a directory where something else is about to go. Without
/// deleting, only an empty one is removed: a directory with anything in it
/// stays, and nothing is written. `sync` removes it with all it holds.
fn clear_the_way(directory: impl AsFd, name: &[u8], delete: bool) -> Result<()> {
    match fsys::stat(&directory, name) {
        Ok(stat) if fsys::file_type(&stat) == FileType::Directory => {
            if delete {
                fsys::remove_tree(&directory, name)?;
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
    root: &Path,
    directory: &[u8],
    extras: &[(Vec<u8>, bool)],
    state: &Mutex<State>,
    mux: &Mux,
) -> Result<()> {
    if extras.is_empty() {
        return Ok(());
    }
    let reporter = state.lock().expect("not poisoned").reporter.clone();
    let mut deleted = false;
    for (path, is_directory) in extras {
        // Each is a name joined to the directory's path.
        let name = &path[if directory.is_empty() {
            0
        } else {
            directory.len() + 1
        }..];
        let result = fsys::open_path_from(root, directory)
            .and_then(|opened| fsys::remove_tree(opened, name));
        match result {
            Ok(()) => deleted = true,
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
            .finish
            .insert(directory.to_vec());
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
