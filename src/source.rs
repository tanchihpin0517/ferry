//! The Source peer: walks its tree, sends the manifest, and sends the files
//! the Destination says it needs.
//!
//! Three things run at once: the walk sends the manifest, the main thread
//! reads the Destination's answers, and a sender thread streams the needed
//! files. They share the connection's writer a packet at a time.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result, bail};

use crate::fsys;
use crate::manifest::{self, FileId, Kind, Manifest, Timestamp};
use crate::protocol::{self, Control, Needs, PacketType};
use crate::record::{self, Header, Trailer};
use crate::walk::{self, Sink};

/// How many bytes of the record stream go in one data packet.
const DATA_PACKET: usize = 256 * 1024;

/// What a run found, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    pub entries: u64,
    pub needed: u64,
    /// Paths on the Source that could not be read.
    pub problems: u64,
}

/// An entry the Source has sent in the manifest, by ID.
struct Known {
    path: Vec<u8>,
    kind: Kind,
    size: u64,
    mtime: Timestamp,
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

/// Runs the Source over a connection. `on_need` is told each path the
/// Destination needs, in manifest order. In a dry run nothing is sent but the
/// manifest.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    root: &OsStr,
    dry_run: bool,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<Summary> {
    let writer = Mutex::new(writer);
    let known: Mutex<Vec<Known>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| -> Result<Summary> {
        let walker = scope.spawn(|| -> Result<u64> {
            let mut sink = ManifestSink {
                writer: &writer,
                known: &known,
                problems: 0,
            };
            walk::walk(root, &mut sink)?;
            Ok(sink.problems)
        });
        let (ids, sender) = if dry_run {
            (None, None)
        } else {
            let (ids, needed) = mpsc::channel();
            let sender = scope.spawn(|| send_files(root, &known, &writer, needed));
            (Some(ids), Some(sender))
        };

        let mut summary = Summary::default();
        let received = receive(reader, &known, ids, on_need, &mut summary);
        let problems = walker.join().expect("the walk panicked");
        let sent = sender.map(|sender| sender.join().expect("the file sender panicked"));
        received?;
        summary.problems = problems?;
        sent.transpose()?;
        summary.entries = known.lock().expect("not poisoned").len() as u64;
        Ok(summary)
    })
}

/// Reads the Destination's answers until it is done: the needs, in a dry run,
/// or the needs and then its report that it wrote everything.
fn receive(
    reader: &mut impl Read,
    known: &Mutex<Vec<Known>>,
    mut ids: Option<Sender<FileId>>,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
    summary: &mut Summary,
) -> Result<()> {
    let dry_run = ids.is_none();
    loop {
        let Some(packet) = protocol::receive(reader)? else {
            bail!("the connection closed before the transfer finished");
        };
        match packet.kind {
            PacketType::Needs => match packet.decode::<Needs>()? {
                Needs::Files(needed) => {
                    for id in needed {
                        let shown = {
                            let known = known.lock().expect("not poisoned");
                            let entry = known.get(id as usize).with_context(|| {
                                format!("the Destination needs unknown ID {id}")
                            })?;
                            display(&entry.path, entry.kind == Kind::Directory)
                        };
                        on_need(&shown)?;
                        summary.needed += 1;
                        if let Some(ids) = &ids {
                            // The sender stops only on an error, which it
                            // reports itself.
                            let _ = ids.send(id);
                        }
                    }
                }
                Needs::End if dry_run => return Ok(()),
                // No more needs: the sender finishes the stream.
                Needs::End => ids = None,
            },
            PacketType::Control => match packet.decode::<Control>()? {
                Control::Done => return Ok(()),
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
}

impl<W: Write> Sink for ManifestSink<'_, W> {
    fn manifest(&mut self, message: Manifest) -> Result<()> {
        {
            let mut known = self.known.lock().expect("not poisoned");
            let mut record = |path: Vec<u8>, entry: &manifest::Entry| {
                debug_assert_eq!(entry.id as usize, known.len(), "IDs follow manifest order");
                known.push(Known {
                    path,
                    kind: entry.kind.clone(),
                    size: entry.size,
                    mtime: entry.mtime,
                });
            };
            match &message {
                Manifest::Root(entry) => record(Vec::new(), entry),
                Manifest::Directory { path, entries } => {
                    for entry in entries {
                        record(manifest::join(path, &entry.name), entry);
                    }
                }
                Manifest::End => {}
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
        eprintln!(
            "ferry: cannot read {}: {error}",
            String::from_utf8_lossy(&display(path, false))
        );
        Ok(())
    }
}

/// Streams the record of each needed file, in the order needed, and ends the
/// stream once the needs end.
fn send_files<W: Write>(
    root: &OsStr,
    known: &Mutex<Vec<Known>>,
    writer: &Mutex<W>,
    needed: Receiver<FileId>,
) -> Result<()> {
    let mut stream = DataWriter {
        writer,
        buffer: Vec::with_capacity(DATA_PACKET),
    };
    for id in needed {
        let file = {
            let known = known.lock().expect("not poisoned");
            let entry = &known[id as usize];
            if entry.kind != Kind::File {
                continue;
            }
            (entry.path.clone(), entry.size, entry.mtime)
        };
        send_file(root, id, &file, &mut stream).with_context(|| {
            format!("send {}", String::from_utf8_lossy(&display(&file.0, false)))
        })?;
    }
    record::write_header(&mut stream, &Header::End)?;
    stream.finish()
}

/// Streams one file's record. A file that changes while it is read is still
/// sent at the size the manifest gave, and marked changed.
fn send_file(
    root: &OsStr,
    id: FileId,
    (path, size, mtime): &(Vec<u8>, u64, Timestamp),
    stream: &mut impl Write,
) -> Result<()> {
    let mut file = open_source_file(root, path)?;
    record::write_header(stream, &Header::File { id, size: *size })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; DATA_PACKET];
    let mut remaining = *size;
    while remaining > 0 {
        let wanted = buffer.len().min(remaining as usize);
        let count = file.read(&mut buffer[..wanted])?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        stream.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    // A file that shrank is padded, so the record keeps its size.
    let mut changed = remaining > 0;
    while remaining > 0 {
        let count = buffer.len().min(remaining as usize);
        buffer[..count].fill(0);
        hasher.update(&buffer[..count]);
        stream.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    // A file that grew, or whose size or mtime is no longer what the manifest
    // said, changed too.
    changed |= file.read(&mut buffer[..1])? > 0;
    let metadata = file.metadata()?;
    changed |= metadata.len() != *size || modified(&metadata) != *mtime;
    let trailer = Trailer {
        hash: *hasher.finalize().as_bytes(),
        changed,
    };
    record::write_trailer(stream, &trailer)
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
    buffer: Vec<u8>,
}

impl<W: Write> DataWriter<'_, W> {
    fn send(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock().expect("not poisoned");
        protocol::send_data(&mut *writer, &self.buffer)?;
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
        let count = bytes.len().min(DATA_PACKET - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..count]);
        if self.buffer.len() == DATA_PACKET {
            self.send().map_err(io::Error::other)?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send().map_err(io::Error::other)
    }
}
