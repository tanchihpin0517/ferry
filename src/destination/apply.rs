//! Carrying out what the decider says: the driver, which tells it each
//! event as it comes and carries out each command, and the executors that
//! apply what it decides, each only reporting what became of it, for the
//! decider to say what that means. Up to 16 makers make the parts match, a
//! directory each, creating the directories, symlinks and special files
//! each needs and setting attributes that differ; a lane of its own deletes
//! each directory's extras; up to 16 writers write the files the Source
//! sends, and the renamers give them their names (`write`, `rename`); up to
//! 16 finishers finish the directories done (`finish`); and the root is
//! applied as it comes, before any part.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};
use rustix::fs::FileType;
use rustix::io::Errno;

use crate::compare::{Comparer, Extras, Verdict};
use crate::dirs::DirCache;
use crate::fsys;
use crate::interrupt;
use crate::manifest::{self, Entry, Kind, Timestamp};
use crate::mux::Mux;
use crate::owners::Ownership;
use crate::pool;
use crate::profile;
use crate::protocol;
use crate::remove;
use crate::report::{Count, Gauge, Reporter, Stage, TRANSFERRING_EVERY};
use crate::source::display;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::decider::{Command, Decider, Deleted, Event, Extra, Made, Part, Unmade};
use super::finish::{FINISHERS, finish_directory};
use super::receive::Compared;
use super::rename;
use super::warnings::Warning;
use super::write::{self, Arrival, BATCH_WAIT, Batched, Job};
use super::{Options, State, missing_parent, root_parent, tell};

/// What the decider and the executors work with, borrowed from the run.
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

/// What the decider takes in a run beside what the other stages tell it:
/// the blocks, decoded and checked, in the order they came. None in a dry
/// run.
#[derive(Default)]
pub(super) struct Outlets {
    pub(super) arrivals: Option<Receiver<Arrival>>,
}

/// How many directories have their parts made at once, a directory each, as
/// in one the kernel makes one name at a time: as many as gained on the
/// filesystems measured; see [`crate::remove::DELETERS`].
const MAKERS: usize = 16;

/// Applies the root and each comparison, in the manifest's order, as they
/// come, as the decider says, which follows each directory until it is
/// finished, and each file until it is settled. In a run, the parts go to
/// the makers, [`MAKERS`] threads, a directory each, the extras to the
/// deleting lane, the files to the writers and the renamers, and the
/// directories done to the finishers, [`FINISHERS`] threads; and once the
/// last is finished, what could not be set is warned of, and the Source
/// told the Destination is done. `ended` says, once the comparisons end,
/// whether the manifest did.
pub(super) fn apply(
    compared: Receiver<Compared>,
    told: Receiver<Event>,
    ended: &AtomicBool,
    applying: &Applying,
    outlets: Outlets,
) -> Result<()> {
    let Applying {
        root,
        state,
        options,
        reporter,
        dirs,
        stop,
        ..
    } = *applying;
    let writers = pool::threads(16);
    let open_files = crate::limits::open_files();
    let batch_size = write::batch_size(open_files, fsys::UNNAMED);
    let mut decider = Decider::new(
        !options.dry_run,
        protocol::through_every(options.in_flight_entries),
        batch_size,
        writers,
        write::waiting_size(open_files, fsys::UNNAMED, batch_size),
        root.as_os_str().as_encoded_bytes().to_vec(),
    );
    let mut acting = Acting {
        applying,
        makers: None,
        lane: None,
        finishers: None,
        writers: None,
        renamers: None,
        wake: None,
        // Jobs handed to the writers: each busy, and one more each.
        writing: reporter.gauge(Stage::Write, writers * 2),
        reported: false,
    };
    if options.dry_run {
        return drive(
            &mut decider,
            compared,
            &told,
            None,
            None,
            ended,
            &mut acting,
        );
    }
    let driven = std::thread::scope(|scope| {
        let stages = Stages::new(scope, stop);
        let (jobs, taken) = crossbeam_channel::unbounded::<(u64, Arc<Part>)>();
        let (lane, extras) = crossbeam_channel::unbounded::<Extra>();
        let (reported, reports) = crossbeam_channel::unbounded::<Event>();
        for _ in 0..MAKERS {
            let taken = taken.clone();
            let reported = reported.clone();
            stages.spawn("make", move |_| {
                while let Ok((job, part)) = profile::wait(profile::State::Work, || taken.recv()) {
                    let made = profile::wait(profile::State::Apply, || {
                        make_part(job, &part, applying.state, applying.dirs)
                    });
                    if reported.send(Event::Made(made)).is_err() {
                        break;
                    }
                }
                Ok(())
            });
        }
        // The deleting lane: a directory's extras at a time, each over up
        // to remove::DELETERS threads.
        let deleting = reported.clone();
        stages.spawn("delete", move |_| {
            while let Ok(extra) = profile::wait(profile::State::Work, || extras.recv()) {
                let _on = profile::on(&extra.path);
                let deleted = profile::wait(profile::State::Delete, || {
                    delete_on_lane(extra, applying.state, applying.stop)
                });
                if deleting.send(Event::Deleted(deleted)).is_err() {
                    break;
                }
            }
            Ok(())
        });
        let (finishing, done) = crossbeam_channel::unbounded::<(Vec<u8>, Entry)>();
        for _ in 0..FINISHERS {
            let done = done.clone();
            let reported = reported.clone();
            stages.spawn("finish", move |_| {
                while let Ok((path, entry)) = profile::wait(profile::State::Work, || done.recv()) {
                    let shown = display(&path, true);
                    let _on = profile::on(&shown);
                    let mut warned = Vec::new();
                    let result = profile::wait(profile::State::Finish, || {
                        finish_directory(
                            applying.state,
                            applying.dirs,
                            &path,
                            &shown,
                            &entry,
                            &mut warned,
                        )
                    });
                    let finished = Event::Finished {
                        path,
                        entry,
                        result,
                        warned,
                    };
                    if reported.send(finished).is_err() {
                        break;
                    }
                }
                Ok(())
            });
        }
        acting.makers = Some(jobs);
        acting.lane = Some(lane);
        acting.finishers = Some(finishing);
        acting.writers = Some(write::writers(
            &stages, writers, root, state, dirs, &reported,
        ));
        acting.renamers = Some(rename::renamer(
            &stages,
            state.delete,
            &state.keep,
            &reported,
        ));
        drop(reported);
        let driven = drive(
            &mut decider,
            compared,
            &told,
            Some(&reports),
            outlets.arrivals,
            ended,
            &mut acting,
        );
        // Its end lets the executors go: the scope then waits for them.
        acting.let_go();
        driven
    });
    // Once some were, that none is being transferred.
    if acting.reported {
        reporter.transferring(Vec::new());
    }
    let warnings = state.warnings.lock().expect("not poisoned").messages();
    for message in warnings {
        tell(reporter, format_args!("warning: {message}"));
    }
    driven?;
    // In order after the results, which must arrive first.
    stop.done()
}

/// The decider's thread: tells it each event as it comes, and carries out
/// what it says, until it has nothing more to decide. What the executors
/// report comes on `reports`, and the blocks on `arrivals`, in a run.
fn drive(
    decider: &mut Decider,
    compared: Receiver<Compared>,
    told: &Receiver<Event>,
    reports: Option<&Receiver<Event>>,
    mut arrivals: Option<Receiver<Arrival>>,
    ended: &AtomicBool,
    acting: &mut Acting,
) -> Result<()> {
    let Applying { stop, reporter, .. } = *acting.applying;
    // Comparisons still to come.
    let mut parts = Some(compared);
    // The files being received and written are reported now and then.
    let reports_due = if reporter.is_open() && reports.is_some() {
        crossbeam_channel::tick(TRANSFERRING_EVERY)
    } else {
        crossbeam_channel::never()
    };
    // When the batch filling has waited long enough.
    let mut woken = crossbeam_channel::never();
    loop {
        // What the others say comes first: the root before any part.
        while let Ok(event) = told.try_recv() {
            acting.carry_out(decider.on(event))?;
        }
        // What the executors said meanwhile is taken all at once, rather
        // than each after a wait on every channel: for small files, a
        // report for each.
        while let Some(event) = reports.and_then(|reports| reports.try_recv().ok()) {
            acting.carry_out(decider.on(event))?;
        }
        acting.writing.set(decider.writing());
        if decider.is_finished() {
            return Ok(());
        }
        let never = crossbeam_channel::never();
        let unreported = crossbeam_channel::never();
        let blockless = crossbeam_channel::never();
        // Made again only once the time to wake is put off.
        if let Some(at) = acting.wake.take() {
            woken = crossbeam_channel::at(at);
        }
        let next = profile::wait(profile::State::Work, || {
            crossbeam_channel::select! {
                recv(told) -> event => event.map_or(Next::Gone, Next::Event),
                recv(parts.as_ref().unwrap_or(&never)) -> done => {
                    done.map_or(Next::PartsEnded, Next::Part)
                }
                recv(reports.unwrap_or(&unreported)) -> report => {
                    report.map_or(Next::Gone, Next::Event)
                }
                recv(arrivals.as_ref().unwrap_or(&blockless)) -> arrival => {
                    arrival.map_or(Next::BlocksCut, Next::Arrival)
                }
                recv(woken) -> _ => Next::Due,
                recv(reports_due) -> _ => Next::Report,
                recv(stop.signal()) -> _ => Next::Gone,
            }
        });
        match next {
            Next::Event(event) => acting.carry_out(decider.on(event))?,
            Next::Arrival(arrival) => {
                let event = match arrival {
                    Arrival::Parts(parts) => Event::Parts(parts),
                    Arrival::Hash(hash) => Event::Hash(hash),
                    Arrival::End => {
                        arrivals = None;
                        Event::BlocksEnded
                    }
                };
                acting.carry_out(decider.on(event))?;
            }
            // The blocks ended before the Source said they did.
            Next::BlocksCut => {
                arrivals = None;
                acting.carry_out(decider.on(Event::BlocksCut))?;
            }
            Next::Due => {
                woken = crossbeam_channel::never();
                acting.carry_out(decider.on(Event::Due))?;
            }
            Next::Report => {
                let files = decider.transferring();
                let none = files.is_empty();
                if acting.reported || !none {
                    reporter.transferring(files);
                }
                acting.reported = !none;
            }
            Next::Part(done) => {
                // What was said before it was compared comes first.
                while let Ok(event) = told.try_recv() {
                    acting.carry_out(decider.on(event))?;
                }
                profile::wait(profile::State::Apply, || {
                    acting.carry_out(decider.on(Event::Compared(done)))
                })?;
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
                acting.carry_out(decider.on(Event::Ended))?;
            }
            // The others are gone, or the Destination stopped.
            Next::Gone => return Err(Stopped.into()),
        }
    }
}

/// What the decider's thread takes next.
enum Next {
    Event(Event),
    Part(Compared),
    PartsEnded,
    Arrival(Arrival),
    /// The blocks ended without their end.
    BlocksCut,
    /// The batch filling has waited long enough.
    Due,
    /// The files being transferred are to be reported.
    Report,
    Gone,
}

/// Carries out what the decider says.
struct Acting<'a> {
    applying: &'a Applying<'a>,
    /// Where the makers take their parts, the lane its extras, the
    /// finishers the directories done, the writers their jobs, and the
    /// renamers their batches; none in a dry run.
    makers: Option<Sender<(u64, Arc<Part>)>>,
    lane: Option<Sender<Extra>>,
    finishers: Option<Sender<(Vec<u8>, Entry)>>,
    writers: Option<Sender<Job>>,
    renamers: Option<Sender<Vec<Batched>>>,
    /// When the decider is to be told the batch filling has waited long
    /// enough, newly put off.
    wake: Option<Instant>,
    /// How many jobs the writers have, for the dashboard.
    writing: Gauge,
    /// Files being transferred were reported last.
    reported: bool,
}

impl Acting<'_> {
    /// Lets the executors go, as their work ends.
    fn let_go(&mut self) {
        self.makers = None;
        self.lane = None;
        self.finishers = None;
        self.writers = None;
        self.renamers = None;
    }

    fn carry_out(&mut self, commands: Vec<Command>) -> Result<()> {
        let Applying {
            mux,
            state,
            reporter,
            stop,
            ..
        } = *self.applying;
        for command in commands {
            // What could not be set is warned of even once the Destination
            // has stopped; nothing else is done, nor said, its failing a
            // consequence.
            if !matches!(command, Command::Warn(_)) {
                stop.check()?;
            }
            match command {
                // An executor gone has stopped the Destination, and says why
                // itself.
                Command::Make(job, part) => {
                    let makers = self.makers.as_ref().expect("makers in a run");
                    let _ = makers.send((job, part));
                }
                Command::Delete(extra) => {
                    let lane = self.lane.as_ref().expect("a lane in a run");
                    let _ = lane.send(extra);
                }
                Command::Finish(path, entry) => {
                    let finishers = self.finishers.as_ref().expect("finishers in a run");
                    let _ = finishers.send((path, entry));
                }
                Command::Write(job) => {
                    let writers = self.writers.as_ref().expect("writers in a run");
                    let _ = writers.send(job);
                }
                Command::Rename(batch) => {
                    let renamers = self.renamers.as_ref().expect("renamers in a run");
                    let _ = renamers.send(batch);
                }
                Command::Wake => self.wake = Some(Instant::now() + BATCH_WAIT),
                Command::Needs(needs) => mux.message(&needs)?,
                Command::Count(count, amount) => reporter.add(count, amount),
                Command::Listed(listed) => reporter.listed(listed),
                Command::Say(line) => reporter.say(line),
                Command::Fail { said, result } => {
                    state.failures.fetch_add(1, Ordering::Relaxed);
                    reporter.add(Count::Failed, 1);
                    if let Some(said) = said {
                        reporter.say(said);
                    }
                    mux.message(&result)?;
                }
                Command::Warn(warned) => state.warn(warned),
                Command::Stop(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Applies the manifest's root, which comes before anything in it: refuses
/// a root of another kind than the Source's, compares it, and, in a run,
/// makes it match, but for a file's content, which is asked for, and a
/// directory's attributes, set once it is finished. Returns its verdict,
/// and what could not be set.
pub(super) fn root(entry: &Entry, applying: &Applying) -> Result<(Verdict, Warned)> {
    let Applying {
        comparer,
        root,
        state,
        options,
        stop,
        ..
    } = *applying;
    refuse_another_kind(root, entry)?;
    let verdict = comparer
        .root(entry, &state.ownership)
        .context("compare the destination's root")?;
    let mut warned = Vec::new();
    if !options.dry_run {
        stop.check()?;
        if let Err(error) = make_root(root, entry, verdict, state, &mut warned) {
            // What could not be set is said even if the transfer stops.
            state.warn(warned);
            return Err(error);
        }
    }
    Ok((verdict, warned))
}

/// Makes the root match, as a maker an entry: a directory made, where it is
/// missing, a symlink or special file made, or a file's attributes set.
fn make_root(
    root: &Path,
    entry: &Entry,
    verdict: Verdict,
    state: &State,
    warned: &mut Warned,
) -> Result<()> {
    match (verdict, &entry.kind) {
        (Verdict::Content, Kind::Directory) => match std::fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => bail!("{} exists and is not a directory", root.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(root).map_err(|error| missing_parent(root, error))?;
                Ok(())
            }
            Err(error) => Err(error).with_context(|| format!("inspect {}", root.display())),
        },
        (Verdict::Same, _) | (_, Kind::Directory) | (Verdict::Content, Kind::File) => Ok(()),
        _ => {
            let (directory, name) = root_parent(root)?;
            let shown = root.as_os_str().as_encoded_bytes();
            apply_entry(
                &directory, &name, shown, entry, verdict, true, state, warned,
            )
        }
    }
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

/// The extras, but, with `-x`, none on another filesystem than the root's:
/// as rsync's `-x` does, nothing is deleted in a directory there, and no
/// directory there is deleted, a mount point, and neither is said.
pub(super) fn on_this_filesystem(
    directory: &OwnedFd,
    mut extras: Extras,
    keep: &fsys::Keep,
) -> Extras {
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
pub(super) fn without_aliases_of_unreadable(
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

/// Makes a part match, as a maker does: its directories, symlinks and
/// special files made, and attributes set, one at a time, as the
/// directory's lock allows no more; its files are asked for by the decider.
/// Returns what became of it, for the decider to say what it means: whether
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
        // Its content is asked for by the decider.
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
}
