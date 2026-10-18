//! Applying what the comparison found: what a directory has extra is dealt
//! with first, then its entries are made to match, and the files that need
//! content are asked for. Also each directory's progress, from announced to
//! done, after which it is finished.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use rustix::fs::FileType;
use rustix::io::Errno;

use crate::compare::{Extras, Verdict};
use crate::fsys;
use crate::interrupt;
use crate::manifest::{self, Entry, FileId, Kind};
use crate::mux::Mux;
use crate::owners::Ownership;
use crate::protocol::{FileKey, Needs, Results};
use crate::report::{Count, Reporter};
use crate::source::display;
use crate::stop::Stop;

use super::receive::Compared;
use super::warnings::Warning;
use super::write::Target;
use super::{Options, State, is_protected, open_under, root_parent, tell};

/// How many manifest entries apart the Source is told it need keep them no
/// more.
const THROUGH_EVERY: FileId = 4096;

/// A directory announced, by its parent's entries or as the root, and not
/// finished yet. Its attributes and mtime are set last, once nothing more is
/// made in it, nor under it: when the manifest has left it, and all under
/// it, and every file in it is written, and every directory in it done.
/// That is as soon as it can be: its mtime changes only with what is made or
/// removed in it, and it can be one that cannot be gone into after.
pub(super) struct Open {
    pub(super) entry: Entry,
    /// It, or something in it, changed: done, it is finished, not just let go.
    pub(super) finish: bool,
    /// The manifest has left it, and all under it.
    pub(super) left: bool,
    /// The files in it needed and not settled yet.
    pub(super) files: usize,
    /// The directories in it not done yet.
    pub(super) children: usize,
    /// Of those, the names whose parts have not come: once it is left, none
    /// will, as for one the Source could not list.
    pub(super) unseen: HashSet<Vec<u8>>,
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
    pub(super) fn announce(&mut self, path: Vec<u8>, entry: Entry) {
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
    pub(super) fn mark_to_finish(&mut self, path: &[u8]) {
        if let Some(open) = self.open.get_mut(path) {
            open.finish = true;
        }
    }

    /// A file in `directory` is needed.
    pub(super) fn file_needed(&mut self, directory: &[u8]) {
        if let Some(open) = self.open.get_mut(directory) {
            open.files += 1;
        }
    }

    /// A file in `directory` is settled: written, or failed.
    pub(super) fn file_settled(&mut self, directory: &[u8]) {
        if let Some(open) = self.open.get_mut(directory) {
            open.files = open.files.saturating_sub(1);
        }
        self.done_if_so(directory.to_vec());
    }

    /// The parts of the directory at `path` are applied, as the manifest
    /// goes into it: what it has left behind, it has left.
    pub(super) fn enter(&mut self, path: &[u8]) {
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
    pub(super) fn leave_all(&mut self) {
        while let Some(top) = self.stack.pop() {
            self.leave(top);
        }
    }

    /// The manifest has left the directory at `path`, and all under it.
    pub(super) fn leave(&mut self, path: Vec<u8>) {
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
    pub(super) fn done_if_so(&mut self, mut path: Vec<u8>) {
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

pub(super) fn send_needs(
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
pub(super) fn take_compared(
    done: Compared,
    waiting: &mut Vec<Compared>,
    through: &mut FileId,
    mux: &Mux,
    root: &Path,
    state: &Mutex<State>,
    options: &Options,
    reporter: &Reporter,
    stop: &Stop,
) -> Result<()> {
    stop.check()?;
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
            stop.check()?;
            apply_extras(
                &path,
                directory.as_deref(),
                extras,
                waiting,
                mux,
                state,
                options,
                reporter,
                stop,
            )?
        }
        None => Vec::new(),
    };
    if !options.dry_run
        && let Some(done) = waiting.first()
    {
        let mut state = state.lock().expect("not poisoned");
        state.enter(&done.path);
        if let Some(mode) = waiting.iter().find_map(|done| done.let_in) {
            state.mark_to_finish(&done.path);
            state.let_in.insert(done.path.clone(), mode);
        }
    }
    let last = waiting
        .iter()
        .rev()
        .find_map(|done| done.entries.last())
        .map(|entry| entry.id);
    for done in waiting.drain(..) {
        stop.check()?;
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
    stop: &Stop,
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
        delete_extras(directory, path, &deletes, state, mux, stop)?;
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
pub(super) fn split(entries: &[Entry], verdicts: &[Verdict]) -> (Vec<FileId>, Vec<FileId>) {
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
pub(super) fn refuse_another_kind(root: &Path, entry: &Entry) -> Result<()> {
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

pub(super) fn apply_root(
    root: &Path,
    entry: &Entry,
    verdict: Verdict,
    state: &mut State,
) -> Result<()> {
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
            state.need(Target {
                directory: Arc::from(&b""[..]),
                entry: entry.clone(),
                is_root: true,
            });
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
    // The directory's path, as its files' targets share it.
    let mut shared: Option<Arc<[u8]>> = None;
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
            let directory = shared.get_or_insert_with(|| Arc::from(path));
            state.need(Target {
                directory: Arc::clone(directory),
                entry: entry.clone(),
                is_root: false,
            });
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
            // What was compared may have been swapped for a symlink since:
            // it fails here, before anything is set. Setting the mode never
            // goes through one either, whatever comes there after this.
            if !entry.kind.is_symlink() && fsys::file_type(&stat) == FileType::Symlink {
                bail!("it became a symlink after it was compared");
            }
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
            // Registered with a directory of its own: one such file at a time.
            let registered_in = Arc::new(directory.as_fd().try_clone_to_owned()?);
            let made = interrupt::create(&registered_in, || {
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
            // Renamed into place, it is kept; otherwise dropping its
            // registration removes it.
            let mut registered = registered;
            if finished.is_ok() {
                registered.disarm();
            }
            finished
        }
    }
}

/// Gives an entry the Source's owner, mapped here, as far as this process may.
/// What it may not set, it leaves, without a word, as rsync does: a user who
/// is not root cannot keep another's files theirs, and keeping their own is
/// what they expect. `chown` takes the user and group to set, `None` leaving
/// one alone.
pub(super) fn set_owner(
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

/// Removes a directory where something else is about to go. Without
/// deleting, only an empty one is removed: a directory with anything in it
/// stays, and nothing is written. `sync` removes it with all it holds, but
/// for what is among `protect`.
pub(super) fn clear_the_way(
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
pub(super) fn delete_extras(
    held: &OwnedFd,
    directory: &[u8],
    extras: &[(Vec<u8>, bool)],
    state: &Mutex<State>,
    mux: &Mux,
    stop: &Stop,
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
        // A directory may hold millions: none more is deleted once the
        // Destination has stopped.
        stop.check()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::tests::entry;

    #[test]
    fn attributes_are_never_set_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Compared as a file whose mode alone differs, then swapped for a
        // link to a file outside.
        std::os::unix::fs::symlink(&outside, root.join("f")).unwrap();
        let mut file = entry(1, b"f", Kind::File, 7);
        file.mode = 0o4777;
        let directory = fsys::open_root(root.as_os_str()).unwrap();
        let error = apply_entry(
            &directory,
            b"f",
            b"f",
            &file,
            Verdict::Attributes,
            &mut State::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }
}
