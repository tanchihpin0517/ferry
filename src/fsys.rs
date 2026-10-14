//! Filesystem access relative to directory file descriptors.
//!
//! Paths are resolved one component at a time from a directory's descriptor,
//! never following a symlink, so a link inside a tree can never lead outside
//! it.

use std::ffi::OsStr;
use std::fs::File;
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

/// Opens the directory at `path` under the root at `root`.
pub fn open_path_from(root: &std::path::Path, path: &[u8]) -> io::Result<OwnedFd> {
    open_path(open_root(root.as_os_str())?, path)
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

const FILE_READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Opens the file `name` in `directory` to read, refusing a symlink.
pub fn open_file(directory: impl AsFd, name: &[u8]) -> io::Result<File> {
    let fd = rustix::fs::openat(
        directory,
        OsStr::from_bytes(name),
        FILE_READ_FLAGS,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// Creates a new, empty, private file to write `name`'s content into, beside
/// it in `directory`. Returns the file and the name it was created under.
pub fn create_temporary(directory: impl AsFd, name: &[u8]) -> io::Result<(File, Vec<u8>)> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    loop {
        let temporary = temporary_name(name);
        match rustix::fs::openat(
            &directory,
            OsStr::from_bytes(&temporary),
            flags,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => return Ok((File::from(fd), temporary)),
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// A name for `name`'s temporary file: `.<name>.ferry-<random>`, or
/// `.ferry-<random>` when that would be longer than a name may be.
pub fn temporary_name(name: &[u8]) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanoseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    let random = format!(
        "{:08x}{:08x}",
        std::process::id() ^ nanoseconds,
        COUNTER.fetch_add(1, Ordering::Relaxed) as u32
    );
    let mut temporary = Vec::with_capacity(name.len() + 24);
    if name.len() + 24 <= 255 {
        temporary.push(b'.');
        temporary.extend_from_slice(name);
    }
    temporary.extend_from_slice(b".ferry-");
    temporary.extend_from_slice(random.as_bytes());
    temporary
}

pub fn rename(directory: impl AsFd, from: &[u8], to: &[u8]) -> io::Result<()> {
    Ok(rustix::fs::renameat(
        &directory,
        OsStr::from_bytes(from),
        &directory,
        OsStr::from_bytes(to),
    )?)
}

/// Removes `name` from `directory`: a directory, which must be empty, or
/// anything else.
pub fn remove(directory: impl AsFd, name: &[u8], is_directory: bool) -> io::Result<()> {
    let flags = if is_directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    Ok(rustix::fs::unlinkat(
        directory,
        OsStr::from_bytes(name),
        flags,
    )?)
}

pub fn make_directory(directory: impl AsFd, name: &[u8], mode: u32) -> io::Result<()> {
    Ok(rustix::fs::mkdirat(
        directory,
        OsStr::from_bytes(name),
        Mode::from_raw_mode(mode as _),
    )?)
}

/// Sets what a [`Timestamps`] leaves alone: the access time.
fn mtime_only(mtime: Timestamp) -> rustix::fs::Timestamps {
    rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: mtime.seconds,
            tv_nsec: mtime.nanoseconds as _,
        },
    }
}

/// Sets the mtime of `name` in `directory`, without following a symlink.
pub fn set_mtime_at(directory: impl AsFd, name: &[u8], mtime: Timestamp) -> io::Result<()> {
    Ok(rustix::fs::utimensat(
        directory,
        OsStr::from_bytes(name),
        &mtime_only(mtime),
        AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

pub fn set_mtime(fd: impl AsFd, mtime: Timestamp) -> io::Result<()> {
    Ok(rustix::fs::futimens(fd, &mtime_only(mtime))?)
}

/// Sets the owner of `name` in `directory`, without following a symlink.
/// `None` leaves that part alone.
pub fn set_owner_at(
    directory: impl AsFd,
    name: &[u8],
    uid: Option<u32>,
    gid: Option<u32>,
) -> io::Result<()> {
    Ok(rustix::fs::chownat(
        directory,
        OsStr::from_bytes(name),
        uid.map(rustix::fs::Uid::from_raw),
        gid.map(rustix::fs::Gid::from_raw),
        AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

pub fn set_owner(fd: impl AsFd, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    Ok(rustix::fs::fchown(
        fd,
        uid.map(rustix::fs::Uid::from_raw),
        gid.map(rustix::fs::Gid::from_raw),
    )?)
}

/// Sets the permissions of `name` in `directory`, which is not a symlink.
pub fn set_mode_at(directory: impl AsFd, name: &[u8], mode: u32) -> io::Result<()> {
    Ok(rustix::fs::chmodat(
        directory,
        OsStr::from_bytes(name),
        Mode::from_raw_mode(mode as _),
        AtFlags::empty(),
    )?)
}

pub fn set_mode(fd: impl AsFd, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::fchmod(fd, Mode::from_raw_mode(mode as _))?)
}

pub fn make_symlink(directory: impl AsFd, name: &[u8], target: &[u8]) -> io::Result<()> {
    Ok(rustix::fs::symlinkat(
        OsStr::from_bytes(target),
        directory,
        OsStr::from_bytes(name),
    )?)
}

/// Creates a FIFO, socket, or device. Sockets and devices may need privilege.
pub fn make_special(directory: impl AsFd, name: &[u8], kind: &Kind, mode: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name)?;
    let fd = directory.as_fd().as_raw_fd();
    let permissions = (mode & 0o7777) as libc::mode_t;
    // SAFETY: `fd` is an open directory and `name` a NUL-terminated string,
    // both alive for the whole call.
    let result = unsafe {
        match kind {
            Kind::Fifo => libc::mkfifoat(fd, name.as_ptr(), permissions),
            Kind::Socket => libc::mknodat(fd, name.as_ptr(), libc::S_IFSOCK | permissions, 0),
            Kind::CharDevice { rdev } => libc::mknodat(
                fd,
                name.as_ptr(),
                libc::S_IFCHR | permissions,
                *rdev as libc::dev_t,
            ),
            Kind::BlockDevice { rdev } => libc::mknodat(
                fd,
                name.as_ptr(),
                libc::S_IFBLK | permissions,
                *rdev as libc::dev_t,
            ),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a special file",
                ));
            }
        }
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
