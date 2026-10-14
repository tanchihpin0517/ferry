//! The Source peer: walks its tree, sends the manifest, and sends the files
//! the Destination says it needs.
//!
//! Three things run at once: the walk sends the manifest, the main thread
//! reads the Destination's answers, and a sender thread streams the needed
//! files. They share the connection's writer a packet at a time.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result, bail};

use crate::frame;
use crate::fsys;
use crate::manifest::{self, FileId, Kind, Manifest, Timestamp};
use crate::names::{self, Database};
use crate::protocol::{self, Control, Needs, PacketType, Results};
use crate::record::{self, Header, Trailer};
use crate::walk::{self, Sink};

/// How many bytes a file is read in at a time.
const READ_CHUNK: usize = 256 * 1024;

/// The zstd level frames are compressed at.
const LEVEL: i32 = 3;

/// What a run found, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    pub entries: u64,
    pub needed: u64,
    /// Paths on the Source that could not be read.
    pub problems: u64,
    /// Entries the Destination could not make match.
    pub failed: u64,
}

/// An entry the Source has sent in the manifest, by ID.
struct Known {
    path: Vec<u8>,
    kind: Kind,
}

/// What the file sender does next.
enum Job {
    Send(FileId),
    /// The needs are over: end the first pass.
    PassEnd,
    /// The files to send again; the stream ends after them.
    Resend(Vec<FileId>),
}

pub struct Options<'a> {
    /// The Source's root.
    pub root: &'a OsStr,
    pub dry_run: bool,
    /// Send no owners' names.
    pub numeric_ids: bool,
    /// Compress the record stream.
    pub compress: bool,
    /// Print what goes wrong.
    pub report: bool,
}

/// A path as the client prints it: `.` for the root, `/` after a directory.
pub fn display(path: &[u8], is_directory: bool) -> Vec<u8> {
    let mut shown = if path.is_empty() {
        b".".to_vec()
    } else {
        path.to_vec()
    };
    if is_directory && !path.is_empty() {
        shown.push(b'/');
    }
    shown
}

/// Runs the Source over a connection. `on_need` is told each path a dry run
/// lists, in manifest order. In a dry run nothing is sent but the manifest.
/// Owners' names are sent unless `numeric_ids`. With `report`, what goes wrong
/// is printed: the Source reports when it is the client, and the Destination
/// when it is.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<Summary> {
    let Options {
        root,
        dry_run,
        numeric_ids,
        compress,
        report,
    } = *options;
    let writer = Mutex::new(writer);
    let known: Mutex<Vec<Known>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| -> Result<Summary> {
        let walker = scope.spawn(|| -> Result<u64> {
            let mut sink = ManifestSink {
                writer: &writer,
                known: &known,
                problems: 0,
                report,
                numeric_ids,
                named_users: HashSet::new(),
                named_groups: HashSet::new(),
            };
            walk::walk(root, &mut sink)?;
            Ok(sink.problems)
        });
        let (jobs, sender) = if dry_run {
            (None, None)
        } else {
            let (jobs, queue) = mpsc::channel();
            let sender = scope.spawn(|| send_files(root, &known, &writer, queue, compress, report));
            (Some(jobs), Some(sender))
        };

        let mut summary = Summary::default();
        let received = receive(reader, &known, jobs, report, on_need, &mut summary);
        let problems = walker.join().expect("the walk panicked");
        let sent = sender.map(|sender| sender.join().expect("the file sender panicked"));
        received?;
        summary.problems = problems? + sent.transpose()?.unwrap_or(0);
        summary.entries = known.lock().expect("not poisoned").len() as u64;
        Ok(summary)
    })
}

/// Reads the Destination's answers until it is done: the needs, in a dry run,
/// or the needs, the results, and then its report that it wrote everything.
fn receive(
    reader: &mut impl Read,
    known: &Mutex<Vec<Known>>,
    mut jobs: Option<Sender<Job>>,
    report: bool,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
    summary: &mut Summary,
) -> Result<()> {
    let dry_run = jobs.is_none();
    // The sender stops only on an error, which it reports itself, so a job it
    // can no longer take is dropped.
    let give = |jobs: &Option<Sender<Job>>, job: Job| {
        if let Some(jobs) = jobs {
            let _ = jobs.send(job);
        }
    };
    loop {
        let Some(packet) = protocol::receive(reader)? else {
            bail!("the connection closed before the transfer finished");
        };
        match packet.kind {
            PacketType::Needs => match packet.decode::<Needs>()? {
                Needs::Files(needed) => {
                    for id in needed {
                        let (shown, is_file) = {
                            let known = known.lock().expect("not poisoned");
                            let entry = known.get(id as usize).with_context(|| {
                                format!("the Destination needs unknown ID {id}")
                            })?;
                            let shown = display(&entry.path, entry.kind == Kind::Directory);
                            (shown, entry.kind == Kind::File)
                        };
                        on_need(&shown)?;
                        summary.needed += 1;
                        // Only files' content is sent; the Destination makes
                        // the rest itself.
                        if is_file {
                            give(&jobs, Job::Send(id));
                        }
                    }
                }
                Needs::Attributes(changed) => {
                    for id in changed {
                        let shown = {
                            let known = known.lock().expect("not poisoned");
                            let entry = known.get(id as usize).with_context(|| {
                                format!("the Destination lists unknown ID {id}")
                            })?;
                            display(&entry.path, entry.kind == Kind::Directory)
                        };
                        on_need(&shown)?;
                        summary.needed += 1;
                    }
                }
                Needs::Deletes(extras) => {
                    for (path, is_directory) in extras {
                        let mut shown = b"deleting ".to_vec();
                        shown.extend(display(&path, is_directory));
                        on_need(&shown)?;
                    }
                }
                Needs::End if dry_run => return Ok(()),
                Needs::End => give(&jobs, Job::PassEnd),
            },
            PacketType::Results => match packet.decode::<Results>()? {
                Results::Failed { id, message } => {
                    let shown = {
                        let known = known.lock().expect("not poisoned");
                        known
                            .get(id as usize)
                            .map(|entry| display(&entry.path, entry.kind == Kind::Directory))
                            .unwrap_or_else(|| format!("#{id}").into_bytes())
                    };
                    if report {
                        eprintln!(
                            "ferry: cannot copy {}: {message}",
                            String::from_utf8_lossy(&shown)
                        );
                    }
                    summary.failed += 1;
                }
                Results::NotDeleted { path, message } => {
                    if report {
                        eprintln!(
                            "ferry: cannot delete {}: {message}",
                            String::from_utf8_lossy(&display(&path, false))
                        );
                    }
                    summary.failed += 1;
                }
                Results::Resend(again) => {
                    give(&jobs, Job::Resend(again));
                    // The stream ends after these, so the sender needs nothing
                    // more.
                    jobs = None;
                }
            },
            PacketType::Control => match packet.decode::<Control>()? {
                Control::Done => return Ok(()),
                Control::Warning { message } if report => eprintln!("ferry: warning: {message}"),
                Control::Warning { .. } => {}
                Control::Error { message } => bail!("the Destination failed: {message}"),
                other => bail!("unexpected {other:?} from the Destination"),
            },
            other => bail!("unexpected {other:?} packet from the Destination"),
        }
    }
}

/// Sends the walk's manifest, and remembers each entry by ID.
struct ManifestSink<'a, W> {
    writer: &'a Mutex<W>,
    known: &'a Mutex<Vec<Known>>,
    problems: u64,
    report: bool,
    numeric_ids: bool,
    /// IDs whose names have been sent, or looked up and found to have none.
    named_users: HashSet<u32>,
    named_groups: HashSet<u32>,
}

impl<W: Write> ManifestSink<'_, W> {
    /// The names of the IDs in `entries` not named before, to send before
    /// them.
    fn new_names(&mut self, entries: &[manifest::Entry]) -> Option<Manifest> {
        if self.numeric_ids {
            return None;
        }
        let mut users: Vec<u32> = entries.iter().map(|entry| entry.uid).collect();
        let mut groups: Vec<u32> = entries.iter().map(|entry| entry.gid).collect();
        users.retain(|uid| self.named_users.insert(*uid));
        groups.retain(|gid| self.named_groups.insert(*gid));
        let users: Vec<(u32, String)> = names::names(Database::Users, &users).into_iter().collect();
        let groups: Vec<(u32, String)> = names::names(Database::Groups, &groups)
            .into_iter()
            .collect();
        (!users.is_empty() || !groups.is_empty()).then_some(Manifest::Names { users, groups })
    }
}

impl<W: Write> Sink for ManifestSink<'_, W> {
    fn manifest(&mut self, message: Manifest) -> Result<()> {
        let names = match &message {
            Manifest::Root(entry) => self.new_names(std::slice::from_ref(entry)),
            Manifest::Directory { entries, .. } => self.new_names(entries),
            _ => None,
        };
        if let Some(names) = names {
            protocol::send(&mut *self.writer.lock().expect("not poisoned"), &names)?;
        }
        {
            let mut known = self.known.lock().expect("not poisoned");
            let mut record = |path: Vec<u8>, entry: &manifest::Entry| {
                debug_assert_eq!(entry.id as usize, known.len(), "IDs follow manifest order");
                known.push(Known {
                    path,
                    kind: entry.kind.clone(),
                });
            };
            match &message {
                Manifest::Root(entry) => record(Vec::new(), entry),
                Manifest::Directory { path, entries } => {
                    for entry in entries {
                        record(manifest::join(path, &entry.name), entry);
                    }
                }
                Manifest::Names { .. } | Manifest::Problem { .. } | Manifest::End => {}
            }
        }
        let mut writer = self.writer.lock().expect("not poisoned");
        protocol::send(&mut *writer, &message)?;
        // Each directory goes out as soon as it is read, so the Destination
        // compares while the walk goes on.
        writer.flush().context("send the manifest")?;
        Ok(())
    }

    fn problem(&mut self, path: &[u8], error: io::Error) -> Result<()> {
        self.problems += 1;
        if self.report {
            eprintln!(
                "ferry: cannot read {}: {error}",
                String::from_utf8_lossy(&display(path, false))
            );
        }
        // The Destination must know, so as not to take what is missing for
        // deleted.
        let problem = Manifest::Problem {
            path: path.to_vec(),
        };
        protocol::send(&mut *self.writer.lock().expect("not poisoned"), &problem)?;
        Ok(())
    }
}

/// Streams the record of each needed file, in the order needed; ends the
/// first pass when the needs end; sends again the files the Destination asks
/// for; and ends the stream. A file that cannot be read is reported and
/// skipped. Returns how many could not be read.
fn send_files<W: Write>(
    root: &OsStr,
    known: &Mutex<Vec<Known>>,
    writer: &Mutex<W>,
    queue: Receiver<Job>,
    compress: bool,
    report: bool,
) -> Result<u64> {
    let mut stream = DataWriter {
        writer,
        buffer: Vec::with_capacity(frame::FRAME),
        compress,
    };
    let mut problems = 0;
    let mut send = |id: FileId, stream: &mut DataWriter<W>| -> Result<()> {
        let path = known.lock().expect("not poisoned")[id as usize]
            .path
            .clone();
        match send_file(root, id, &path, stream) {
            Ok(()) => Ok(()),
            // An error in the stream itself ends the transfer.
            Err(SendError::Stream(error)) => Err(error),
            // Any other is this file's alone.
            Err(SendError::Read(error)) => {
                problems += 1;
                if report {
                    eprintln!(
                        "ferry: cannot read {}: {error}",
                        String::from_utf8_lossy(&display(&path, false))
                    );
                }
                // The Destination must know too: it may be the one reporting,
                // and `sync` must not delete.
                stream.finish()?;
                let problem = Manifest::Problem { path };
                let mut writer = writer.lock().expect("not poisoned");
                protocol::send(&mut *writer, &problem)?;
                writer.flush()?;
                Ok(())
            }
        }
    };
    loop {
        // With nothing more to send right now, the frame goes out unfilled,
        // so that the last files of a run, or of a slow walk, never wait.
        let job = match queue.try_recv() {
            Ok(job) => job,
            Err(mpsc::TryRecvError::Empty) => {
                stream.finish()?;
                match queue.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        };
        match job {
            Job::Send(id) => send(id, &mut stream)?,
            Job::PassEnd => {
                record::write_header(&mut stream, &Header::PassEnd)?;
                // The Destination answers only once it has read this.
                stream.finish()?;
            }
            Job::Resend(ids) => {
                for id in ids {
                    send(id, &mut stream)?;
                }
                break;
            }
        }
    }
    record::write_header(&mut stream, &Header::End)?;
    stream.finish()?;
    Ok(problems)
}

enum SendError {
    /// The file could not be read before its record began.
    Read(io::Error),
    /// The record stream could not be written.
    Stream(anyhow::Error),
}

/// Streams one file's record, at the size and mtime the file has when it is
/// opened. A file that changes while it is read is still sent at that size,
/// and marked changed. A file that cannot be opened sends nothing.
fn send_file(
    root: &OsStr,
    id: FileId,
    path: &[u8],
    stream: &mut impl Write,
) -> std::result::Result<(), SendError> {
    let mut file = open_source_file(root, path).map_err(SendError::Read)?;
    let opened = file.metadata().map_err(SendError::Read)?;
    let size = opened.len();
    let mtime = modified(&opened);
    let streamed = (|| -> Result<()> {
        record::write_header(stream, &Header::File { id, size, mtime })?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0; READ_CHUNK];
        let mut remaining = size;
        let mut changed = false;
        while remaining > 0 {
            let wanted = buffer.len().min(remaining as usize);
            // A read error once the record has begun counts as a change: the
            // record is finished with padding, and the Destination discards
            // it.
            let count = file.read(&mut buffer[..wanted]).unwrap_or_else(|_| {
                changed = true;
                0
            });
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            stream.write_all(&buffer[..count])?;
            remaining -= count as u64;
        }
        // A file that shrank is padded, so the record keeps its size.
        changed |= remaining > 0;
        while remaining > 0 {
            let count = buffer.len().min(remaining as usize);
            buffer[..count].fill(0);
            hasher.update(&buffer[..count]);
            stream.write_all(&buffer[..count])?;
            remaining -= count as u64;
        }
        // A file that grew, or whose size or mtime changed while it was read,
        // changed too.
        changed |= file.read(&mut buffer[..1]).map_or(true, |count| count > 0);
        changed |= file.metadata().map_or(true, |closed| {
            closed.len() != size || modified(&closed) != mtime
        });
        let trailer = Trailer {
            hash: *hasher.finalize().as_bytes(),
            changed,
        };
        record::write_trailer(stream, &trailer)
    })();
    streamed.map_err(SendError::Stream)
}

fn modified(metadata: &std::fs::Metadata) -> Timestamp {
    use std::os::unix::fs::MetadataExt;
    Timestamp {
        seconds: metadata.mtime(),
        nanoseconds: metadata.mtime_nsec() as u32,
    }
}

/// Opens a file of the tree by its path relative to the root, never following
/// a symlink below the root. The empty path is the root itself.
fn open_source_file(root: &OsStr, path: &[u8]) -> io::Result<File> {
    if path.is_empty() {
        return File::open(root);
    }
    let (directory, name) = match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&b""[..], path),
    };
    let directory = fsys::open_path(fsys::open_root(root)?, directory)?;
    fsys::open_file(directory, name)
}

/// Writes the record stream as data packets, a packet's worth at a time.
struct DataWriter<'a, W> {
    writer: &'a Mutex<W>,
    /// The frame being filled.
    buffer: Vec<u8>,
    compress: bool,
}

impl<W: Write> DataWriter<'_, W> {
    /// Sends the frame being filled, if it holds anything.
    fn send(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let frame = frame::encode(&self.buffer, self.compress, LEVEL)?;
        let mut writer = self.writer.lock().expect("not poisoned");
        protocol::send_data(&mut *writer, &frame)?;
        writer.flush().context("send data")?;
        self.buffer.clear();
        Ok(())
    }

    /// Sends whatever is buffered, ending the stream's last packet.
    fn finish(&mut self) -> Result<()> {
        self.send()
    }
}

impl<W: Write> Write for DataWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(frame::FRAME - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        if self.buffer.len() == frame::FRAME {
            self.send().map_err(io::Error::other)?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send().map_err(io::Error::other)
    }
}
