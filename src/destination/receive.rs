//! Reading what the Source sends, each pipe by a thread of its own: on the
//! manifest's, each record is checked as it arrives, the root applied, and
//! each directory's entries compared in batches by a compare thread and a
//! pool of stat workers, for the decider to apply in the manifest's order,
//! each directory's batches in one once its last has; on the data's, the
//! records of files' bytes go to the checkers, which hand them on in the
//! order they came, and the hashes of files sent in segments, what the
//! Source could not open, and the data's end, go to the decider; on
//! control, the Source's failure stops the Destination.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::ops::Range;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender};

use crate::block::{self, Data};
use crate::compare::{Extras, Prepared, Tree, Verdict, Verdicts};
use crate::fsys;
use crate::manifest::{self, Entry, FileId, Kind, Manifest, Place};
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::{self, Control};
use crate::report::Count;
use crate::scan;
use crate::sorted::{self, Keyed, Spill};
use crate::stage::{self, Stages};
use crate::stop::{Stop, Stopped};

use super::Held;
use super::apply::{self, Applying, Outlets, apply, as_deleted};
use super::decider::{Event, FileHash};

/// What the data's reader hands the checkers: a record of a file's bytes,
/// in the buffer it was read into, and what is held of it until its bytes
/// are let go of.
pub(super) struct Incoming {
    pub(super) buffer: Vec<u8>,
    pub(super) record: Data<Range<usize>>,
    pub(super) held: Held,
}

/// Reads the data's pipe until its end: each record of a file's bytes to
/// the checkers, in order, once the Destination holds it, as [`Holding`]
/// bounds; the hashes of files sent in segments, and what the Source could
/// not open, to the decider; and, at the data's end, how many records of
/// files' bytes there were. Its failure is kept as the cause before the
/// records end with it, as [`receive`]'s is.
///
/// [`Holding`]: super::Holding
pub(super) fn receive_data(
    reader: &mut impl crate::mux::In,
    applying: &Applying,
    events: &Sender<Event>,
    holding: &Arc<super::Holding>,
    mut records: pool::Submitter<Incoming>,
    control_end: &Receiver<()>,
) -> Result<()> {
    let result = receive_data_records(reader, applying, events, holding, &mut records, control_end);
    let result = applying.stop.failing(result);
    drop(records);
    result
}

/// What [`receive_data`] does, until the pipe ends or it fails.
fn receive_data_records(
    reader: &mut impl crate::mux::In,
    applying: &Applying,
    events: &Sender<Event>,
    holding: &Arc<super::Holding>,
    records: &mut pool::Submitter<Incoming>,
    control_end: &Receiver<()>,
) -> Result<()> {
    let Applying {
        state,
        options,
        reporter,
        stop,
        ..
    } = *applying;
    let mut reader = io::BufReader::with_capacity(64 << 10, reader);
    // What the data took on the wire, as told so far.
    let mut wired = 0;
    // Records of files' bytes, handed on.
    let mut handed = 0;
    // The data's end came: all the Source sent.
    let mut ended = false;
    // The decider is told; once it is gone, it said why.
    let tell = |event: Event| events.send(event).map_err(|_| Stopped);
    loop {
        let read = profile::wait(profile::State::ReadConnection, || block::read(&mut reader));
        // What the data took on the wire, compressed or not, as it is read.
        let wire = reader.get_ref().wire();
        reporter.add(Count::Wire, wire - wired);
        wired = wire;
        let (buffer, record) = match read {
            Ok(Some(_)) if ended => bail!("the data went on after its end"),
            Ok(Some(read)) => read,
            // The pipe's end, after the data's; or, in a dry run, with none
            // on it, which the decider is told at once, as the data's end.
            Ok(None) if ended => return Ok(()),
            Ok(None) if options.dry_run => return Ok(tell(Event::DataEnd(handed))?),
            // Not a record: this side's to say, at once.
            Err(error) if !protocol::is_cut(&error) => return Err(error),
            // Ended before the data did, as the pipes of a Source that
            // failed end, or cut short: the records end once its failure, if
            // it said one, is known; ended without the data's end, the
            // decider says the data ended early.
            early => {
                after_control(control_end);
                early?;
                return Ok(());
            }
        };
        match record {
            Data::File { ref bytes, .. } | Data::Segment { ref bytes, .. } => {
                let held = holding.take(block::cost(bytes.len()), stop)?;
                records.submit(Incoming {
                    buffer,
                    record,
                    held,
                })?;
                handed += 1;
            }
            Data::Hash { file, hash } => tell(Event::Hash(FileHash { file, hash }))?,
            // A needed file the Source could not open never comes: it is
            // settled, so its directory is finished without it.
            Data::Unopened { file, path: _ } => {
                state.source_problems.fetch_add(1, Ordering::Relaxed);
                tell(Event::Unread(file))?;
            }
            // Every record came, which the decider is told at once: the
            // Source ends its control only after the Destination is done.
            Data::End => {
                ended = true;
                tell(Event::DataEnd(handed))?;
            }
        }
    }
}

/// Waits for the Source's control pipe to end, before what finds another of
/// its pipes ended, or cut short, says so: the Source's failure, if it said
/// one, is the cause, which it said on control before its pipes ended, and
/// control ends with them, its end going first.
pub(super) fn after_control(control_end: &Receiver<()>) {
    let _ = profile::wait(profile::State::ReadConnection, || control_end.recv());
}

/// Reads the Source's control pipe, which has nothing on it but its end, or
/// the Source's failure: that is the cause, and stops the Destination.
pub(super) fn receive_control(reader: &mut impl Read, stop: &Stop) -> Result<()> {
    let Some(packet) = profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
    else {
        return Ok(());
    };
    match packet.decode::<Control>()? {
        // The Source's failure is the cause, and it knows of it.
        Control::Error { message } => {
            stop.peer_failed(anyhow!("the Source failed: {message}"));
            Err(Stopped.into())
        }
        other => bail!("unexpected {other:?} from the Source"),
    }
}

/// Reads the manifest's records, and hands them on, to the comparison and
/// the decider. What the decider says goes to the stages in `outlets`.
pub(super) fn read_manifest(
    reader: &mut (impl Read + Send),
    applying: &Applying,
    outlets: Outlets,
    events: Sender<Event>,
    told: Receiver<Event>,
    control_end: &Receiver<()>,
) -> Result<()> {
    let Applying {
        comparer,
        state,
        options,
        stop,
        ..
    } = *applying;
    // The Destination's own tree is scanned as the manifest comes, by a
    // thread of its own, one directory at a time; a pool stats the entries
    // of each batch, and the decider applies the results, in the manifest's
    // order, as they come: this thread only reads records, and hands on,
    // but for the root, which it applies before any batch comes.
    // `ended` is set as the manifest ends, before the batches do: the
    // comparisons end either way, the needs only then.
    let ended = &AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<()> {
        let stages = Stages::new(scope, stop);
        let (mut comparisons, compared) = pool::ordered(
            &stages,
            scan::STATS,
            64,
            Profiled {
                role: "stat",
                work: profile::State::Compare,
                full: profile::State::ToStat,
            },
            |comparison: Comparison| {
                let (path, entries, last, ownership, prepared) = match comparison {
                    Comparison::Unlistable(path) => return Event::Unlistable(path),
                    Comparison::Batch {
                        path,
                        entries,
                        last,
                        ownership,
                        prepared,
                    } => (path, entries, last, ownership, prepared),
                };
                let directory = prepared.directory.as_ref().ok().cloned().flatten();
                let let_in = prepared.let_in;
                let refused = prepared.refused;
                let unreadable = prepared.unreadable;
                let verdicts =
                    comparer.part(prepared.directory, &entries, &prepared.present, &ownership);
                let mut extras = prepared.extras;
                let mut kept = Vec::new();
                let mut replacing = HashSet::new();
                // Whether files are written in it without a name, as they may
                // be while its extras are deleted: tried once, for one with
                // extras, as a file is made, and let go of at once.
                let unnamed = !options.dry_run
                    && matches!(&extras, Some(Ok(listed)) if !listed.is_empty())
                    && directory.as_ref().is_some_and(|directory| {
                        matches!(fsys::create_unnamed(&**directory), Ok(Some(_)))
                    });
                // What a run finds as it goes, a dry run finds here.
                if options.dry_run
                    && let Some(directory) = &directory
                {
                    if options.delete {
                        replacing = replacing_directories(directory, &entries, &verdicts);
                    }
                    if let Some(Ok(listed)) = extras {
                        // One that cannot be read back is the decider's to
                        // stop the transfer for.
                        extras = Some(
                            match as_deleted(directory, listed, &unreadable, &state.keep) {
                                Ok((listed, aliases)) => {
                                    kept = aliases;
                                    Ok(listed)
                                }
                                Err(error) => Err(error),
                            },
                        );
                    }
                }
                Event::Compared(Compared {
                    path,
                    entries,
                    last,
                    directory,
                    verdicts,
                    present: prepared.present,
                    extras,
                    unreadable,
                    kept,
                    replacing,
                    unnamed,
                    let_in,
                    refused,
                })
            },
        );
        let (parts_in, parts_out) = crossbeam_channel::bounded::<Part>(PARTS);
        let root_bytes = options.root.clone();
        stages.spawn("compare", move |stop| {
            // What the Source does not have is found before anything here
            // changes: each directory is listed before its batches are
            // applied.
            let mut tree = Tree::new(
                root_bytes,
                options.delete,
                !options.dry_run,
                options.protect.first().copied(),
            );
            while let Ok(part) = profile::wait(profile::State::Work, || parts_out.recv()) {
                // Going into a directory may change its permissions.
                stop.check()?;
                let (path, entries, last, ownership, prepared) = match part {
                    Part::Batch {
                        dir,
                        place,
                        entries,
                        last,
                        ownership,
                    } => {
                        let prepared =
                            tree.batch(dir, &place, &entries, last).with_context(|| {
                                format!(
                                    "compare {}",
                                    String::from_utf8_lossy(&crate::source::display(
                                        &place.path,
                                        true
                                    ))
                                )
                            })?;
                        (place.path, entries, last, ownership, prepared)
                    }
                    Part::Unlistable(path) => {
                        comparisons.submit(Comparison::Unlistable(path))?;
                        continue;
                    }
                    Part::Gone(gone) => {
                        tree.gone(&gone);
                        continue;
                    }
                };
                if path.is_empty()
                    && let Ok(Some(directory)) = &prepared.directory
                {
                    hold_root(directory, applying);
                }
                // Kept as they are changed, to be given back should the
                // transfer fail before it is finished, applied or not.
                if let Some(mode) = prepared.let_in {
                    state
                        .let_in
                        .lock()
                        .expect("not poisoned")
                        .insert(path.clone(), mode);
                }
                comparisons.submit(Comparison::Batch {
                    path,
                    entries,
                    last,
                    ownership,
                    prepared,
                })?;
            }
            Ok(())
        });
        // The comparisons, put back in order, for the decider to wait on
        // with what it is told; those of a directory whose extras `sync`
        // deletes held until its last, past the bound on disk.
        let (in_order, applied) = crossbeam_channel::bounded::<Event>(PARTS);
        let delete = options.delete;
        stages.spawn("compared", move |_| {
            let mut holding = Holding::new(delete, Spill::default());
            for done in compared {
                for done in holding.take(done)? {
                    if in_order.send(done).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        });
        stages.spawn("apply", move |_| {
            apply(applied, told, ended, applying, outlets)
        });
        // Kept before the comparisons end, which the decider waits on.
        stop.failing(receive(
            reader,
            applying,
            &events,
            Some(parts_in),
            ended,
            control_end,
        ))
    })
}

/// The loop of [`read_manifest`]: reads each record of the manifest, checks
/// it, and hands it on, until its pipe ends. A stage on the scope's own
/// thread. Its failure, or its panic, is kept as the cause before the
/// comparisons end with it: the decider, finding them ended short, takes
/// that for a consequence, not for the cause in its place.
fn receive(
    reader: &mut impl Read,
    applying: &Applying,
    events: &Sender<Event>,
    mut parts_in: Option<crossbeam_channel::Sender<Part>>,
    ended: &AtomicBool,
    control_end: &Receiver<()>,
) -> Result<()> {
    let result = stage::run("receive", || {
        receive_records(reader, applying, events, &mut parts_in, ended, control_end)
    });
    let result = applying.stop.failing(result);
    drop(parts_in);
    result
}

/// What [`receive`] does, until it is done or fails.
fn receive_records(
    reader: &mut impl Read,
    applying: &Applying,
    events: &Sender<Event>,
    parts_in: &mut Option<crossbeam_channel::Sender<Part>>,
    ended: &AtomicBool,
    control_end: &Receiver<()>,
) -> Result<()> {
    let Applying {
        state,
        options,
        reporter,
        stop,
        ..
    } = *applying;
    let mut records = io::BufReader::with_capacity(64 << 10, reader);
    // Nothing of the manifest is acted on before it is checked.
    let mut check = manifest::Check::default();
    let most = if options.batch_entries == 0 {
        scan::BATCH
    } else {
        options.batch_entries
    };
    // The directory the last `In` named, while it has not ended, and its
    // entries not handed on yet.
    let mut current: Option<(FileId, Place)> = None;
    let mut entries = Vec::new();
    // The decider is told; once it is gone, it said why.
    let tell = |event: Event| events.send(event).map_err(|_| Stopped);
    let hand = |parts_in: &Option<crossbeam_channel::Sender<Part>>, part: Part| {
        let sent = profile::wait(profile::State::ToCompare, || {
            parts_in
                .as_ref()
                .map(|parts_in| parts_in.send(part).is_ok())
        });
        match sent {
            None => bail!("the manifest went on after its end"),
            // The compare thread failed, and said why.
            Some(false) => Err(Stopped.into()),
            Some(true) => Ok(()),
        }
    };
    // Hands on the entries of the directory `dir` at `place` as a batch,
    // its last if `last`.
    let batch = |parts_in: &Option<crossbeam_channel::Sender<Part>>,
                 (dir, place): (FileId, Place),
                 entries: Vec<Entry>,
                 last: bool| {
        reporter.add(Count::Entries, entries.len() as u64);
        let ownership = Arc::clone(&state.ownership);
        hand(
            parts_in,
            Part::Batch {
                dir,
                place,
                entries,
                last,
                ownership,
            },
        )
    };
    loop {
        // Nothing more of the manifest is acted on once the Destination
        // has stopped.
        stop.check()?;
        let record = match profile::wait(profile::State::ReadConnection, || {
            protocol::read_record::<Manifest>(&mut records)
        }) {
            Ok(Some(record)) => record,
            // Not a record: this side's to say, at once.
            Err(error) if !protocol::is_cut(&error) => return Err(error),
            ended => {
                after_control(control_end);
                let Some(record) = ended? else {
                    // The end of the pipe is the end only once the manifest
                    // has ended: a dry run's listing cut short is no
                    // listing.
                    ensure!(
                        check.is_ended(),
                        "the connection closed before the manifest ended"
                    );
                    return Ok(());
                };
                record
            }
        };
        match record {
            Manifest::User { id, name } => {
                if !options.numeric_ids {
                    // Looked up and added in place, without the state.
                    state.ownership.learn(&[(id, name)], &[]);
                }
            }
            Manifest::Group { id, name } => {
                if !options.numeric_ids {
                    state.ownership.learn(&[], &[(id, name)]);
                }
            }
            Manifest::Problem { path: _ } => {
                // The Source reports it; here it is only counted. What it
                // could not read is in the manifest, as unreadable, so it is
                // never taken for extra.
                state.source_problems.fetch_add(1, Ordering::Relaxed);
            }
            Manifest::End => {
                check.end().map_err(refused)?;
                // The decider ends the needs, once it has applied every
                // batch.
                ended.store(true, Ordering::SeqCst);
                drop(parts_in.take());
            }
            Manifest::Root(entry) => {
                check.root(&entry).map_err(refused)?;
                // Applied before any batch comes, so before any is compared.
                let (verdict, warned) = apply::root(&entry, applying)?;
                tell(Event::Root {
                    entry,
                    verdict,
                    warned,
                })?;
            }
            Manifest::In { dir } => {
                let place = check.enter(dir).map_err(refused)?;
                if current.as_ref().is_some_and(|(open, _)| *open != dir) {
                    let open = current.take().expect("there");
                    if !entries.is_empty() {
                        batch(parts_in, open, std::mem::take(&mut entries), false)?;
                    }
                }
                current = Some((dir, place));
            }
            Manifest::Entry(entry) => {
                check.entry(&entry).map_err(refused)?;
                entries.push(entry);
                if entries.len() >= most {
                    let open = current.clone().expect("an In before any entry");
                    batch(parts_in, open, std::mem::take(&mut entries), false)?;
                }
            }
            Manifest::Listed { dir } => {
                let ending = check.listed(dir).map_err(refused)?;
                let theirs = match current.take() {
                    Some(open) if open.0 == dir => std::mem::take(&mut entries),
                    other => {
                        current = other;
                        Vec::new()
                    }
                };
                batch(parts_in, (dir, ending.place), theirs, true)?;
                if !ending.gone.is_empty() {
                    hand(parts_in, Part::Gone(ending.gone))?;
                }
            }
            Manifest::Unlistable { dir } => {
                let ending = check.unlistable(dir).map_err(refused)?;
                hand(parts_in, Part::Unlistable(ending.place.path))?;
                hand(parts_in, Part::Gone(ending.gone))?;
            }
        }
    }
}

/// A record the check of the manifest refuses, the Source's doing; but
/// what the check keeps not going to disk is this side's.
fn refused(error: anyhow::Error) -> anyhow::Error {
    let not_kept = error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(sorted::is_temp_failure)
    });
    if not_kept {
        error
    } else {
        error.context("the Source sent an invalid manifest")
    }
}

/// How many parts may wait for the compare thread: a few, since the stat
/// pool's queue holds the rest.
const PARTS: usize = 4;

/// What the compare thread is handed, in the manifest's order.
enum Part {
    /// The next entries of the directory `dir`, at `place`: its last, once
    /// it is listed.
    Batch {
        dir: FileId,
        place: Place,
        entries: Vec<Entry>,
        last: bool,
        ownership: Arc<Ownership>,
    },
    /// The directory at this path is not listed: what is in it stays as it
    /// is.
    Unlistable(Vec<u8>),
    /// These directories have gone: nothing more of them, nor under them,
    /// comes.
    Gone(Vec<FileId>),
}

/// What the compare thread hands a stat thread: a batch, as it prepared it,
/// to compare; or a directory not listed, which goes on to the decider in
/// its place.
enum Comparison {
    Batch {
        path: Vec<u8>,
        entries: Vec<Entry>,
        /// The directory's last batch.
        last: bool,
        ownership: Arc<Ownership>,
        prepared: Prepared,
    },
    Unlistable(Vec<u8>),
}

/// Holds the root as the comparison opened it, for what is under it to be
/// reached from it; and, with `-x`, takes its filesystem for the one
/// nothing is deleted off. The root's first part is compared before any
/// other, so before anything is deleted.
fn hold_root(directory: &Arc<OwnedFd>, applying: &Applying) {
    applying.dirs.hold_root(Arc::clone(directory));
    #[allow(clippy::unnecessary_cast)]
    if applying.state.one_file_system
        && let Ok(stat) = rustix::fs::fstat(&**directory)
    {
        let _ = applying.state.keep.device.set(stat.st_dev as u64);
    }
}

/// The entries of a part, in a dry run of `sync`, that would replace a
/// directory, which a run deletes with all it holds: those of another kind,
/// to be made, where `directory` has a directory by their name.
fn replacing_directories(
    directory: &OwnedFd,
    entries: &[Entry],
    verdicts: &Verdicts,
) -> HashSet<FileId> {
    entries
        .iter()
        .zip(&verdicts.verdicts)
        .filter(|(entry, verdict)| **verdict == Verdict::Content && entry.kind != Kind::Directory)
        .filter(|(entry, _)| {
            fsys::stat(directory, &entry.name)
                .is_ok_and(|stat| fsys::file_type(&stat) == rustix::fs::FileType::Directory)
        })
        .map(|(entry, _)| entry.id)
        .collect()
}

/// Holds the compared parts of each directory whose extras `sync` deletes,
/// one that the comparison opened, until its last, past the bound on disk;
/// then gives them in order, its extras, and what goes with them, with its
/// first, so that the decider deals with them before anything of its parts.
/// What else is compared goes on at once.
struct Holding {
    delete: bool,
    spill: Spill,
    held: HashMap<Vec<u8>, HeldParts>,
}

/// A directory's parts held: its directory, as the comparison opened it,
/// and its parts, but for that, in order.
struct HeldParts {
    directory: Option<Arc<OwnedFd>>,
    parts: Keyed,
}

/// A part held, as it is kept: all a part has but its path, its directory,
/// and what comes with a last part only.
#[derive(serde::Serialize, serde::Deserialize)]
struct HeldPart {
    entries: Vec<Entry>,
    verdicts: Vec<Verdict>,
    failures: Vec<(FileId, String)>,
    unopened: Option<String>,
    present: Vec<bool>,
    replacing: Vec<FileId>,
    let_in: Option<u32>,
}

impl Holding {
    fn new(delete: bool, spill: Spill) -> Self {
        Self {
            delete,
            spill,
            held: HashMap::new(),
        }
    }

    /// Takes what was compared, and gives back what goes on now.
    fn take(&mut self, event: Event) -> Result<Vec<Event>> {
        let Event::Compared(done) = event else {
            return Ok(vec![event]);
        };
        let held = self.held.contains_key(&done.path);
        // Only a directory there has extras for `sync` to delete.
        if !(held || self.delete && done.directory.is_some()) {
            return Ok(vec![Event::Compared(done)]);
        }
        if !done.last {
            let parts = &mut self
                .held
                .entry(done.path.clone())
                .or_insert_with(|| HeldParts {
                    directory: done.directory.clone(),
                    parts: Keyed::new(&self.spill),
                })
                .parts;
            let number = (parts.len() as u64).to_be_bytes();
            let part = HeldPart {
                entries: done.entries,
                verdicts: done.verdicts.verdicts,
                failures: done.verdicts.failures,
                unopened: done.verdicts.unopened,
                present: done.present,
                replacing: done.replacing.into_iter().collect(),
                let_in: done.let_in,
            };
            parts.push(&number, &postcard::to_stdvec(&part)?)?;
            return Ok(Vec::new());
        }
        let Some(HeldParts {
            directory,
            mut parts,
        }) = self.held.remove(&done.path)
        else {
            return Ok(vec![Event::Compared(done)]);
        };
        // The extras, and what goes with them, with the first part.
        let mut last = done;
        let mut with_first = Some((
            last.extras.take(),
            std::mem::take(&mut last.unreadable),
            std::mem::take(&mut last.kept),
        ));
        let mut given = Vec::with_capacity(parts.len() + 1);
        while let Some((_, part)) = parts.next_record()? {
            let part: HeldPart = postcard::from_bytes(&part).context("a part held")?;
            let (extras, unreadable, kept) = with_first.take().unwrap_or_default();
            given.push(Event::Compared(Compared {
                path: last.path.clone(),
                entries: part.entries,
                last: false,
                directory: directory.clone(),
                verdicts: Verdicts {
                    verdicts: part.verdicts,
                    failures: part.failures,
                    unopened: part.unopened,
                },
                present: part.present,
                extras,
                unreadable,
                kept,
                replacing: part.replacing.into_iter().collect(),
                unnamed: last.unnamed,
                let_in: part.let_in,
                refused: false,
            }));
        }
        if let Some((extras, unreadable, kept)) = with_first {
            last.extras = extras;
            last.unreadable = unreadable;
            last.kept = kept;
        }
        given.push(Event::Compared(last));
        Ok(given)
    }
}

/// A directory of the manifest, compared by a worker.
pub(super) struct Compared {
    pub(super) path: Vec<u8>,
    pub(super) entries: Vec<Entry>,
    /// The directory's last part.
    pub(super) last: bool,
    /// The Destination's directory, as the comparison opened and listed it:
    /// what is deleted from it is deleted from that very directory, however
    /// its path has changed since. None if it was not there.
    pub(super) directory: Option<Arc<OwnedFd>>,
    pub(super) verdicts: Verdicts,
    /// For each entry, whether the directory had something by its name, as
    /// listed.
    pub(super) present: Vec<bool>,
    /// For `sync`, with the directory's last part: what the Destination has
    /// that the Source does not; in a dry run, but for what a run would keep,
    /// which is in `kept`.
    pub(super) extras: Option<io::Result<Extras>>,
    /// For `sync`, with the directory's last batch: the names in it, in any
    /// batch, the Source could not read, whose aliases are not deleted.
    pub(super) unreadable: Vec<Vec<u8>>,
    /// In a dry run of `sync`, with the directory's last part: the extras a
    /// run would keep, as what the Source could not read, under another
    /// name.
    pub(super) kept: Vec<Vec<u8>>,
    /// In a dry run of `sync`: the entries that would replace a directory,
    /// which a run deletes with all it holds.
    pub(super) replacing: HashSet<FileId>,
    /// In a run of `sync`, with the directory's last part, where it has
    /// extras: files are written in it without a name, so not under a
    /// temporary one, beside the extras deleted.
    pub(super) unnamed: bool,
    /// Its permissions were changed to work in it, from these: they are set
    /// again when it is finished, or given back if it is not.
    pub(super) let_in: Option<u32>,
    /// It is the Source's root, which the comparison did not go into.
    pub(super) refused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::tests::entry;

    /// A part of `dir` as compared, of files numbered from `first`, in a
    /// directory the comparison opened if `opened`.
    fn part(dir: &[u8], first: FileId, count: u64, last: bool, opened: bool) -> Compared {
        let entries: Vec<Entry> = (first..first + count)
            .map(|id| entry(id, format!("f{id:05}").as_bytes(), Kind::File, id))
            .collect();
        Compared {
            path: dir.to_vec(),
            present: vec![true; entries.len()],
            verdicts: Verdicts {
                verdicts: vec![Verdict::Content; entries.len()],
                failures: vec![(first, "could not".to_owned())],
                unopened: None,
            },
            entries,
            last,
            directory: opened.then(|| Arc::new(OwnedFd::from(std::fs::File::open("/").unwrap()))),
            extras: None,
            unreadable: Vec::new(),
            kept: Vec::new(),
            replacing: HashSet::from([first]),
            unnamed: last,
            let_in: (first == 1).then_some(0o700),
            refused: false,
        }
    }

    /// What the check of the manifest cannot keep on disk fails this side,
    /// naming `TMPDIR`; anything else it refuses is the Source's invalid
    /// manifest.
    #[test]
    fn the_check_not_keeping_its_own_is_no_invalid_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let mut check = manifest::Check::new(Spill {
            limits: crate::sorted::Limits { held: 1, fan_in: 4 },
            temp: temp.path().join("missing"),
        });
        check.root(&entry(0, b"", Kind::Directory, 0)).unwrap();
        check.enter(0).unwrap();
        let directory = entry(1, b"a", Kind::Directory, 0);
        let error = format!("{:#}", refused(check.entry(&directory).unwrap_err()));
        assert!(!error.contains("invalid manifest"), "{error}");
        assert!(error.contains("keep the directories announced"), "{error}");
        assert!(error.contains("TMPDIR"), "{error}");
        let error = format!("{:#}", refused(check.enter(7).unwrap_err()));
        assert!(
            error.contains("the Source sent an invalid manifest"),
            "{error}"
        );
    }

    /// What a part given on says of itself, to compare.
    fn shown(event: &Event) -> String {
        let Event::Compared(done) = event else {
            return "other".to_owned();
        };
        format!(
            "{} {:?} last {} verdicts {:?} failures {:?} present {} replacing {:?} \
             unnamed {} let_in {:?} extras {:?} unreadable {:?} kept {:?} opened {}",
            String::from_utf8_lossy(&done.path),
            done.entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            done.last,
            done.verdicts.verdicts.len(),
            done.verdicts.failures,
            done.present.len(),
            done.replacing,
            done.unnamed,
            done.let_in,
            done.extras
                .as_ref()
                .map(|extras| extras.as_ref().map(Extras::len).ok()),
            done.unreadable,
            done.kept,
            done.directory.is_some(),
        )
    }

    /// The parts of a directory whose extras `sync` deletes are held until
    /// its last, then given in order, its extras, and what goes with them,
    /// with its first; past the bound, they are held on disk, and come back
    /// the same. Others go on at once.
    #[test]
    fn parts_of_a_directory_whose_extras_are_deleted_are_held_until_its_last() {
        let temp = tempfile::tempdir().unwrap();
        for held in [crate::sorted::HELD, 500] {
            let spill = Spill {
                limits: crate::sorted::Limits { held, fan_in: 4 },
                temp: temp.path().to_owned(),
            };
            let mut holding = Holding::new(true, spill);
            let mut given = Vec::new();
            for index in 0..5 {
                let mut done = part(b"d", 1 + index * 10, 10, index == 4, true);
                if index == 4 {
                    done.extras = Some(Ok(Extras::of(vec![(b"old".to_vec(), false)])));
                    done.unreadable = vec![b"u".to_vec()];
                    done.kept = vec![b"k".to_vec()];
                }
                let now = holding.take(Event::Compared(done)).unwrap();
                if index < 4 {
                    assert!(now.is_empty(), "part {index} given on");
                }
                given.extend(now);
            }
            let shown: Vec<String> = given.iter().map(shown).collect();
            assert_eq!(shown.len(), 5, "{held}");
            assert_eq!(
                shown[0],
                "d [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] last false verdicts 10 failures \
                 [(1, \"could not\")] present 10 replacing {1} unnamed true let_in Some(448) \
                 extras Some(Some(1)) unreadable [[117]] kept [[107]] opened true",
                "{held}"
            );
            for later in &shown[1..4] {
                assert!(later.contains("last false"), "{later}");
                assert!(later.contains("extras None"), "{later}");
                assert!(later.contains("let_in None"), "{later}");
            }
            assert!(shown[4].starts_with("d [41,"), "{}", shown[4]);
            assert!(shown[4].contains("last true"), "{}", shown[4]);
            assert!(shown[4].contains("extras None"), "{}", shown[4]);
            assert!(holding.held.is_empty());
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);

        // Not deleting, or a directory the comparison did not open: at once.
        for (delete, opened) in [(false, true), (true, false)] {
            let mut holding = Holding::new(delete, Spill::default());
            let now = holding
                .take(Event::Compared(part(b"d", 1, 3, false, opened)))
                .unwrap();
            assert_eq!(now.len(), 1);
        }
    }
}
