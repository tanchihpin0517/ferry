//! Filesystem access relative to directory file descriptors.
//!
//! Paths are resolved one component at a time from a directory's descriptor,
//! never following a symlink, so a link inside a tree can never lead outside
//! it.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

use crate::manifest::{Entry, FileId, Kind, Timestamp};
use crate::protocol::{FileKey, RootIdentity};

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
/// permissions are then the caller's to set again. Returns, if they were
/// changed, what they were.
pub fn open_directory_to_work_in(
    parent: impl AsFd,
    name: &[u8],
) -> io::Result<(OwnedFd, Option<u32>)> {
    let (directory, let_in) = match open_directory(&parent, name) {
        Ok(directory) => (directory, None),
        Err(error) if is_denied(&error) => match let_owner_in_at(&parent, name) {
            Ok(Some(mode)) => (open_directory(&parent, name)?, Some(mode)),
            _ => return Err(error),
        },
        Err(error) => return Err(error),
    };
    let let_in = let_in.or(let_owner_in(&directory)?);
    Ok((directory, let_in))
}

/// Opens a transfer's root directory, as [`open_root`] does, to work in, as
/// [`open_directory_to_work_in`] does.
pub fn open_root_to_work_in(path: &OsStr) -> io::Result<(OwnedFd, Option<u32>)> {
    let (directory, let_in) = match open_root(path) {
        Ok(directory) => (directory, None),
        Err(error) if is_denied(&error) => {
            // The root path is the user's, symlink and all.
            let allowed = rustix::fs::stat(path).ok().and_then(|stat| {
                let mode = lacking_owner_access(&stat)?;
                rustix::fs::chmod(path, Mode::from_raw_mode(mode as _))
                    .is_ok()
                    .then(|| mode_of(&stat))
            });
            let Some(before) = allowed else {
                return Err(error);
            };
            (open_root(path)?, Some(before))
        }
        Err(error) => return Err(error),
    };
    let let_in = let_in.or(let_owner_in(&directory)?);
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

/// A file's permissions, as they are set.
fn mode_of(stat: &Stat) -> u32 {
    #[allow(clippy::unnecessary_cast)]
    let mode = stat.st_mode as u32;
    mode & 0o7777
}

/// Gives the owner of an open directory read, write, and search on it, if
/// it is this process and lacks any. Returns, if it did, what they were.
fn let_owner_in(directory: impl AsFd) -> io::Result<Option<u32>> {
    let stat = rustix::fs::fstat(&directory)?;
    match lacking_owner_access(&stat) {
        Some(mode) => {
            set_mode(&directory, mode)?;
            Ok(Some(mode_of(&stat)))
        }
        None => Ok(None),
    }
}

/// [`let_owner_in`], for a directory that cannot be opened to be read:
/// never following a symlink, whatever `name` comes to be.
fn let_owner_in_at(parent: impl AsFd, name: &[u8]) -> io::Result<Option<u32>> {
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
        let stat = rustix::fs::fstat(&directory)?;
        let Some(mode) = lacking_owner_access(&stat) else {
            return Ok(None);
        };
        let named = format!("/proc/self/fd/{}", directory.as_raw_fd());
        rustix::fs::chmod(named.as_str(), Mode::from_raw_mode(mode as _))?;
        Ok(Some(mode_of(&stat)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let stat = stat(&parent, name)?;
        let Some(mode) = lacking_owner_access(&stat) else {
            return Ok(None);
        };
        rustix::fs::chmodat(
            &parent,
            OsStr::from_bytes(name),
            Mode::from_raw_mode(mode as _),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        Ok(Some(mode_of(&stat)))
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

/// The status of `name` in `directory`, without following a symlink, nor,
/// on Linux, mounting what an automounter would mount there: a scan of an
/// autofs `/home` mounts no one's home it does not go into.
pub fn stat(directory: impl AsFd, name: &[u8]) -> io::Result<Stat> {
    Ok(rustix::fs::statat(
        directory,
        OsStr::from_bytes(name),
        AtFlags::SYMLINK_NOFOLLOW | NO_AUTOMOUNT,
    )?)
}

#[cfg(target_os = "linux")]
const NO_AUTOMOUNT: AtFlags = AtFlags::NO_AUTOMOUNT;
// macOS mounts nothing on a stat.
#[cfg(not(target_os = "linux"))]
const NO_AUTOMOUNT: AtFlags = AtFlags::empty();

/// The status of a transfer's root, following a symlink the user named.
pub fn stat_root(path: &OsStr) -> io::Result<Stat> {
    Ok(rustix::fs::statat(rustix::fs::CWD, path, AtFlags::empty())?)
}

/// The status of a transfer's root, as [`stat_root`], but of a symlink
/// itself, not what it points to.
pub fn lstat_root(path: &OsStr) -> io::Result<Stat> {
    Ok(rustix::fs::statat(
        rustix::fs::CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
    )?)
}

/// What a status says a file is, to its kernel.
pub fn key(stat: &Stat) -> FileKey {
    #[allow(clippy::unnecessary_cast)]
    FileKey {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    }
}

/// What a transfer's root is, for the client to check against the other
/// root before anything changes; see [`RootIdentity`]. What cannot be found
/// is left out: a chain that stops short, or none, compares as nothing.
pub fn root_identity(root: &OsStr) -> RootIdentity {
    let kernel = kernel();
    let homes = homes();
    let unknown = || RootIdentity {
        kernel: kernel.clone(),
        canonical: root.as_bytes().to_vec(),
        exists: false,
        chain: Vec::new(),
        homes: homes.clone(),
    };
    // A root not there yet is under its nearest ancestor that is: the names
    // between, nearest the root first.
    let mut existing = Path::new(if root.is_empty() {
        OsStr::new(".")
    } else {
        root
    })
    .to_owned();
    let mut missing: Vec<OsString> = Vec::new();
    let canonical = loop {
        match std::fs::canonicalize(&existing) {
            Ok(canonical) => break canonical,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                let Some(name) = existing.file_name() else {
                    return unknown();
                };
                missing.push(name.to_owned());
                existing = match existing.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
                    _ => PathBuf::from("."),
                };
            }
            Err(_) => return unknown(),
        }
    };
    // Each directory up from there, by its canonical path, which no symlink
    // is in: a stat needs only search on the way, where opening `..` would
    // need to read each.
    let mut chain = Vec::new();
    let mut up = Some(canonical.as_path());
    while let Some(path) = up {
        match stat_root(path.as_os_str()) {
            Ok(stat) => chain.push(key(&stat)),
            Err(_) => break,
        }
        up = path.parent();
    }
    let mut canonical = canonical;
    for name in missing.iter().rev() {
        canonical.push(name);
    }
    RootIdentity {
        kernel,
        canonical: canonical.into_os_string().into_vec(),
        exists: missing.is_empty(),
        chain,
        homes,
    }
}

/// The running kernel, as it names its boot; empty where it cannot be found.
fn kernel() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|boot| boot.trim().to_owned())
            .unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        let mut buffer = [0u8; 64];
        let mut length = buffer.len();
        // SAFETY: the name is a C string, and the buffer and its length are
        // this function's, the length saying how much of it was written.
        let found = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        if !found {
            return String::new();
        }
        let written = &buffer[..length.min(buffer.len())];
        let written = written.split(|&byte| byte == 0).next().unwrap_or_default();
        String::from_utf8_lossy(written).trim().to_owned()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        String::new()
    }
}

/// The canonical home directories of the users this process acts for: its
/// own, as `HOME` and the user database say, and, run through `sudo`, the
/// user's who ran it.
fn homes() -> Vec<Vec<u8>> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    candidates.extend(home_of(rustix::process::getuid().as_raw()));
    if let Some(uid) = std::env::var("SUDO_UID")
        .ok()
        .and_then(|uid| uid.parse().ok())
    {
        candidates.extend(home_of(uid));
    }
    let mut homes = Vec::new();
    for candidate in candidates {
        if let Ok(home) = std::fs::canonicalize(candidate) {
            let home = home.into_os_string().into_vec();
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
    }
    homes
}

/// A user's home directory, as the user database has it.
fn home_of(uid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: the call gets a zeroed record, a buffer of the stated length,
    // and a result pointer, and the home is read only when the call found
    // one, while the buffer it points into is alive.
    unsafe {
        let mut record: libc::passwd = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        libc::getpwuid_r(
            uid,
            &mut record,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        if result.is_null() || record.pw_dir.is_null() {
            return None;
        }
        let home = std::ffi::CStr::from_ptr(record.pw_dir).to_bytes();
        (!home.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(home)))
    }
}

/// Why a directory was not deleted: it is the Source's root, or holds it.
#[derive(Debug)]
pub struct Protected;

impl std::fmt::Display for Protected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("it is the Source's root, or a directory that holds it")
    }
}

impl std::error::Error for Protected {}

/// Whether `error` is a deletion [`remove_tree`] refused, as [`Protected`].
pub fn is_protected(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Protected>())
}

/// Why a directory was not deleted, with `-x`: it is on another filesystem
/// than the root's, or holds one that is, at `path` under it, which is kept
/// with all that holds it; what else it had is deleted.
#[derive(Debug)]
pub struct Elsewhere {
    pub path: Vec<u8>,
}

impl std::fmt::Display for Elsewhere {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.path.is_empty() {
            formatter.write_str("it is on another filesystem")
        } else {
            write!(
                formatter,
                "{}/ in it is on another filesystem",
                String::from_utf8_lossy(&self.path)
            )
        }
    }
}

impl std::error::Error for Elsewhere {}

/// Whether `error` is a deletion [`remove_tree`] stopped short of, as
/// [`Elsewhere`].
pub fn is_elsewhere(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Elsewhere>())
}

/// What [`remove_tree`] leaves as it is.
#[derive(Debug, Default)]
pub struct Keep {
    /// Directories never deleted, nor let into, at any depth: the Source's
    /// root, and those that hold it.
    pub protect: Vec<FileKey>,
    /// With `-x`, the filesystem to stay on, once the root's is known: a
    /// directory on another is not gone into, nor deleted.
    pub device: std::sync::OnceLock<u64>,
}

impl Keep {
    pub fn protecting(protect: Vec<FileKey>) -> Self {
        Self {
            protect,
            device: std::sync::OnceLock::new(),
        }
    }

    /// Whether `stat`, of a directory, is of one on another filesystem than
    /// the one to stay on, if there is one.
    #[allow(clippy::unnecessary_cast)]
    pub fn is_elsewhere(&self, stat: &Stat) -> bool {
        self.device
            .get()
            .is_some_and(|&device| stat.st_dev as u64 != device)
    }
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

/// The name `directory` keeps for what `name` finds there, where that is
/// not `name` itself: a filesystem that ignores case takes `A` for `a`, and
/// one that keeps names normalized, an `é` for another `é`. `None` where
/// `directory` holds `name` as it is. The name kept is found by the inode
/// `found` has, and is empty where none in the listing has it.
pub fn alias(directory: impl AsFd, name: &[u8], found: &Stat) -> io::Result<Option<Vec<u8>>> {
    let mut kept = None;
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let listed = entry.file_name().to_bytes();
        if listed == name {
            return Ok(None);
        }
        #[allow(clippy::unnecessary_cast)]
        if kept.is_none() && entry.ino() as u64 == found.st_ino as u64 {
            kept = Some(listed.to_vec());
        }
    }
    Ok(Some(kept.unwrap_or_default()))
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

/// `O_NONBLOCK` so that opening a FIFO does not wait for a writer, and
/// `O_NOCTTY` so that a terminal never becomes the controlling one.
const FILE_READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY);

/// Opens the regular file `name` in `directory` to read, refusing a symlink.
/// Returns the file and its status as opened.
///
/// Opening never waits. What is no longer a regular file, such as a FIFO or a
/// directory put where the scan found a file, is closed again unread and
/// refused. Nothing is checked before opening: such files are rare, and a
/// `stat` before every open costs a round trip per file on a network
/// filesystem.
pub fn open_regular_file(directory: impl AsFd, name: &[u8]) -> io::Result<(File, Stat)> {
    one_name(name)?;
    open_regular(directory.as_fd(), OsStr::from_bytes(name), FILE_READ_FLAGS)
}

/// Opens a transfer's root that is a file, as `open_regular_file` does, but
/// following a symlink the user named.
pub fn open_regular_root(path: &OsStr) -> io::Result<(File, Stat)> {
    open_regular(rustix::fs::CWD, path, FILE_READ_FLAGS - OFlags::NOFOLLOW)
}

fn open_regular(directory: BorrowedFd, name: &OsStr, flags: OFlags) -> io::Result<(File, Stat)> {
    let fd = open_without_atime(directory, name, flags)?;
    let opened = rustix::fs::fstat(&fd)?;
    if file_type(&opened) != FileType::RegularFile {
        return Err(io::Error::other("it is no longer a regular file"));
    }
    // A regular file is read as one that blocks: with `O_NONBLOCK`, reading
    // one under a mandatory lock would fail with `EAGAIN`.
    rustix::fs::fcntl_setfl(&fd, rustix::fs::fcntl_getfl(&fd)? - OFlags::NONBLOCK)?;
    Ok((File::from(fd), opened))
}

/// Opens without updating the file's access time, where the kernel lets it:
/// Linux allows `O_NOATIME` only for a file the process owns, or with
/// `CAP_FOWNER`. Once refused, it is not tried again.
#[cfg(target_os = "linux")]
fn open_without_atime(directory: BorrowedFd, name: &OsStr, flags: OFlags) -> io::Result<OwnedFd> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOATIME: AtomicBool = AtomicBool::new(true);
    if NOATIME.load(Ordering::Relaxed) {
        match rustix::fs::openat(directory, name, flags | OFlags::NOATIME, Mode::empty()) {
            Err(rustix::io::Errno::PERM) => NOATIME.store(false, Ordering::Relaxed),
            opened => return Ok(opened?),
        }
    }
    Ok(rustix::fs::openat(directory, name, flags, Mode::empty())?)
}

#[cfg(not(target_os = "linux"))]
fn open_without_atime(directory: BorrowedFd, name: &OsStr, flags: OFlags) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(directory, name, flags, Mode::empty())?)
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

/// Whether a file may be made without a name, and given one once written:
/// on Linux, where the filesystem can. Such a file holds a descriptor of its
/// own as it waits for its name.
pub const UNNAMED: bool = cfg!(target_os = "linux");

/// The drives whose filesystem made no file without a name: not asked again.
#[cfg(target_os = "linux")]
static NO_UNNAMED: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

/// Creates a new, empty, private file without a name in `directory`, to be
/// given one with [`link_unnamed`] once written: until then, it goes when it
/// is closed, however the process ends. `None` where the filesystem makes
/// none, which is remembered for its drive, or where a name could not be
/// given it, with no `/proc`; then a temporary name is for the caller to
/// make.
#[cfg(target_os = "linux")]
pub fn create_unnamed(directory: impl AsFd) -> io::Result<Option<File>> {
    use rustix::io::Errno;
    static PROC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*PROC.get_or_init(|| Path::new("/proc/self/fd").is_dir()) {
        return Ok(None);
    }
    let refused = NO_UNNAMED.lock().expect("not poisoned").clone();
    // Its drive is looked at only once one refused: none, mostly.
    if !refused.is_empty() && refused.contains(&rustix::fs::fstat(&directory)?.st_dev) {
        return Ok(None);
    }
    // Not `O_EXCL`, which would keep it from ever taking a name.
    let flags = OFlags::TMPFILE | OFlags::WRONLY | OFlags::CLOEXEC;
    match rustix::fs::openat(&directory, ".", flags, Mode::from_raw_mode(0o600)) {
        Ok(fd) => Ok(Some(File::from(fd))),
        // A kernel that knows no `O_TMPFILE` takes it for `O_DIRECTORY`.
        Err(Errno::OPNOTSUPP | Errno::ISDIR | Errno::INVAL) => {
            let drive = rustix::fs::fstat(&directory)?.st_dev;
            NO_UNNAMED.lock().expect("not poisoned").push(drive);
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn create_unnamed(_directory: impl AsFd) -> io::Result<Option<File>> {
    Ok(None)
}

/// Gives `file`, made by [`create_unnamed`], the name `name` in
/// `directory`, which must not be taken: `EEXIST` if it is. Through
/// `/proc`, as linking by the descriptor itself needs a privilege.
pub fn link_unnamed(file: &File, directory: impl AsFd, name: &[u8]) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    one_name(name)?;
    Ok(rustix::fs::linkat(
        rustix::fs::CWD,
        format!("/proc/self/fd/{}", file.as_raw_fd()),
        directory,
        OsStr::from_bytes(name),
        AtFlags::SYMLINK_FOLLOW,
    )?)
}

/// Gives `file`, made by [`create_unnamed`], a temporary name for `name`'s
/// in `directory`, as [`create_temporary`] makes: for it to be renamed over
/// what has `name`. Returns the name.
pub fn link_temporary(file: &File, directory: impl AsFd, name: &[u8]) -> io::Result<((), Vec<u8>)> {
    one_name(name)?;
    loop {
        let temporary = temporary_name(name);
        match link_unnamed(file, &directory, &temporary) {
            Ok(()) => return Ok(((), temporary)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
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

/// Sets the permissions of `name` in `directory`, never through a symlink,
/// whatever `name` comes to be: a symlink is refused, as `ELOOP`, so that a
/// link put where a file was compared cannot have a file outside changed.
pub fn set_mode_at(directory: impl AsFd, name: &[u8], mode: u32) -> io::Result<()> {
    one_name(name)?;
    #[cfg(target_os = "linux")]
    {
        // A path-only descriptor holds the entry itself, a symlink as one, so
        // what is looked at is what is changed; its permissions are set
        // through it as procfs names it. Without procfs, nothing is set.
        use std::os::fd::AsRawFd;
        let held = rustix::fs::openat(
            &directory,
            OsStr::from_bytes(name),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        if file_type(&rustix::fs::fstat(&held)?) == FileType::Symlink {
            return Err(rustix::io::Errno::LOOP.into());
        }
        let named = format!("/proc/self/fd/{}", held.as_raw_fd());
        Ok(rustix::fs::chmod(
            named.as_str(),
            Mode::from_raw_mode(mode as _),
        )?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Not following a symlink, macOS's chmodat changes the link itself:
        // one is refused first, and one put there meanwhile has only itself
        // changed, never what it points to.
        if file_type(&stat(&directory, name)?) == FileType::Symlink {
            return Err(rustix::io::Errno::LOOP.into());
        }
        Ok(rustix::fs::chmodat(
            directory,
            OsStr::from_bytes(name),
            Mode::from_raw_mode(mode as _),
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }
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

/// How deep a tree [`remove_tree`] deletes: each level holds its directory
/// open, so one deeper is not deleted, and the removal fails there.
pub const REMOVE_DEPTH: usize = 4096;

/// Removes `name` from `directory`, and everything under it if it is a
/// directory, never following a symlink. A directory this process owns is
/// let into first, if it must be, to empty it, as rsync does, and given its
/// permissions back if it then cannot be. A directory among those `keep`
/// protects, at any depth, is not deleted, nor let into: the removal stops
/// there, as [`Protected`]. One on another filesystem than `keep` stays on
/// is not gone into, as rsync's `-x` does: it and all that holds it are
/// kept, all else deleted, and the removal fails as [`Elsewhere`]. It goes
/// down a level at a time, not by recursion, so that no tree, however deep,
/// runs the thread out of stack; none deeper than [`REMOVE_DEPTH`] is
/// deleted.
pub fn remove_tree(directory: impl AsFd, name: &[u8], keep: &Keep) -> io::Result<()> {
    /// A directory being emptied: open, with its permissions if it was let
    /// into, and the names in it still to remove, the next last; and
    /// whether it holds another filesystem, and so stays.
    struct Level {
        name: Vec<u8>,
        directory: OwnedFd,
        let_in: Option<u32>,
        children: Vec<Vec<u8>>,
        pinned: bool,
    }

    /// What became of a name.
    enum Entered {
        Removed,
        Opened(Level),
        Elsewhere,
    }

    /// `name` in `parent` removed, if it is no directory, or opened to be
    /// emptied.
    fn enter(parent: BorrowedFd, name: &[u8], keep: &Keep) -> io::Result<Entered> {
        one_name(name)?;
        if (!keep.protect.is_empty() || keep.device.get().is_some())
            && let Ok(stat) = stat(parent, name)
            && file_type(&stat) == FileType::Directory
        {
            if keep.protect.contains(&key(&stat)) {
                return Err(io::Error::other(Protected));
            }
            if keep.is_elsewhere(&stat) {
                return Ok(Entered::Elsewhere);
            }
        }
        let (directory, let_in) = match open_directory_to_work_in(parent, name) {
            Ok(opened) => opened,
            // Not a directory, or a symlink to one: removed as it is.
            Err(error)
                if matches!(
                    rustix::io::Errno::from_io_error(&error),
                    Some(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP)
                ) =>
            {
                return remove(parent, name, false).map(|()| Entered::Removed);
            }
            Err(error) => return Err(error),
        };
        match read_names(&directory) {
            Ok(mut children) => {
                children.reverse();
                Ok(Entered::Opened(Level {
                    name: name.to_vec(),
                    directory,
                    let_in,
                    children,
                    pinned: false,
                }))
            }
            Err(error) => {
                if let Some(mode) = let_in {
                    let _ = set_mode(&directory, mode);
                }
                Err(error)
            }
        }
    }

    let deepest = crate::inject::remove_depth().unwrap_or(REMOVE_DEPTH);
    let directory = directory.as_fd();
    let top = match enter(directory, name, keep)? {
        Entered::Removed => return Ok(()),
        Entered::Elsewhere => return Err(io::Error::other(Elsewhere { path: Vec::new() })),
        Entered::Opened(top) => top,
    };
    let mut levels = vec![top];
    // The first directory on another filesystem, under `name`.
    let mut elsewhere: Option<Vec<u8>> = None;
    let removed = loop {
        let depth = levels.len();
        let level = levels.last_mut().expect("one at least");
        if let Some(child) = level.children.pop() {
            match enter(level.directory.as_fd(), &child, keep) {
                Ok(Entered::Opened(deeper)) if depth >= deepest => {
                    if let Some(mode) = deeper.let_in {
                        let _ = set_mode(&deeper.directory, mode);
                    }
                    break Err(io::Error::other(format!(
                        "it is more than {deepest} directories deep"
                    )));
                }
                Ok(Entered::Opened(next)) => levels.push(next),
                Ok(Entered::Removed) => {}
                Ok(Entered::Elsewhere) => {
                    if elsewhere.is_none() {
                        let mut path = Vec::new();
                        for level in &levels[1..] {
                            path = crate::manifest::join(&path, &level.name);
                        }
                        elsewhere = Some(crate::manifest::join(&path, &child));
                    }
                    levels.last_mut().expect("one at least").pinned = true;
                }
                Err(error) => break Err(error),
            }
            continue;
        }
        let emptied = levels.pop().expect("one at least");
        // Emptied but for another filesystem: it stays, and so does all
        // that holds it.
        if emptied.pinned {
            if let Some(mode) = emptied.let_in {
                let _ = set_mode(&emptied.directory, mode);
            }
            match levels.last_mut() {
                Some(parent) => {
                    parent.pinned = true;
                    continue;
                }
                None => {
                    break Err(io::Error::other(Elsewhere {
                        path: elsewhere.take().unwrap_or_default(),
                    }));
                }
            }
        }
        let parent = levels
            .last()
            .map_or(directory, |level| level.directory.as_fd());
        if let Err(error) = remove(parent, &emptied.name, true) {
            levels.push(emptied);
            break Err(error);
        }
        if levels.is_empty() {
            break Ok(());
        }
    };
    // What could not be emptied is given back the permissions it had.
    for level in levels.iter().rev() {
        if let Some(mode) = level.let_in {
            let _ = set_mode(&level.directory, mode);
        }
    }
    removed
}

/// Hands what was written to `fd` to the drive, and on Linux waits until it
/// is on storage.
pub fn fsync(fd: impl AsFd) -> io::Result<()> {
    Ok(rustix::fs::fsync(fd)?)
}

/// Whether a [`barrier`] needs a file open on its drive: one kept for each
/// drive a batch writes to, and none where it does nothing.
pub const BARRIER_NEEDS_A_FILE: bool = cfg!(target_os = "macos");

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

    use std::os::unix::fs::PermissionsExt;

    fn key_of(path: &Path) -> FileKey {
        key(&stat_root(path.as_os_str()).unwrap())
    }

    /// A name a directory holds as it is has no alias, nor has a hard link
    /// to another; one the filesystem takes for another's, where it ignores
    /// case, as CI's ext4-casefold does, has that one's: with a hard link
    /// beside it, either name of the two.
    #[test]
    fn an_alias_is_the_name_kept_for_another() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a"), "a").unwrap();
        std::fs::hard_link(temp.path().join("a"), temp.path().join("b")).unwrap();
        let directory = open_root(temp.path().as_os_str()).unwrap();
        for name in [&b"a"[..], b"b"] {
            let found = stat(&directory, name).unwrap();
            assert_eq!(alias(&directory, name, &found).unwrap(), None);
        }
        if let Ok(found) = stat(&directory, b"A") {
            let kept = alias(&directory, b"A", &found).unwrap();
            assert!(matches!(kept.as_deref(), Some(b"a" | b"b")), "{kept:?}");
        }
    }

    /// A file made without a name holds what is written to it once given
    /// one, private until its mode is set; a name already taken is refused,
    /// and a temporary one is not, beside it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_without_a_name_is_given_one_once_written() {
        use std::io::Write;
        let Some(temp) = crate::test_support::unnamed_tempdir() else {
            return;
        };
        let directory = open_root(temp.path().as_os_str()).unwrap();
        let mut file = create_unnamed(&directory).unwrap().unwrap();
        file.write_all(b"content").unwrap();
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        link_unnamed(&file, &directory, b"f").unwrap();
        assert_eq!(std::fs::read(temp.path().join("f")).unwrap(), b"content");
        let mode = std::fs::metadata(temp.path().join("f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let other = create_unnamed(&directory).unwrap().unwrap();
        let taken = link_unnamed(&other, &directory, b"f").unwrap_err();
        assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
        let ((), temporary) = link_temporary(&other, &directory, b"f").unwrap();
        assert!(temporary.starts_with(b".f.ferry-"));
        assert!(temp.path().join(OsStr::from_bytes(&temporary)).exists());
    }

    #[test]
    fn a_roots_identity_is_its_chain_up_to_slash() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::write(a.join("file"), "x").unwrap();

        let identity = root_identity(a.join("sub").as_os_str());
        assert!(identity.exists);
        assert_eq!(identity.chain[0], key_of(&a.join("sub")));
        assert_eq!(identity.chain[1], key_of(&a));
        assert_eq!(*identity.chain.last().unwrap(), key_of(Path::new("/")));
        assert_eq!(
            identity.canonical,
            std::fs::canonicalize(a.join("sub"))
                .unwrap()
                .into_os_string()
                .into_vec()
        );
        assert_eq!(identity.kernel, root_identity(a.as_os_str()).kernel);

        // A file's chain starts with the file.
        let file = root_identity(a.join("file").as_os_str());
        assert!(file.exists);
        assert_eq!(file.chain[0], key_of(&a.join("file")));
        assert_eq!(file.chain[1], key_of(&a));
    }

    #[test]
    fn a_root_not_there_yet_is_under_its_nearest_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let identity = root_identity(a.join("new/deeper").as_os_str());
        assert!(!identity.exists);
        assert_eq!(identity.chain[0], key_of(&a));
        let canonical = std::fs::canonicalize(&a).unwrap().join("new/deeper");
        assert_eq!(identity.canonical, canonical.into_os_string().into_vec());
    }

    #[test]
    fn a_root_named_through_a_symlink_is_where_it_points() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::os::unix::fs::symlink(&a, temp.path().join("link")).unwrap();
        let through = root_identity(temp.path().join("link").as_os_str());
        assert_eq!(through.chain, root_identity(a.as_os_str()).chain);
    }

    /// Only a regular file is opened to read, and opening never waits.
    #[test]
    fn only_a_regular_file_is_opened_to_read() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("file"), b"content").unwrap();
        std::fs::create_dir(root.join("directory")).unwrap();
        std::os::unix::fs::symlink("file", root.join("link")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.join("socket")).unwrap();
        let opened = open_root(root.as_os_str()).unwrap();
        make_special(&opened, b"fifo", &Kind::Fifo, 0o644).unwrap();

        let (file, stat) = open_regular_file(&opened, b"file").unwrap();
        assert_eq!(stat.st_size, 7);
        assert_eq!(key(&stat), key_of(&root.join("file")));
        // Read as a file that blocks; on Linux, without touching its atime.
        let flags = rustix::fs::fcntl_getfl(&file).unwrap();
        assert!(!flags.contains(OFlags::NONBLOCK));
        #[cfg(target_os = "linux")]
        assert!(flags.contains(OFlags::NOATIME));

        for name in ["fifo", "directory"] {
            let error = open_regular_file(&opened, name.as_bytes()).unwrap_err();
            assert_eq!(
                error.to_string(),
                "it is no longer a regular file",
                "{name}"
            );
        }
        let error = open_regular_file(&opened, b"link").unwrap_err();
        assert_eq!(
            rustix::io::Errno::from_io_error(&error),
            Some(rustix::io::Errno::LOOP)
        );
        // Linux says ENXIO, macOS EOPNOTSUPP.
        let error = open_regular_file(&opened, b"socket").unwrap_err();
        assert!(
            matches!(
                rustix::io::Errno::from_io_error(&error),
                Some(rustix::io::Errno::NXIO | rustix::io::Errno::OPNOTSUPP)
            ),
            "{error}"
        );

        // A root follows the symlink the user named, and is refused the same.
        let (_, stat) = open_regular_root(root.join("link").as_os_str()).unwrap();
        assert_eq!(key(&stat), key_of(&root.join("file")));
        let error = open_regular_root(root.join("fifo").as_os_str()).unwrap_err();
        assert_eq!(error.to_string(), "it is no longer a regular file");
    }

    #[test]
    fn a_mode_is_never_set_through_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::fs::write(root.join("file"), "file").unwrap();
        let opened = open_root(root.as_os_str()).unwrap();

        let error = set_mode_at(&opened, b"link", 0o4777).unwrap_err();
        assert_eq!(
            rustix::io::Errno::from_io_error(&error),
            Some(rustix::io::Errno::LOOP),
            "{error}"
        );
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&outside), 0o600);
        // A file is set as asked.
        set_mode_at(&opened, b"file", 0o640).unwrap();
        assert_eq!(mode(&root.join("file")), 0o640);
    }

    /// With `-x`, a directory on another filesystem than the one to stay
    /// on is not deleted, nor gone into; on that one, it is. A test can make
    /// no other filesystem without root, so here the one to stay on is
    /// another: see tests/owners.rs for mount points within a tree.
    #[test]
    fn a_directory_on_another_filesystem_is_not_deleted() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("extra/inner")).unwrap();
        std::fs::write(temp.path().join("extra/inner/file"), "x").unwrap();
        let opened = open_root(temp.path().as_os_str()).unwrap();
        #[allow(clippy::unnecessary_cast)]
        let device = rustix::fs::fstat(&opened).unwrap().st_dev as u64;
        let elsewhere = Keep::default();
        elsewhere.device.set(device + 1).unwrap();
        let error = remove_tree(&opened, b"extra", &elsewhere).unwrap_err();
        assert!(is_elsewhere(&error), "{error}");
        assert_eq!(error.to_string(), "it is on another filesystem");
        assert!(temp.path().join("extra/inner/file").exists());
        // A file is no directory, and goes wherever it is.
        std::fs::write(temp.path().join("loose"), "x").unwrap();
        remove_tree(&opened, b"loose", &elsewhere).unwrap();
        assert!(!temp.path().join("loose").exists());

        let here = Keep::default();
        here.device.set(device).unwrap();
        remove_tree(&opened, b"extra", &here).unwrap();
        assert!(!temp.path().join("extra").exists());
    }

    #[test]
    fn a_protected_directory_is_never_deleted_nor_let_into() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("extra/kept/inner")).unwrap();
        std::fs::write(root.join("extra/loose"), "x").unwrap();
        let kept = root.join("extra/kept");
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o500)).unwrap();
        let protect = [key_of(&kept)];
        let opened = open_root(root.as_os_str()).unwrap();

        // At the top, or deeper down, the removal stops there.
        let error =
            remove_tree(&opened, b"extra", &Keep::protecting(protect.to_vec())).unwrap_err();
        assert!(is_protected(&error), "{error}");
        let extra = open_directory(&opened, b"extra").unwrap();
        let error = remove_tree(&extra, b"kept", &Keep::protecting(protect.to_vec())).unwrap_err();
        assert!(is_protected(&error), "{error}");
        assert!(kept.join("inner").is_dir());
        // Not let into: its permissions are as they were.
        assert_eq!(
            std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
            0o500
        );
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Without it, all goes.
        remove_tree(&opened, b"extra", &Keep::default()).unwrap();
        assert!(!root.join("extra").exists());
    }

    /// A directory let into to be deleted, which then cannot be, gets its
    /// permissions back: here, one that holds a directory to protect.
    #[test]
    fn a_directory_let_into_that_cannot_be_deleted_gets_its_permissions_back() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("extra/kept")).unwrap();
        let protect = [key_of(&root.join("extra/kept"))];
        let extra = root.join("extra");
        std::fs::set_permissions(&extra, std::fs::Permissions::from_mode(0o000)).unwrap();
        let opened = open_root(root.as_os_str()).unwrap();
        let error =
            remove_tree(&opened, b"extra", &Keep::protecting(protect.to_vec())).unwrap_err();
        assert!(is_protected(&error), "{error}");
        let mode = std::fs::metadata(&extra).unwrap().permissions().mode() & 0o7777;
        std::fs::set_permissions(&extra, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode, 0o000);
    }

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
        assert_eq!(let_in, Some(0o000));
        assert_eq!(mode(&parent.join("locked")), 0o700);
        assert!(open_directory_to_work_in(&opened, b"link").is_err());
        assert_eq!(mode(&temp.path().join("elsewhere")), 0o000);
        // One its owner may work in already is left as it is.
        set(&parent.join("locked"), 0o750);
        let (_, let_in) = open_directory_to_work_in(&opened, b"locked").unwrap();
        assert_eq!(let_in, None);
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
                remove_tree(&root, name, &Keep::default()),
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
