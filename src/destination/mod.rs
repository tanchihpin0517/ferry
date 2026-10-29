//! The Destination peer: compares the manifest with its own tree, says what it
//! needs, and makes its tree match.
//!
//! Its stages run under one [`Stop`], and pass work on over channels. The
//! main thread reads the manifest's pipe, checks each record, applies the
//! root, and hands the rest on; a thread reads the data's pipe, and one
//! control (`receive`). A compare thread and a pool of stat workers compare
//! each batch of a directory's entries with the tree, a part of the
//! directory, which the decider takes once the directory's last has come,
//! all its parts at once. The decider takes each
//! comparison as it comes, and what the other stages tell it, one at a
//! time, on a thread of its own, and says what to do, doing no I/O itself
//! (`decider`): it has what a part changes made by up to 16 makers, a
//! directory each, which create the directories, symlinks, and special
//! files it needs and set attributes that differ; it asks for a part's
//! files as it is compared, and alone follows each directory until it is
//! done, and each file until it is settled (`apply`). A directory's extras
//! are deleted on a lane of its own, which only that directory's parts
//! wait for (`apply`, `crate::remove`). A pool checks the records of files'
//! bytes, each against its hash, which a feeder hands to the decider in
//! order; it hands each file, or segment of a large one, to
//! one of 16 writers, and the files written to the renamers in batches,
//! each named once its part is made (`write`, `rename`). Up to 16
//! finishers set each directory's attributes and mtime once it is done, as
//! the decider hands it over, one only after every directory under it,
//! since writing into a directory changes its mtime (`finish`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::block;
use crate::compare::Comparer;
use crate::dirs::{self, DirCache};
use crate::fsys;
use crate::mux::Mux;
use crate::owners::Ownership;
use crate::pool::{self, Profiled};
use crate::profile;
use crate::protocol::{self, FileKey, PacketType};
use crate::report::{Reporter, Stage};
use crate::stage::Stages;
use crate::stop::{Stop, Stopped};

mod apply;
mod decider;
mod finish;
mod receive;
mod rename;
#[cfg(test)]
mod tests;
mod warnings;
mod write;

use apply::{Applying, Outlets};
use finish::restore_let_in;
use receive::{Incoming, read_manifest, receive_control, receive_data};
use warnings::Warnings;
use write::Arrival;

pub struct Options {
    /// The Destination's root, as path bytes.
    pub root: Vec<u8>,
    pub dry_run: bool,
    pub modify_window: i64,
    pub numeric_ids: bool,
    /// Delete what the Source does not have.
    pub delete: bool,
    /// Where to report what is found and what goes wrong: to the client, on
    /// the server's control connection; or nowhere.
    pub report: Reporter,
    /// The Source's root, then each directory above it, when the client
    /// found them on the same kernel: none of them is deleted, nor the
    /// Source's root gone into. Reaching one stops the transfer.
    pub protect: Vec<FileKey>,
    /// How many entries of a directory are compared at a time, at most; 0
    /// for [`crate::scan::BATCH`].
    pub batch_entries: usize,
    /// How much of the files' bytes that came it may hold, as the hello says,
    /// which is the data's window too; see [`crate::protocol::data_credit`].
    pub data_credit: u64,
    /// Delete in no directory on another filesystem than the root's: `-x`.
    pub one_file_system: bool,
}

/// What went wrong, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    /// Entries that could not be made to match, or deleted.
    pub failed: u64,
    /// Paths the Source could not read.
    pub problems: u64,
}

/// Reports a line for standard error.
fn tell(reporter: &Reporter, message: std::fmt::Arguments) {
    reporter.say(message.to_string());
}

/// What the stages share; each directory's progress is the decider's
/// alone.
struct State {
    ownership: Arc<Ownership>,
    /// What could not be done, from any stage, told at the end.
    warnings: Mutex<Warnings>,
    /// Whether to delete what the Source does not have.
    delete: bool,
    /// What is never deleted; see [`Options::protect`]. With `-x`, the
    /// root's filesystem, to delete on no other, once the root is opened.
    keep: Arc<fsys::Keep>,
    /// Stay on the root's filesystem: [`Options::one_file_system`].
    one_file_system: bool,
    /// Entries that could not be made to match, or deleted.
    failures: AtomicU64,
    /// Paths the Source could not read.
    source_problems: AtomicU64,
    /// The directories let into, and not finished yet, with the permissions
    /// they had: given back should the transfer fail before.
    let_in: Mutex<HashMap<Vec<u8>, u32>>,
}

impl State {
    /// Warns of what an executor could not set: its owner, told, says so.
    fn warn(&self, warned: apply::Warned) {
        if warned.is_empty() {
            return;
        }
        let mut warnings = self.warnings.lock().expect("not poisoned");
        for (warning, shown) in warned {
            warnings.add(warning, &shown);
        }
    }

    fn new(delete: bool) -> Self {
        Self {
            ownership: Arc::new(Ownership::here()),
            warnings: Mutex::default(),
            delete,
            keep: Arc::new(fsys::Keep::default()),
            one_file_system: false,
            failures: AtomicU64::new(0),
            source_problems: AtomicU64::new(0),
            let_in: Mutex::default(),
        }
    }
}

/// Checks what the data's reader hands on: a record of a file's bytes,
/// against what the Source made of them. What the Destination holds of it is
/// let go of once its bytes are: once written, or failed; or at once, should
/// they not match.
fn check(incoming: Incoming) -> Result<Arrival> {
    let Incoming {
        buffer,
        record,
        held,
    } = incoming;
    let part = block::part(buffer, record, move || drop(held)).context("a record of no file")?;
    Ok(Arrival(vec![part]))
}

/// What the Destination holds of the files' bytes that came, as
/// [`block::cost`] counts it, from when the data's reader takes a record of
/// them to when they are let go of: once written, or failed. The reader waits while
/// the bound is held, so what comes waits in the data's pipe, whose window
/// is then not given back, and the Source waits: what the Source sent and
/// this side holds stays bounded, and nothing past the reader waits for
/// room.
pub(super) struct Holding {
    held: Mutex<u64>,
    bound: u64,
    /// Rung as bytes are let go of.
    freed: (Sender<()>, Receiver<()>),
}

impl Holding {
    fn new(bound: u64) -> Self {
        Self {
            held: Mutex::new(0),
            bound,
            freed: crossbeam_channel::bounded(1),
        }
    }

    /// Holds `cost` more, waiting while the bound is held, but for a record
    /// alone, which always fits; [`Stopped`] once the Destination stops.
    pub(super) fn take(self: &Arc<Self>, cost: u64, stop: &Stop) -> Result<Held> {
        loop {
            {
                let mut held = self.held.lock().expect("not poisoned");
                if *held == 0 || *held + cost <= self.bound {
                    *held += cost;
                    return Ok(Held {
                        holding: Arc::clone(self),
                        cost,
                    });
                }
            }
            profile::wait(profile::State::ToCheck, || {
                crossbeam_channel::select! {
                    recv(self.freed.1) -> _ => Ok(()),
                    recv(stop.signal()) -> _ => Err(Stopped),
                }
            })?;
        }
    }
}

/// What the Destination holds of one record, let go of as it is dropped.
pub(super) struct Held {
    holding: Arc<Holding>,
    cost: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        *self.holding.held.lock().expect("not poisoned") -= self.cost;
        let _ = self.holding.freed.0.try_send(());
    }
}

/// What the Destination reads from the Source: the manifest's pipe, the
/// data's, and control.
pub struct Reading {
    pub manifest: Box<dyn Read + Send>,
    pub data: Box<dyn crate::mux::In>,
    pub control: Box<dyn Read + Send>,
}

/// Runs the Destination over one stream of the Source's packets, which it
/// answers on `writer`, as the peers sent them before they had pipes: for
/// the tests and the fuzzer. Each packet goes on the pipe its kind does,
/// as it is read, a manifest's and a data's as a record, an empty data
/// packet ending the data, and what the Destination writes goes on as it
/// writes it, each of its answers as a packet of its own.
pub fn run<W: Write + Send>(
    reader: &mut (impl Read + Send),
    writer: &mut W,
    options: &Options,
) -> Result<Summary> {
    let pipe = || crossbeam_channel::unbounded::<io::Result<Vec<u8>>>();
    let ((manifest, manifest_in), (data, data_in), (control, control_in)) =
        (pipe(), pipe(), pipe());
    let mut data = Some(data);
    let (out, written) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (answers, answers_in) = crossbeam_channel::unbounded::<io::Result<Vec<u8>>>();
    let mux = Mux::for_destination(out.clone(), answers, Vec::new());
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut records = io::BufReader::new(crate::mux::Piped::new(answers_in));
            while let Ok(Some(answer)) = protocol::read_record::<protocol::Answer>(&mut records) {
                let Ok(packet) = protocol::encode(&answer) else {
                    return;
                };
                if out.send(packet).is_err() {
                    return;
                }
            }
        });
        scope.spawn(move || {
            loop {
                let packet = match protocol::receive(reader) {
                    Ok(Some(packet)) => packet,
                    Ok(None) => return,
                    // Broken, as a stream cut: so are the pipes it carried.
                    Err(error) => {
                        let broken = || {
                            io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                format!("the stream broke: {error:#}"),
                            )
                        };
                        let _ = manifest.send(Err(broken()));
                        if let Some(data) = &data {
                            let _ = data.send(Err(broken()));
                        }
                        return;
                    }
                };
                // An empty data packet ends the data.
                if packet.kind == PacketType::Data && packet.size() == 0 {
                    data = None;
                    continue;
                }
                // A manifest's packet, and a data's, as the record it holds,
                // whatever it holds.
                let records = match packet.kind {
                    PacketType::Manifest => Some(&manifest),
                    PacketType::Data => data.as_ref(),
                    _ => None,
                };
                if let Some(pipe) = records {
                    let payload = packet.into_payload();
                    let mut record = Vec::with_capacity(payload.len() + 5);
                    let mut length = payload.len();
                    loop {
                        let byte = (length & 0x7f) as u8;
                        length >>= 7;
                        if length == 0 {
                            record.push(byte);
                            break;
                        }
                        record.push(byte | 0x80);
                    }
                    record.extend_from_slice(&payload);
                    let _ = pipe.send(Ok(record));
                    continue;
                }
                let pipe = match packet.kind {
                    PacketType::Control => &control,
                    PacketType::Data => continue,
                    _ => &manifest,
                };
                let kind = packet.kind;
                let payload = packet.into_payload();
                let mut bytes = vec![kind as u8];
                bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&payload);
                let _ = pipe.send(Ok(bytes));
            }
        });
        scope.spawn(move || {
            // Once the writer fails, what comes is let go of: no one waits.
            let mut failed = false;
            for packet in written {
                failed = failed || writer.write_all(&packet).is_err();
            }
        });
        let reading = Reading {
            manifest: Box::new(crate::mux::Piped::new(manifest_in)),
            data: Box::new(crate::mux::Piped::new(data_in)),
            control: Box::new(crate::mux::Piped::new(control_in)),
        };
        run_on_pipes(reading, mux, options)
    })
}

/// Runs the Destination on the pipes it reads, the Source's, writing its
/// own on `mux`. A dry run reports each path it lists, in manifest order.
/// The Destination reports the counts and what it cannot do; the Source
/// reports the paths it cannot read.
pub fn run_on_pipes(reading: Reading, mux: Mux, options: &Options) -> Result<Summary> {
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));
    // The directories under the root, shared by the stages that work there.
    let dirs = &DirCache::new(root.as_os_str(), dirs::KEPT);
    // The first failure of any stage stops them all, and is the one told.
    let stop = Stop::new(Some(mux.clone()));
    let reporter = options.report.clone();
    // What the stages share, each part of it shared as it may be.
    let state = &State {
        keep: Arc::new(fsys::Keep::protecting(options.protect.clone())),
        one_file_system: options.one_file_system,
        ..State::new(options.delete)
    };
    let Reading {
        mut manifest,
        mut data,
        mut control,
    } = reading;
    let applying = &Applying {
        mux: &mux,
        comparer: &comparer,
        root,
        state,
        options,
        reporter: &reporter,
        dirs,
        stop: &stop,
    };

    std::thread::scope(|scope| {
        let stages = Stages::new(scope, &stop);
        // What the Source may have sent and this side holds of it.
        let credit = protocol::data_credit(options.data_credit);
        let holding = Arc::new(Holding::new(credit));
        // Bounded by what is held, as everything after the data's reader is.
        let (data_out, arrivals) = crossbeam_channel::unbounded::<Arrival>();
        // What the readers tell the decider.
        let (events, told) = crossbeam_channel::unbounded();
        // The files' bytes go to the decider, in a run.
        let outlets = Outlets {
            arrivals: (!options.dry_run).then_some(arrivals),
        };

        // The records of files' bytes are checked by a pool, and fed to the
        // decider in order by a thread of their own. The pool has room for
        // as many as can be held, so the data's reader never waits for it.
        let checking = (credit / block::cost(0)) as usize + 1;
        let (records, checked) = pool::gauged(
            &stages,
            pool::threads(8),
            checking,
            Some(reporter.gauge(Stage::Decompress, 16)),
            Profiled {
                role: "check",
                work: profile::State::Check,
                full: profile::State::ToCheck,
            },
            check,
        );
        let feeding = stages.spawn("feeder", move |stop| {
            for arrival in checked {
                // Kept before the decider's data ends, as this returns.
                let arrival = stop.failing(arrival)?;
                if profile::wait(profile::State::ToDecider, || data_out.send(arrival)).is_err() {
                    // The decider stopped, and says why itself.
                    break;
                }
            }
            Ok(())
        });
        // Closed as control ends, which what finds another pipe ended waits
        // for.
        let (control_open, control_end) = crossbeam_channel::bounded::<()>(0);
        stages.spawn("control", move |stop| {
            let _open = control_open;
            receive_control(&mut control, stop)
        });
        let (more, data_control_end) = (events.clone(), control_end.clone());
        stages.spawn("receive data", move |_| {
            receive_data(
                &mut data,
                applying,
                &more,
                &holding,
                records,
                &data_control_end,
            )
        });
        let read = read_manifest(&mut manifest, applying, outlets, events, told, &control_end);
        if let Err(error) = read {
            stop.fail(error);
        }
        feeding.join();
        stop.close();
        reporter.close();
    });
    if let Err(error) = stop.result() {
        restore_let_in(state, dirs);
        return Err(error);
    }
    Ok(Summary {
        failed: state.failures.load(Ordering::Relaxed),
        problems: state.source_problems.load(Ordering::Relaxed),
    })
}

/// The directory a root is in, as its path names it: `.` for one of a name
/// alone, as `b` or `b/`.
fn parent_of(root: &Path) -> &Path {
    match root.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The directory a root that is not a directory is in, and its name there.
fn root_parent(root: &Path) -> Result<(OwnedFd, Vec<u8>)> {
    let parent = parent_of(root);
    let name = root
        .file_name()
        .with_context(|| format!("{} has no file name", root.display()))?;
    let parent =
        fsys::open_root(parent.as_os_str()).map_err(|error| missing_parent(root, error))?;
    Ok((parent, name.as_bytes().to_vec()))
}

/// An error making or opening the root, said as its directory missing where
/// it is: then nothing can be copied, and the transfer stops.
fn missing_parent(root: &Path, error: io::Error) -> anyhow::Error {
    if error.kind() != io::ErrorKind::NotFound {
        return error.into();
    }
    let parent = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    MissingParent(format!(
        "the directory {}, where {} goes, does not exist",
        parent.display(),
        root.display()
    ))
    .into()
}

/// The directory the root goes in does not exist.
#[derive(Debug)]
struct MissingParent(String);

impl std::fmt::Display for MissingParent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MissingParent {}

/// Whether an error is, at its root, a deletion refused because what was to
/// be deleted is to be protected; see [`fsys::Protected`].
fn is_protected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(fsys::is_protected)
    })
}

/// Whether an error ends the transfer, rather than failing its entry alone:
/// the drive is full, or out of the user's quota, or read-only, so that
/// every entry after it would fail the same way.
fn ends_the_transfer(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(io_ends_the_transfer)
    })
}

/// As [`ends_the_transfer`], for an I/O error.
fn io_ends_the_transfer(error: &io::Error) -> bool {
    use rustix::io::Errno;
    Errno::from_io_error(error)
        .is_some_and(|errno| [Errno::NOSPC, Errno::DQUOT, Errno::ROFS].contains(&errno))
}
