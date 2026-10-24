//! The Destination's decider: all it keeps of what it applies, changed only
//! by the events it is told, one at a time, by one thread. What it decides
//! is said as commands, which the makers, the deleting lane, the writing,
//! the finisher and the connection carry out. It does no I/O, and waits on
//! nothing, so that what it does is tested on events made up.
//!
//! It applies what the comparison found, as it comes. What a directory has
//! extra is dealt with first, then its entries are made to match, and the
//! files that need content are asked for. It alone follows each directory's
//! progress, from announced to done, after which it is finished, once every
//! directory under it is, as the other stages tell it what became of each
//! file and directory; and it alone says what an error an executor met
//! means: that an entry failed, or that the transfer stops. It is done once
//! the last directory is finished.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::compare::{Extras, Verdict, Verdicts};
use crate::fsys;
use crate::manifest::{self, Entry, FileId, Kind};
use crate::protocol::{Needs, Results};
use crate::report::{Count, Listed};
use crate::source::display;

use super::apply::Warned;
use super::receive::Compared;
use super::write::Target;
use super::{MissingParent, ends_the_transfer, io_ends_the_transfer, is_protected};

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
    /// Its parts being made by the makers, or waiting for it to be made.
    jobs: usize,
}

/// The directory a path is in, and its name there; none for the root.
pub(super) fn parent_and_name(path: &[u8]) -> Option<(&[u8], &[u8])> {
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

/// How deep a path is: the root is 0.
pub(super) fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        1 + path.iter().filter(|&&byte| byte == b'/').count()
    }
}

/// What the decider keeps of the directories: those not done, the files
/// they wait for, and those done and not finished yet. A directory done is
/// handed to the finishers, and a file needed has its target handed to the
/// writing, by the commands each method adds.
#[derive(Default)]
struct Directories {
    /// The directories announced and not done yet, by path; the root's is
    /// empty. See [`Open`].
    open: HashMap<Vec<u8>, Open>,
    /// The directories whose parts were applied, as the manifest went into
    /// them, depth first: each one's parent is below it.
    stack: Vec<Vec<u8>>,
    /// Files whose content is coming, by ID: the directory each is in, for
    /// it to wait for them, or `None` for the root itself.
    needed: HashMap<FileId, Option<Arc<[u8]>>>,
    /// For each directory, how many under it, done to be finished, are not
    /// finished yet.
    unfinished: HashMap<Vec<u8>, usize>,
    /// The directories done, waiting for those under them to be finished.
    waiting: HashMap<Vec<u8>, Entry>,
    /// How many directories the finishers have.
    finishing: usize,
}

impl Directories {
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
                jobs: 0,
            },
        );
    }

    /// A part of the directory at `path` is given to the makers.
    fn job_started(&mut self, path: &[u8]) {
        if let Some(open) = self.open.get_mut(path) {
            open.jobs += 1;
        }
    }

    /// A part of the directory at `path` is made: it may be done.
    fn job_done(&mut self, path: &[u8], commands: &mut Vec<Command>) {
        if let Some(open) = self.open.get_mut(path) {
            open.jobs = open.jobs.saturating_sub(1);
        }
        self.done_if_so(path.to_vec(), commands);
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
    fn file_settled(&mut self, directory: &[u8], commands: &mut Vec<Command>) {
        if let Some(open) = self.open.get_mut(directory) {
            open.files = open.files.saturating_sub(1);
        }
        self.done_if_so(directory.to_vec(), commands);
    }

    /// The parts of the directory at `path` are applied, as the manifest
    /// goes into it: what it has left behind, it has left.
    fn enter(&mut self, path: &[u8], commands: &mut Vec<Command>) {
        while let Some(top) = self.stack.last() {
            if is_under(path, top) {
                break;
            }
            let top = self.stack.pop().expect("one is there");
            self.leave(top, commands);
        }
        if let Some((parent, name)) = parent_and_name(path)
            && let Some(parent) = self.open.get_mut(parent)
        {
            parent.unseen.remove(name);
        }
        self.stack.push(path.to_vec());
    }

    /// The manifest is over: it has left every directory.
    fn leave_all(&mut self, commands: &mut Vec<Command>) {
        while let Some(top) = self.stack.pop() {
            self.leave(top, commands);
        }
    }

    /// The manifest has left the directory at `path`, and all under it.
    fn leave(&mut self, path: Vec<u8>, commands: &mut Vec<Command>) {
        let unseen = match self.open.get_mut(&path) {
            Some(open) => std::mem::take(&mut open.unseen),
            None => return,
        };
        for name in unseen {
            let child = manifest::join(&path, &name);
            if let Some(open) = self.open.get_mut(&child) {
                open.left = true;
            }
            self.done_if_so(child, commands);
        }
        if let Some(open) = self.open.get_mut(&path) {
            open.left = true;
        }
        self.done_if_so(path, commands);
    }

    /// If the directory at `path` is done, lets it go, to be finished if it
    /// is to be, and so on up, as each one's last undone directory is.
    fn done_if_so(&mut self, mut path: Vec<u8>, commands: &mut Vec<Command>) {
        loop {
            let done = self.open.get(&path).is_some_and(|open| {
                open.left && open.files == 0 && open.children == 0 && open.jobs == 0
            });
            if !done {
                return;
            }
            let open = self.open.remove(&path).expect("there");
            if open.finish {
                self.finish(path.clone(), open.entry, commands);
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

    /// A file is needed: its directory waits for it, and its target goes to
    /// the writing, before the file is asked for, so before its bytes can
    /// come.
    fn need(&mut self, target: Target, commands: &mut Vec<Command>) {
        let id = target.entry.id;
        let directory = (!target.is_root).then(|| Arc::clone(&target.directory));
        if let Some(directory) = &directory {
            self.file_needed(directory);
        }
        self.needed.insert(id, directory);
        commands.push(Command::Target(id, target));
    }

    /// A needed file is settled, written or failed, or the Source said it
    /// could not open it: its directory has one fewer to wait for. One not
    /// waited for, or said again, is nothing.
    fn settle(&mut self, id: FileId, commands: &mut Vec<Command>) {
        if let Some(Some(directory)) = self.needed.remove(&id) {
            self.file_settled(&directory, commands);
        }
    }

    /// Whether a file is needed, and not settled yet.
    fn is_needed(&self, id: FileId) -> bool {
        self.needed.contains_key(&id)
    }

    /// Finishes what is still open once everything is written, deepest
    /// first, should anything be: every directory is finished as it is
    /// done, but one with a file that never came would otherwise never be.
    fn finish_the_rest(&mut self, commands: &mut Vec<Command>) {
        self.leave_all(commands);
        let mut left: Vec<(Vec<u8>, Entry)> = self
            .open
            .drain()
            .filter(|(_, open)| open.finish)
            .map(|(path, open)| (path, open.entry))
            .collect();
        left.sort_by_key(|(path, _)| std::cmp::Reverse(depth(path)));
        for (path, entry) in left {
            self.finish(path, entry, commands);
        }
        self.unstick(commands);
    }

    /// A directory done is finished, by up to [`super::finish::FINISHERS`]
    /// at once, each a directory, but only once every directory under it
    /// that is to be finished is, as one that cannot be gone into after is
    /// finished last. They come done deepest first, so all under one come
    /// before it.
    fn finish(&mut self, path: Vec<u8>, entry: Entry, commands: &mut Vec<Command>) {
        if let Some((parent, _)) = parent_and_name(&path) {
            *self.unfinished.entry(parent.to_vec()).or_default() += 1;
        }
        if self.unfinished.contains_key(&path) {
            self.waiting.insert(path, entry);
        } else {
            self.finishing += 1;
            commands.push(Command::Finish(path, entry));
        }
    }

    /// A directory is finished, or could not be: the one it is in is
    /// finished, if it waited for no other.
    fn finished(&mut self, path: &[u8], commands: &mut Vec<Command>) {
        self.finishing -= 1;
        if let Some((parent, _)) = parent_and_name(path) {
            let left = self.unfinished.get_mut(parent).map(|left| {
                *left -= 1;
                *left
            });
            if left == Some(0) {
                self.unfinished.remove(parent);
                if let Some(entry) = self.waiting.remove(parent) {
                    self.finishing += 1;
                    commands.push(Command::Finish(parent.to_vec(), entry));
                }
            }
        }
        self.unstick(commands);
    }

    /// A directory still waiting once none is being finished waits for one
    /// that never came, as none should: it is finished all the same, the
    /// deepest first.
    fn unstick(&mut self, commands: &mut Vec<Command>) {
        if self.finishing > 0 {
            return;
        }
        let Some(path) = self.waiting.keys().max_by_key(|path| depth(path)).cloned() else {
            return;
        };
        let entry = self.waiting.remove(&path).expect("there");
        self.unfinished.remove(&path);
        self.finishing += 1;
        commands.push(Command::Finish(path, entry));
    }

    /// Whether every directory done is finished.
    fn all_finished(&self) -> bool {
        self.finishing == 0 && self.waiting.is_empty()
    }
}

/// What the decider is told.
pub(super) enum Event {
    /// The manifest's root, checked and compared, and, in a run, made to
    /// match, but for a file's content, by its executor, which could not set
    /// what is in `warned`: taken before any part, as it came before them.
    Root {
        entry: Entry,
        verdict: Verdict,
        warned: Warned,
    },
    /// A part of the manifest, compared, in the manifest's order.
    Compared(Compared),
    /// Every part is compared, and the manifest has ended.
    Ended,
    /// What became of a part a maker made match.
    Made(Made),
    /// What became of a directory's extras on the deleting lane.
    Deleted(Deleted),
    /// A directory finished, as `entry` says, or what kept it from being,
    /// and what could not be set.
    Finished {
        path: Vec<u8>,
        entry: Entry,
        result: Result<()>,
        warned: Warned,
    },
    /// A needed file the Source said it could not open: it never comes.
    Unread(FileId),
    /// A needed file written.
    Settled(FileId),
    /// A needed file not written, or not given its name, as `shown`, and
    /// why, for the decider to say what that means.
    Failed {
        id: FileId,
        shown: Vec<u8>,
        error: anyhow::Error,
    },
    /// The Source sent every block, every file of them settled; these needed
    /// files, as shown, did not come.
    StreamEnded(Vec<(FileId, Vec<u8>)>),
}

/// What the decider has decided.
pub(super) enum Command {
    /// Hand the makers a part, by its job.
    Make(u64, Arc<Part>),
    /// Hand the deleting lane a directory's extras.
    Delete(Extra),
    /// Hand the finishers a directory done, and all under it finished, to
    /// set its attributes and mtime.
    Finish(Vec<u8>, Entry),
    /// Hand the writing a needed file's target, before the file is asked
    /// for, so before its bytes can come.
    Target(FileId, Target),
    /// Every target is handed over: the writing need wait for none more.
    EndTargets,
    /// Tell the Source what is needed, or how far this side has compared.
    Needs(Needs),
    /// Count, for the report.
    Count(Count, u64),
    /// Report what a dry run lists, in the manifest's order.
    Listed(Vec<Listed>),
    /// Report a line for standard error.
    Say(String),
    /// An entry failed: count it, report why, if `said`, and tell the
    /// Source.
    Fail {
        said: Option<String>,
        result: Results,
    },
    /// Warn, at the end, of what an executor could not set: carried out
    /// even once the transfer has stopped.
    Warn(Warned),
    /// Stop the transfer, for this reason: the decider says nothing more.
    Stop(anyhow::Error),
}

/// A compared part to make match, but for its files' content: its
/// directories, symlinks and special files made, and its attributes set.
/// The decider gives it to the makers, and asks for its files once it is
/// made.
pub(super) struct Part {
    pub(super) path: Vec<u8>,
    pub(super) directory: Option<Arc<OwnedFd>>,
    pub(super) entries: Vec<Entry>,
    pub(super) verdicts: Vec<Verdict>,
    pub(super) present: Vec<bool>,
}

/// What a directory has extra, for `sync`, to delete on the deleting lane:
/// the directory as the comparison listed it, and the names in it the
/// Source could not read, whose aliases are kept.
pub(super) struct Extra {
    pub(super) path: Vec<u8>,
    pub(super) directory: Arc<OwnedFd>,
    pub(super) extras: Extras,
    pub(super) unreadable: Vec<Vec<u8>>,
}

/// What became of a directory's extras on the deleting lane: those it went
/// to delete, and what became of each, and those kept as aliases of what
/// the Source could not read.
pub(super) struct Deleted {
    pub(super) path: Vec<u8>,
    pub(super) extras: Extras,
    pub(super) results: Vec<io::Result<()>>,
    pub(super) kept: Vec<Vec<u8>>,
}

/// What became of a part a maker made match, for the decider to say what
/// it means: a maker only reports.
pub(super) struct Made {
    pub(super) job: u64,
    /// Its directory could not be opened, so nothing in it was made.
    pub(super) unopened: Option<io::Error>,
    /// Each error met, in the order of the entries.
    pub(super) errors: Vec<Unmade>,
    /// What could not be set.
    pub(super) warned: Warned,
}

/// An entry a maker could not make match, and why.
pub(super) struct Unmade {
    pub(super) id: FileId,
    pub(super) path: Vec<u8>,
    pub(super) directory: bool,
    pub(super) error: anyhow::Error,
}

/// What the makers and the deleting lane have, as the decider follows it:
/// the parts given to the makers, and those waiting for their directory to
/// be made, or its extras deleted; and the directories whose extras the
/// lane has.
#[derive(Default)]
struct Makers {
    next: u64,
    /// Every part given out, or waiting, by job.
    parts: HashMap<u64, Arc<Part>>,
    /// The directories a part given out, or waiting, is to make.
    making: HashSet<Vec<u8>>,
    /// The directories whose extras the lane is deleting: nothing is made
    /// in one before they are deleted.
    deleting: HashSet<Vec<u8>>,
    /// The parts waiting for their directory to be made, or its extras
    /// deleted, by its path.
    parked: HashMap<Vec<u8>, Vec<u64>>,
    /// The first ID of each part whose files are not asked for yet: the
    /// Source is told it need keep no entry only up to before the least.
    unasked: BTreeSet<FileId>,
}

impl Makers {
    /// Nothing is being made, nor deleted, nor waits to be.
    fn idle(&self) -> bool {
        self.parts.is_empty() && self.deleting.is_empty()
    }

    /// Gives out the parts that waited for the directory at `path`, once it
    /// is made and its extras deleted.
    fn release(&mut self, path: &[u8], commands: &mut Vec<Command>) {
        if self.making.contains(path) || self.deleting.contains(path) {
            return;
        }
        for job in self.parked.remove(path).unwrap_or_default() {
            commands.push(Command::Make(job, Arc::clone(&self.parts[&job])));
        }
    }
}

/// The Destination's decider; see the module. In a run, it gives each part
/// to the makers and goes on; a part waits for its directory to be made,
/// and its extras deleted, and its files are asked for once it is made. In
/// a dry run, it lists what a run would do instead.
pub(super) struct Decider {
    /// What is compared is made to match: not a dry run, which lists it.
    run: bool,
    /// How many entries compared the Source is told of at a time; see
    /// [`crate::protocol::through_every`].
    through_every: FileId,
    directories: Directories,
    makers: Makers,
    /// The compared parts of a directory waiting for its last.
    waiting: Vec<Compared>,
    /// The last ID compared, and the last the Source was told it need keep
    /// no more.
    compared_to: FileId,
    through: FileId,
    /// Every part is compared, and the manifest has ended.
    ended: bool,
    /// The comparisons ended, and the needs end once every part is made.
    needs_end: bool,
    /// The writing may still settle files: in a run, until the Source has
    /// sent every block, and every file of them is settled.
    stream: bool,
}

impl Decider {
    /// A decider for a run, or a dry run, that tells the Source how far it
    /// has compared every `through_every` entries or so.
    pub(super) fn new(run: bool, through_every: FileId) -> Self {
        Self {
            run,
            through_every,
            directories: Directories::default(),
            makers: Makers::default(),
            waiting: Vec::new(),
            compared_to: 0,
            through: 0,
            ended: false,
            needs_end: false,
            stream: run,
        }
    }

    /// Whether there is nothing more to decide: every part is compared, and,
    /// in a run, every file settled and every directory finished.
    pub(super) fn is_finished(&self) -> bool {
        self.ended && !self.stream && self.directories.all_finished()
    }

    /// Takes an event, and says what to do about it: last, should the
    /// transfer stop, why.
    pub(super) fn on(&mut self, event: Event) -> Vec<Command> {
        let mut commands = Vec::new();
        if let Err(error) = self.take(event, &mut commands) {
            commands.push(Command::Stop(error));
        }
        commands
    }

    fn take(&mut self, event: Event, commands: &mut Vec<Command>) -> Result<()> {
        match event {
            Event::Root {
                entry,
                verdict,
                warned,
            } => self.root(&entry, verdict, warned, commands),
            Event::Compared(done) => {
                self.compared(done, commands)?;
                self.tell_through(commands);
            }
            Event::Ended => self.ended(commands),
            Event::Made(made) => {
                self.made(made, commands)?;
                self.made_or_deleted(commands);
            }
            Event::Deleted(deleted) => {
                self.deleted(deleted, commands)?;
                self.made_or_deleted(commands);
            }
            Event::Finished {
                path,
                entry,
                result,
                warned,
            } => self.finished(&path, &entry, result, warned, commands)?,
            Event::Unread(id) | Event::Settled(id) => self.directories.settle(id, commands),
            Event::Failed { id, shown, error } => {
                self.directories.settle(id, commands);
                let why = why_failed(&shown, error)?;
                fail_file(id, &shown, why, commands);
            }
            Event::StreamEnded(never) => self.stream_ended(never, commands),
        }
        Ok(())
    }

    /// Takes the manifest's root, which comes before anything in it,
    /// compared, and in a run made, by its executor: follows it, if a
    /// directory, has its content sent, if a file that needs it, or lists
    /// it, in a dry run.
    fn root(
        &mut self,
        entry: &Entry,
        verdict: Verdict,
        warned: Warned,
        commands: &mut Vec<Command>,
    ) {
        if !warned.is_empty() {
            commands.push(Command::Warn(warned));
        }
        if self.run {
            if entry.kind == Kind::Directory {
                self.directories.announce(Vec::new(), entry.clone());
                if verdict != Verdict::Same {
                    self.directories.mark_to_finish(b"");
                }
            } else if verdict == Verdict::Content && entry.kind == Kind::File {
                let target = Target {
                    directory: Arc::from(&b""[..]),
                    entry: entry.clone(),
                    is_root: true,
                    // Its verdict does not say whether it is missing.
                    exists: true,
                };
                self.directories.need(target, commands);
            }
        } else if verdict.is_listed() {
            commands.push(Command::Listed(vec![Listed::to_copy(b".".to_vec())]));
        }
        count(Count::Entries, 1, commands);
        if verdict == Verdict::Content && entry.kind == Kind::File {
            count(Count::Needed, 1, commands);
            count(Count::ToSend, entry.size, commands);
        }
        let (files, attributes) = split(std::slice::from_ref(entry), &[verdict]);
        send_needs(files, attributes, commands);
    }

    /// Takes a part as it is compared. A directory's parts wait for its last,
    /// as rsync's do: then, for `sync`, what it has extra is dealt with first,
    /// and each part after, so that nothing made for it is ever taken for
    /// extra, as it would be on a filesystem that takes two names for one, such
    /// as one that ignores case. `copy` deletes nothing, and goes the same way.
    fn compared(&mut self, done: Compared, commands: &mut Vec<Command>) -> Result<()> {
        // Gone into, the Destination would write into the Source as it is read,
        // and `sync` delete from it.
        if done.refused {
            bail!(
                "the Source's root is inside the Destination, at {}",
                String::from_utf8_lossy(&display(&done.path, true))
            );
        }
        let last = done.last;
        self.waiting.push(done);
        if !last {
            return Ok(());
        }
        let mut parts = std::mem::take(&mut self.waiting);
        // Only a directory's last part comes with its extras, and only for `sync`.
        let final_part = parts.last_mut().expect("its last part");
        if let Some(extras) = final_part.extras.take() {
            let path = final_part.path.clone();
            let directory = final_part.directory.clone();
            let unreadable = std::mem::take(&mut final_part.unreadable);
            let kept = std::mem::take(&mut final_part.kept);
            self.extras(path, directory, extras, unreadable, &kept, commands);
        }
        if self.run {
            let path = &parts[0].path;
            self.directories.enter(path, commands);
            if parts.iter().any(|done| done.let_in.is_some()) {
                self.directories.mark_to_finish(path);
            }
        }
        let last = parts
            .iter()
            .rev()
            .find_map(|done| done.entries.last())
            .map(|entry| entry.id);
        for done in parts {
            if self.run {
                self.take_part(done, commands);
            } else {
                list(done, commands);
            }
        }
        // Every entry up to its last is compared: the Source may let go of
        // those whose files are asked for, told now and then.
        if let Some(last) = last {
            self.compared_to = last;
        }
        Ok(())
    }

    /// Deals with what the directory at `path` has that the Source does
    /// not, before anything of its parts: lists it, in a dry run, which
    /// found what a run would keep, or gives it to the deleting lane, which
    /// its parts then wait for. The Source is not told: however many there
    /// are, nothing about them goes in a message.
    fn extras(
        &mut self,
        path: Vec<u8>,
        directory: Option<Arc<OwnedFd>>,
        extras: io::Result<Extras>,
        unreadable: Vec<Vec<u8>>,
        kept: &[Vec<u8>],
        commands: &mut Vec<Command>,
    ) {
        let extras = match extras {
            Ok(extras) => extras,
            Err(error) => {
                let message = format!("cannot list what it holds: {error}");
                let said = format!(
                    "cannot delete in {}: {message}",
                    String::from_utf8_lossy(&display(&path, true))
                );
                commands.push(Command::Fail {
                    said: Some(said),
                    result: Results::NotDeleted { path, message },
                });
                return;
            }
        };
        // Extras come only from a listing, so only from a directory held.
        let Some(directory) = directory else {
            return;
        };
        if !self.run {
            tell_kept(&path, kept, commands);
            let listed: Vec<Listed> = extras
                .iter()
                .map(|(name, is_directory)| {
                    Listed::to_delete(display(&manifest::join(&path, name), *is_directory))
                })
                .collect();
            if !listed.is_empty() {
                commands.push(Command::Listed(listed));
            }
            return;
        }
        if extras.is_empty() {
            return;
        }
        self.makers.deleting.insert(path.clone());
        self.directories.job_started(&path);
        commands.push(Command::Delete(Extra {
            path,
            directory,
            extras,
            unreadable,
        }));
    }

    /// Takes a compared part, in a run: announces its directories, tells what
    /// could not be compared, and gives the rest to the makers, unless its
    /// directory is still to be made, or its extras deleted, when it waits
    /// for that.
    fn take_part(&mut self, done: Compared, commands: &mut Vec<Command>) {
        let Compared {
            path,
            entries,
            directory,
            verdicts,
            present,
            ..
        } = done;
        let Verdicts {
            verdicts,
            failures: uncompared,
            unopened,
        } = verdicts;
        for entry in &entries {
            if entry.kind == Kind::Directory {
                self.directories
                    .announce(manifest::join(&path, &entry.name), entry.clone());
            }
        }
        tell_uncompared(&path, &entries, uncompared, unopened.as_deref(), commands);
        if verdicts.iter().all(|&verdict| verdict == Verdict::Same) {
            return;
        }
        // The directories it makes, or changes: none is done before it is made,
        // as only then is it known whether each is to be finished; and the parts
        // of one it makes wait for it.
        for (entry, &verdict) in entries.iter().zip(&verdicts) {
            if entry.kind == Kind::Directory && verdict != Verdict::Same {
                let child = manifest::join(&path, &entry.name);
                self.directories.job_started(&child);
                if verdict == Verdict::Content {
                    self.makers.making.insert(child);
                }
            }
        }
        let job = self.makers.next;
        self.makers.next += 1;
        if let Some(first) = entries.first() {
            self.makers.unasked.insert(first.id);
        }
        self.directories.job_started(&path);
        let waits = self.makers.making.contains(&path) || self.makers.deleting.contains(&path);
        let part = Arc::new(Part {
            path,
            directory,
            entries,
            verdicts,
            present,
        });
        self.makers.parts.insert(job, Arc::clone(&part));
        if waits {
            self.makers
                .parked
                .entry(part.path.clone())
                .or_default()
                .push(job);
        } else {
            commands.push(Command::Make(job, part));
        }
    }

    /// Takes a part a maker made: tells what could not be made, marks what is
    /// to be finished, asks for its files, which have their directory now, and
    /// gives out the parts that waited for the directories it made, or failed
    /// to.
    fn made(&mut self, made: Made, commands: &mut Vec<Command>) -> Result<()> {
        let part = self
            .makers
            .parts
            .remove(&made.job)
            .expect("a part given out");
        let Part {
            path,
            entries,
            verdicts,
            present,
            ..
        } = &*part;
        // What could not be set is said even if the transfer stops.
        let failures = what_failed(&made, entries, verdicts);
        let Made {
            unopened, warned, ..
        } = made;
        if !warned.is_empty() {
            commands.push(Command::Warn(warned));
        }
        let failures = failures?;
        tell_failed(path, entries, &failures, true, commands);
        let failed: HashSet<FileId> = failures.iter().map(|(id, _)| *id).collect();
        if unopened.is_none() {
            // Creating or replacing anything in a directory changes its mtime.
            if verdicts.contains(&Verdict::Content) {
                self.directories.mark_to_finish(path);
            }
            for (entry, &verdict) in entries.iter().zip(verdicts) {
                if entry.kind == Kind::Directory
                    && verdict != Verdict::Same
                    && !failed.contains(&entry.id)
                {
                    self.directories
                        .mark_to_finish(&manifest::join(path, &entry.name));
                }
            }
        }
        // The directory's path, as its files' targets share it.
        let mut shared: Option<Arc<[u8]>> = None;
        for ((entry, &verdict), &exists) in entries.iter().zip(verdicts).zip(present) {
            if verdict == Verdict::Content
                && entry.kind == Kind::File
                && !failed.contains(&entry.id)
            {
                let directory = shared.get_or_insert_with(|| Arc::from(path.as_slice()));
                let target = Target {
                    directory: Arc::clone(directory),
                    entry: entry.clone(),
                    is_root: false,
                    exists,
                };
                self.directories.need(target, commands);
            }
        }
        ask_for(entries, verdicts, &failed, commands);
        for (entry, &verdict) in entries.iter().zip(verdicts) {
            if entry.kind == Kind::Directory && verdict != Verdict::Same {
                let child = manifest::join(path, &entry.name);
                if verdict == Verdict::Content {
                    self.makers.making.remove(&child);
                    self.makers.release(&child, commands);
                }
                self.directories.job_done(&child, commands);
            }
        }
        if let Some(first) = entries.first() {
            self.makers.unasked.remove(&first.id);
        }
        self.directories.job_done(path, commands);
        Ok(())
    }

    /// Takes a directory's extras, deleted on the lane: tells what became of
    /// them, marks the directory to be finished if any went, and gives out its
    /// parts, which waited.
    fn deleted(&mut self, deleted: Deleted, commands: &mut Vec<Command>) -> Result<()> {
        let Deleted {
            path,
            extras,
            results,
            kept,
        } = deleted;
        tell_kept(&path, &kept, commands);
        if tell_deleted(&path, &extras, results, commands)? {
            // Deleting changes the directory's mtime, which is set again.
            self.directories.mark_to_finish(&path);
        }
        self.makers.deleting.remove(&path);
        self.makers.release(&path, commands);
        self.directories.job_done(&path, commands);
        Ok(())
    }

    /// Takes a directory a finisher finished, or could not: one that could
    /// not be fails alone, said as its entry, and the rest are finished all
    /// the same, but where the drive is full or read-only, the transfer
    /// stops.
    fn finished(
        &mut self,
        path: &[u8],
        entry: &Entry,
        result: Result<()>,
        warned: Warned,
        commands: &mut Vec<Command>,
    ) -> Result<()> {
        if !warned.is_empty() {
            commands.push(Command::Warn(warned));
        }
        if let Err(error) = result {
            let shown = display(path, true);
            if ends_the_transfer(&error) {
                return Err(error.context(format!("finish {}", String::from_utf8_lossy(&shown))));
            }
            let message = format!("could not be finished: {error:#}");
            fail_file(entry.id, &shown, message, commands);
        }
        self.directories.finished(path, commands);
        Ok(())
    }

    /// A maker or the lane is done: the Source may let go of more entries,
    /// and the needs end, if the comparisons have, once nothing more is
    /// being made.
    fn made_or_deleted(&mut self, commands: &mut Vec<Command>) {
        self.tell_through(commands);
        if self.needs_end && self.makers.idle() {
            self.needs_end = false;
            self.end_needs(commands);
        }
    }

    /// Every comparison came out, and the manifest has ended: the needs end,
    /// once every part is made.
    fn ended(&mut self, commands: &mut Vec<Command>) {
        self.ended = true;
        if self.run {
            self.directories.leave_all(commands);
        }
        if !self.run || self.makers.idle() {
            self.end_needs(commands);
        } else {
            self.needs_end = true;
        }
    }

    /// Every file is asked for: the writing need wait for no more targets,
    /// and the Source is told the needs end.
    fn end_needs(&mut self, commands: &mut Vec<Command>) {
        if self.run {
            commands.push(Command::EndTargets);
        }
        commands.push(Command::Needs(Needs::End));
    }

    /// Tells the Source, now and then, that every entry up to the last
    /// compared is, and those needed asked for, but before the first of a
    /// part whose files are not asked for yet.
    fn tell_through(&mut self, commands: &mut Vec<Command>) {
        let asked = match self.makers.unasked.first() {
            Some(&first) => self.compared_to.min(first.saturating_sub(1)),
            None => self.compared_to,
        };
        if asked >= self.through + self.through_every {
            commands.push(Command::Needs(Needs::Through(asked)));
            self.through = asked;
        }
    }

    /// The Source sent every block, and every file of them is settled: a
    /// needed file that neither came nor was said not to open never came,
    /// which fails it, not the transfer; and what is still open is finished.
    fn stream_ended(&mut self, never: Vec<(FileId, Vec<u8>)>, commands: &mut Vec<Command>) {
        for (id, shown) in never {
            if self.directories.is_needed(id) {
                fail_file(id, &shown, "the Source never sent it".to_owned(), commands);
                self.directories.settle(id, commands);
            }
        }
        self.directories.finish_the_rest(commands);
        self.stream = false;
    }
}

/// Counts `amount` more of `count`, for the report, if any.
fn count(count: Count, amount: u64, commands: &mut Vec<Command>) {
    if amount > 0 {
        commands.push(Command::Count(count, amount));
    }
}

/// Lists what a compared part would change, in a dry run, tells what could
/// not be compared, and asks for what it needs; its directory's extras are
/// dealt with apart, by [`Decider::extras`].
fn list(done: Compared, commands: &mut Vec<Command>) {
    let Compared {
        path,
        entries,
        verdicts,
        replacing,
        ..
    } = done;
    let Verdicts {
        verdicts,
        failures: uncompared,
        unopened,
    } = verdicts;
    let mut listed = Vec::new();
    for (entry, verdict) in entries.iter().zip(&verdicts) {
        if !verdict.is_listed() {
            continue;
        }
        let child = manifest::join(&path, &entry.name);
        // In `sync`, a directory something else replaces is deleted, with
        // all it holds: said so, as a run would do.
        if replacing.contains(&entry.id) {
            listed.push(Listed::to_delete(display(&child, true)));
        }
        listed.push(Listed::to_copy(display(
            &child,
            entry.kind == Kind::Directory,
        )));
    }
    let failed = tell_uncompared(&path, &entries, uncompared, unopened.as_deref(), commands);
    if !listed.is_empty() {
        commands.push(Command::Listed(listed));
    }
    ask_for(&entries, &verdicts, &failed, commands);
}

/// A file failed, as `shown`: counted, reported, and the Source told.
fn fail_file(id: FileId, shown: &[u8], message: String, commands: &mut Vec<Command>) {
    let said = format!("cannot copy {}: {message}", String::from_utf8_lossy(shown));
    commands.push(Command::Fail {
        said: Some(said),
        result: Results::Failed { id, message },
    });
}

/// Tells what of a part could not be compared, each failing alone, and
/// left as it is; a directory that could not be opened is told once.
/// Returns their IDs.
fn tell_uncompared(
    path: &[u8],
    entries: &[Entry],
    uncompared: Vec<(FileId, String)>,
    unopened: Option<&str>,
    commands: &mut Vec<Command>,
) -> HashSet<FileId> {
    if let Some(error) = unopened {
        commands.push(Command::Say(format!(
            "cannot compare in {}: {error}",
            String::from_utf8_lossy(&display(path, true))
        )));
    }
    tell_failed(path, entries, &uncompared, unopened.is_none(), commands);
    uncompared.into_iter().map(|(id, _)| id).collect()
}

/// Counts and tells each entry of a part that failed, and why, telling the
/// Source too; each is said only with `say`.
fn tell_failed(
    path: &[u8],
    entries: &[Entry],
    failures: &[(FileId, String)],
    say: bool,
    commands: &mut Vec<Command>,
) {
    for (id, message) in failures {
        let said = by_id(entries, *id).filter(|_| say).map(|entry| {
            let child = manifest::join(path, &entry.name);
            format!(
                "cannot copy {}: {message}",
                String::from_utf8_lossy(&display(&child, entry.kind == Kind::Directory))
            )
        });
        commands.push(Command::Fail {
            said,
            result: Results::Failed {
                id: *id,
                message: message.clone(),
            },
        });
    }
}

/// Warns of each extra kept as what the Source could not read, under
/// another name.
fn tell_kept(path: &[u8], kept: &[Vec<u8>], commands: &mut Vec<Command>) {
    for name in kept {
        commands.push(Command::Say(format!(
            "warning: not deleting {}: it is what the source could not read, under \
             another name",
            String::from_utf8_lossy(&display(&manifest::join(path, name), false))
        )));
    }
}

/// Tells what became of the extras of the directory at `directory`, each
/// with its result: one on another filesystem is warned of, one that cannot
/// be deleted fails alone, but one to protect, or a drive full or
/// read-only, stops the transfer. Returns whether any was deleted.
pub(super) fn tell_deleted(
    directory: &[u8],
    extras: &[(Vec<u8>, bool)],
    results: Vec<io::Result<()>>,
    commands: &mut Vec<Command>,
) -> Result<bool> {
    let mut deleted = false;
    for ((name, is_directory), result) in extras.iter().zip(results) {
        let path = manifest::join(directory, name);
        let shown = || String::from_utf8_lossy(&display(&path, *is_directory)).into_owned();
        match result {
            Ok(()) => deleted = true,
            // What else it held is deleted; it stays, as rsync's `-x` keeps
            // it, which is no failure.
            Err(error) if fsys::is_elsewhere(&error) => commands.push(Command::Say(format!(
                "warning: not deleting {}: {error}",
                shown()
            ))),
            Err(error) if fsys::is_protected(&error) => {
                bail!("will not delete {}: {error}", shown());
            }
            // Nothing more could be deleted.
            Err(error) if io_ends_the_transfer(&error) => {
                bail!("cannot delete {}: {error}", shown());
            }
            Err(error) => {
                let said = format!("cannot delete {}: {error}", shown());
                commands.push(Command::Fail {
                    said: Some(said),
                    result: Results::NotDeleted {
                        path,
                        message: error.to_string(),
                    },
                });
            }
        }
    }
    Ok(deleted)
}

/// Asks the Source for what a part needs, but what failed: it has nowhere
/// to go.
fn ask_for(
    entries: &[Entry],
    verdicts: &[Verdict],
    failed: &HashSet<FileId>,
    commands: &mut Vec<Command>,
) {
    let (files, attributes) = split(entries, verdicts);
    let files: Vec<FileId> = files
        .into_iter()
        .filter(|id| !failed.contains(id))
        .collect();
    let attributes: Vec<FileId> = attributes
        .into_iter()
        .filter(|id| !failed.contains(id))
        .collect();
    let sizes: HashMap<FileId, u64> = entries
        .iter()
        .filter(|entry| entry.kind == Kind::File)
        .map(|entry| (entry.id, entry.size))
        .collect();
    let needed: Vec<u64> = files
        .iter()
        .filter_map(|id| sizes.get(id).copied())
        .collect();
    count(Count::Needed, needed.len() as u64, commands);
    count(Count::ToSend, needed.iter().sum(), commands);
    send_needs(files, attributes, commands);
}

fn send_needs(files: Vec<FileId>, attributes: Vec<FileId>, commands: &mut Vec<Command>) {
    if !files.is_empty() {
        commands.push(Command::Needs(Needs::Files(files)));
    }
    if !attributes.is_empty() {
        commands.push(Command::Needs(Needs::Attributes(attributes)));
    }
}

/// The entry of a part with `id`, found at once: the manifest is checked to
/// number a part's entries in turn.
fn by_id(entries: &[Entry], id: FileId) -> Option<&Entry> {
    let first = entries.first()?.id;
    let entry = entries.get(usize::try_from(id.checked_sub(first)?).ok()?)?;
    (entry.id == id).then_some(entry)
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

/// What became of a part means, as the decider alone says: a deletion
/// refused because what was to go was to be protected, or a drive full or
/// read-only, stops the transfer; any other error fails its entry alone,
/// and a directory that could not be opened fails all it was to change.
/// Returns the entries failed, and why.
fn what_failed(
    made: &Made,
    entries: &[Entry],
    verdicts: &[Verdict],
) -> Result<Vec<(FileId, String)>> {
    if let Some(error) = &made.unopened {
        return Ok(entries
            .iter()
            .zip(verdicts)
            .filter(|(_, verdict)| **verdict != Verdict::Same)
            .map(|(entry, _)| (entry.id, format!("cannot open its directory: {error}")))
            .collect());
    }
    let mut failures = Vec::new();
    for unmade in &made.errors {
        let Unmade {
            id,
            path,
            directory,
            error,
        } = unmade;
        if !directory && is_protected(error) {
            bail!(
                "will not delete {} to make {}: {error:#}",
                String::from_utf8_lossy(&display(path, true)),
                String::from_utf8_lossy(&display(path, false)),
            );
        }
        if ends_the_transfer(error) {
            bail!(cannot_copy(path, *directory, error));
        }
        failures.push((*id, format!("{error:#}")));
    }
    Ok(failures)
}

/// Why a file failed, as said of it, unless the error stops the transfer,
/// which it returns: the root, which is this file, has nowhere to go; what
/// was in the way, to be deleted, was to be protected; or every file after
/// it would fail the same way.
fn why_failed(shown: &[u8], error: anyhow::Error) -> Result<String> {
    if error.is::<MissingParent>() {
        return Err(error);
    }
    let shown = String::from_utf8_lossy(shown);
    if is_protected(&error) {
        return Err(error.context(format!("will not delete {shown}/ to make {shown}")));
    }
    if ends_the_transfer(&error) {
        return Err(error.context(format!("write {shown}")));
    }
    Ok(format!("{error:#}"))
}

/// Why the transfer stops, as `path` could not be copied.
fn cannot_copy(path: &[u8], is_directory: bool, error: &anyhow::Error) -> String {
    format!(
        "cannot copy {}: {error:#}",
        String::from_utf8_lossy(&display(path, is_directory))
    )
}

#[cfg(test)]
mod tests {
    use rustix::io::Errno;

    use super::*;
    use crate::destination::tests::entry;

    fn directory(id: FileId, name: &[u8]) -> Entry {
        entry(id, name, Kind::Directory, 0)
    }

    /// A part as compared: its entries' verdicts, and nothing listed in a
    /// directory the comparison did not find.
    fn compared(path: &[u8], entries: Vec<Entry>, verdicts: Vec<Verdict>, last: bool) -> Compared {
        Compared {
            path: path.to_vec(),
            present: vec![false; entries.len()],
            entries,
            last,
            directory: None,
            verdicts: Verdicts {
                verdicts,
                failures: Vec::new(),
                unopened: None,
            },
            extras: None,
            unreadable: Vec::new(),
            kept: Vec::new(),
            replacing: HashSet::new(),
            let_in: None,
            refused: false,
        }
    }

    /// A directory held, as the comparison holds one it listed: any will
    /// do, as the decider only hands it on.
    fn held() -> Option<Arc<OwnedFd>> {
        Some(Arc::new(OwnedFd::from(std::fs::File::open("/").unwrap())))
    }

    /// A directory finished, as the finishers say.
    fn finished(path: &[u8], id: FileId) -> Event {
        Event::Finished {
            path: path.to_vec(),
            entry: directory(id, b"x"),
            result: Ok(()),
            warned: Vec::new(),
        }
    }

    /// A part made, with nothing met.
    fn made(job: u64) -> Event {
        Event::Made(Made {
            job,
            unopened: None,
            errors: Vec::new(),
            warned: Vec::new(),
        })
    }

    /// What each command says, short, to be compared.
    fn said(commands: &[Command]) -> Vec<String> {
        let shown = |path: &[u8]| String::from_utf8_lossy(path).into_owned();
        commands
            .iter()
            .map(|command| match command {
                Command::Make(job, part) => format!("make {job} {}", shown(&part.path)),
                Command::Delete(extra) => format!("delete in {}", shown(&extra.path)),
                Command::Finish(path, _) => format!("finish {}", shown(path)),
                Command::Target(id, _) => format!("target {id}"),
                Command::EndTargets => "end targets".to_owned(),
                Command::Needs(needs) => format!("{needs:?}"),
                Command::Count(count, amount) => format!("count {count:?} {amount}"),
                Command::Listed(listed) => {
                    let listed: Vec<String> = listed
                        .iter()
                        .map(|listed| {
                            let sign = if listed.deleting { "-" } else { "+" };
                            format!("{sign}{}", shown(&listed.path))
                        })
                        .collect();
                    format!("listed {}", listed.join(" "))
                }
                Command::Say(line) => format!("say {line}"),
                Command::Fail { said, result } => match said {
                    Some(said) => format!("fail {result:?}, said {said}"),
                    None => format!("fail {result:?}"),
                },
                Command::Warn(warned) => format!("warn {}", warned.len()),
                Command::Stop(error) => format!("stop: {error:#}"),
            })
            .collect()
    }

    /// The decider takes `event`, and says what each command it gives says.
    fn on(decider: &mut Decider, event: Event) -> Vec<String> {
        said(&decider.on(event))
    }

    /// A run, from the root to the end: each part is made as it comes, but
    /// one whose directory another is making, which waits for it; the files
    /// of each are asked for once it is made, their targets first; the
    /// Source is told how far it may let go, but never past a part not
    /// asked for; the needs end once nothing more is made; directories are
    /// finished as they are done, deepest first; and the decider is done
    /// once the stream is.
    #[test]
    fn a_run_goes_from_the_root_to_the_end() {
        let mut decider = Decider::new(true, 1);
        let root = Event::Root {
            entry: directory(0, b""),
            verdict: Verdict::Content,
            warned: Vec::new(),
        };
        assert_eq!(on(&mut decider, root), ["count Entries 1", "Files([0])"]);
        let part = compared(
            b"",
            vec![directory(1, b"a"), entry(2, b"f", Kind::File, 5)],
            vec![Verdict::Content, Verdict::Content],
            true,
        );
        assert_eq!(on(&mut decider, Event::Compared(part)), ["make 0 "]);
        let part = compared(
            b"a",
            vec![entry(3, b"g", Kind::File, 7)],
            vec![Verdict::Content],
            true,
        );
        // `a` is still being made.
        assert!(on(&mut decider, Event::Compared(part)).is_empty());
        assert_eq!(
            on(&mut decider, made(0)),
            [
                "target 2",
                "count Needed 1",
                "count ToSend 5",
                "Files([1, 2])",
                "make 1 a",
                "Through(2)",
            ]
        );
        // The part of `a` is still being made.
        assert!(on(&mut decider, Event::Ended).is_empty());
        assert_eq!(
            on(&mut decider, made(1)),
            [
                "target 3",
                "count Needed 1",
                "count ToSend 7",
                "Files([3])",
                "Through(3)",
                "end targets",
                "End",
            ]
        );
        assert!(on(&mut decider, Event::Settled(2)).is_empty());
        // The root is done too, but waits for `a` to be finished.
        assert_eq!(on(&mut decider, Event::Settled(3)), ["finish a"]);
        assert!(on(&mut decider, Event::StreamEnded(Vec::new())).is_empty());
        assert!(!decider.is_finished());
        assert_eq!(on(&mut decider, finished(b"a", 1)), ["finish "]);
        assert!(!decider.is_finished());
        assert!(on(&mut decider, finished(b"", 0)).is_empty());
        assert!(decider.is_finished());
    }

    /// A directory's extras go to the lane before anything is made in it:
    /// its parts wait for them, but the parts of another directory do not.
    #[test]
    fn a_directorys_parts_wait_for_its_extras_to_be_deleted() {
        let mut decider = Decider::new(true, 1_000);
        let root = Event::Root {
            entry: directory(0, b""),
            verdict: Verdict::Same,
            warned: Vec::new(),
        };
        assert_eq!(on(&mut decider, root), ["count Entries 1"]);
        let mut part = compared(
            b"",
            vec![directory(1, b"b"), entry(2, b"n", Kind::File, 3)],
            vec![Verdict::Same, Verdict::Content],
            true,
        );
        part.directory = held();
        part.extras = Some(Ok(vec![(b"old".to_vec(), false)]));
        assert_eq!(on(&mut decider, Event::Compared(part)), ["delete in "]);
        let part = compared(
            b"b",
            vec![entry(3, b"m", Kind::File, 3)],
            vec![Verdict::Content],
            true,
        );
        assert_eq!(on(&mut decider, Event::Compared(part)), ["make 1 b"]);
        let deleted = Event::Deleted(Deleted {
            path: Vec::new(),
            extras: vec![(b"old".to_vec(), false)],
            results: vec![Ok(())],
            kept: Vec::new(),
        });
        assert_eq!(on(&mut decider, deleted), ["make 0 "]);
    }

    /// What the lane met is for the decider to say: an extra on another
    /// filesystem is warned of, one that could not be deleted fails alone,
    /// and one to protect, or a full drive, stops the transfer.
    #[test]
    fn what_the_lane_met_is_said_by_the_decider() {
        let extras = vec![
            (b"gone".to_vec(), false),
            (b"mount".to_vec(), true),
            (b"locked".to_vec(), false),
        ];
        let elsewhere = io::Error::other(fsys::Elsewhere { path: Vec::new() });
        let results = vec![Ok(()), Err(elsewhere), Err(io::Error::from(Errno::ACCESS))];
        let mut commands = Vec::new();
        assert!(tell_deleted(b"d", &extras, results, &mut commands).unwrap());
        assert_eq!(
            said(&commands),
            [
                "say warning: not deleting d/mount/: it is on another filesystem",
                "fail NotDeleted { path: [100, 47, 108, 111, 99, 107, 101, 100], message: \
                 \"Permission denied (os error 13)\" }, said cannot delete d/locked: \
                 Permission denied (os error 13)",
            ]
        );
        let protected = vec![Err(io::Error::other(fsys::Protected))];
        let error = tell_deleted(b"d", &extras[..1], protected, &mut Vec::new()).unwrap_err();
        assert!(
            error.to_string().starts_with("will not delete d/gone: "),
            "{error:#}"
        );
        let full = vec![Err(io::Error::from(Errno::NOSPC))];
        let error = tell_deleted(b"d", &extras[..1], full, &mut Vec::new()).unwrap_err();
        assert!(
            error.to_string().starts_with("cannot delete d/gone: "),
            "{error:#}"
        );
    }

    /// A dry run lists, in the manifest's order, a directory's extras before
    /// its entries, but those a run would keep, which it warns of; an entry
    /// that would replace a directory, after saying the directory would go;
    /// and nothing that would stay the same. Its needs end with the
    /// comparisons, and so does it.
    #[test]
    fn a_dry_run_lists_in_the_manifests_order() {
        let mut decider = Decider::new(false, 1_000);
        let root = Event::Root {
            entry: directory(0, b""),
            verdict: Verdict::Content,
            warned: Vec::new(),
        };
        assert_eq!(
            on(&mut decider, root),
            ["listed +.", "count Entries 1", "Files([0])"]
        );
        let mut part = compared(
            b"",
            vec![
                entry(1, b"f", Kind::File, 4),
                directory(2, b"d"),
                entry(3, b"s", Kind::File, 4),
            ],
            vec![Verdict::Content, Verdict::Content, Verdict::Same],
            true,
        );
        part.directory = held();
        part.extras = Some(Ok(vec![(b"x".to_vec(), false), (b"y".to_vec(), true)]));
        part.kept = vec![b"K".to_vec()];
        part.replacing = HashSet::from([1]);
        assert_eq!(
            on(&mut decider, Event::Compared(part)),
            [
                "say warning: not deleting K: it is what the source could not read, under \
                 another name",
                "listed -x -y/",
                "listed -f/ +f +d/",
                "count Needed 1",
                "count ToSend 4",
                "Files([1, 2])",
            ]
        );
        assert!(!decider.is_finished());
        assert_eq!(on(&mut decider, Event::Ended), ["End"]);
        assert!(decider.is_finished());
    }

    /// A root that is a file, with content to send, has its target handed to
    /// the writing before it is asked for; nothing is to be finished.
    #[test]
    fn a_root_that_is_a_file_is_asked_for_after_its_target() {
        let mut decider = Decider::new(true, 1_000);
        let root = Event::Root {
            entry: entry(0, b"", Kind::File, 9),
            verdict: Verdict::Content,
            warned: Vec::new(),
        };
        assert_eq!(
            on(&mut decider, root),
            [
                "target 0",
                "count Entries 1",
                "count Needed 1",
                "count ToSend 9",
                "Files([0])",
            ]
        );
        assert_eq!(on(&mut decider, Event::Ended), ["end targets", "End"]);
        assert!(on(&mut decider, Event::Settled(0)).is_empty());
        assert!(on(&mut decider, Event::StreamEnded(Vec::new())).is_empty());
        assert!(decider.is_finished());
    }

    /// The Source's root, found inside the Destination, stops the transfer
    /// before anything of it is applied.
    #[test]
    fn the_sources_root_inside_the_destination_stops_the_transfer() {
        let mut decider = Decider::new(true, 1_000);
        let mut part = compared(
            b"sub",
            vec![entry(5, b"f", Kind::File, 1)],
            vec![Verdict::Content],
            true,
        );
        part.refused = true;
        assert_eq!(
            on(&mut decider, Event::Compared(part)),
            ["stop: the Source's root is inside the Destination, at sub/"]
        );
    }

    /// A file that failed settles its directory, which is finished, and
    /// fails alone, as one that never came does; but a full drive stops the
    /// transfer.
    #[test]
    fn a_file_that_fails_settles_its_directory() {
        let mut decider = Decider::new(true, 1_000);
        let root = Event::Root {
            entry: directory(0, b""),
            verdict: Verdict::Content,
            warned: Vec::new(),
        };
        decider.on(root);
        let part = compared(
            b"",
            vec![
                entry(1, b"f", Kind::File, 1),
                entry(2, b"g", Kind::File, 1),
                entry(3, b"h", Kind::File, 1),
            ],
            vec![Verdict::Content; 3],
            true,
        );
        decider.on(Event::Compared(part));
        decider.on(made(0));
        decider.on(Event::Ended);
        let failed = Event::Failed {
            id: 1,
            shown: b"f".to_vec(),
            error: anyhow::anyhow!("odd"),
        };
        assert_eq!(
            on(&mut decider, failed),
            ["fail Failed { id: 1, message: \"odd\" }, said cannot copy f: odd"]
        );
        let ended = Event::StreamEnded(vec![(2, b"g".to_vec()), (3, b"h".to_vec())]);
        decider.on(Event::Settled(3));
        assert_eq!(
            on(&mut decider, ended),
            [
                "fail Failed { id: 2, message: \"the Source never sent it\" }, said cannot \
                 copy g: the Source never sent it",
                "finish ",
            ]
        );
        let mut decider = Decider::new(true, 1_000);
        let full = Event::Failed {
            id: 1,
            shown: b"f".to_vec(),
            error: io::Error::from(Errno::NOSPC).into(),
        };
        let said = on(&mut decider, full);
        assert!(
            matches!(&said[..], [stop] if stop.starts_with("stop: write f: ")),
            "{said:?}"
        );
    }

    /// A decider with the root to finish, and `d` in it, done, handed to the
    /// finishers.
    fn finishing_d() -> Decider {
        let mut decider = Decider::new(true, 1_000);
        let root = Event::Root {
            entry: directory(0, b""),
            verdict: Verdict::Content,
            warned: Vec::new(),
        };
        decider.on(root);
        let part = compared(b"", vec![directory(1, b"d")], vec![Verdict::Content], true);
        decider.on(Event::Compared(part));
        decider.on(Event::Compared(compared(
            b"d",
            Vec::new(),
            Vec::new(),
            true,
        )));
        decider.on(made(0));
        assert_eq!(
            on(&mut decider, Event::Ended),
            ["finish d", "end targets", "End"]
        );
        assert!(on(&mut decider, Event::StreamEnded(Vec::new())).is_empty());
        decider
    }

    /// A directory that could not be finished fails alone, said as its
    /// entry, and the one it is in is finished all the same; but a full
    /// drive stops the transfer. The decider is done only once the last
    /// directory is finished.
    #[test]
    fn a_directory_that_cannot_be_finished_fails_alone() {
        let mut decider = finishing_d();
        let failed = Event::Finished {
            path: b"d".to_vec(),
            entry: directory(1, b"d"),
            result: Err(io::Error::from(Errno::ACCESS).into()),
            warned: vec![(super::super::warnings::Warning::Mode, b"d/".to_vec())],
        };
        assert_eq!(
            on(&mut decider, failed),
            [
                "warn 1",
                "fail Failed { id: 1, message: \"could not be finished: Permission denied (os \
                 error 13)\" }, said cannot copy d/: could not be finished: Permission denied \
                 (os error 13)",
                "finish ",
            ]
        );
        assert!(!decider.is_finished());
        assert!(on(&mut decider, finished(b"", 0)).is_empty());
        assert!(decider.is_finished());

        let mut decider = finishing_d();
        let full = Event::Finished {
            path: b"d".to_vec(),
            entry: directory(1, b"d"),
            result: Err(io::Error::from(Errno::NOSPC).into()),
            warned: Vec::new(),
        };
        let said = on(&mut decider, full);
        assert!(
            matches!(&said[..], [stop] if stop.starts_with("stop: finish d/: ")),
            "{said:?}"
        );
    }

    /// What a maker could not set is warned of even when what it met stops
    /// the transfer, and before.
    #[test]
    fn what_could_not_be_set_is_warned_of_even_when_the_transfer_stops() {
        let mut decider = Decider::new(true, 1_000);
        let part = compared(
            b"",
            vec![entry(1, b"l", Kind::Fifo, 0)],
            vec![Verdict::Content],
            true,
        );
        assert_eq!(on(&mut decider, Event::Compared(part)), ["make 0 "]);
        let made = Event::Made(Made {
            job: 0,
            unopened: None,
            errors: vec![Unmade {
                id: 1,
                path: b"l".to_vec(),
                directory: false,
                error: io::Error::from(Errno::ROFS).into(),
            }],
            warned: vec![(super::super::warnings::Warning::Owner, b"l".to_vec())],
        });
        let said = on(&mut decider, made);
        assert!(
            matches!(&said[..], [warn, stop] if warn == "warn 1" && stop.starts_with("stop: cannot copy l: ")),
            "{said:?}"
        );
    }

    /// The directories handed to the finishers, in order, of the commands
    /// given, which are let go of.
    fn to_finish(commands: &mut Vec<Command>) -> Vec<Vec<u8>> {
        commands
            .drain(..)
            .filter_map(|command| match command {
                Command::Finish(path, _) => Some(path),
                _ => None,
            })
            .collect()
    }

    /// A file needed in `directory`, of ID `id`.
    fn file(id: FileId, directory: &[u8]) -> Target {
        Target {
            directory: Arc::from(directory),
            entry: entry(id, b"f", Kind::File, 1),
            is_root: false,
            exists: false,
        }
    }

    /// Directories are done as the manifest leaves them, and all their files
    /// are settled, deepest first; one never listed, once its parent is
    /// left; one to be let go, without being finished. Each is finished
    /// once those under it are. Nothing stays open.
    #[test]
    fn directories_are_finished_as_they_are_done_deepest_first() {
        let mut directories = Directories::default();
        let mut commands = Vec::new();
        let shown = |paths: Vec<Vec<u8>>| -> Vec<String> {
            paths
                .into_iter()
                .map(|path| String::from_utf8(path).unwrap())
                .collect()
        };
        // The root, with a/ and b/ in it; a/ with a file and c/; b/ never
        // listed.
        directories.announce(Vec::new(), directory(0, b""));
        directories.mark_to_finish(b"");
        directories.enter(b"", &mut commands);
        directories.announce(b"a".to_vec(), directory(1, b"a"));
        directories.announce(b"b".to_vec(), directory(2, b"b"));
        directories.mark_to_finish(b"a");
        directories.mark_to_finish(b"b");
        directories.enter(b"a", &mut commands);
        directories.need(file(9, b"a"), &mut commands);
        directories.announce(b"a/c".to_vec(), directory(3, b"c"));
        directories.enter(b"a/c", &mut commands);
        // Left, nothing under it, nothing to finish: let go.
        directories.enter(b"a/d-not-announced", &mut commands);
        assert_eq!(shown(to_finish(&mut commands)), Vec::<String>::new());
        assert!(!directories.open.contains_key(b"a/c".as_slice()));
        // The manifest is over; a/ waits for its file.
        directories.leave_all(&mut commands);
        assert_eq!(shown(to_finish(&mut commands)), ["b"]);
        assert!(directories.open.contains_key(b"a".as_slice()));
        assert!(directories.open.contains_key(b"".as_slice()));
        directories.settle(9, &mut commands);
        // The root is done too, but waits for a/ and b/ to be finished.
        assert_eq!(shown(to_finish(&mut commands)), ["a"]);
        directories.finished(b"b", &mut commands);
        assert_eq!(shown(to_finish(&mut commands)), Vec::<String>::new());
        directories.finished(b"a", &mut commands);
        assert_eq!(shown(to_finish(&mut commands)), [""]);
        directories.finished(b"", &mut commands);
        assert!(
            directories.open.is_empty() && directories.all_finished(),
            "{:?}",
            directories.open.keys().collect::<Vec<_>>()
        );
    }

    /// A directory whose needed file the Source could not open is finished
    /// once it says so, not at the end; an ID it does not wait for, or
    /// said again, is nothing.
    #[test]
    fn a_directory_is_finished_once_its_file_the_source_could_not_open_is_settled() {
        let mut directories = Directories::default();
        let mut commands = Vec::new();
        directories.announce(Vec::new(), directory(0, b""));
        directories.enter(b"", &mut commands);
        directories.announce(b"a".to_vec(), directory(1, b"a"));
        directories.announce(b"b".to_vec(), directory(2, b"b"));
        directories.mark_to_finish(b"a");
        directories.enter(b"a", &mut commands);
        directories.need(file(3, b"a"), &mut commands);
        // The manifest has left a/, which waits for its file.
        directories.enter(b"b", &mut commands);
        directories.settle(99, &mut commands);
        assert!(to_finish(&mut commands).is_empty());
        directories.settle(3, &mut commands);
        assert_eq!(to_finish(&mut commands), [b"a".to_vec()]);
        directories.settle(3, &mut commands);
        assert!(to_finish(&mut commands).is_empty());
    }

    /// A part's entry is found by its ID at once, however large the part:
    /// one where every entry failed is reported in time.
    #[test]
    fn an_entry_is_found_by_its_id_at_once() {
        let entries: Vec<Entry> = (0..10_000)
            .map(|index| entry(5 + index, b"f", Kind::File, 0))
            .collect();
        assert_eq!(by_id(&entries, 5).map(|entry| entry.id), Some(5));
        assert_eq!(by_id(&entries, 10_004).map(|entry| entry.id), Some(10_004));
        assert!(by_id(&entries, 4).is_none());
        assert!(by_id(&entries, 10_005).is_none());
        assert!(by_id(&[], 5).is_none());
    }

    /// Every order of `count` things, as their indices.
    fn orders(count: usize) -> Vec<Vec<usize>> {
        if count == 0 {
            return vec![Vec::new()];
        }
        let mut all = Vec::new();
        for order in orders(count - 1) {
            for at in 0..=order.len() {
                let mut order = order.clone();
                order.insert(at, count - 1);
                all.push(order);
            }
        }
        all
    }

    /// A directory is done once its parts are made, its files settled, the
    /// manifest has left it, and every directory in it is done, in whatever
    /// order those come: one in it, never seen, once its own part is made
    /// and the manifest has left its parent, and before it; each is
    /// finished once those under it are; the root last.
    #[test]
    fn a_directory_is_finished_once_all_in_it_is_done_in_any_order() {
        for order in orders(4) {
            let mut directories = Directories::default();
            let mut commands = Vec::new();
            directories.announce(Vec::new(), directory(0, b""));
            directories.mark_to_finish(b"");
            directories.enter(b"", &mut commands);
            directories.announce(b"a".to_vec(), directory(1, b"a"));
            directories.enter(b"a", &mut commands);
            directories.mark_to_finish(b"a");
            // A part of `a` given to the makers, which is to make `a/b`, whose
            // own part is given to them too, and needs a file in `a`.
            directories.job_started(b"a");
            directories.announce(b"a/b".to_vec(), directory(2, b"b"));
            directories.mark_to_finish(b"a/b");
            directories.job_started(b"a/b");
            directories.need(file(3, b"a"), &mut commands);
            commands.clear();
            let mut happened = [false; 4];
            for &event in &order {
                match event {
                    0 => directories.job_done(b"a", &mut commands),
                    1 => directories.settle(3, &mut commands),
                    2 => directories.job_done(b"a/b", &mut commands),
                    _ => directories.leave(b"a".to_vec(), &mut commands),
                }
                happened[event] = true;
                let mut expected = Vec::new();
                if (event == 2 || event == 3) && happened[2] && happened[3] {
                    expected.push(b"a/b".to_vec());
                }
                assert_eq!(to_finish(&mut commands), expected, "{order:?}, at {event}");
            }
            // Done, `a` waits for `a/b` to be finished.
            directories.finished(b"a/b", &mut commands);
            assert_eq!(to_finish(&mut commands), [b"a".to_vec()], "{order:?}");
            directories.leave_all(&mut commands);
            assert!(to_finish(&mut commands).is_empty(), "{order:?}");
            directories.finished(b"a", &mut commands);
            assert_eq!(to_finish(&mut commands), [b"".to_vec()], "{order:?}");
        }
    }

    /// What is still open once everything is written, as a directory whose
    /// file never came, is finished all the same, deepest first, each once
    /// those under it are.
    #[test]
    fn what_is_still_open_at_the_end_is_finished_deepest_first() {
        let mut directories = Directories::default();
        let mut commands = Vec::new();
        directories.announce(Vec::new(), directory(0, b""));
        directories.mark_to_finish(b"");
        for (path, id) in [(&b"a"[..], 1), (b"a/b", 2), (b"a/b/c", 3)] {
            directories.announce(path.to_vec(), directory(id, b"x"));
            directories.enter(path, &mut commands);
            directories.mark_to_finish(path);
        }
        directories.need(file(10, b"a/b/c"), &mut commands);
        directories.need(file(11, b"a"), &mut commands);
        directories.leave_all(&mut commands);
        directories.settle(10, &mut commands);
        // `a/b` is done too, but waits for `a/b/c` to be finished.
        assert_eq!(to_finish(&mut commands), [b"a/b/c".to_vec()]);
        assert!(directories.is_needed(11));
        directories.finish_the_rest(&mut commands);
        assert!(to_finish(&mut commands).is_empty());
        assert!(directories.open.is_empty());
        for (finished, next) in [(&b"a/b/c"[..], &b"a/b"[..]), (b"a/b", b"a"), (b"a", b"")] {
            directories.finished(finished, &mut commands);
            assert_eq!(to_finish(&mut commands), [next.to_vec()]);
        }
        assert!(!directories.all_finished());
        directories.finished(b"", &mut commands);
        assert!(directories.all_finished());
    }

    /// A part waits for its directory to be made, and its extras deleted,
    /// and goes to the makers once both are, and only once.
    #[test]
    fn a_part_waits_for_its_directory_made_and_its_extras_deleted() {
        let part = Part {
            path: b"d/e".to_vec(),
            directory: None,
            entries: Vec::new(),
            verdicts: Vec::new(),
            present: Vec::new(),
        };
        let mut makers = Makers {
            next: 2,
            parts: HashMap::from([(1, Arc::new(part))]),
            making: HashSet::from([b"d".to_vec()]),
            deleting: HashSet::from([b"d".to_vec()]),
            parked: HashMap::from([(b"d".to_vec(), vec![1])]),
            unasked: BTreeSet::new(),
        };
        let mut commands = Vec::new();
        makers.making.remove(&b"d"[..]);
        makers.release(b"d", &mut commands);
        assert!(commands.is_empty(), "given out before its extras went");
        makers.deleting.remove(&b"d"[..]);
        makers.release(b"d", &mut commands);
        assert_eq!(said(&commands), ["make 1 d/e"]);
        commands.clear();
        makers.release(b"d", &mut commands);
        assert!(commands.is_empty(), "given out twice");
    }

    /// What a maker met is for the decider to say: a deletion refused, to
    /// protect the Source's root, stops the transfer, but in making a
    /// directory fails it alone; a full drive stops it, said as the first
    /// that stops it; anything else fails its entry alone; and a directory
    /// that could not be opened fails all it was to change.
    #[test]
    fn what_a_maker_met_is_said_by_the_decider() {
        let entries = [
            entry(1, b"d", Kind::Directory, 0),
            entry(2, b"l", Kind::Fifo, 0),
            entry(3, b"same", Kind::Fifo, 0),
        ];
        let verdicts = [Verdict::Content, Verdict::Content, Verdict::Same];
        let protected = || anyhow::Error::from(io::Error::other(fsys::Protected));
        let made = |errors| Made {
            job: 0,
            unopened: None,
            errors,
            warned: Vec::new(),
        };
        let unmade = |id, path: &[u8], directory, error| Unmade {
            id,
            path: path.to_vec(),
            directory,
            error,
        };
        let ids =
            |failed: &[(FileId, String)]| failed.iter().map(|(id, _)| *id).collect::<Vec<_>>();

        let failed = what_failed(
            &made(vec![unmade(1, b"d", true, protected())]),
            &entries,
            &verdicts,
        )
        .unwrap();
        assert_eq!(ids(&failed), [1]);

        let error = what_failed(
            &made(vec![unmade(2, b"l", false, protected())]),
            &entries,
            &verdicts,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("will not delete l/ to make l: "),
            "{error:#}"
        );

        let full = anyhow::Error::from(io::Error::from(Errno::NOSPC));
        let error = what_failed(
            &made(vec![
                unmade(1, b"d", true, anyhow::anyhow!("odd")),
                unmade(2, b"l", false, full),
            ]),
            &entries,
            &verdicts,
        )
        .unwrap_err();
        assert!(
            error.to_string().starts_with("cannot copy l: "),
            "{error:#}"
        );

        let failed = what_failed(
            &made(vec![unmade(2, b"l", false, anyhow::anyhow!("odd"))]),
            &entries,
            &verdicts,
        )
        .unwrap();
        assert_eq!(failed, [(2, "odd".to_string())]);

        let unopened = Made {
            job: 0,
            unopened: Some(io::Error::from(Errno::ACCESS)),
            errors: Vec::new(),
            warned: Vec::new(),
        };
        let failed = what_failed(&unopened, &entries, &verdicts).unwrap();
        assert_eq!(ids(&failed), [1, 2]);
        assert!(
            failed[0].1.starts_with("cannot open its directory: "),
            "{failed:?}"
        );
    }

    /// What a writer met is for the decider to say too: the root with
    /// nowhere to go, something to be protected in the way, and a full drive
    /// stop the transfer; anything else fails the file alone.
    #[test]
    fn what_a_writer_met_is_said_by_the_decider() {
        let missing = MissingParent("the directory d does not exist".to_owned());
        let error = why_failed(b"d/f", missing.into()).unwrap_err();
        assert_eq!(error.to_string(), "the directory d does not exist");
        let protected = anyhow::Error::from(io::Error::other(fsys::Protected));
        let error = why_failed(b"f", protected).unwrap_err();
        assert_eq!(error.to_string(), "will not delete f/ to make f");
        let full = anyhow::Error::from(io::Error::from(Errno::NOSPC));
        let error = why_failed(b"f", full).unwrap_err();
        assert_eq!(error.to_string(), "write f");
        assert_eq!(why_failed(b"f", anyhow::anyhow!("odd")).unwrap(), "odd");
    }
}
