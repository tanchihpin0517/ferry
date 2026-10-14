//! The Destination peer: compares the manifest with its own tree, says what it
//! needs, and writes the files the Source sends.
//!
//! The main thread reads packets: it compares each manifest message as it
//! arrives, creates the directories it needs, and hands data packets to a
//! writer thread, which writes the files in the record stream.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rustix::fs::FileType;

use crate::compare::Comparer;
use crate::fsys;
use crate::manifest::{self, Entry, FileId, Kind, Manifest};
use crate::protocol::{self, Control, Needs, PacketType};
use crate::record::{self, ChannelReader, Header};
use crate::source::display;

pub struct Options {
    /// The Destination's root, as path bytes.
    pub root: Vec<u8>,
    pub dry_run: bool,
    pub modify_window: i64,
}

/// Where a needed file goes: a name in a directory under the root, or the
/// root itself when the root is a file.
struct Target {
    directory: Vec<u8>,
    entry: Entry,
}

/// Runs the Destination over a connection. `on_need` is told each path it
/// needs, in manifest order.
pub fn run<W: Write + Send>(
    reader: &mut impl Read,
    writer: &mut W,
    options: &Options,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let writer = Mutex::new(writer);
    let targets: Mutex<HashMap<FileId, Target>> = Mutex::new(HashMap::new());
    let comparer = Comparer::new(options.root.clone(), options.modify_window);
    let root = Path::new(OsStr::from_bytes(&options.root));

    std::thread::scope(|scope| -> Result<()> {
        let (data, stream) = mpsc::channel::<Vec<u8>>();
        let files = (!options.dry_run).then(|| {
            scope.spawn(|| -> Result<()> {
                let result = write_files(root, &targets, ChannelReader::new(stream));
                let mut writer = writer.lock().expect("not poisoned");
                match &result {
                    Ok(()) => protocol::send(&mut *writer, &Control::Done)?,
                    Err(error) => protocol::send(
                        &mut *writer,
                        &Control::Error {
                            message: format!("{error:#}"),
                        },
                    )?,
                }
                writer.flush()?;
                result
            })
        });

        let read = read_packets(
            reader, &writer, &comparer, root, &targets, options, data, on_need,
        );
        let written = files.map(|files| files.join().expect("the file writer panicked"));
        read?;
        written.transpose()?;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn read_packets<W: Write>(
    reader: &mut impl Read,
    writer: &Mutex<&mut W>,
    comparer: &Comparer,
    root: &Path,
    targets: &Mutex<HashMap<FileId, Target>>,
    options: &Options,
    data: mpsc::Sender<Vec<u8>>,
    on_need: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    // Dropping `data` when this returns ends the record stream for the writer.
    while let Some(packet) = protocol::receive(reader)? {
        match packet.kind {
            PacketType::Manifest => {
                let needs = match packet.decode::<Manifest>()? {
                    Manifest::Root(entry) => {
                        let needed = comparer
                            .root(&entry)
                            .context("compare the destination's root")?;
                        if needed {
                            on_need(b".")?;
                            if !options.dry_run {
                                prepare_root(root, &entry, targets)?;
                            }
                            vec![entry.id]
                        } else {
                            Vec::new()
                        }
                    }
                    Manifest::Directory { path, entries } => {
                        let needed = comparer.directory(&path, &entries).with_context(|| {
                            format!("compare {}", String::from_utf8_lossy(&display(&path, true)))
                        })?;
                        let wanted: HashSet<FileId> = needed.iter().copied().collect();
                        for entry in entries
                            .into_iter()
                            .filter(|entry| wanted.contains(&entry.id))
                        {
                            on_need(&display(
                                &manifest::join(&path, &entry.name),
                                entry.kind == Kind::Directory,
                            ))?;
                            if !options.dry_run {
                                prepare(root, &path, entry, targets)?;
                            }
                        }
                        needed
                    }
                    Manifest::End => {
                        let mut writer = writer.lock().expect("not poisoned");
                        protocol::send(&mut *writer, &Needs::End)?;
                        writer.flush()?;
                        continue;
                    }
                };
                if !needs.is_empty() {
                    let mut writer = writer.lock().expect("not poisoned");
                    protocol::send(&mut *writer, &Needs::Files(needs))?;
                    writer.flush()?;
                }
            }
            PacketType::Data => {
                if data.send(packet.into_payload()).is_err() {
                    // The writer stopped, and reports why itself.
                    bail!("the file writer stopped");
                }
            }
            other => bail!("unexpected {other:?} packet from the Source"),
        }
    }
    Ok(())
}

/// Makes ready for the root: a directory is created now; a file is written
/// when it arrives.
fn prepare_root(
    root: &Path,
    entry: &Entry,
    targets: &Mutex<HashMap<FileId, Target>>,
) -> Result<()> {
    match entry.kind {
        Kind::Directory => match std::fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => bail!("{} exists and is not a directory", root.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(root).with_context(|| format!("create {}", root.display()))
            }
            Err(error) => Err(error).with_context(|| format!("inspect {}", root.display())),
        },
        Kind::File => {
            targets.lock().expect("not poisoned").insert(
                entry.id,
                Target {
                    directory: Vec::new(),
                    entry: entry.clone(),
                },
            );
            Ok(())
        }
        // Other kinds of root come with the rest of `rsync -a`.
        _ => Ok(()),
    }
}

/// Makes ready for a needed entry of the directory at `directory`.
fn prepare(
    root: &Path,
    directory: &[u8],
    entry: Entry,
    targets: &Mutex<HashMap<FileId, Target>>,
) -> Result<()> {
    match entry.kind {
        Kind::Directory => {
            let parent = fsys::open_path(fsys::open_root(root.as_os_str())?, directory)?;
            make_directory(&parent, &entry.name, entry.mode).with_context(|| {
                format!(
                    "create {}",
                    String::from_utf8_lossy(&display(
                        &manifest::join(directory, &entry.name),
                        true
                    ))
                )
            })
        }
        Kind::File => {
            targets.lock().expect("not poisoned").insert(
                entry.id,
                Target {
                    directory: directory.to_vec(),
                    entry,
                },
            );
            Ok(())
        }
        // Symlinks, devices, FIFOs, and sockets come with the rest of
        // `rsync -a`.
        _ => Ok(()),
    }
}

/// Creates a directory, replacing whatever else is there but a directory.
/// The directory is made writable by its owner, so its entries can be written.
fn make_directory(parent: impl AsFd, name: &[u8], mode: u32) -> Result<()> {
    match fsys::stat(&parent, name) {
        Ok(stat) if fsys::file_type(&stat) == FileType::Directory => return Ok(()),
        Ok(_) => fsys::remove(&parent, name, false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fsys::make_directory(&parent, name, (mode & 0o7777) | 0o700)?;
    Ok(())
}

/// Writes every file in the record stream, until it ends.
fn write_files(
    root: &Path,
    targets: &Mutex<HashMap<FileId, Target>>,
    mut stream: ChannelReader,
) -> Result<()> {
    loop {
        match record::read_header(&mut stream)? {
            Header::File { id, size } => {
                let target = targets
                    .lock()
                    .expect("not poisoned")
                    .remove(&id)
                    .with_context(|| format!("the Source sent unneeded ID {id}"))?;
                let shown = display(
                    &manifest::join(&target.directory, &target.entry.name),
                    false,
                );
                write_file(root, &target, size, &mut stream)
                    .with_context(|| format!("write {}", String::from_utf8_lossy(&shown)))?;
            }
            Header::End => return Ok(()),
        }
    }
}

/// Writes one file: into a temporary file beside it, checked against the
/// Source's hash, then renamed into place.
fn write_file(root: &Path, target: &Target, size: u64, stream: &mut impl Read) -> Result<()> {
    let (directory, name) = target_directory(root, target)?;
    let (mut file, temporary) = fsys::create_temporary(&directory, &name)?;
    let written = (|| -> Result<()> {
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0; 256 * 1024];
        let mut remaining = size;
        while remaining > 0 {
            let count = buffer.len().min(remaining as usize);
            stream
                .read_exact(&mut buffer[..count])
                .context("read the record stream")?;
            hasher.update(&buffer[..count]);
            file.write_all(&buffer[..count])?;
            remaining -= count as u64;
        }
        let trailer = record::read_trailer(stream)?;
        if trailer.changed {
            bail!("the file changed on the Source while it was read");
        }
        if *hasher.finalize().as_bytes() != trailer.hash {
            bail!("the content's hash does not match the Source's");
        }
        apply_metadata(&file, &target.entry)?;
        clear_the_way(&directory, &name)?;
        fsys::rename(&directory, &temporary, &name)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fsys::remove(&directory, &temporary, false);
    }
    drop(file);
    written
}

/// The directory a target is written in, and its name there.
fn target_directory(root: &Path, target: &Target) -> Result<(OwnedFd, Vec<u8>)> {
    if target.entry.name.is_empty() {
        // The root is a file: its directory is the root's parent.
        let parent = match root.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let name = root
            .file_name()
            .with_context(|| format!("{} has no file name", root.display()))?;
        return Ok((
            fsys::open_root(parent.as_os_str())?,
            name.as_bytes().to_vec(),
        ));
    }
    let directory = fsys::open_path(fsys::open_root(root.as_os_str())?, &target.directory)?;
    Ok((directory, target.entry.name.clone()))
}

/// Removes an empty directory where a file is about to go. A directory with
/// anything in it stays, and the file is not written.
fn clear_the_way(directory: impl AsFd, name: &[u8]) -> Result<()> {
    match fsys::stat(&directory, name) {
        Ok(stat) if fsys::file_type(&stat) == FileType::Directory => {
            fsys::remove(&directory, name, true)
                .context("a directory that is not empty is where the file belongs")?;
        }
        _ => {}
    }
    Ok(())
}

fn apply_metadata(file: &File, entry: &Entry) -> Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(entry.mode & 0o777))?;
    let mtime = if entry.mtime.seconds >= 0 {
        SystemTime::UNIX_EPOCH + Duration::new(entry.mtime.seconds as u64, entry.mtime.nanoseconds)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(entry.mtime.seconds.unsigned_abs())
            + Duration::from_nanos(u64::from(entry.mtime.nanoseconds))
    };
    file.set_modified(mtime)?;
    Ok(())
}
