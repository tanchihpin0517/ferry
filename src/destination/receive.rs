//! Reading the Source's packets: each manifest message is checked as it
//! arrives, and its parts compared by a compare thread and a pool of stat
//! workers, for the applier to apply in the manifest's order; data packets
//! go to the frames' decompression.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender};

use crate::compare::{Extras, Prepared, Tree, Verdicts};
use crate::frame;
use crate::manifest::{self, Entry, Manifest};
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::{self, Control, ManifestDecoder, PacketType};
use crate::report::Count;
use crate::scan;
use crate::stage::Stages;
use crate::stop::Stopped;

use super::apply::{Applying, Directories, Event, apply};

/// Reads the Source's packets, and hands each on: the manifest to the
/// comparison and the applier, the data to the frames' decompression.
#[allow(clippy::too_many_arguments)]
pub(super) fn read_packets(
    reader: &mut impl Read,
    applying: &Applying,
    directories: Directories,
    events: Sender<Event>,
    told: Receiver<Event>,
    frames: pool::Submitter<Vec<u8>>,
) -> Result<()> {
    let Applying {
        comparer,
        state,
        options,
        reporter: _,
        stop,
        ..
    } = *applying;
    // The Destination's own tree is scanned as the manifest comes, by a
    // thread of its own, one directory at a time; a pool stats the entries
    // of each part, and the applier applies the results, in the manifest's
    // order, as they come: this thread only reads packets, and hands on.
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
                    present: prepared.present,
                    extras: prepared.extras,
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
            while let Ok(part) = profile::wait(profile::State::Work, || parts_out.recv()) {
                // Going into a directory may change its permissions.
                stop.check()?;
                let prepared = tree.part(&part.path, &part.entries, part.more);
                // Kept as they are changed, to be given back should the
                // transfer fail before it is finished, applied or not.
                if let Some(mode) = prepared.let_in {
                    state
                        .let_in
                        .lock()
                        .expect("not poisoned")
                        .insert(part.path.clone(), mode);
                }
                let comparison = Comparison {
                    path: part.path,
                    entries: part.entries,
                    last: !part.more,
                    ownership: part.ownership,
                    prepared,
                };
                comparisons.submit(comparison)?;
            }
            Ok(())
        });
        // The comparisons, put back in order, for the applier to wait on
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
            apply(directories, applied, told, ended, applying)
        });
        // Kept before the comparisons end, which the applier waits on.
        stop.failing(receive(
            reader,
            applying,
            &events,
            Some(parts_in),
            ended,
            frames,
        ))
    })
}

/// The loop of [`read_packets`]: reads each packet, checks it, and hands it
/// on, until the connection ends. Its failure is kept as the cause before
/// the record stream ends with it: the parser, finding the stream ended
/// short, takes that for a consequence, not for the cause in its place.
fn receive(
    reader: &mut impl Read,
    applying: &Applying,
    events: &Sender<Event>,
    mut parts_in: Option<crossbeam_channel::Sender<Part>>,
    ended: &AtomicBool,
    mut frames: pool::Submitter<Vec<u8>>,
) -> Result<()> {
    let received = receive_packets(reader, applying, events, &mut parts_in, ended, &mut frames);
    let received = applying.stop.failing(received);
    drop(frames);
    drop(parts_in);
    received
}

/// What [`receive`] does, until it is done or fails.
fn receive_packets(
    reader: &mut impl Read,
    applying: &Applying,
    events: &Sender<Event>,
    parts_in: &mut Option<crossbeam_channel::Sender<Part>>,
    ended: &AtomicBool,
    frames: &mut pool::Submitter<Vec<u8>>,
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
    // `frames`, dropped as [`receive`] returns, ends the record stream for
    // the parser, which the applier may be waiting on.
    let mut assembler = frame::Assembler::default();
    // Where the data is compressed, so is the manifest, as one stream.
    let mut manifests = ManifestDecoder::new()?;
    let _thread = profile::thread("receive");
    // The applier is told; once it is gone, it said why.
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
                        // The applier ends the needs, once it has applied every
                        // part.
                        ended.store(true, Ordering::SeqCst);
                        drop(parts_in.take());
                    }
                    Manifest::Root(entry) => {
                        check.root(&entry).context(invalid)?;
                        tell(Event::Root(entry))?;
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
                for frame in assembler.push(&payload)? {
                    frames.submit(frame)?;
                }
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
    /// For `sync`: what the Destination has that the Source does not.
    pub(super) extras: Option<io::Result<Extras>>,
    /// Its permissions were changed to work in it, from these: they are set
    /// again when it is finished, or given back if it is not.
    pub(super) let_in: Option<u32>,
    /// It is the Source's root, which the comparison did not go into.
    pub(super) refused: bool,
}
