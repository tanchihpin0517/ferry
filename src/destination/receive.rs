//! Reading the Source's packets: each manifest message is checked and
//! compared as it arrives, by a compare thread and a pool of stat workers,
//! and the comparisons are applied in the manifest's order; data packets go
//! to the frames' decompression.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::compare::{Comparer, Extras, Prepared, Tree, Verdict};
use crate::frame;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::mux::Mux;
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::{self, Control, Needs, PacketType};
use crate::report::{Count, Reporter};
use crate::scan;
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

use super::apply::{apply_root, refuse_another_kind, send_needs, split, take_compared};
use super::write::settled;
use super::{Options, State};

#[allow(clippy::too_many_arguments)]
pub(super) fn read_packets(
    reader: &mut impl Read,
    mux: &Mux,
    comparer: &Comparer,
    root: &Path,
    state: &Mutex<State>,
    options: &Options,
    reporter: &Reporter,
    frames: &mut pool::Submitter<Vec<u8>>,
    stop: &Stop,
) -> Result<()> {
    // The Destination's own tree is scanned as the manifest comes, by a
    // thread of its own, one directory at a time; a pool stats the entries
    // of each part, and the results are applied here, in the manifest's order.
    std::thread::scope(|scope| -> Result<()> {
        // The results are taken here, which also feeds the comparisons:
        // their room is not bounded, or this would wait on itself. They are
        // taken as each packet is.
        let (mut comparisons, mut compared) = pool::ordered_unbounded(
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
                    extras: prepared.extras,
                    let_in,
                    refused,
                }
            },
        );
        let (parts_in, parts_out) = mpsc::sync_channel::<Part>(PARTS);
        let root_bytes = options.root.clone();
        Stages::new(scope, stop).spawn("compare", move |stop| {
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
        let mut parts_in = Some(parts_in);
        // The compared parts of a directory waiting for its last.
        let mut waiting: Vec<Compared> = Vec::new();
        // The last ID the Source was told it need keep no more.
        let mut through: FileId = 0;
        // Nothing of the manifest is acted on before it is checked.
        let mut check = manifest::Check::default();
        let invalid = "the Source sent an invalid manifest";
        // Dropping `frames` when this returns ends the record stream for the
        // writer.
        let mut assembler = frame::Assembler::default();
        let _thread = profile::thread("receive");
        loop {
            // Nothing more of the manifest is acted on once the Destination
            // has stopped.
            stop.check()?;
            while let Some(done) = compared.try_next() {
                profile::wait(profile::State::Apply, || {
                    take_compared(
                        done,
                        &mut waiting,
                        &mut through,
                        mux,
                        root,
                        state,
                        options,
                        reporter,
                        stop,
                    )
                })?;
            }
            let Some(packet) =
                profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
            else {
                // The end of the connection is the end only once the
                // manifest has ended: a dry run's listing cut short is no
                // listing.
                ensure!(
                    check.is_ended(),
                    "the connection closed before the manifest ended"
                );
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
                    Manifest::Problem { path, file } => {
                        // The Source reports it; here it is only counted. What
                        // it could not read is in the manifest, as unreadable,
                        // so it is never taken for extra.
                        let _ = path;
                        state.lock().expect("not poisoned").source_problems += 1;
                        // A needed file it could not open never comes: it is
                        // settled, so its directory is finished without it.
                        // One not waited for, whatever the Source says, is
                        // nothing to settle.
                        if let Some(id) = file {
                            settled(state, id);
                        }
                    }
                    Manifest::End => {
                        check.end().context(invalid)?;
                        // Every comparison comes out before the needs end.
                        drop(parts_in.take());
                        for done in compared.by_ref() {
                            profile::wait(profile::State::Apply, || {
                                take_compared(
                                    done,
                                    &mut waiting,
                                    &mut through,
                                    mux,
                                    root,
                                    state,
                                    options,
                                    reporter,
                                    stop,
                                )
                            })?;
                        }
                        if !options.dry_run {
                            state.lock().expect("not poisoned").leave_all();
                        }
                        mux.message(&Needs::End)?;
                    }
                    Manifest::Root(entry) => {
                        check.root(&entry).context(invalid)?;
                        refuse_another_kind(root, &entry)?;
                        let mut state = state.lock().expect("not poisoned");
                        let verdict = comparer
                            .root(&entry, &state.ownership)
                            .context("compare the destination's root")?;
                        if entry.kind == Kind::Directory && !options.dry_run {
                            state.announce(Vec::new(), entry.clone());
                        }
                        if verdict.is_listed() && options.dry_run {
                            reporter.listed(vec![b".".to_vec()]);
                        }
                        if !options.dry_run {
                            stop.check()?;
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
                        check.directory(&path, &entries, more).context(invalid)?;
                        reporter.add(Count::Entries, entries.len() as u64);
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
                            // The compare thread failed, and said why.
                            Some(false) => return Err(Stopped.into()),
                            Some(true) => {}
                        }
                    }
                },
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
    })
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
    pub(super) verdicts: io::Result<Vec<Verdict>>,
    /// For `sync`: what the Destination has that the Source does not.
    pub(super) extras: Option<io::Result<Extras>>,
    /// Its permissions were changed to work in it, from these: they are set
    /// again when it is finished, or given back if it is not.
    pub(super) let_in: Option<u32>,
    /// It is the Source's root, which the comparison did not go into.
    pub(super) refused: bool,
}
