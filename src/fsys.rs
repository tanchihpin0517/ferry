//! Filesystem access relative to directory file descriptors.
//!
//! Paths are resolved one component at a time from a directory's descriptor,
//! never following a symlink, so a link inside a tree can never lead outside
//! it.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

use crate::manifest::{Entry, FileId, Kind, Timestamp};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Opens the directory `name` inside `parent`, refusing a symlink.
pub fn open_directory(parent: impl AsFd, name: &[u8]) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        parent,
        OsStr::from_bytes(name),
        DIRECTORY_FLAGS,
        Mode::empty(),
    )?)
}

/// Opens a transfer's root directory. The root path itself may be a symlink,
/// as the user named it; nothing below it may.
pub fn open_root(path: &OsStr) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::openat(
        rustix::fs::CWD,
        path,
        flags,
        Mode::empty(),
    )?)
}

/// Opens the directory at `path`, relative to `root`, one component at a time.
pub fn open_path(root: impl AsFd, path: &[u8]) -> io::Result<OwnedFd> {
    let mut directory = rustix::io::dup(root)?;
    for component in path
        .split(|&byte| byte == b'/')
        .filter(|component| !component.is_empty())
    {
        directory = open_directory(&directory, component)?;
    }
    Ok(directory)
}

/// The status of `name` in `directory`, without following a symlink.
pub fn stat(directory: impl AsFd, name: &[u8]) -> io::Result<Stat> {
    Ok(rustix::fs::statat(
        directory,
        OsStr::from_bytes(name),
        AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

/// The status of a transfer's root, following a symlink the user named.
pub fn stat_root(path: &OsStr) -> io::Result<Stat> {
    Ok(rustix::fs::statat(rustix::fs::CWD, path, AtFlags::empty())?)
}

pub fn read_link(directory: impl AsFd, name: &[u8]) -> io::Result<Vec<u8>> {
    Ok(rustix::fs::readlinkat(directory, OsStr::from_bytes(name), Vec::new())?.into_bytes())
}

/// The names in a directory, without `.` and `..`, sorted by bytes.
pub fn read_names(directory: impl AsFd) -> io::Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

pub fn file_type(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

pub fn mtime(stat: &Stat) -> Timestamp {
    // The field types differ between platforms and rustix backends.
    #[allow(clippy::unnecessary_cast)]
    Timestamp {
        seconds: stat.st_mtime as i64,
        nanoseconds: stat.st_mtime_nsec as u32,
    }
}

/// Describes a filesystem object as a manifest entry. `link_target` is read
/// only for a symlink.
pub fn entry(
    id: FileId,
    name: Vec<u8>,
    stat: &Stat,
    link_target: impl FnOnce() -> io::Result<Vec<u8>>,
) -> io::Result<Entry> {
    #[allow(clippy::unnecessary_cast)]
    let rdev = stat.st_rdev as u64;
    let kind = match file_type(stat) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Directory,
        FileType::Symlink => Kind::Symlink {
            target: link_target()?,
        },
        FileType::Fifo => Kind::Fifo,
        FileType::Socket => Kind::Socket,
        FileType::CharacterDevice => Kind::CharDevice { rdev },
        FileType::BlockDevice => Kind::BlockDevice { rdev },
        FileType::Unknown => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unknown file type",
            ));
        }
    };
    #[allow(clippy::unnecessary_cast)]
    Ok(Entry {
        id,
        name,
        size: if kind == Kind::File {
            stat.st_size as u64
        } else {
            0
        },
        kind,
        mtime: mtime(stat),
        mode: stat.st_mode as u32 & 0o7777,
        uid: stat.st_uid as u32,
        gid: stat.st_gid as u32,
    })
}
