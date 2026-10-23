//! The applier: applies what the comparison found, as it comes. What a
//! directory has extra is dealt with first, then its entries are made to
//! match, and the files that need content are asked for. It alone follows
//! each directory's progress, from announced to done, after which it is
//! finished, as the other stages tell it what became of each file.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};
use rustix::fs::FileType;
use rustix::io::Errno;

use crate::compare::{Comparer, Extras, Verdict, Verdicts};
use crate::dirs::DirCache;
use crate::fsys;
use crate::interrupt;
use crate::manifest::{self, Entry, FileId, Kind, Timestamp};
use crate::mux::Mux;
use crate::owners::Ownership;
use crate::profile;
use crate::protocol::{Needs, Results};
use crate::remove;
use crate::report::{Count, Listed, Reporter};
use crate::source::display;
use crate::stop::{Stop, Stopped};

use super::receive::Compared;
use super::warnings::Warning;
use super::write::Target;
use super::{
    Options, State, ends_the_transfer, io_ends_the_transfer, is_protected, missing_parent,
    root_parent, tell,
};

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
    /// Its parts being made by the makers, or waiting for it to be made.
    pub(super) jobs: usize,
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

/// What the applier alone keeps: the directories not finished, and the
/// files they wait for.
pub(super) struct Directories {
    /// The directories announced and not finished yet, by path; the root's
    /// is empty. See [`Open`].
    pub(super) open: HashMap<Vec<u8>, Open>,
    /// The directories whose parts were applied, as the manifest went into
    /// them, depth first: each one's parent is below it.
    stack: Vec<Vec<u8>>,
    /// Files whose content is coming, by ID: the directory each is in, for
    /// it to wait for them, or `None` for the root itself.
    needed: HashMap<FileId, Option<Arc<[u8]>>>,
    /// Where a directory done goes to be finished, in the order it is done.
    finishing: Option<crossbeam_channel::Sender<(Vec<u8>, Entry)>>,
    /// Where each needed file's target goes to the writing, before the file
    /// is asked for; let go once every one is sent.
    targets: Option<Sender<(FileId, Target)>>,
}

impl Directories {
    /// Directories that go to `finishing` once done, whose files' targets
    /// go to the writing on `targets`; none of either in a dry run.
    pub(super) fn new(
        finishing: Option<crossbeam_channel::Sender<(Vec<u8>, Entry)>>,
        targets: Option<Sender<(FileId, Target)>>,
    ) -> Self {
        Self {
            open: HashMap::new(),
            stack: Vec::new(),
            needed: HashMap::new(),
            finishing,
            targets,
        }
    }

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
    fn job_done(&mut self, path: &[u8]) {
        if let Some(open) = self.open.get_mut(path) {
            open.jobs = open.jobs.saturating_sub(1);
        }
        self.done_if_so(path.to_vec());
    }

    /// The directory at `path` is to be finished, once done.
    pub(super) fn mark_to_finish(&mut self, path: &[u8]) {
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
            let done = self.open.get(&path).is_some_and(|open| {
                open.left && open.files == 0 && open.children == 0 && open.jobs == 0
            });
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

    /// A file is needed: its directory waits for it, and its target goes to
    /// the writing, before the file is asked for, so before its bytes can
    /// come.
    pub(super) fn need(&mut self, target: Target) {
        let id = target.entry.id;
        let directory = (!target.is_root).then(|| Arc::clone(&target.directory));
        if let Some(directory) = &directory {
            self.file_needed(directory);
        }
        self.needed.insert(id, directory);
        if let Some(targets) = &self.targets {
            let _ = targets.send((id, target));
        }
    }

    /// A needed file is settled, written or failed, or the Source said it
    /// could not open it: its directory has one fewer to wait for. One not
    /// waited for, or said again, is nothing.
    pub(super) fn settle(&mut self, id: FileId) {
        if let Some(Some(directory)) = self.needed.remove(&id) {
            self.file_settled(&directory);
        }
    }

    /// Whether a file is needed, and not settled yet.
    pub(super) fn is_needed(&self, id: FileId) -> bool {
        self.needed.contains_key(&id)
    }

    /// Every target is sent: the writing need wait for none more.
    pub(super) fn every_target_sent(&mut self) {
        self.targets.take();
    }

    /// Finishes what is still open once everything is written, deepest
    /// first, should anything be: every directory is finished as it is
    /// done, but one with a file that never came would otherwise never be.
    /// Then lets the finisher end.
    pub(super) fn finish_the_rest(&mut self) {
        self.leave_all();
        let mut left: Vec<(Vec<u8>, Entry)> = self
            .open
            .drain()
            .filter(|(_, open)| open.finish)
            .map(|(path, open)| (path, open.entry))
            .collect();
        left.sort_by_key(|(path, _)| std::cmp::Reverse(depth(path)));
        if let Some(finishing) = self.finishing.take() {
            for directory in left {
                let _ = finishing.send(directory);
            }
        }
    }
}

/// How deep a path is: the root is 0.
pub(super) fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        1 + path.iter().filter(|&&byte| byte == b'/').count()
    }
}

/// What the other stages tell the applier.
pub(super) enum Event {
    /// The manifest's root, checked: applied before any part, as it came
    /// before them.
    Root(Entry),
    /// A needed file the Source said it could not open: it never comes.
    Unread(FileId),
    /// A needed file written, or failed.
    Settled(FileId),
    /// The Source sent every block, every file of them settled; these needed
    /// files, as shown, did not come.
    StreamEnded(Vec<(FileId, Vec<u8>)>),
}

/// What the applier works with, borrowed from the run.
pub(super) struct Applying<'a> {
    pub(super) mux: &'a Mux,
    pub(super) comparer: &'a Comparer,
    pub(super) root: &'a Path,
    pub(super) state: &'a State,
    pub(super) options: &'a Options,
    pub(super) reporter: &'a Reporter,
    pub(super) dirs: &'a DirCache,
    pub(super) stop: &'a Stop,
}

/// How many directories have their parts made at once, a directory each, as
/// in one the kernel makes one name at a time: as many as gained on the
/// filesystems measured; see [`crate::remove::DELETERS`].
const MAKERS: usize = 16;

/// A compared part to make match, but for its files' content: its
/// directories, symlinks and special files made, and its attributes set.
/// The applier gives it to the makers, and asks for its files once it is
/// made.
struct Part {
    path: Vec<u8>,
    directory: Option<Arc<OwnedFd>>,
    entries: Vec<Entry>,
    verdicts: Vec<Verdict>,
    present: Vec<bool>,
}

/// What a directory has extra, for `sync`, to delete on the deleting lane:
/// the directory as the comparison listed it, and the names in it the
/// Source could not read, whose aliases are kept.
struct Extra {
    path: Vec<u8>,
    directory: Arc<OwnedFd>,
    extras: Extras,
    unreadable: Vec<Vec<u8>>,
}

/// What became of a directory's extras on the deleting lane: those it went
/// to delete, and what became of each, and those kept as aliases of what
/// the Source could not read.
struct Deleted {
    path: Vec<u8>,
    extras: Extras,
    results: Vec<io::Result<()>>,
    kept: Vec<Vec<u8>>,
}

/// What a maker or the deleting lane did.
enum Done {
    Made(Made),
    Deleted(Deleted),
}

/// What became of a part a maker made match, for the applier to say what it
/// means: a maker only reports.
struct Made {
    job: u64,
    /// Its directory could not be opened, so nothing in it was made.
    unopened: Option<io::Error>,
    /// Each error met, in the order of the entries.
    errors: Vec<Unmade>,
    /// What could not be set.
    warned: Warned,
}

/// An entry a maker could not make match, and why.
struct Unmade {
    id: FileId,
    path: Vec<u8>,
    directory: bool,
    error: anyhow::Error,
}

/// The parts given to the makers, and those waiting for their directory to
/// be made, or its extras deleted; and the extras given to the deleting
/// lane: the applier alone follows them.
struct Makers {
    jobs: Sender<(u64, Arc<Part>)>,
    lane: Sender<Extra>,
    done: Receiver<Done>,
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
    fn release(&mut self, path: &[u8]) {
        if self.making.contains(path) || self.deleting.contains(path) {
            return;
        }
        for job in self.parked.remove(path).unwrap_or_default() {
            let _ = self.jobs.send((job, Arc::clone(&self.parts[&job])));
        }
    }
}

/// The applier: applies the root and each comparison, in the manifest's
/// order, as they come, and follows each directory until it is finished,
/// as the other stages say what became of each file. In a run, it gives
/// each part to the makers, [`MAKERS`] threads, a directory each, and goes
/// on; a part waits for its directory to be made, and its files are asked
/// for once it is made. `ended` says, once the comparisons end, whether the
/// manifest did.
pub(super) fn apply(
    directories: Directories,
    compared: Receiver<Compared>,
    events: Receiver<Event>,
    ended: &AtomicBool,
    applying: &Applying,
) -> Result<()> {
    if applying.options.dry_run {
        return follow(directories, compared, events, ended, applying, None);
    }
    std::thread::scope(|scope| {
        let (jobs, taken) = crossbeam_channel::unbounded::<(u64, Arc<Part>)>();
        let (lane, extras) = crossbeam_channel::unbounded::<Extra>();
        let (finished, done) = crossbeam_channel::unbounded::<Done>();
        for _ in 0..MAKERS {
            let taken = taken.clone();
            let finished = finished.clone();
            scope.spawn(move || {
                let _thread = profile::thread("make");
                while let Ok((job, part)) = profile::wait(profile::State::Work, || taken.recv()) {
                    let made = profile::wait(profile::State::Apply, || {
                        make_part(job, &part, applying.state, applying.dirs)
                    });
                    if finished.send(Done::Made(made)).is_err() {
                        break;
                    }
                }
            });
        }
        // The deleting lane: a directory's extras at a time, each over up
        // to remove::DELETERS threads.
        scope.spawn(move || {
            let _thread = profile::thread("delete");
            while let Ok(extra) = profile::wait(profile::State::Work, || extras.recv()) {
                let deleted = profile::wait(profile::State::Delete, || {
                    delete_on_lane(extra, applying.state, applying.stop)
                });
                if finished.send(Done::Deleted(deleted)).is_err() {
                    break;
                }
            }
        });
        let makers = Makers {
            jobs,
            lane,
            done,
            next: 0,
            parts: HashMap::new(),
            making: HashSet::new(),
            deleting: HashSet::new(),
            parked: HashMap::new(),
            unasked: BTreeSet::new(),
        };
        // Its end lets the makers go: the scope then waits for them.
        follow(directories, compared, events, ended, applying, Some(makers))
    })
}

/// The applier's loop, with the makers in a run, none in a dry run.
fn follow(
    mut directories: Directories,
    compared: Receiver<Compared>,
    events: Receiver<Event>,
    ended: &AtomicBool,
    applying: &Applying,
    mut makers: Option<Makers>,
) -> Result<()> {
    let Applying { stop, options, .. } = *applying;
    // The compared parts of a directory waiting for its last.
    let mut waiting: Vec<Compared> = Vec::new();
    // The last ID compared, and the last the Source was told it need keep
    // no more.
    let mut compared_to: FileId = 0;
    let mut through: FileId = 0;
    // Comparisons still to come, and, but in a dry run, the blocks.
    let mut parts = Some(compared);
    let mut stream = !options.dry_run;
    // The comparisons ended, and the needs end once every part is made.
    let mut needs_end = false;
    loop {
        // What the others say comes first: the root before any part.
        while let Ok(event) = events.try_recv() {
            on_event(event, &mut directories, &mut stream, applying)?;
        }
        if parts.is_none() && !stream {
            return Ok(());
        }
        let never = crossbeam_channel::never();
        let unmade = crossbeam_channel::never();
        let next = profile::wait(profile::State::Work, || {
            crossbeam_channel::select! {
                recv(events) -> event => event.map_or(Next::Gone, Next::Event),
                recv(parts.as_ref().unwrap_or(&never)) -> done => {
                    done.map_or(Next::PartsEnded, Next::Part)
                }
                recv(makers.as_ref().map_or(&unmade, |makers| &makers.done)) -> done => {
                    done.map_or(Next::Gone, Next::Done)
                }
                recv(stop.signal()) -> _ => Next::Gone,
            }
        });
        match next {
            Next::Event(event) => on_event(event, &mut directories, &mut stream, applying)?,
            Next::Part(done) => {
                // What was said before it was compared comes first.
                while let Ok(event) = events.try_recv() {
                    on_event(event, &mut directories, &mut stream, applying)?;
                }
                profile::wait(profile::State::Apply, || {
                    take_compared(
                        done,
                        &mut waiting,
                        &mut compared_to,
                        &mut directories,
                        makers.as_mut(),
                        applying,
                    )
                })?;
                tell_through(&mut through, compared_to, makers.as_ref(), applying)?;
            }
            Next::Done(done) => {
                let makers = makers.as_mut().expect("done by the makers or the lane");
                match done {
                    Done::Made(made) => on_made(made, &mut directories, makers, applying)?,
                    Done::Deleted(deleted) => {
                        on_deleted(deleted, &mut directories, makers, applying)?;
                    }
                }
                tell_through(&mut through, compared_to, Some(makers), applying)?;
                if needs_end && makers.idle() {
                    needs_end = false;
                    end_needs(&mut directories, applying)?;
                }
            }
            Next::PartsEnded => {
                parts = None;
                // Every comparison came out: the needs end, if the manifest
                // did; if not, or if they ended as the Destination stopped,
                // it is stopping.
                if !ended.load(Ordering::SeqCst) {
                    return Err(Stopped.into());
                }
                stop.check()?;
                if !options.dry_run {
                    directories.leave_all();
                }
                if makers.as_ref().is_none_or(Makers::idle) {
                    end_needs(&mut directories, applying)?;
                } else {
                    needs_end = true;
                }
            }
            // The others are gone, or the Destination stopped.
            Next::Gone => return Err(Stopped.into()),
        }
    }
}

/// Every file is asked for: the writing need wait for no more targets, and
/// the Source is told the needs end.
fn end_needs(directories: &mut Directories, applying: &Applying) -> Result<()> {
    directories.every_target_sent();
    applying.mux.message(&Needs::End)
}

/// Tells the Source, now and then, that every entry up to the last compared
/// is, and those needed asked for, but before the first of a part whose
/// files are not asked for yet.
fn tell_through(
    through: &mut FileId,
    compared_to: FileId,
    makers: Option<&Makers>,
    applying: &Applying,
) -> Result<()> {
    let asked = match makers.and_then(|makers| makers.unasked.first()) {
        Some(&first) => compared_to.min(first.saturating_sub(1)),
        None => compared_to,
    };
    if asked >= *through + crate::protocol::through_every(applying.options.in_flight_entries) {
        applying.mux.message(&Needs::Through(asked))?;
        *through = asked;
    }
    Ok(())
}

/// What the applier takes next.
enum Next {
    Event(Event),
    Part(Compared),
    Done(Done),
    PartsEnded,
    Gone,
}

/// Takes what another stage said.
fn on_event(
    event: Event,
    directories: &mut Directories,
    stream: &mut bool,
    applying: &Applying,
) -> Result<()> {
    match event {
        Event::Root(entry) => apply_the_root(&entry, directories, applying),
        Event::Unread(id) | Event::Settled(id) => {
            directories.settle(id);
            Ok(())
        }
        Event::StreamEnded(never) => {
            // A needed file that neither came nor was said not to open
            // never came, which fails it, not the transfer.
            for (id, shown) in never {
                if directories.is_needed(id) {
                    file_failed(applying, id, &shown, "the Source never sent it".to_owned())?;
                    directories.settle(id);
                }
            }
            directories.finish_the_rest();
            *stream = false;
            Ok(())
        }
    }
}

/// Applies the manifest's root, which comes before anything in it.
fn apply_the_root(entry: &Entry, directories: &mut Directories, applying: &Applying) -> Result<()> {
    let Applying {
        mux,
        comparer,
        root,
        state,
        options,
        reporter,
        stop,
        ..
    } = *applying;
    refuse_another_kind(root, entry)?;
    let verdict = comparer
        .root(entry, &state.ownership)
        .context("compare the destination's root")?;
    if entry.kind == Kind::Directory && !options.dry_run {
        directories.announce(Vec::new(), entry.clone());
    }
    if verdict.is_listed() && options.dry_run {
        reporter.listed(vec![Listed::to_copy(b".".to_vec())]);
    }
    if !options.dry_run {
        stop.check()?;
        apply_root(root, entry, verdict, state, directories)?;
    }
    reporter.add(Count::Entries, 1);
    if verdict == Verdict::Content && entry.kind == Kind::File {
        reporter.add(Count::Needed, 1);
        reporter.add(Count::ToSend, entry.size);
    }
    let (files, attributes) = split(std::slice::from_ref(entry), &[verdict]);
    send_needs(mux, files, attributes)
}

/// A file failed: counted, reported, and the Source told; nothing more is
/// said of one once the Destination has stopped, its failing a consequence.
pub(super) fn file_failed(
    applying: &Applying,
    id: FileId,
    shown: &[u8],
    message: String,
) -> Result<()> {
    file_failed_in(
        applying.state,
        applying.reporter,
        applying.mux,
        applying.stop,
        id,
        shown,
        message,
    )
}

/// As [`file_failed`], with what it needs given apart.
pub(super) fn file_failed_in(
    state: &State,
    reporter: &Reporter,
    mux: &Mux,
    stop: &Stop,
    id: FileId,
    shown: &[u8],
    message: String,
) -> Result<()> {
    if stop.is_stopped() {
        return Ok(());
    }
    state.failures.fetch_add(1, Ordering::Relaxed);
    reporter.add(Count::Failed, 1);
    tell(
        reporter,
        format_args!("cannot copy {}: {message}", String::from_utf8_lossy(shown)),
    );
    mux.message(&Results::Failed { id, message })
}

pub(super) fn send_needs(mux: &Mux, files: Vec<FileId>, attributes: Vec<FileId>) -> Result<()> {
    if !files.is_empty() {
        mux.message(&Needs::Files(files))?;
    }
    if !attributes.is_empty() {
        mux.message(&Needs::Attributes(attributes))?;
    }
    Ok(())
}

/// Takes a part as it is compared. A directory's parts wait for its last,
/// as rsync's do: then, for `sync`, what it has extra is dealt with first,
/// and each part after, so that nothing made for it is ever taken for
/// extra, as it would be on a filesystem that takes two names for one, such
/// as one that ignores case. `copy` deletes nothing, and goes the same way.
fn take_compared(
    done: Compared,
    waiting: &mut Vec<Compared>,
    compared_to: &mut FileId,
    directories: &mut Directories,
    mut makers: Option<&mut Makers>,
    applying: &Applying,
) -> Result<()> {
    let Applying { options, stop, .. } = *applying;
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
        // What is under the root is reached from it, as the comparison
        // opened it.
        applying.dirs.hold_root(Arc::clone(directory));
        // With `-x`, it is the filesystem nothing is deleted off.
        #[allow(clippy::unnecessary_cast)]
        if applying.state.one_file_system
            && let Ok(stat) = rustix::fs::fstat(&**directory)
        {
            let _ = applying.state.keep.device.set(stat.st_dev as u64);
        }
    }
    let last = done.last;
    waiting.push(done);
    if !last {
        return Ok(());
    }
    // Only a directory's last part comes with its extras, and only for `sync`.
    if let Some(extras) = waiting.last_mut().and_then(|done| done.extras.take()) {
        let path = waiting[0].path.clone();
        let directory = waiting.last().and_then(|done| done.directory.clone());
        stop.check()?;
        apply_extras(
            &path,
            directory,
            extras,
            waiting,
            directories,
            makers.as_deref_mut(),
            applying,
        )?;
    }
    if !options.dry_run
        && let Some(done) = waiting.first()
    {
        directories.enter(&done.path);
        if waiting.iter().any(|done| done.let_in.is_some()) {
            directories.mark_to_finish(&done.path);
        }
    }
    let last = waiting
        .iter()
        .rev()
        .find_map(|done| done.entries.last())
        .map(|entry| entry.id);
    for done in waiting.drain(..) {
        stop.check()?;
        match &mut makers {
            Some(makers) => take_part(done, directories, makers, applying)?,
            None => list_compared(done, applying)?,
        }
    }
    // Every entry up to its last is compared: the Source may let go of
    // those whose files are asked for, told now and then.
    if let Some(last) = last {
        *compared_to = last;
    }
    Ok(())
}

/// Deals with what the directory at `path` has that the Source does not,
/// before anything of its parts: lists it, in a dry run, or gives it to the
/// deleting lane, which its parts then wait for. The Source is not told:
/// however many there are, nothing about them goes in a message.
fn apply_extras(
    path: &[u8],
    directory: Option<Arc<OwnedFd>>,
    extras: io::Result<Extras>,
    parts: &[Compared],
    directories: &mut Directories,
    makers: Option<&mut Makers>,
    applying: &Applying,
) -> Result<()> {
    let Applying {
        mux,
        state,
        reporter,
        ..
    } = *applying;
    let extras = match extras {
        Ok(extras) => extras,
        Err(error) => {
            state.failures.fetch_add(1, Ordering::Relaxed);
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
            return Ok(());
        }
    };
    // Extras come only from a listing, so only from a directory held.
    let Some(directory) = directory else {
        return Ok(());
    };
    let unreadable: Vec<Vec<u8>> = parts
        .iter()
        .flat_map(|done| &done.entries)
        .filter(|entry| entry.kind == Kind::Unreadable)
        .map(|entry| entry.name.clone())
        .collect();
    let Some(makers) = makers else {
        let extras = on_this_filesystem(&directory, extras, &state.keep);
        let (extras, kept) = without_aliases_of_unreadable(&directory, &unreadable, extras);
        tell_kept(path, &kept, reporter);
        reporter.listed(
            extras
                .iter()
                .map(|(name, is_directory)| {
                    Listed::to_delete(display(&manifest::join(path, name), *is_directory))
                })
                .collect(),
        );
        return Ok(());
    };
    if extras.is_empty() {
        return Ok(());
    }
    makers.deleting.insert(path.to_vec());
    directories.job_started(path);
    let _ = makers.lane.send(Extra {
        path: path.to_vec(),
        directory,
        extras,
        unreadable,
    });
    Ok(())
}

/// Deletes a directory's extras, on the deleting lane: first leaving out,
/// with `-x`, what is on another filesystem, and what is the Source could
/// not read under another name.
fn delete_on_lane(extra: Extra, state: &State, stop: &Stop) -> Deleted {
    let Extra {
        path,
        directory,
        extras,
        unreadable,
    } = extra;
    let extras = on_this_filesystem(&directory, extras, &state.keep);
    let (extras, kept) = without_aliases_of_unreadable(&directory, &unreadable, extras);
    let results = delete_extras(&directory, &extras, &state.keep, stop);
    Deleted {
        path,
        extras,
        results,
        kept,
    }
}

/// Takes a directory's extras, deleted on the lane: tells what became of
/// them, marks the directory to be finished if any went, and gives out its
/// parts, which waited.
fn on_deleted(
    deleted: Deleted,
    directories: &mut Directories,
    makers: &mut Makers,
    applying: &Applying,
) -> Result<()> {
    let Deleted {
        path,
        extras,
        results,
        kept,
    } = deleted;
    tell_kept(&path, &kept, applying.reporter);
    if tell_deleted(
        &path,
        &extras,
        results,
        applying.state,
        applying.mux,
        applying.stop,
    )? {
        // Deleting changes the directory's mtime, which is set again.
        directories.mark_to_finish(&path);
    }
    makers.deleting.remove(&path);
    makers.release(&path);
    directories.job_done(&path);
    Ok(())
}

/// The extras, but, with `-x`, none on another filesystem than the root's:
/// as rsync's `-x` does, nothing is deleted in a directory there, and no
/// directory there is deleted, a mount point, and neither is said.
fn on_this_filesystem(directory: &OwnedFd, mut extras: Extras, keep: &fsys::Keep) -> Extras {
    if keep.device.get().is_none() || extras.is_empty() {
        return extras;
    }
    if rustix::fs::fstat(directory).is_ok_and(|stat| keep.is_elsewhere(&stat)) {
        return Vec::new();
    }
    extras.retain(|(name, is_directory)| {
        !*is_directory || !fsys::stat(directory, name).is_ok_and(|stat| keep.is_elsewhere(&stat))
    });
    extras
}

/// The extras, but those that are what the Source could not read, under
/// another name the filesystem takes for the same, as one that ignores case
/// does, which are returned apart: what it could not read is never deleted.
fn without_aliases_of_unreadable(
    directory: &OwnedFd,
    unreadable: &[Vec<u8>],
    mut extras: Extras,
) -> (Extras, Vec<Vec<u8>>) {
    if unreadable.is_empty() || extras.is_empty() {
        return (extras, Vec::new());
    }
    #[allow(clippy::unnecessary_cast)]
    let identity = |stat: &rustix::fs::Stat| (stat.st_dev as u64, stat.st_ino as u64);
    let same: HashSet<(u64, u64)> = unreadable
        .iter()
        .filter_map(|name| fsys::stat(directory, name).ok())
        .map(|stat| identity(&stat))
        .collect();
    let mut kept = Vec::new();
    extras.retain(|(name, _)| {
        let aliased = fsys::stat(directory, name).is_ok_and(|stat| same.contains(&identity(&stat)));
        if aliased {
            kept.push(name.clone());
        }
        !aliased
    });
    (extras, kept)
}

/// Warns of each extra kept as what the Source could not read, under
/// another name.
fn tell_kept(path: &[u8], kept: &[Vec<u8>], reporter: &Reporter) {
    for name in kept {
        tell(
            reporter,
            format_args!(
                "warning: not deleting {}: it is what the source could not read, under \
                 another name",
                String::from_utf8_lossy(&display(&manifest::join(path, name), false))
            ),
        );
    }
}

/// Lists what a compared part would change, in a dry run, tells what could
/// not be compared, and asks for what it needs; its directory's extras are
/// dealt with apart, by [`apply_extras`].
fn list_compared(done: Compared, applying: &Applying) -> Result<()> {
    let Applying {
        options, reporter, ..
    } = *applying;
    let Compared {
        path,
        entries,
        directory,
        verdicts,
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
        if options.delete
            && *verdict == Verdict::Content
            && entry.kind != Kind::Directory
            && let Some(directory) = &directory
            && fsys::stat(&**directory, &entry.name)
                .is_ok_and(|stat| fsys::file_type(&stat) == FileType::Directory)
        {
            listed.push(Listed::to_delete(display(&child, true)));
        }
        listed.push(Listed::to_copy(display(
            &child,
            entry.kind == Kind::Directory,
        )));
    }
    let failed = tell_uncompared(&path, &entries, uncompared, unopened.as_deref(), applying)?;
    reporter.listed(listed);
    ask_for(&entries, &verdicts, &failed, applying)
}

/// Tells what of a part could not be compared, each failing alone, and
/// left as it is; a directory that could not be opened is told once.
/// Returns their IDs.
fn tell_uncompared(
    path: &[u8],
    entries: &[Entry],
    uncompared: Vec<(FileId, String)>,
    unopened: Option<&str>,
    applying: &Applying,
) -> Result<HashSet<FileId>> {
    if let Some(error) = unopened {
        tell(
            applying.reporter,
            format_args!(
                "cannot compare in {}: {error}",
                String::from_utf8_lossy(&display(path, true))
            ),
        );
    }
    tell_failed(path, entries, &uncompared, unopened.is_none(), applying)?;
    Ok(uncompared.into_iter().map(|(id, _)| id).collect())
}

/// Counts and tells each entry of a part that failed, and why, telling the
/// Source too; each is said only with `say`.
fn tell_failed(
    path: &[u8],
    entries: &[Entry],
    failures: &[(FileId, String)],
    say: bool,
    applying: &Applying,
) -> Result<()> {
    let Applying {
        mux,
        state,
        reporter,
        ..
    } = *applying;
    state
        .failures
        .fetch_add(failures.len() as u64, Ordering::Relaxed);
    reporter.add(Count::Failed, failures.len() as u64);
    for (id, message) in failures {
        if say && let Some(entry) = by_id(entries, *id) {
            let child = manifest::join(path, &entry.name);
            tell(
                reporter,
                format_args!(
                    "cannot copy {}: {message}",
                    String::from_utf8_lossy(&display(&child, entry.kind == Kind::Directory))
                ),
            );
        }
        mux.message(&Results::Failed {
            id: *id,
            message: message.clone(),
        })?;
    }
    Ok(())
}

/// Asks the Source for what a part needs, but what failed: it has nowhere
/// to go.
fn ask_for(
    entries: &[Entry],
    verdicts: &[Verdict],
    failed: &HashSet<FileId>,
    applying: &Applying,
) -> Result<()> {
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
    applying.reporter.add(Count::Needed, needed.len() as u64);
    applying.reporter.add(Count::ToSend, needed.iter().sum());
    send_needs(applying.mux, files, attributes)
}

/// Takes a compared part, in a run: announces its directories, tells what
/// could not be compared, and gives the rest to the makers, unless its
/// directory is still to be made, when it waits for that.
fn take_part(
    done: Compared,
    directories: &mut Directories,
    makers: &mut Makers,
    applying: &Applying,
) -> Result<()> {
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
            directories.announce(manifest::join(&path, &entry.name), entry.clone());
        }
    }
    tell_uncompared(&path, &entries, uncompared, unopened.as_deref(), applying)?;
    if verdicts.iter().all(|&verdict| verdict == Verdict::Same) {
        return Ok(());
    }
    // The directories it makes, or changes: none is done before it is made,
    // as only then is it known whether each is to be finished; and the parts
    // of one it makes wait for it.
    for (entry, &verdict) in entries.iter().zip(&verdicts) {
        if entry.kind == Kind::Directory && verdict != Verdict::Same {
            let child = manifest::join(&path, &entry.name);
            directories.job_started(&child);
            if verdict == Verdict::Content {
                makers.making.insert(child);
            }
        }
    }
    let job = makers.next;
    makers.next += 1;
    if let Some(first) = entries.first() {
        makers.unasked.insert(first.id);
    }
    directories.job_started(&path);
    let waits = makers.making.contains(&path) || makers.deleting.contains(&path);
    let part = Arc::new(Part {
        path,
        directory,
        entries,
        verdicts,
        present,
    });
    makers.parts.insert(job, Arc::clone(&part));
    if waits {
        makers
            .parked
            .entry(part.path.clone())
            .or_default()
            .push(job);
    } else {
        let _ = makers.jobs.send((job, part));
    }
    Ok(())
}

/// Takes a part a maker made: tells what could not be made, marks what is
/// to be finished, asks for its files, which have their directory now, and
/// gives out the parts that waited for the directories it made, or failed
/// to.
fn on_made(
    made: Made,
    directories: &mut Directories,
    makers: &mut Makers,
    applying: &Applying,
) -> Result<()> {
    let part = makers.parts.remove(&made.job).expect("a part given out");
    let Part {
        path,
        entries,
        verdicts,
        present,
        ..
    } = &*part;
    // What could not be set is said even if the transfer stops.
    let failures = what_failed(&made, entries, verdicts);
    applying.state.warn(made.warned);
    let failures = failures?;
    tell_failed(path, entries, &failures, true, applying)?;
    let failed: HashSet<FileId> = failures.iter().map(|(id, _)| *id).collect();
    if made.unopened.is_none() {
        // Creating or replacing anything in a directory changes its mtime.
        if verdicts.contains(&Verdict::Content) {
            directories.mark_to_finish(path);
        }
        for (entry, &verdict) in entries.iter().zip(verdicts) {
            if entry.kind == Kind::Directory
                && verdict != Verdict::Same
                && !failed.contains(&entry.id)
            {
                directories.mark_to_finish(&manifest::join(path, &entry.name));
            }
        }
    }
    // The directory's path, as its files' targets share it.
    let mut shared: Option<Arc<[u8]>> = None;
    for ((entry, &verdict), &exists) in entries.iter().zip(verdicts).zip(present) {
        if verdict == Verdict::Content && entry.kind == Kind::File && !failed.contains(&entry.id) {
            let directory = shared.get_or_insert_with(|| Arc::from(path.as_slice()));
            directories.need(Target {
                directory: Arc::clone(directory),
                entry: entry.clone(),
                is_root: false,
                exists,
            });
        }
    }
    ask_for(entries, verdicts, &failed, applying)?;
    for (entry, &verdict) in entries.iter().zip(verdicts) {
        if entry.kind == Kind::Directory && verdict != Verdict::Same {
            let child = manifest::join(path, &entry.name);
            if verdict == Verdict::Content {
                makers.making.remove(&child);
                makers.release(&child);
            }
            directories.job_done(&child);
        }
    }
    if let Some(first) = entries.first() {
        makers.unasked.remove(&first.id);
    }
    directories.job_done(path);
    Ok(())
}

/// The entry of a part with `id`, found at once: the manifest is checked to
/// number a part's entries in turn.
fn by_id(entries: &[Entry], id: FileId) -> Option<&Entry> {
    let first = entries.first()?.id;
    let entry = entries.get(usize::try_from(id.checked_sub(first)?).ok()?)?;
    (entry.id == id).then_some(entry)
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

fn apply_root(
    root: &Path,
    entry: &Entry,
    verdict: Verdict,
    state: &State,
    directories: &mut Directories,
) -> Result<()> {
    match (verdict, &entry.kind) {
        (Verdict::Same, _) => Ok(()),
        (Verdict::Content, Kind::Directory) => {
            match std::fs::symlink_metadata(root) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => bail!("{} exists and is not a directory", root.display()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    std::fs::create_dir(root).map_err(|error| missing_parent(root, error))?;
                    state.root_made.store(true, Ordering::Relaxed);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("inspect {}", root.display()));
                }
            }
            directories.mark_to_finish(b"");
            Ok(())
        }
        (_, Kind::Directory) => {
            directories.mark_to_finish(b"");
            Ok(())
        }
        (Verdict::Content, Kind::File) => {
            directories.need(Target {
                directory: Arc::from(&b""[..]),
                entry: entry.clone(),
                is_root: true,
                // Its verdict does not say whether it is missing.
                exists: true,
            });
            Ok(())
        }
        _ => {
            let (directory, name) = root_parent(root)?;
            let shown = root.as_os_str().as_encoded_bytes();
            let mut warned = Vec::new();
            let applied = apply_entry(
                &directory,
                &name,
                shown,
                entry,
                verdict,
                true,
                state,
                &mut warned,
            );
            state.warn(warned);
            applied
        }
    }
}

/// Makes a part match, as a maker does: its directories, symlinks and
/// special files made, and attributes set, one at a time, as the
/// directory's lock allows no more; its files are asked for by the applier.
/// Returns what became of it, for the applier to say what it means: whether
/// its directory could be opened, each error met, and what could not be set.
fn make_part(job: u64, part: &Part, state: &State, dirs: &DirCache) -> Made {
    let Part {
        path,
        directory: listed,
        entries,
        verdicts,
        present,
    } = part;
    // The directory as the comparison listed it; one it did not find, made
    // since by its parent, from the root.
    let opened = match listed {
        Some(listed) => rustix::io::dup(&**listed).map_err(io::Error::from),
        None => dirs
            .open(path)
            .and_then(|directory| Ok(rustix::io::dup(&*directory)?)),
    };
    let directory = match opened {
        Ok(directory) => directory,
        Err(error) => {
            return Made {
                job,
                unopened: Some(error),
                errors: Vec::new(),
                warned: Vec::new(),
            };
        }
    };
    let mut errors = Vec::new();
    let mut warned = Vec::new();
    for ((entry, &verdict), &exists) in entries.iter().zip(verdicts).zip(present) {
        let child = manifest::join(path, &entry.name);
        if entry.kind == Kind::Directory {
            if verdict == Verdict::Content
                && let Err(error) = make_directory(&directory, &entry.name, entry.mode, exists)
            {
                // Not made, it is not gone into, under any name.
                dirs.refuse(&child);
                errors.push(Unmade {
                    id: entry.id,
                    path: child,
                    directory: true,
                    error,
                });
            }
            continue;
        }
        // Its content is asked for by the applier.
        if verdict == Verdict::Content && entry.kind == Kind::File {
            continue;
        }
        if let Err(error) = apply_entry(
            &directory,
            &entry.name,
            &child,
            entry,
            verdict,
            exists,
            state,
            &mut warned,
        ) {
            errors.push(Unmade {
                id: entry.id,
                path: child,
                directory: false,
                error,
            });
        }
    }
    Made {
        job,
        unopened: None,
        errors,
        warned,
    }
}

/// What became of a part means, as the applier alone says: a deletion
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

/// Why the transfer stops, as `path` could not be copied.
fn cannot_copy(path: &[u8], is_directory: bool, error: &anyhow::Error) -> String {
    format!(
        "cannot copy {}: {error:#}",
        String::from_utf8_lossy(&display(path, is_directory))
    )
}

/// Makes a symlink or special file match, or sets a file's attributes. Files'
/// content and directories are handled elsewhere. `exists` is whether
/// `directory` was listed with `name`.
#[allow(clippy::too_many_arguments)]
fn apply_entry(
    directory: impl AsFd,
    name: &[u8],
    shown: &[u8],
    entry: &Entry,
    verdict: Verdict,
    exists: bool,
    state: &State,
    warned: &mut Warned,
) -> Result<()> {
    match verdict {
        Verdict::Same => Ok(()),
        Verdict::Times => match fsys::set_mtime_at(&directory, name, entry.mtime) {
            Err(error) if unsettable(&error, true) => {
                warned.push((Warning::Times, shown.to_vec()));
                Ok(())
            }
            set => Ok(set?),
        },
        Verdict::Attributes => {
            let stat = fsys::stat(&directory, name)?;
            // What was compared may have been swapped for a symlink since:
            // it fails here, before anything is set. Setting the mode never
            // goes through one either, whatever comes there after this.
            if !entry.kind.is_symlink() && fsys::file_type(&stat) == FileType::Symlink {
                bail!("it became a symlink after it was compared");
            }
            #[allow(clippy::unnecessary_cast)]
            let current = (stat.st_uid as u32, stat.st_gid as u32);
            let setters = Setters {
                chown: |uid, gid| fsys::set_owner_at(&directory, name, uid, gid),
                chmod: (!entry.kind.is_symlink())
                    .then_some(|mode| fsys::set_mode_at(&directory, name, mode)),
                utimes: |mtime| fsys::set_mtime_at(&directory, name, mtime),
            };
            Ok(set_attributes(
                state, shown, current, entry, setters, warned,
            )?)
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
                    warned.push((Warning::Special, shown.to_vec()));
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let finished = (|| -> Result<()> {
                let stat = fsys::stat(&directory, &temporary)?;
                #[allow(clippy::unnecessary_cast)]
                let current = (stat.st_uid as u32, stat.st_gid as u32);
                let setters = Setters {
                    chown: |uid, gid| fsys::set_owner_at(&directory, &temporary, uid, gid),
                    chmod: (!entry.kind.is_symlink())
                        .then_some(|mode| fsys::set_mode_at(&directory, &temporary, mode)),
                    utimes: |mtime| fsys::set_mtime_at(&directory, &temporary, mtime),
                };
                set_attributes(state, shown, current, entry, setters, warned)?;
                clear_the_way(&directory, name, exists, state.delete, &state.keep)?;
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
) -> io::Result<()> {
    let uid = ownership.uid(entry.uid);
    let gid = ownership.gid(entry.gid);
    let set_uid = (uid != current_uid && ownership.can_set_user(uid, current_uid)).then_some(uid);
    let set_gid = (gid != current_gid && ownership.can_set_group(gid, current_uid)).then_some(gid);
    if set_uid.is_some() || set_gid.is_some() {
        chown(set_uid, set_gid)?;
    }
    Ok(())
}

/// Whether setting an attribute failed because this process may not set it,
/// or the filesystem does not keep it: as root's `chown` where root is
/// squashed, or `chmod` on FAT. Then the entry is kept, and this is warned
/// of. With `invalid`, an invalid value is one too: a time the filesystem
/// cannot hold, as FAT's before 1980, or an owner with no ID here, as in a
/// user namespace that maps only some.
pub(super) fn unsettable(error: &io::Error, invalid: bool) -> bool {
    match Errno::from_io_error(error) {
        Some(errno) if errno == Errno::PERM => true,
        // The same on Linux, not on macOS.
        Some(errno) if errno == Errno::OPNOTSUPP || errno == Errno::NOTSUP => true,
        Some(errno) if errno == Errno::INVAL => invalid,
        _ => false,
    }
}

/// How an entry's attributes are set: its owner, then its permissions,
/// unless it is a symlink, then its mtime.
pub(super) struct Setters<O, M, T> {
    pub(super) chown: O,
    pub(super) chmod: Option<M>,
    pub(super) utimes: T,
}

/// What an executor could not set, by kind and path, which the owner it
/// reports to warns of.
pub(super) type Warned = Vec<(Warning, Vec<u8>)>;

/// Gives an entry the Source's owner, permissions, and mtime, in that order,
/// each as far as it can, its owner owned by `current`: what this process
/// may not set, or the filesystem does not keep, goes in `warned`, by kind,
/// as `shown`, and the rest is still set. Any other error is returned, the
/// first, once each is tried. A user that could not be set takes setuid
/// away, and a group, setgid: the entry would otherwise run as someone the
/// Source never let it, such as root, where rsync leaves the permissions
/// unset altogether.
pub(super) fn set_attributes<O, M, T>(
    state: &State,
    shown: &[u8],
    (current_uid, current_gid): (u32, u32),
    entry: &Entry,
    setters: Setters<O, M, T>,
    warned: &mut Warned,
) -> io::Result<()>
where
    O: FnOnce(Option<u32>, Option<u32>) -> io::Result<()>,
    M: FnOnce(u32) -> io::Result<()>,
    T: FnOnce(Timestamp) -> io::Result<()>,
{
    let unset = std::cell::Cell::new((false, false));
    let owner = set_owner(
        &state.ownership,
        current_uid,
        current_gid,
        entry,
        |uid, gid| {
            let set = (setters.chown)(uid, gid);
            if set.is_err() {
                unset.set((uid.is_some(), gid.is_some()));
            }
            set
        },
    );
    let mut mode = entry.mode & 0o7777;
    let (user_unset, group_unset) = unset.get();
    if user_unset {
        mode &= !0o4000; // setuid
    }
    if group_unset {
        mode &= !0o2000; // setgid
    }
    let lost = mode != entry.mode & 0o7777 && setters.chmod.is_some();
    let mode = setters.chmod.map(|chmod| chmod(mode));
    if lost && mode.as_ref().is_some_and(|set| set.is_ok()) {
        warned.push((Warning::Setid, shown.to_vec()));
    }
    let times = (setters.utimes)(entry.mtime);
    let mut first = None;
    for (result, warning) in [
        (owner, Warning::Owner),
        (mode.unwrap_or(Ok(())), Warning::Mode),
        (times, Warning::Times),
    ] {
        match result {
            Ok(()) => {}
            Err(error) if unsettable(&error, warning != Warning::Mode) => {
                warned.push((warning, shown.to_vec()));
            }
            Err(error) => {
                first.get_or_insert(error);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// Creates a directory, replacing whatever else is there but a directory.
/// The directory is made writable by its owner, so its entries can be written;
/// its own permissions are set at the end. `exists` is whether the
/// directory it goes in was listed with its name; see [`not_an_alias`].
fn make_directory(parent: impl AsFd, name: &[u8], mode: u32, exists: bool) -> Result<()> {
    match fsys::stat(&parent, name) {
        Ok(stat) => {
            if !exists {
                not_an_alias(&parent, name, &stat)?;
            }
            if fsys::file_type(&stat) == FileType::Directory {
                return Ok(());
            }
            fsys::remove(&parent, name, false)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fsys::make_directory(&parent, name, (mode & 0o7777) | 0o700)?;
    Ok(())
}

/// Removes a directory where something else is about to go. Without
/// deleting, only an empty one is removed: a directory with anything in it
/// stays, and nothing is written. `sync` removes it with all it holds, but
/// for what `keep` keeps. `exists` is whether `directory` was listed
/// with `name`; see [`not_an_alias`].
pub(super) fn clear_the_way(
    directory: impl AsFd,
    name: &[u8],
    exists: bool,
    delete: bool,
    keep: &fsys::Keep,
) -> Result<()> {
    let Ok(stat) = fsys::stat(&directory, name) else {
        return Ok(());
    };
    if !exists {
        not_an_alias(&directory, name, &stat)?;
    }
    if fsys::file_type(&stat) == FileType::Directory {
        if delete {
            fsys::remove_tree(&directory, name, keep)?;
        } else {
            fsys::remove(&directory, name, true)
                .context("a directory that is not empty is where this belongs")?;
        }
    }
    Ok(())
}

/// Fails what is to have `name` in `directory`, which the comparison did not
/// list with it, where something has the name now under another the
/// filesystem takes for the same, as one that ignores case takes `A` for
/// `a`: often another of the Source's, made just before. Neither is then
/// replaced, nor written into, nor deleted. What came by the name itself
/// since it was compared is replaced, as ever.
fn not_an_alias(directory: impl AsFd, name: &[u8], found: &rustix::fs::Stat) -> Result<()> {
    match fsys::alias(directory, name, found)? {
        None => Ok(()),
        Some(kept) if kept.is_empty() => {
            bail!("the filesystem here takes its name for another's, already here")
        }
        Some(kept) => bail!(
            "the filesystem here takes its name for that of {}, already here",
            String::from_utf8_lossy(&kept)
        ),
    }
}

/// Deletes, for `sync`, what a directory has that the Source does not:
/// `extras` are names in `held`, and whether each is a directory. What the
/// Source listed but could not read is in the manifest, as unreadable, so it
/// is never among these. With a directory among them, the deletion spreads
/// over [`remove::DELETERS`] threads, a directory to each; names in one
/// directory go one at a time anyway. A directory may hold millions: none
/// more is deleted once the Destination has stopped. Returns what became of
/// each.
pub(super) fn delete_extras(
    held: &OwnedFd,
    extras: &[(Vec<u8>, bool)],
    keep: &fsys::Keep,
    stop: &Stop,
) -> Vec<io::Result<()>> {
    if extras.is_empty() {
        return Vec::new();
    }
    let deleters = if extras.iter().any(|(_, is_directory)| *is_directory) {
        remove::DELETERS
    } else {
        1
    };
    remove::trees(held.as_fd(), extras, keep, deleters, &|| stop.is_stopped())
}

/// Tells what became of the extras of the directory at `directory`, each
/// with its result: one on another filesystem is warned of, one that cannot
/// be deleted fails alone, but one to protect, or a drive full or
/// read-only, stops the transfer. Returns whether any was deleted. Nothing
/// is said once the Destination has stopped.
pub(super) fn tell_deleted(
    directory: &[u8],
    extras: &[(Vec<u8>, bool)],
    results: Vec<io::Result<()>>,
    state: &State,
    mux: &Mux,
    stop: &Stop,
) -> Result<bool> {
    stop.check()?;
    let reporter = &state.reporter;
    let mut deleted = false;
    for ((name, is_directory), result) in extras.iter().zip(results) {
        let path = manifest::join(directory, name);
        match result {
            Ok(()) => deleted = true,
            // What else it held is deleted; it stays, as rsync's `-x` keeps
            // it, which is no failure.
            Err(error) if fsys::is_elsewhere(&error) => tell(
                reporter,
                format_args!(
                    "warning: not deleting {}: {error}",
                    String::from_utf8_lossy(&display(&path, *is_directory))
                ),
            ),
            Err(error) if fsys::is_protected(&error) => {
                bail!(
                    "will not delete {}: {error}",
                    String::from_utf8_lossy(&display(&path, *is_directory))
                );
            }
            // Nothing more could be deleted.
            Err(error) if io_ends_the_transfer(&error) => {
                bail!(
                    "cannot delete {}: {error}",
                    String::from_utf8_lossy(&display(&path, *is_directory))
                );
            }
            Err(error) => {
                state.failures.fetch_add(1, Ordering::Relaxed);
                reporter.add(Count::Failed, 1);
                tell(
                    reporter,
                    format_args!(
                        "cannot delete {}: {error}",
                        String::from_utf8_lossy(&display(&path, *is_directory))
                    ),
                );
                mux.message(&Results::NotDeleted {
                    path,
                    message: error.to_string(),
                })?;
            }
        }
    }
    Ok(deleted)
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
            true,
            &State::new(false),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o600
        );
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
    /// left; one to be let go, without being finished. Nothing stays open.
    #[test]
    fn directories_are_finished_as_they_are_done_deepest_first() {
        let (finishing, done) = crossbeam_channel::unbounded();
        let mut directories = Directories::new(Some(finishing), None);
        let directory = |id: FileId, name: &[u8]| entry(id, name, Kind::Directory, 0);
        let finished = |done: &crossbeam_channel::Receiver<(Vec<u8>, Entry)>| -> Vec<String> {
            done.try_iter()
                .map(|(path, _)| String::from_utf8(path).unwrap())
                .collect()
        };
        // The root, with a/ and b/ in it; a/ with a file and c/; b/ never
        // listed.
        directories.announce(Vec::new(), directory(0, b""));
        directories.mark_to_finish(b"");
        directories.enter(b"");
        directories.announce(b"a".to_vec(), directory(1, b"a"));
        directories.announce(b"b".to_vec(), directory(2, b"b"));
        directories.mark_to_finish(b"a");
        directories.mark_to_finish(b"b");
        directories.enter(b"a");
        directories.need(file(9, b"a"));
        directories.announce(b"a/c".to_vec(), directory(3, b"c"));
        directories.enter(b"a/c");
        // Left, nothing under it, nothing to finish: let go.
        directories.enter(b"a/d-not-announced");
        assert_eq!(finished(&done), Vec::<String>::new());
        assert!(!directories.open.contains_key(b"a/c".as_slice()));
        // The manifest is over; a/ waits for its file.
        directories.leave_all();
        assert_eq!(finished(&done), ["b"]);
        assert!(directories.open.contains_key(b"a".as_slice()));
        assert!(directories.open.contains_key(b"".as_slice()));
        directories.settle(9);
        assert_eq!(finished(&done), ["a", ""]);
        assert!(
            directories.open.is_empty(),
            "{:?}",
            directories.open.keys().collect::<Vec<_>>()
        );
    }

    /// A directory whose needed file the Source could not open is finished
    /// once it says so, not at the end; an ID it does not wait for, or
    /// said again, is nothing.
    #[test]
    fn a_directory_is_finished_once_its_file_the_source_could_not_open_is_settled() {
        let (finishing, done) = crossbeam_channel::unbounded();
        let mut directories = Directories::new(Some(finishing), None);
        let directory = |id: FileId, name: &[u8]| entry(id, name, Kind::Directory, 0);
        directories.announce(Vec::new(), directory(0, b""));
        directories.enter(b"");
        directories.announce(b"a".to_vec(), directory(1, b"a"));
        directories.announce(b"b".to_vec(), directory(2, b"b"));
        directories.mark_to_finish(b"a");
        directories.enter(b"a");
        directories.need(file(3, b"a"));
        // The manifest has left a/, which waits for its file.
        directories.enter(b"b");
        directories.settle(99);
        assert!(done.try_recv().is_err());
        directories.settle(3);
        let finished: Vec<Vec<u8>> = done.try_iter().map(|(path, _)| path).collect();
        assert_eq!(finished, [b"a".to_vec()]);
        directories.settle(3);
        assert!(done.try_recv().is_err());
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

    /// The directories finished since last asked, in order, as their paths.
    fn finished(finishing: &Receiver<(Vec<u8>, Entry)>) -> Vec<Vec<u8>> {
        finishing.try_iter().map(|(path, _)| path).collect()
    }

    /// Directories, as the applier follows them, and what they finish.
    fn directories() -> (Directories, Receiver<(Vec<u8>, Entry)>) {
        let (finishing, finished) = crossbeam_channel::unbounded();
        (Directories::new(Some(finishing), None), finished)
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

    /// A directory is finished once its parts are made, its files settled,
    /// the manifest has left it, and every directory in it is done, in
    /// whatever order those come: one in it, never seen, once its own part
    /// is made and the manifest has left its parent, and before it; the root
    /// last.
    #[test]
    fn a_directory_is_finished_once_all_in_it_is_done_in_any_order() {
        for order in orders(4) {
            let (mut directories, finishing) = directories();
            directories.announce(Vec::new(), entry(0, b"", Kind::Directory, 0));
            directories.mark_to_finish(b"");
            directories.enter(b"");
            directories.announce(b"a".to_vec(), entry(1, b"a", Kind::Directory, 0));
            directories.enter(b"a");
            directories.mark_to_finish(b"a");
            // A part of `a` given to the makers, which is to make `a/b`, whose
            // own part is given to them too, and needs a file in `a`.
            directories.job_started(b"a");
            directories.announce(b"a/b".to_vec(), entry(2, b"b", Kind::Directory, 0));
            directories.mark_to_finish(b"a/b");
            directories.job_started(b"a/b");
            directories.need(file(3, b"a"));
            let mut happened = [false; 4];
            for &event in &order {
                match event {
                    0 => directories.job_done(b"a"),
                    1 => directories.settle(3),
                    2 => directories.job_done(b"a/b"),
                    _ => directories.leave(b"a".to_vec()),
                }
                happened[event] = true;
                let mut expected = Vec::new();
                if (event == 2 || event == 3) && happened[2] && happened[3] {
                    expected.push(b"a/b".to_vec());
                }
                if happened.iter().all(|&happened| happened) {
                    expected.push(b"a".to_vec());
                }
                assert_eq!(finished(&finishing), expected, "{order:?}, at {event}");
            }
            directories.leave_all();
            assert_eq!(finished(&finishing), [b"".to_vec()], "{order:?}");
        }
    }

    /// What is still open once everything is written, as a directory whose
    /// file never came, is finished all the same, deepest first.
    #[test]
    fn what_is_still_open_at_the_end_is_finished_deepest_first() {
        let (mut directories, finishing) = directories();
        directories.announce(Vec::new(), entry(0, b"", Kind::Directory, 0));
        directories.mark_to_finish(b"");
        for (path, id) in [(&b"a"[..], 1), (b"a/b", 2), (b"a/b/c", 3)] {
            directories.announce(path.to_vec(), entry(id, b"x", Kind::Directory, 0));
            directories.enter(path);
            directories.mark_to_finish(path);
        }
        directories.need(file(10, b"a/b/c"));
        directories.need(file(11, b"a"));
        directories.leave_all();
        directories.settle(10);
        assert_eq!(finished(&finishing), [b"a/b/c".to_vec(), b"a/b".to_vec()]);
        assert!(directories.is_needed(11));
        directories.finish_the_rest();
        assert_eq!(finished(&finishing), [b"a".to_vec(), b"".to_vec()]);
        assert!(directories.open.is_empty());
    }

    /// A part waits for its directory to be made, and its extras deleted,
    /// and goes to the makers once both are.
    #[test]
    fn a_part_waits_for_its_directory_made_and_its_extras_deleted() {
        let (jobs, taken) = crossbeam_channel::unbounded();
        let (lane, _extras) = crossbeam_channel::unbounded();
        let (_made, done) = crossbeam_channel::unbounded();
        let part = Part {
            path: b"d/e".to_vec(),
            directory: None,
            entries: Vec::new(),
            verdicts: Vec::new(),
            present: Vec::new(),
        };
        let mut makers = Makers {
            jobs,
            lane,
            done,
            next: 2,
            parts: HashMap::from([(1, Arc::new(part))]),
            making: HashSet::from([b"d".to_vec()]),
            deleting: HashSet::from([b"d".to_vec()]),
            parked: HashMap::from([(b"d".to_vec(), vec![1])]),
            unasked: BTreeSet::new(),
        };
        makers.making.remove(&b"d"[..]);
        makers.release(b"d");
        assert!(
            taken.try_recv().is_err(),
            "given out before its extras went"
        );
        makers.deleting.remove(&b"d"[..]);
        makers.release(b"d");
        assert_eq!(taken.try_recv().unwrap().0, 1);
        makers.release(b"d");
        assert!(taken.try_recv().is_err(), "given out twice");
    }

    /// What a maker met is for the applier to say: a deletion refused, to
    /// protect the Source's root, stops the transfer, but in making a
    /// directory fails it alone; a full drive stops it, said as the first
    /// that stops it; anything else fails its entry alone; and a directory
    /// that could not be opened fails all it was to change.
    #[test]
    fn what_a_maker_met_is_said_by_the_applier() {
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
}
