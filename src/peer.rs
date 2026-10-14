//! The two peers of a transfer.
//!
//! For now the client is always the Source and the server the Destination
//! (a push), and every transfer is a dry run: the Destination compares and
//! says what it needs, and nothing is copied.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::compare::Comparer;
use crate::manifest::{self, Kind, Manifest};
use crate::protocol::{self, Control, Needs, PacketType};
use crate::session::{self, Connection};
use crate::walk::{self, Sink};

/// What a run found, for the client to report.
#[derive(Debug, Default)]
pub struct Summary {
    pub entries: u64,
    pub needed: u64,
    /// Paths on the Source that could not be read.
    pub problems: u64,
}

/// A path as the client prints it: `.` for the root, `/` after a directory.
fn display(path: &[u8], is_directory: bool) -> Vec<u8> {
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

/// The Source's side of a dry run: walks `root`, sends the manifest, and
/// writes each path the Destination needs to `out`, in manifest order.
pub fn source_dry_run(
    connection: Connection,
    root: &OsStr,
    out: &mut impl Write,
) -> Result<Summary> {
    let (mut reader, writer, server) = connection.split();
    // Every path sent, indexed by ID, so needs can be printed by path.
    let paths: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

    let summary = std::thread::scope(|scope| -> Result<Summary> {
        let sender = scope.spawn(|| -> Result<u64> {
            let mut sink = ManifestSink {
                writer,
                paths: &paths,
                problems: 0,
            };
            walk::walk(root, &mut sink)?;
            sink.writer.flush().context("send the manifest")?;
            // Dropping the writer closes the server's input once it has
            // everything.
            Ok(sink.problems)
        });

        let mut summary = Summary::default();
        let received = receive_needs(&mut reader, &paths, out, &mut summary);
        let problems = sender.join().expect("the manifest sender panicked");
        received?;
        summary.problems = problems?;
        summary.entries = paths.lock().expect("not poisoned").len() as u64;
        Ok(summary)
    })?;
    server.wait()?;
    Ok(summary)
}

fn receive_needs(
    reader: &mut impl Read,
    paths: &Mutex<Vec<Vec<u8>>>,
    out: &mut impl Write,
    summary: &mut Summary,
) -> Result<()> {
    loop {
        let Some(packet) = protocol::receive(reader)? else {
            bail!("the server closed the connection before the comparison finished");
        };
        match packet.kind {
            PacketType::Needs => match packet.decode::<Needs>()? {
                Needs::Files(ids) => {
                    let paths = paths.lock().expect("not poisoned");
                    for id in ids {
                        let path = paths
                            .get(id as usize)
                            .with_context(|| format!("the server needs unknown ID {id}"))?;
                        out.write_all(path)?;
                        out.write_all(b"\n")?;
                        summary.needed += 1;
                    }
                }
                Needs::End => return Ok(()),
            },
            PacketType::Control => match packet.decode::<Control>()? {
                Control::Error { message } => bail!("the server failed: {message}"),
                other => bail!("unexpected {other:?} from the server"),
            },
            other => bail!("unexpected {other:?} packet from the server"),
        }
    }
}

/// Sends the walk's manifest and records each entry's path by ID.
struct ManifestSink<'a, W> {
    writer: W,
    paths: &'a Mutex<Vec<Vec<u8>>>,
    problems: u64,
}

impl<W: Write> Sink for ManifestSink<'_, W> {
    fn manifest(&mut self, message: Manifest) -> Result<()> {
        {
            let mut paths = self.paths.lock().expect("not poisoned");
            let mut record = |id: u64, path: &[u8], kind: &Kind| {
                debug_assert_eq!(id as usize, paths.len(), "IDs follow manifest order");
                paths.push(display(path, *kind == Kind::Directory));
            };
            match &message {
                Manifest::Root(entry) => record(entry.id, b"", &entry.kind),
                Manifest::Directory { path, entries } => {
                    for entry in entries {
                        record(entry.id, &manifest::join(path, &entry.name), &entry.kind);
                    }
                }
                Manifest::End => {}
            }
        }
        protocol::send(&mut self.writer, &message)?;
        // Each directory goes out as soon as it is read, so the Destination
        // compares while the walk goes on.
        self.writer.flush().context("send the manifest")?;
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

/// The server's side: the handshake, then the Destination's comparison.
/// A failure is sent to the client before the server exits.
pub fn serve(reader: &mut impl Read, writer: &mut impl Write, build: &str) -> Result<()> {
    let hello = session::server_handshake(reader, writer, build)?;
    let result = destination_dry_run(reader, writer, hello.destination, hello.dry_run);
    if let Err(error) = &result {
        // The client may already be gone; the error is returned either way.
        let _ = protocol::send(
            writer,
            &Control::Error {
                message: format!("{error:#}"),
            },
        );
        let _ = writer.flush();
    }
    result
}

fn destination_dry_run(
    reader: &mut impl Read,
    writer: &mut impl Write,
    root: Vec<u8>,
    dry_run: bool,
) -> Result<()> {
    if !dry_run {
        bail!("copying files is not implemented yet; only --dry-run works");
    }
    let comparer = Comparer::new(root);
    loop {
        let Some(packet) = protocol::receive(reader)? else {
            bail!("the client closed the connection before the manifest ended");
        };
        let needs = match packet.decode::<Manifest>()? {
            Manifest::Root(entry) => {
                if comparer
                    .root(&entry)
                    .context("compare the destination's root")?
                {
                    vec![entry.id]
                } else {
                    Vec::new()
                }
            }
            Manifest::Directory { path, entries } => {
                comparer.directory(&path, &entries).with_context(|| {
                    format!("compare {}", String::from_utf8_lossy(&display(&path, true)))
                })?
            }
            Manifest::End => break,
        };
        if !needs.is_empty() {
            protocol::send(writer, &Needs::Files(needs))?;
            writer.flush()?;
        }
    }
    protocol::send(writer, &Needs::End)?;
    writer.flush()?;
    // Wait for the client to close the connection.
    while protocol::receive(reader)?.is_some() {}
    Ok(())
}
