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
use crate::protocol::{self, Control, Needs, PacketType};
use crate::record::{self, ChannelReader, Header};
use crate::source::display;

pub struct Options {
    /// The Destination's root, as path bytes.
    pub root: Vec<u8>,
    pub dry_run: bool,
    pub modify_window: i64,
    pub numeric_ids: bool,
    pub fsync: bool,
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
}

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
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
) -> Result<()> {
    let writer = Mutex::new(writer);
    let state = Mutex::new(State {
        ownership: Ownership::here(),
        targets: HashMap::new(),
        directories: HashMap::new(),
        finish: HashSet::new(),
        warnings: Warnings::default(),
    });
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));

    std::thread::scope(|scope| -> Result<()> {
        let (data, stream) = mpsc::channel::<Vec<u8>>();
        let files = (!options.dry_run).then(|| {
            scope.spawn(|| -> Result<()> {
                let result = write_files(root, &state, ChannelReader::new(stream), options.fsync)
                    .and_then(|()| finish_directories(root, &state, options.fsync));
                let mut writer = writer.lock().expect("not poisoned");
                for message in state.lock().expect("not poisoned").warnings.messages() {
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
                let (files, attributes) = match message {
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
                        split(&[entry], &[verdict])
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
                        if !options.dry_run {
                            apply_directory(root, &path, &entries, &verdicts, &mut state)?;
                        }
                        split(&entries, &verdicts)
                    }
                };
                if !files.is_empty() {
                    send(writer, &Needs::Files(files))?;
                }
                if !attributes.is_empty() {
                    send(writer, &Needs::Attributes(attributes))?;
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
/// thread can: everything but files' content.
fn apply_directory(
    root: &Path,
    path: &[u8],
    entries: &[Entry],
    verdicts: &[Verdict],
    state: &mut State,
) -> Result<()> {
    if verdicts.iter().all(|&verdict| verdict == Verdict::Same) {
        return Ok(());
    }
    let directory = fsys::open_path(fsys::open_root(root.as_os_str())?, path)
        .with_context(|| format!("open {}", String::from_utf8_lossy(&display(path, true))))?;
    // Creating or replacing anything in a directory changes its mtime.
    if verdicts.contains(&Verdict::Content) {
        state.finish.insert(path.to_vec());
    }
    for (entry, &verdict) in entries.iter().zip(verdicts) {
        let child = manifest::join(path, &entry.name);
        if entry.kind == Kind::Directory {
            if verdict == Verdict::Content {
                make_directory(&directory, &entry.name, entry.mode).with_context(|| {
                    format!("create {}", String::from_utf8_lossy(&display(&child, true)))
                })?;
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
        apply_entry(&directory, &entry.name, &child, entry, verdict, state).with_context(|| {
            format!(
                "update {}",
                String::from_utf8_lossy(&display(&child, false))
            )
        })?;
    }
    Ok(())
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
                clear_the_way(&directory, name)?;
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

/// Writes every file in the record stream, until it ends.
fn write_files(
    root: &Path,
    state: &Mutex<State>,
    mut stream: ChannelReader,
    fsync: bool,
) -> Result<()> {
    loop {
        match record::read_header(&mut stream)? {
            Header::File { id, size } => {
                let target = state
                    .lock()
                    .expect("not poisoned")
                    .targets
                    .remove(&id)
                    .with_context(|| format!("the Source sent unneeded ID {id}"))?;
                let shown = display(
                    &manifest::join(&target.directory, &target.entry.name),
                    false,
                );
                write_file(root, &target, &shown, size, &mut stream, state, fsync)
                    .with_context(|| format!("write {}", String::from_utf8_lossy(&shown)))?;
            }
            Header::End => return Ok(()),
        }
    }
}

/// Writes one file: into a temporary file beside it, checked against the
/// Source's hash, given its attributes, made durable unless told not to, then
/// renamed into place. Its directory is made durable at the end.
fn write_file(
    root: &Path,
    target: &Target,
    shown: &[u8],
    size: u64,
    stream: &mut impl Read,
    state: &Mutex<State>,
    fsync: bool,
) -> Result<()> {
    let (directory, name) = target_directory(root, target)?;
    let (mut file, temporary) = fsys::create_temporary(&directory, &name)?;
    let registered = interrupt::register(&directory, &temporary)?;
    let written = (|| -> Result<()> {
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0; 256 * 1024];
        let mut remaining = size;
        while remaining > 0 {
            let count = buffer.len().min(remaining as usize);
            stream
                .read_exact(&mut buffer[..count])
                .context("read the record stream")?;
            hasher.update(&buffer[..count]);
            file.write_all(&buffer[..count])?;
            remaining -= count as u64;
        }
        let trailer = record::read_trailer(stream)?;
        if trailer.changed {
            bail!("the file changed on the Source while it was read");
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
        clear_the_way(&directory, &name)?;
        fsys::rename(&directory, &temporary, &name)?;
        // A root that is a file has no directory of its own that is finished.
        if fsync && target.entry.name.is_empty() {
            rustix::fs::fsync(&directory)?;
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = fsys::remove(&directory, &temporary, false);
    }
    drop(registered);
    drop(file);
    written
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

/// Removes an empty directory where something else is about to go. A
/// directory with anything in it stays, and nothing is written.
fn clear_the_way(directory: impl AsFd, name: &[u8]) -> Result<()> {
    match fsys::stat(&directory, name) {
        Ok(stat) if fsys::file_type(&stat) == FileType::Directory => {
            fsys::remove(&directory, name, true)
                .context("a directory that is not empty is where this belongs")?;
        }
        _ => {}
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
