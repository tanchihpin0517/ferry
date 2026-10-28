//! Reading the Source's packets: each manifest message is checked as it
//! arrives, the root applied, and the parts compared by a compare thread
//! and a pool of stat workers, for the decider to apply in the manifest's
//! order; the blocks in data packets go to the decoders, which hand them on
//! in the order the Source numbered them, however they came; the hashes of
//! files sent in segments, what the Source could not read, and the blocks'
//! end, go to the decider, in the order they came, with the Source's
//! messages.

use std::collections::{BTreeSet, HashSet};
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender};

use crate::block;
use crate::compare::{Extras, Prepared, Tree, Verdict, Verdicts};
use crate::fsys;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::{self, BlocksEnd, Control, FileHash, ManifestDecoder, PacketType};
use crate::report::Count;
use crate::scan;
use crate::stage::Stages;
use crate::stop::Stopped;

use super::apply::{
    self, Applying, Outlets, apply, on_this_filesystem, without_aliases_of_unreadable,
};
use super::decider::Event;

/// What the receiver hands the decoders: a block, still encoded, at the
/// place its number says.
pub(super) type Incoming = Vec<u8>;

/// What came of the blocks, on every stream: their numbers, and how many the
/// Source said it sent, once it says.
#[derive(Default)]
pub(super) struct Received {
    numbers: Numbers,
    end: Option<u64>,
}

impl Received {
    /// Block `sequence` came.
    fn came(&mut self, sequence: u64) -> Result<()> {
        self.numbers.came(sequence, self.end)
    }

    /// The Source says it sent `end` blocks.
    fn end(&mut self, end: u64) -> Result<()> {
        ensure!(
            self.end.replace(end).is_none(),
            "the Source said twice that its blocks ended"
        );
        self.numbers.end(end)
    }
}

/// Reads the blocks the Source sends on one of the streams but the first,
/// numbered `index`, which carry blocks only, until its end: each block to
/// the decoders, at the place its number says, as [`Received`] counts it.
pub(super) fn receive_blocks(
    reader: &mut impl Read,
    index: usize,
    received: &Mutex<Received>,
    mut blocks: pool::Submitter<Incoming>,
    reporter: &crate::report::Reporter,
) -> Result<()> {
    let _thread = profile::thread("receive");
    let mut assembler = block::Assembler::default();
    while let Some(packet) =
        profile::wait(profile::State::ReadConnection, || protocol::receive(reader))
            .with_context(|| format!("stream {index}"))?
    {
        ensure!(
            packet.kind == PacketType::Data,
            "the Source sent a {:?} packet on stream {index}, which carries blocks only",
            packet.kind
        );
        let payload = packet.into_payload();
        reporter.add(Count::Wire, payload.len() as u64);
        for block in assembler.push(&payload)? {
            let sequence = block::sequence(&block);
            received.lock().expect("not poisoned").came(sequence)?;
            blocks.submit_at(sequence, block)?;
        }
    }
    Ok(())
}

/// The numbers of the blocks that came: each once, and below how many the
/// Source said it sent, once it says. What came in order is kept as a
/// count, and only what came ahead of a gap one by one: blocks come in
/// order on each stream.
#[derive(Default)]
struct Numbers {
    /// Every block below this came.
    below: u64,
    /// Those above it that came.
    above: BTreeSet<u64>,
}

impl Numbers {
    /// Block `sequence` came, of `end`, if the Source said how many.
    fn came(&mut self, sequence: u64, end: Option<u64>) -> Result<()> {
        // Numbered from 0, a Source never comes to the last: the one after
        // it would be past what a number holds.
        ensure!(
            sequence < u64::MAX,
            "the Source sent block {sequence}, past any it can number"
        );
        if let Some(end) = end {
            ensure!(
                sequence < end,
                "the Source sent block {sequence} of the {end} it said it sent"
            );
        }
        ensure!(
            sequence >= self.below && self.above.insert(sequence),
            "the Source sent block {sequence} twice"
        );
        while self.above.remove(&self.below) {
            self.below += 1;
        }
        Ok(())
    }

    /// The Source says it sent `end` blocks: none that came may be past
    /// them.
    fn end(&self, end: u64) -> Result<()> {
        let highest = self.above.last().copied().or(self.below.checked_sub(1));
        if let Some(highest) = highest {
            ensure!(
                highest < end,
                "the Source sent block {highest} of the {end} it said it sent"
            );
        }
        Ok(())
    }
}

/// Reads the Source's packets, and hands each on: the manifest to the
/// comparison and the decider, the blocks to the decoders. What the decider
/// says goes to the stages in `outlets`.
pub(super) fn read_packets(
    reader: &mut impl Read,
    applying: &Applying,
    outlets: Outlets,
    events: Sender<Event>,
    told: Receiver<Event>,
    blocks: pool::Submitter<Incoming>,
    received: &Mutex<Received>,
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
    // of each part, and the decider applies the results, in the manifest's
    // order, as they come: this thread only reads packets, and hands on,
    // but for the root, which it applies before any part comes.
    // `ended` is set as the manifest ends, before the parts do: the
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
            |Comparison {
                 path,
                 entries,
                 last,
                 ownership,
                 prepared,
                 unreadable,
             }: Comparison| {
                let directory = prepared.directory.as_ref().ok().cloned().flatten();
                let let_in = prepared.let_in;
                let refused = prepared.refused;
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
                        let listed = on_this_filesystem(directory, listed, &state.keep);
                        let (listed, aliases) =
                            without_aliases_of_unreadable(directory, &unreadable, listed);
                        extras = Some(Ok(listed));
                        kept = aliases;
                    }
                }
                Compared {
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
                }
            },
        );
        let (parts_in, parts_out) = crossbeam_channel::bounded::<Part>(PARTS);
        let root_bytes = options.root.clone();
        stages.spawn("compare", move |stop| {
            // What the Source does not have is found before anything here
            // changes: each directory is listed before its parts are applied.
            let mut tree = Tree::new(
                root_bytes,
                options.delete,
                !options.dry_run,
                options.protect.first().copied(),
            );
            // The names the Source could not read, in the directory whose
            // parts come: its extras keep what is one of them under another
            // name.
            let mut unreadable = Vec::new();
            while let Ok(part) = profile::wait(profile::State::Work, || parts_out.recv()) {
                // Going into a directory may change its permissions.
                stop.check()?;
                let prepared = tree.part(&part.path, &part.entries, part.more);
                if part.path.is_empty()
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
                        .insert(part.path.clone(), mode);
                }
                if options.delete {
                    unreadable.extend(
                        part.entries
                            .iter()
                            .filter(|entry| entry.kind == Kind::Unreadable)
                            .map(|entry| entry.name.clone()),
                    );
                }
                let comparison = Comparison {
                    path: part.path,
                    entries: part.entries,
                    last: !part.more,
                    ownership: part.ownership,
                    prepared,
                    // With its extras, which only its last part has.
                    unreadable: if part.more {
                        Vec::new()
                    } else {
                        std::mem::take(&mut unreadable)
                    },
                };
                comparisons.submit(comparison)?;
            }
            Ok(())
        });
        // The comparisons, put back in order, for the decider to wait on
        // with what it is told.
        let (in_order, applied) = crossbeam_channel::bounded::<Compared>(PARTS);
        stages.spawn("compared", move |_| {
            for done in compared {
                if in_order.send(done).is_err() {
                    break;
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
            blocks,
            received,
        ))
    })
}

/// The loop of [`read_packets`]: reads each packet, checks it, and hands it
/// on, until the connection ends. Its failure is kept as the cause before
/// the blocks end with it: the writing, finding them ended short, takes that
/// for a consequence, not for the cause in its place.
fn receive(
    reader: &mut impl Read,
    applying: &Applying,
    events: &Sender<Event>,
    mut parts_in: Option<crossbeam_channel::Sender<Part>>,
    ended: &AtomicBool,
    mut blocks: pool::Submitter<Incoming>,
    received: &Mutex<Received>,
) -> Result<()> {
    let result = receive_packets(
        reader,
        applying,
        events,
        &mut parts_in,
        ended,
        &mut blocks,
        received,
    );
    let result = applying.stop.failing(result);
    drop(blocks);
    drop(parts_in);
    result
}

/// What [`receive`] does, until it is done or fails.
fn receive_packets(
    reader: &mut impl Read,
    applying: &Applying,
    events: &Sender<Event>,
    parts_in: &mut Option<crossbeam_channel::Sender<Part>>,
    ended: &AtomicBool,
    blocks: &mut pool::Submitter<Incoming>,
    received: &Mutex<Received>,
) -> Result<()> {
    let Applying {
        state,
        options,
        reporter,
        stop,
        ..
    } = *applying;
    // Nothing of the manifest is acted on before it is checked.
    let mut check = manifest::Check::default();
    let invalid = "the Source sent an invalid manifest";
    // `blocks`, dropped as [`receive`] returns, ends them for the writing,
    // which the decider may be waiting on.
    let mut assembler = block::Assembler::default();
    // Where the data is compressed, so is the manifest, as one stream.
    let mut manifests = ManifestDecoder::new()?;
    let _thread = profile::thread("receive");
    // The decider is told; once it is gone, it said why.
    let tell = |event: Event| events.send(event).map_err(|_| Stopped);
    loop {
        // Nothing more of the manifest is acted on once the Destination
        // has stopped.
        stop.check()?;
        let Some(packet) =
            profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
        else {
            // The end of the connection is the end only once the manifest
            // has ended: a dry run's listing cut short is no listing.
            ensure!(
                check.is_ended(),
                "the connection closed before the manifest ended"
            );
            return Ok(());
        };
        match packet.kind {
            PacketType::Manifest | PacketType::ManifestStream => {
                let message = if packet.kind == PacketType::Manifest {
                    packet.decode::<Manifest>()?
                } else {
                    profile::wait(profile::State::Decompress, || manifests.decode(&packet))?
                };
                match message {
                    Manifest::Names { users, groups } => {
                        if !options.numeric_ids {
                            // Looked up and added in place, without the state.
                            let ownership = Arc::clone(&state.ownership);
                            ownership.learn(&users, &groups);
                        }
                    }
                    Manifest::Problem { path, file } => {
                        // The Source reports it; here it is only counted. What
                        // it could not read is in the manifest, as unreadable,
                        // so it is never taken for extra.
                        let _ = path;
                        state.source_problems.fetch_add(1, Ordering::Relaxed);
                        // A needed file it could not open never comes: it is
                        // settled, so its directory is finished without it.
                        if let Some(id) = file {
                            tell(Event::Unread(id))?;
                        }
                    }
                    Manifest::End => {
                        check.end().context(invalid)?;
                        // The decider ends the needs, once it has applied every
                        // part.
                        ended.store(true, Ordering::SeqCst);
                        drop(parts_in.take());
                    }
                    Manifest::Root(entry) => {
                        check.root(&entry).context(invalid)?;
                        // Applied before any part comes, so before any is
                        // compared.
                        let (verdict, warned) = apply::root(&entry, applying)?;
                        tell(Event::Root {
                            entry,
                            verdict,
                            warned,
                        })?;
                    }
                    Manifest::Directory {
                        path,
                        entries,
                        more,
                    } => {
                        check.directory(&path, &entries, more).context(invalid)?;
                        reporter.add(Count::Entries, entries.len() as u64);
                        let ownership = Arc::clone(&state.ownership);
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
                            // The compare thread failed, and said why.
                            Some(false) => return Err(Stopped.into()),
                            Some(true) => {}
                        }
                    }
                }
            }
            PacketType::Control => match packet.decode::<Control>()? {
                // The Source's failure is the cause, and it knows of it.
                Control::Error { message } => {
                    stop.peer_failed(anyhow!("the Source failed: {message}"));
                    return Err(Stopped.into());
                }
                other => bail!("unexpected {other:?} from the Source"),
            },
            PacketType::Data => {
                let payload = packet.into_payload();
                reporter.add(Count::Wire, payload.len() as u64);
                for block in assembler.push(&payload)? {
                    let sequence = block::sequence(&block);
                    received.lock().expect("not poisoned").came(sequence)?;
                    blocks.submit_at(sequence, block)?;
                }
            }
            PacketType::FileHash => tell(Event::Hash(packet.decode::<FileHash>()?))?,
            PacketType::BlocksEnd => {
                let BlocksEnd { blocks: end } = packet.decode()?;
                received.lock().expect("not poisoned").end(end)?;
                tell(Event::BlocksEnd(end))?;
            }
            other => bail!("unexpected {other:?} packet from the Source"),
        }
    }
}

/// How many parts may wait for the compare thread: a few, since the stat
/// pool's queue holds the rest.
const PARTS: usize = 4;

/// A directory of the manifest, or a part of one, for the compare thread.
struct Part {
    pub(super) path: Vec<u8>,
    pub(super) entries: Vec<Entry>,
    pub(super) more: bool,
    pub(super) ownership: Arc<Ownership>,
}

/// A part, as the compare thread prepared it, for a stat thread to compare.
struct Comparison {
    pub(super) path: Vec<u8>,
    pub(super) entries: Vec<Entry>,
    /// The directory's last part.
    pub(super) last: bool,
    pub(super) ownership: Arc<Ownership>,
    pub(super) prepared: Prepared,
    /// For `sync`, with the directory's last part: the names in it, in any
    /// part, the Source could not read.
    pub(super) unreadable: Vec<Vec<u8>>,
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
    /// For `sync`, with the directory's last part: the names in it, in any
    /// part, the Source could not read, whose aliases are not deleted.
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
