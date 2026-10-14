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
use std::sync::Mutex;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use rustix::fs::FileType;
use rustix::io::Errno;

use crate::compare::{Comparer, Verdict};
use crate::fsys;
use crate::interrupt;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::owners::Ownership;
use crate::protocol::{self, Control, Needs, PacketType, Results};
use crate::record::{self, ChannelReader, Header};
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
    /// Print what goes wrong: the Destination reports when it is the client.
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

/// Prints `ferry: <message>` when this side reports.
fn tell(report: bool, message: std::fmt::Arguments) {
    if report {
        eprintln!("ferry: {message}");
    }
}

/// What the main thread and the writer thread share.
struct State {
    ownership: Ownership,
    /// Files whose content is coming, by ID.
    targets: HashMap<FileId, Target>,
    /// Every directory in the manifest, by path; the root's is empty.
    directories: HashMap<Vec<u8>, Entry>,
    /// Directories whose attributes and mtime are set at the end.
    finish: HashSet<Vec<u8>>,
    warnings: Warnings,
    /// Whether to delete what the Source does not have.
    delete: bool,
    /// Entries to delete once the transfer has succeeded: paths, and whether
    /// each is a directory.
    deletions: Vec<(Vec<u8>, bool)>,
    /// Entries that could not be made to match. Any keeps `sync` from
    /// deleting.
    failures: u64,
    /// Paths the Source could not read. Any keeps `sync` from deleting, since
    /// what is missing from the manifest is not known to be gone.
    source_problems: u64,
    report: bool,
}

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
#[derive(Clone)]
struct Target {
    directory: Vec<u8>,
    entry: Entry,
}

/// Runs the Destination over a connection. `on_need` is told each path a dry
/// run lists, in manifest order.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<Summary> {
    let writer = Mutex::new(writer);
    let state = Mutex::new(State {
        ownership: Ownership::here(),
        targets: HashMap::new(),
        directories: HashMap::new(),
        finish: HashSet::new(),
        warnings: Warnings::default(),
        delete: options.delete,
        deletions: Vec::new(),
        failures: 0,
        source_problems: 0,
        report: options.report,
    });
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));

    std::thread::scope(|scope| -> Result<()> {
        let (data, stream) = mpsc::channel::<Vec<u8>>();
        let files = (!options.dry_run).then(|| {
            scope.spawn(|| -> Result<()> {
                let result = write_files(
                    root,
                    &state,
                    &writer,
                    ChannelReader::new(stream),
                    options.fsync,
                )
                .and_then(|()| delete_extras(root, &state, &writer))
                .and_then(|()| finish_directories(root, &state, options.fsync));
                let mut writer = writer.lock().expect("not poisoned");
                for message in state.lock().expect("not poisoned").warnings.messages() {
                    tell(options.report, format_args!("warning: {message}"));
                    protocol::send(&mut *writer, &Control::Warning { message })?;
                }
                match &result {
                    Ok(()) => protocol::send(&mut *writer, &Control::Done)?,
                    Err(error) => protocol::send(
                        &mut *writer,
                        &Control::Error {
                            message: format!("{error:#}"),
                        },
                    )?,
                }
                writer.flush()?;
                result
            })
        });

        let read = read_packets(
            reader, &writer, &comparer, root, &state, options, data, on_need,
        );
        let written = files.map(|files| files.join().expect("the file writer panicked"));
        read?;
        written.transpose()?;
        Ok(())
    })?;
    let state = state.into_inner().expect("not poisoned");
    Ok(Summary {
        failed: state.failures,
        problems: state.source_problems,
    })
}

#[allow(clippy::too_many_arguments)]
fn read_packets<W: Write>(
    reader: &mut impl Read,
    writer: &Mutex<&mut W>,
    comparer: &Comparer,
    root: &Path,
    state: &Mutex<State>,
    options: &Options,
    data: mpsc::Sender<Vec<u8>>,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    // Dropping `data` when this returns ends the record stream for the writer.
    while let Some(packet) = protocol::receive(reader)? {
        match packet.kind {
            PacketType::Manifest => {
                let message = packet.decode::<Manifest>()?;
                let (files, attributes, deletes) = match message {
                    Manifest::Names { users, groups } => {
                        if !options.numeric_ids {
                            state
                                .lock()
                                .expect("not poisoned")
                                .ownership
                                .learn(&users, &groups);
                        }
                        continue;
                    }
                    Manifest::Problem { path } => {
                        state.lock().expect("not poisoned").source_problems += 1;
                        tell(
                            options.report,
                            format_args!(
                                "cannot read {} on the source",
                                String::from_utf8_lossy(&display(&path, false))
                            ),
                        );
                        continue;
                    }
                    Manifest::End => {
                        send(writer, &Needs::End)?;
                        continue;
                    }
                    Manifest::Root(entry) => {
                        let mut state = state.lock().expect("not poisoned");
                        let verdict = comparer
                            .root(&entry, &state.ownership)
                            .context("compare the destination's root")?;
                        if entry.kind == Kind::Directory {
                            state.directories.insert(Vec::new(), entry.clone());
                        }
                        if verdict.is_listed() {
                            on_need(b".")?;
                        }
                        if !options.dry_run {
                            apply_root(root, &entry, verdict, &mut state)?;
                        }
                        let (files, attributes) = split(&[entry], &[verdict]);
                        (files, attributes, Vec::new())
                    }
                    Manifest::Directory { path, entries } => {
                        let mut state = state.lock().expect("not poisoned");
                        let verdicts = comparer
                            .directory(&path, &entries, &state.ownership)
                            .with_context(|| {
                                format!(
                                    "compare {}",
                                    String::from_utf8_lossy(&display(&path, true))
                                )
                            })?;
                        for (entry, verdict) in entries.iter().zip(&verdicts) {
                            let child = manifest::join(&path, &entry.name);
                            if verdict.is_listed() {
                                on_need(&display(&child, entry.kind == Kind::Directory))?;
                            }
                            if entry.kind == Kind::Directory {
                                state.directories.insert(child, entry.clone());
                            }
                        }
                        // What the Source does not have, found before anything
                        // here changes.
                        let mut deletes = Vec::new();
                        if options.delete {
                            match comparer.extras(&path, &entries) {
                                Ok(extras) => {
                                    for (name, is_directory) in extras {
                                        let extra = manifest::join(&path, &name);
                                        let mut shown = b"deleting ".to_vec();
                                        shown.extend(display(&extra, is_directory));
                                        on_need(&shown)?;
                                        state.deletions.push((extra.clone(), is_directory));
                                        deletes.push((extra, is_directory));
                                    }
                                }
                                Err(error) => {
                                    state.failures += 1;
                                    let message = format!("cannot list what it holds: {error}");
                                    tell(
                                        options.report,
                                        format_args!(
                                            "cannot delete in {}: {message}",
                                            String::from_utf8_lossy(&display(&path, true))
                                        ),
                                    );
                                    let results = Results::NotDeleted {
                                        path: path.clone(),
                                        message,
                                    };
                                    protocol::send(
                                        &mut *writer.lock().expect("not poisoned"),
                                        &results,
                                    )?;
                                }
                            }
                        }
                        let failures = if options.dry_run {
                            Vec::new()
                        } else {
                            apply_directory(root, &path, &entries, &verdicts, &mut state)
                        };
                        state.failures += failures.len() as u64;
                        drop(state);
                        for (id, message) in &failures {
                            if let Some(entry) = entries.iter().find(|entry| entry.id == *id) {
                                let child = manifest::join(&path, &entry.name);
                                tell(
                                    options.report,
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
                            let mut writer = writer.lock().expect("not poisoned");
                            protocol::send(&mut *writer, &results)?;
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
                if !files.is_empty() {
                    send(writer, &Needs::Files(files))?;
                }
                if !attributes.is_empty() {
                    send(writer, &Needs::Attributes(attributes))?;
                }
                if !deletes.is_empty() {
                    send(writer, &Needs::Deletes(deletes))?;
                }
            }
            PacketType::Data => {
                if data.send(packet.into_payload()).is_err() {
                    // The writer stopped, and reports why itself.
                    bail!("the file writer stopped");
                }
            }
            other => bail!("unexpected {other:?} packet from the Source"),
        }
    }
    Ok(())
}

fn send<W: Write>(writer: &Mutex<&mut W>, needs: &Needs) -> Result<()> {
    let mut writer = writer.lock().expect("not poisoned");
    protocol::send(&mut *writer, needs)?;
    writer.flush()?;
    Ok(())
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
            set_owner(state, shown, uid, gid, entry, |uid, gid| {
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
            let temporary = loop {
                let candidate = fsys::temporary_name(name);
                let made = match &entry.kind {
                    Kind::Symlink { target } => fsys::make_symlink(&directory, &candidate, target),
                    kind => fsys::make_special(&directory, &candidate, kind, entry.mode),
                };
                match made {
                    Ok(()) => break candidate,
                    Err(error) if Errno::from_io_error(&error) == Some(Errno::EXIST) => continue,
                    Err(error) if Errno::from_io_error(&error) == Some(Errno::PERM) => {
                        state.warnings.add(Warning::Special, shown);
                        return Ok(());
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let registered = interrupt::register(&directory, &temporary)?;
            let finished = (|| -> Result<()> {
                let stat = fsys::stat(&directory, &temporary)?;
                #[allow(clippy::unnecessary_cast)]
                let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
                set_owner(state, shown, uid, gid, entry, |uid, gid| {
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

/// Gives an entry the Source's owner, mapped here, as far as this process may,
/// and records a warning for what it may not. `chown` takes the user and group
/// to set, `None` leaving one alone.
fn set_owner(
    state: &mut State,
    shown: &[u8],
    current_uid: u32,
    current_gid: u32,
    entry: &Entry,
    chown: impl FnOnce(Option<u32>, Option<u32>) -> io::Result<()>,
) -> Result<()> {
    let uid = state.ownership.uid(entry.uid);
    let gid = state.ownership.gid(entry.gid);
    let mut set_uid = None;
    let mut set_gid = None;
    if uid != current_uid {
        if state.ownership.can_set_user(uid, current_uid) {
            set_uid = Some(uid);
        } else {
            state.warnings.add(Warning::Owner, shown);
        }
    }
    if gid != current_gid {
        if state.ownership.can_set_group(gid) {
            set_gid = Some(gid);
        } else {
            state.warnings.add(Warning::Group, shown);
        }
    }
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
    Done,
    /// The file changed on the Source while it was read.
    Changed,
    Failed(anyhow::Error),
}

/// Writes every file in the record stream, until it ends, reporting each file
/// that fails and, when the first pass ends, those to send again. A file that
/// changes again while it is sent again fails.
fn write_files<W: Write>(
    root: &Path,
    state: &Mutex<State>,
    writer: &Mutex<&mut W>,
    mut stream: ChannelReader,
    fsync: bool,
) -> Result<()> {
    let mut resend = Vec::new();
    let mut second_pass = false;
    let report_here = state.lock().expect("not poisoned").report;
    loop {
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
                let failure =
                    match write_file(root, &target, &shown, size, &mut stream, state, fsync)? {
                        Written::Done => None,
                        Written::Changed if !second_pass => {
                            resend.push(id);
                            continue;
                        }
                        Written::Changed => Some("it kept changing while it was read".to_owned()),
                        Written::Failed(error) => Some(format!("{error:#}")),
                    };
                let mut locked = state.lock().expect("not poisoned");
                locked.targets.remove(&id);
                if failure.is_some() {
                    locked.failures += 1;
                }
                drop(locked);
                if let Some(message) = failure {
                    tell(
                        report_here,
                        format_args!("cannot copy {}: {message}", String::from_utf8_lossy(&shown)),
                    );
                    report(writer, &Results::Failed { id, message })?;
                }
            }
            Header::PassEnd => {
                report(writer, &Results::Resend(std::mem::take(&mut resend)))?;
                second_pass = true;
            }
            Header::End => return Ok(()),
        }
    }
}

fn report<W: Write>(writer: &Mutex<&mut W>, results: &Results) -> Result<()> {
    let mut writer = writer.lock().expect("not poisoned");
    protocol::send(&mut *writer, results)?;
    writer.flush()?;
    Ok(())
}

/// Writes one file: into a temporary file beside it, checked against the
/// Source's hash, given its attributes, made durable unless told not to, then
/// renamed into place. Its directory is made durable at the end.
///
/// The record is read to its end whatever happens to the file, so that the
/// stream stays in step; only an error reading the stream itself is returned
/// as an error.
fn write_file(
    root: &Path,
    target: &Target,
    shown: &[u8],
    size: u64,
    stream: &mut impl Read,
    state: &Mutex<State>,
    fsync: bool,
) -> Result<Written> {
    let opened = target_directory(root, target).and_then(|(directory, name)| {
        let (file, temporary) = fsys::create_temporary(&directory, &name)?;
        let registered = interrupt::register(&directory, &temporary)?;
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
        if trailer.changed {
            return Ok(Written::Changed);
        }
        if *hasher.finalize().as_bytes() != trailer.hash {
            bail!("the content's hash does not match the Source's");
        }
        apply_file_attributes(&file, &target.entry, shown, state)?;
        if fsync {
            // The content is on disk before its name is, so that a power loss
            // never leaves the name with missing content.
            file.sync_all()?;
        }
        let delete = state.lock().expect("not poisoned").delete;
        clear_the_way(&directory, &name, delete)?;
        fsys::rename(&directory, &temporary, &name)?;
        // A root that is a file has no directory of its own that is finished.
        if fsync && target.entry.name.is_empty() {
            rustix::fs::fsync(&directory)?;
        }
        Ok(Written::Done)
    })();
    let written = finished.unwrap_or_else(Written::Failed);
    if !matches!(written, Written::Done) {
        let _ = fsys::remove(&directory, &temporary, false);
    }
    drop(registered);
    Ok(written)
}

/// Owner first, since setting it clears setuid and setgid, then permissions,
/// then the mtime, last since nothing may write to the file after it.
fn apply_file_attributes(
    file: &File,
    entry: &Entry,
    shown: &[u8],
    state: &Mutex<State>,
) -> Result<()> {
    let metadata = file.metadata()?;
    let mut state = state.lock().expect("not poisoned");
    set_owner(
        &mut state,
        shown,
        metadata.uid(),
        metadata.gid(),
        entry,
        |uid, gid| fsys::set_owner(file, uid, gid),
    )?;
    drop(state);
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
            set_owner(&mut state, &shown, uid, gid, &entry, |uid, gid| {
                fsys::set_owner(&directory, uid, gid)
            })?;
            fsys::set_mode(&directory, entry.mode & 0o7777)?;
            fsys::set_mtime(&directory, entry.mtime)?;
            if fsync {
                rustix::fs::fsync(&directory)?;
            }
            Ok(())
        })();
        finished.with_context(|| format!("finish {}", String::from_utf8_lossy(&shown)))?;
    }
    Ok(())
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

/// Deletes what the Source does not have, for `sync`, now that everything
/// else is written; or nothing, if anything failed or the Source could not
/// read everything, since then what looks extra may not be.
fn delete_extras<W: Write>(
    root: &Path,
    state: &Mutex<State>,
    writer: &Mutex<&mut W>,
) -> Result<()> {
    let mut state = state.lock().expect("not poisoned");
    if state.deletions.is_empty() {
        return Ok(());
    }
    let deletions = std::mem::take(&mut state.deletions);
    if state.failures > 0 || state.source_problems > 0 {
        let reason = if state.source_problems > 0 {
            "some of the source could not be read"
        } else {
            "some entries could not be copied"
        };
        let message = format!("deleted nothing, since {reason}");
        tell(state.report, format_args!("warning: {message}"));
        protocol::send(
            &mut *writer.lock().expect("not poisoned"),
            &Control::Warning { message },
        )?;
        return Ok(());
    }
    for (path, _) in deletions {
        let (parent, name) = match path.iter().rposition(|&byte| byte == b'/') {
            Some(slash) => (path[..slash].to_vec(), path[slash + 1..].to_vec()),
            None => (Vec::new(), path.clone()),
        };
        let deleted = fsys::open_path_from(root, &parent)
            .and_then(|directory| fsys::remove_tree(directory, &name));
        match deleted {
            // Deleting changes the directory's mtime, which is set again.
            Ok(()) => {
                state.finish.insert(parent);
            }
            Err(error) => {
                state.failures += 1;
                tell(
                    state.report,
                    format_args!(
                        "cannot delete {}: {error}",
                        String::from_utf8_lossy(&display(&path, false))
                    ),
                );
                let results = Results::NotDeleted {
                    path,
                    message: error.to_string(),
                };
                protocol::send(&mut *writer.lock().expect("not poisoned"), &results)?;
            }
        }
    }
    Ok(())
}

/// What the Destination could not do, by kind, with a few of the paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Warning {
    Owner,
    Group,
    Special,
}

impl Warning {
    fn describe(self) -> &'static str {
        match self {
            Warning::Owner => "kept this user as owner: giving them another needs root",
            Warning::Group => {
                "kept a group of this user's: giving them another needs root or membership in it"
            }
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
                    "{count} entr{} {} ({}{more})",
                    if *count == 1 { "y" } else { "ies" },
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

    /// What a Source would send for one file, `hello`, sent twice: changed
    /// in the first pass, and changed or not in the second.
    fn packets(changed_again: bool) -> Vec<u8> {
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
        };
        protocol::send(&mut packets, &directory).unwrap();
        protocol::send(&mut packets, &Manifest::End).unwrap();
        let mut stream = Vec::new();
        for changed in [true, changed_again] {
            let header = Header::File {
                id: 1,
                size: 5,
                mtime: file.mtime,
            };
            record::write_header(&mut stream, &header).unwrap();
            stream.extend_from_slice(b"hello");
            let trailer = Trailer {
                hash: *blake3::hash(b"hello").as_bytes(),
                changed,
            };
            record::write_trailer(&mut stream, &trailer).unwrap();
            if changed {
                record::write_header(&mut stream, &Header::PassEnd).unwrap();
            }
        }
        if changed_again {
            // The Source sends again once only.
            record::write_header(&mut stream, &Header::PassEnd).unwrap();
        }
        record::write_header(&mut stream, &Header::End).unwrap();
        protocol::send_data(&mut packets, &stream).unwrap();
        packets
    }

    /// Runs the Destination on `packets`, and returns what it sent back.
    fn run_on(root: &Path, packets: &[u8]) -> Vec<(PacketType, Vec<u8>)> {
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run: false,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: false,
            report: false,
        };
        let mut output = Vec::new();
        run(&mut &packets[..], &mut output, &options, &mut |_| Ok(())).unwrap();
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
    fn a_file_that_changed_is_sent_again_and_written() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let answers = run_on(&root, &packets(false));
        assert_eq!(decoded::<Results>(&answers), [Results::Resend(vec![1])]);
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert_eq!(std::fs::read(root.join("f")).unwrap(), b"hello");
    }

    #[test]
    fn a_file_that_keeps_changing_fails_and_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let answers = run_on(&root, &packets(true));
        let results = decoded::<Results>(&answers);
        assert_eq!(results[0], Results::Resend(vec![1]));
        assert!(
            matches!(&results[1], Results::Failed { id: 1, message } if message.contains("kept changing")),
            "{results:?}"
        );
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}
