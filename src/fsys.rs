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
    one_name(name)?;
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

/// Opens the directory `name` inside `parent`, as [`open_directory`] does,
/// to work in: one this process owns, but may not read, write, or go into,
/// is given read, write, and search for its owner first, as rsync does. Its
/// permissions are then the caller's to set again. Returns whether they
/// were changed.
pub fn open_directory_to_work_in(parent: impl AsFd, name: &[u8]) -> io::Result<(OwnedFd, bool)> {
    let (directory, let_in) = match open_directory(&parent, name) {
        Ok(directory) => (directory, false),
        Err(error) if is_denied(&error) => match let_owner_in_at(&parent, name) {
            Ok(true) => (open_directory(&parent, name)?, true),
            _ => return Err(error),
        },
        Err(error) => return Err(error),
    };
    let let_in = let_owner_in(&directory)? || let_in;
    Ok((directory, let_in))
}

/// Opens a transfer's root directory, as [`open_root`] does, to work in, as
/// [`open_directory_to_work_in`] does.
pub fn open_root_to_work_in(path: &OsStr) -> io::Result<(OwnedFd, bool)> {
    let (directory, let_in) = match open_root(path) {
        Ok(directory) => (directory, false),
        Err(error) if is_denied(&error) => {
            // The root path is the user's, symlink and all.
            let allowed = rustix::fs::stat(path)
                .ok()
                .and_then(|stat| lacking_owner_access(&stat))
                .is_some_and(|mode| {
                    rustix::fs::chmod(path, Mode::from_raw_mode(mode as _)).is_ok()
                });
            if !allowed {
                return Err(error);
            }
            (open_root(path)?, true)
        }
        Err(error) => return Err(error),
    };
    let let_in = let_owner_in(&directory)? || let_in;
    Ok((directory, let_in))
}

fn is_denied(error: &io::Error) -> bool {
    rustix::io::Errno::from_io_error(error) == Some(rustix::io::Errno::ACCESS)
}

/// For a directory this process owns, and lacks read, write, or search on,
/// the permissions that give its owner all three; `None` for anything else,
/// or for root, who needs none of them.
fn lacking_owner_access(stat: &Stat) -> Option<u32> {
    let euid = rustix::process::geteuid();
    #[allow(clippy::unnecessary_cast)]
    let (uid, mode) = (stat.st_uid as u32, stat.st_mode as u32 & 0o7777);
    (!euid.is_root()
        && uid == euid.as_raw()
        && file_type(stat) == FileType::Directory
        && mode & 0o700 != 0o700)
        .then_some(mode | 0o700)
}

/// Gives the owner of an open directory read, write, and search on it, if
/// it is this process and lacks any. Returns whether it did.
fn let_owner_in(directory: impl AsFd) -> io::Result<bool> {
    let stat = rustix::fs::fstat(&directory)?;
    match lacking_owner_access(&stat) {
        Some(mode) => {
            set_mode(&directory, mode)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// [`let_owner_in`], for a directory that cannot be opened to be read:
/// never following a symlink, whatever `name` comes to be.
fn let_owner_in_at(parent: impl AsFd, name: &[u8]) -> io::Result<bool> {
    one_name(name)?;
    #[cfg(target_os = "linux")]
    {
        // A path-only descriptor needs no permission on the directory; its
        // permissions are set through it as procfs names it.
        use std::os::fd::AsRawFd;
        let directory = rustix::fs::openat(
            &parent,
            OsStr::from_bytes(name),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let Some(mode) = lacking_owner_access(&rustix::fs::fstat(&directory)?) else {
            return Ok(false);
        };
        let named = format!("/proc/self/fd/{}", directory.as_raw_fd());
        rustix::fs::chmod(named.as_str(), Mode::from_raw_mode(mode as _))?;
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Some(mode) = lacking_owner_access(&stat(&parent, name)?) else {
            return Ok(false);
        };
        rustix::fs::chmodat(
            &parent,
            OsStr::from_bytes(name),
            Mode::from_raw_mode(mode as _),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        Ok(true)
    }
}

/// Refuses what is not one name in a directory: empty, `.`, `..`, or with a
/// `/` in it. Every call that changes or goes into what a name names goes
/// through it, so that none ever reaches out of the directory it is given,
/// whatever name it is given.
fn one_name(name: &[u8]) -> io::Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{:?} is not a name", String::from_utf8_lossy(name)),
        ));
    }
    Ok(())
}

/// Opens the directory at `path`, relative to `root`, one component at a
/// time. A `.` or `..` is refused, so that it never leads out of the root,
/// whatever the path.
pub fn open_path(root: impl AsFd, path: &[u8]) -> io::Result<OwnedFd> {
    let mut directory = rustix::io::dup(root)?;
    for component in path
        .split(|&byte| byte == b'/')
        .filter(|component| !component.is_empty())
    {
        if component == b"." || component == b".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{:?} in a path", String::from_utf8_lossy(component)),
            ));
        }
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

/// A name in a directory, and its type as the directory says it, which is
/// `Unknown` on filesystems that do not say.
pub struct Listed {
    pub name: Vec<u8>,
    pub file_type: FileType,
}

impl Listed {
    /// Whether it may be a directory: it is one, or its type is unknown.
    pub fn may_be_directory(&self) -> bool {
        matches!(self.file_type, FileType::Directory | FileType::Unknown)
    }
}

/// The names in a directory with their types, without `.` and `..`, sorted
/// by bytes.
pub fn read_entries(directory: impl AsFd) -> io::Result<Vec<Listed>> {
    let mut listed = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            listed.push(Listed {
                name: name.to_vec(),
                file_type: entry.file_type(),
            });
        }
    }
    listed.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(listed)
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
    one_name(name)?;
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
    one_name(name)?;
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
    one_name(from)?;
    one_name(to)?;
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
    one_name(name)?;
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
    one_name(name)?;
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
    one_name(name)?;
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
    one_name(name)?;
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
    one_name(name)?;
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
    one_name(name)?;
    Ok(rustix::fs::symlinkat(
        OsStr::from_bytes(target),
        directory,
        OsStr::from_bytes(name),
    )?)
}

/// Creates a FIFO, socket, or device. Sockets and devices may need privilege.
pub fn make_special(directory: impl AsFd, name: &[u8], kind: &Kind, mode: u32) -> io::Result<()> {
    one_name(name)?;
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

/// Removes `name` from `directory`, and everything under it if it is a
/// directory, never following a symlink. A directory this process owns is
/// let into first, if it must be, to empty it, as rsync does.
pub fn remove_tree(directory: impl AsFd, name: &[u8]) -> io::Result<()> {
    one_name(name)?;
    let subdirectory = match open_directory_to_work_in(&directory, name) {
        Ok((subdirectory, _)) => subdirectory,
        // Not a directory, or a symlink to one: removed as it is.
        Err(error)
            if matches!(
                rustix::io::Errno::from_io_error(&error),
                Some(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
            ) =>
        {
            return remove(&directory, name, false);
        }
        Err(error) => return Err(error),
    };
    for child in read_names(&subdirectory)? {
        remove_tree(&subdirectory, &child)?;
    }
    remove(&directory, name, true)
}

/// Hands what was written to `fd` to the drive, and on Linux waits until it
/// is on storage.
pub fn fsync(fd: impl AsFd) -> io::Result<()> {
    Ok(rustix::fs::fsync(fd)?)
}

/// Makes what was handed to the drive before reach storage before anything
/// handed to it after, so that a name renamed into place after its content is
/// never there without it, even across a power loss. It applies to the whole
/// drive `fd` is on, so one call orders a batch of files.
///
/// On Linux nothing is needed: `fsync` already waited for storage. On macOS,
/// `fsync` only hands the data to the drive, which may reorder it in its own
/// cache; `F_BARRIERFSYNC` sets a barrier the drive keeps, without waiting for
/// it to empty its cache.
pub fn barrier(fd: impl AsFd) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if apple_fcntl(&fd, libc::F_BARRIERFSYNC).is_err() {
        return fsync(fd);
    }
    let _ = fd;
    Ok(())
}

/// Makes everything written so far durable: on storage, not in a cache.
///
/// On Linux, `fsync` does this. On macOS, `F_FULLFSYNC` asks the drive to
/// empty its own cache; that is slow, and covers the whole drive, so it is
/// called once, when a transfer is done.
pub fn sync_durable(fd: impl AsFd) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if apple_fcntl(&fd, libc::F_FULLFSYNC).is_ok() {
        return Ok(());
    }
    Ok(rustix::fs::fsync(fd)?)
}

/// A filesystem that does not support the request, such as some network
/// ones, refuses it; the caller then falls back to `fsync`.
#[cfg(target_os = "macos")]
fn apple_fcntl(fd: &impl AsFd, request: libc::c_int) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is open for the whole call, and these requests
    // take no argument.
    if unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), request) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_never_leads_out_of_its_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("root/a")).unwrap();
        let root = open_root(temp.path().join("root").as_os_str()).unwrap();
        assert!(open_path(&root, b"a").is_ok());
        assert!(open_path(&root, b"a/").is_ok());
        for path in [&b".."[..], b"a/..", b"a/../..", b".", b"a/./a"] {
            let error = open_path(&root, path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{path:?}");
        }
    }

    /// Whatever name a change is given, it stays in its directory: what is
    /// not one name is refused, and nothing beside the directory changes.
    /// A directory is let into only as itself: never through a symlink to
    /// it, whose target keeps its permissions.
    #[test]
    fn a_directory_is_let_into_never_through_a_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        if rustix::process::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        std::fs::create_dir_all(parent.join("locked")).unwrap();
        std::fs::create_dir_all(temp.path().join("elsewhere")).unwrap();
        symlink(temp.path().join("elsewhere"), parent.join("link")).unwrap();
        let mode = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        let set = |path: &std::path::Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        set(&parent.join("locked"), 0o000);
        set(&temp.path().join("elsewhere"), 0o000);
        let opened = open_root(parent.as_os_str()).unwrap();

        let (_, let_in) = open_directory_to_work_in(&opened, b"locked").unwrap();
        assert!(let_in);
        assert_eq!(mode(&parent.join("locked")), 0o700);
        assert!(open_directory_to_work_in(&opened, b"link").is_err());
        assert_eq!(mode(&temp.path().join("elsewhere")), 0o000);
        // One its owner may work in already is left as it is.
        set(&parent.join("locked"), 0o750);
        let (_, let_in) = open_directory_to_work_in(&opened, b"locked").unwrap();
        assert!(!let_in);
        assert_eq!(mode(&parent.join("locked")), 0o750);
        set(&temp.path().join("elsewhere"), 0o755);
    }

    #[test]
    fn a_change_never_reaches_out_of_its_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("root/a")).unwrap();
        std::fs::write(temp.path().join("outside"), "outside").unwrap();
        let root = open_root(temp.path().join("root").as_os_str()).unwrap();
        for name in [&b""[..], b".", b"..", b"../outside", b"a/b"] {
            let refused = [
                remove_tree(&root, name),
                remove(&root, name, false),
                remove(&root, name, true),
                make_directory(&root, name, 0o755),
                make_symlink(&root, name, b"x"),
                rename(&root, b"a", name),
                rename(&root, name, b"a"),
                set_mode_at(&root, name, 0o700),
                open_directory(&root, name).map(|_| ()),
            ];
            for result in refused {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
            }
        }
        assert_eq!(
            std::fs::read(temp.path().join("outside")).unwrap(),
            b"outside"
        );
        assert!(temp.path().join("root/a").is_dir());
    }

    #[test]
    fn ordered_and_durable_syncs_work_here() {
        let temp = tempfile::tempdir().unwrap();
        let file = File::create(temp.path().join("file")).unwrap();
        // On macOS the requests themselves must work, not only the fallback.
        #[cfg(target_os = "macos")]
        {
            apple_fcntl(&file, libc::F_BARRIERFSYNC).unwrap();
            apple_fcntl(&file, libc::F_FULLFSYNC).unwrap();
        }
        fsync(&file).unwrap();
        barrier(&file).unwrap();
        sync_durable(&file).unwrap();
    }
}
