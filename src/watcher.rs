//! The watcher: a server in its third role, through a watch, that watches
//! SOURCE across the watch's rounds and says when it changed, so that a
//! round comes only then. It has no peer and no stream: what it says goes
//! on its control connection, and it ends as the client closes that.
//!
//! First it judges whether every change in SOURCE can be seen: each
//! filesystem SOURCE spans, its own and each mounted under it, or with
//! `-x` its own alone, must be one whose every change the kernel here is
//! told of. On Linux, that is ext2 to ext4, XFS, Btrfs, tmpfs, and
//! overlayfs, by the type `statfs` says, and the mount table's for those
//! under it, which are not touched; any other, as NFS, which says nothing
//! of what another machine changed on it, is not. On macOS, it is one the
//! system says is local.
//!
//! Then it watches: on Linux, each directory with inotify, as it does not
//! watch a tree, each new directory as it comes, but not what is opened or
//! read, as each round's own reading would be taken for a change; on
//! macOS, the whole tree with FSEvents. A SOURCE that is not a directory is
//! watched by the directory that holds it. Each change is said within
//! [`GATHER`] of it, many in one.
//!
//! What it cannot see, it says, and watches no more, each round of the
//! watch then coming on time: a filesystem not among those, the limit on
//! watches reached, or SOURCE itself, or what holds it, deleted or moved,
//! whose watches went with it.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::Receiver;

use crate::report::Report;

/// How long changes are gathered into one word to the client.
pub const GATHER: Duration = Duration::from_millis(50);

/// How the watcher tells the client what it says, from any thread.
pub type Telling = Arc<dyn Fn(&Report) -> Result<()> + Send + Sync>;

/// What the watcher says, to the client.
type Tell<'a> = &'a (dyn Fn(&Report) -> Result<()> + Send + Sync);

/// Watches `root`, as SOURCE names it, saying what changes there with
/// `tell`, until `closed` says the client closed the control connection.
/// With `one_file_system`, as `-x`, only its own filesystem counts.
pub fn serve(
    root: &OsStr,
    one_file_system: bool,
    telling: &Telling,
    closed: &Receiver<()>,
) -> Result<()> {
    let tell: Tell = &**telling;
    let root = Path::new(root);
    // A directory is watched itself, what it holds too; anything else by
    // the directory that holds it.
    let directory = std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_dir());
    let watched = if directory {
        root.to_path_buf()
    } else {
        match root.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        }
    };
    let unseen = match std::fs::canonicalize(&watched) {
        Ok(canonical) => unseen(&watched, &canonical, directory && !one_file_system),
        Err(error) => Some((watched.clone(), format!("it cannot be read: {error}"))),
    };
    if let Some((path, why)) = unseen {
        unwatched(tell, &path, why);
        let _ = closed.recv();
        return Ok(());
    }
    let telling = Arc::clone(telling);
    std::thread::Builder::new()
        .name("watcher".to_owned())
        .spawn(move || {
            let tell: Tell = &*telling;
            if let Err(error) = watch(&watched, directory, one_file_system, tell) {
                unwatched(tell, &watched, format!("{error:#}"));
            }
        })?;
    // The client closed the control connection: the watch is over, and the
    // server ends, what watches with it.
    let _ = closed.recv();
    Ok(())
}

/// Tells the client what changes at `path` cannot be seen, and why.
fn unwatched(tell: Tell, path: &Path, why: String) {
    let _ = tell(&Report::Unwatched {
        path: path.as_os_str().as_bytes().to_vec(),
        why,
    });
}

/// What in `watched`, named so and found at `canonical`, is on a
/// filesystem whose changes cannot all be seen, and why: the filesystem
/// itself, or, with `under`, one mounted under it.
fn unseen(watched: &Path, canonical: &Path, under: bool) -> Option<(PathBuf, String)> {
    if crate::inject::unwatched() {
        return Some((
            watched.to_path_buf(),
            "it is on a filesystem a test says its changes are not seen on".to_owned(),
        ));
    }
    if let Err(why) = filesystem::seen(canonical) {
        return Some((watched.to_path_buf(), why));
    }
    if !under {
        return None;
    }
    filesystem::mounted_under(canonical)
        .into_iter()
        .find_map(|(mount, kind)| filesystem::seen_kind(&kind).err().map(|why| (mount, why)))
}

#[cfg(target_os = "linux")]
mod filesystem {
    //! Which filesystems tell the kernel every change, on Linux.

    use std::path::{Path, PathBuf};

    /// Filesystems whose every change the kernel here is told of, by the
    /// type `statfs` says, and as the mount table names them.
    const SEEN: [(i64, &str); 7] = [
        (0xEF53, "ext2"),
        (0xEF53, "ext3"),
        (0xEF53, "ext4"),
        (0x5846_5342, "xfs"),
        (0x9123_683E, "btrfs"),
        (0x0102_1994, "tmpfs"),
        (0x794C_7630, "overlay"),
    ];

    /// Whether every change on the filesystem `path` is on is seen, or why
    /// not.
    pub fn seen(path: &Path) -> Result<(), String> {
        let found =
            rustix::fs::statfs(path).map_err(|error| format!("it cannot be read: {error}"))?;
        #[allow(clippy::unnecessary_cast)]
        let kind = found.f_type as i64;
        if SEEN.iter().any(|(magic, _)| *magic == kind) {
            return Ok(());
        }
        // Named as the mount table names it, where it can be found there.
        let name = mounted_at(path).unwrap_or_else(|| format!("a filesystem of type {kind:#x}"));
        Err(format!(
            "it is on {name}, not a filesystem whose every change is told here, as ext4, XFS, Btrfs, tmpfs or overlayfs"
        ))
    }

    /// The type of the filesystem `path`, a canonical path, is on, as the
    /// mount table names it: that of the last mount at the longest mount
    /// point that holds it.
    fn mounted_at(path: &Path) -> Option<String> {
        let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        table
            .lines()
            .filter_map(|line| {
                let (before, after) = line.split_once(" - ")?;
                let mount = unescape(before.split(' ').nth(4)?);
                let kind = after.split(' ').next()?.to_owned();
                path.starts_with(&mount).then_some((mount, kind))
            })
            .fold(
                None,
                |found: Option<(PathBuf, String)>, (mount, kind)| match found {
                    // A later mount at the same point is over the one before.
                    Some(longest) if longest.0.as_os_str().len() > mount.as_os_str().len() => {
                        Some(longest)
                    }
                    _ => Some((mount, kind)),
                },
            )
            .map(|(_, kind)| kind)
    }

    /// Whether every change on a filesystem the mount table names `kind` is
    /// seen, or why not.
    pub fn seen_kind(kind: &str) -> Result<(), String> {
        if SEEN.iter().any(|(_, name)| *name == kind) {
            Ok(())
        } else {
            Err(format!(
                "it is on {kind}, not a filesystem whose every change is told here, as ext4, XFS, Btrfs, tmpfs or overlayfs"
            ))
        }
    }

    /// Each filesystem mounted under `root`, a canonical path, and its
    /// type, as the mount table says.
    pub fn mounted_under(root: &Path) -> Vec<(PathBuf, String)> {
        let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return Vec::new();
        };
        table
            .lines()
            .filter_map(|line| {
                let (before, after) = line.split_once(" - ")?;
                let mount = unescape(before.split(' ').nth(4)?);
                let kind = after.split(' ').next()?.to_owned();
                (mount.starts_with(root) && mount != root).then_some((mount, kind))
            })
            .collect()
    }

    /// A path as the mount table writes it: a space, tab, newline or
    /// backslash as `\` and three octal digits.
    fn unescape(written: &str) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        let bytes = written.as_bytes();
        let mut path = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let octal = bytes.get(index + 1..index + 4).and_then(|digits| {
                digits
                    .iter()
                    .all(|digit| (b'0'..=b'7').contains(digit))
                    .then(|| {
                        digits
                            .iter()
                            .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'))
                    })
            });
            match (bytes[index], octal) {
                (b'\\', Some(value)) if value <= 0xFF => {
                    path.push(value as u8);
                    index += 4;
                }
                (byte, _) => {
                    path.push(byte);
                    index += 1;
                }
            }
        }
        PathBuf::from(std::ffi::OsString::from_vec(path))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_mount_table_s_escapes_are_read() {
            assert_eq!(unescape(r"/mnt/a\040b"), PathBuf::from("/mnt/a b"));
            assert_eq!(unescape(r"/mnt/a\134b"), PathBuf::from(r"/mnt/a\b"));
            assert_eq!(unescape(r"/mnt/a\9b"), PathBuf::from(r"/mnt/a\9b"));
        }

        #[test]
        fn the_root_s_filesystem_is_named_as_the_mount_table_names_it() {
            let named = mounted_at(Path::new("/")).unwrap();
            assert!(!named.is_empty());
            assert!(!named.contains(' '), "{named}");
        }

        #[test]
        fn local_filesystems_are_seen_and_nfs_is_not() {
            assert!(seen_kind("ext4").is_ok());
            assert!(seen_kind("overlay").is_ok());
            assert!(seen_kind("nfs4").unwrap_err().contains("nfs4"));
            assert!(seen_kind("wekafs").is_err());
        }
    }
}

#[cfg(target_os = "macos")]
mod filesystem {
    //! Which filesystems tell the kernel every change, on macOS: those the
    //! system says are local.

    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    /// Whether every change on the filesystem `path` is on is seen, or why
    /// not.
    pub fn seen(path: &Path) -> Result<(), String> {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "its path holds a NUL".to_owned())?;
        let mut found: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is a C string, and `found` a statfs to fill.
        if unsafe { libc::statfs(name.as_ptr(), &mut found) } != 0 {
            return Err(format!(
                "it cannot be read: {}",
                std::io::Error::last_os_error()
            ));
        }
        local(&found)
    }

    /// Whether the filesystem `found` says it is local, or what it is.
    fn local(found: &libc::statfs) -> Result<(), String> {
        if found.f_flags & libc::MNT_LOCAL as u32 != 0 {
            return Ok(());
        }
        // SAFETY: the system ends its type's name with a NUL.
        let kind = unsafe { CStr::from_ptr(found.f_fstypename.as_ptr()) };
        Err(format!(
            "it is on {}, not a local filesystem, whose every change is told here",
            kind.to_string_lossy()
        ))
    }

    /// On macOS, a filesystem mounted under SOURCE is judged as it is
    /// listed, with its own flags: see [`mounted_under`].
    pub fn seen_kind(kind: &str) -> Result<(), String> {
        if kind.is_empty() {
            Ok(())
        } else {
            Err(kind.to_owned())
        }
    }

    /// Each filesystem mounted under `root`, a canonical path, with why its
    /// changes are not all seen, or nothing where they are.
    pub fn mounted_under(root: &Path) -> Vec<(PathBuf, String)> {
        let mut mounted: *mut libc::statfs = std::ptr::null_mut();
        // SAFETY: the system points `mounted` at as many as it returns,
        // which stay until the next call.
        let count = unsafe { libc::getmntinfo(&mut mounted, libc::MNT_NOWAIT) };
        if count <= 0 || mounted.is_null() {
            return Vec::new();
        }
        // SAFETY: as many as the system said.
        let all = unsafe { std::slice::from_raw_parts(mounted, count as usize) };
        all.iter()
            .filter_map(|found| {
                // SAFETY: the system ends the mount point with a NUL.
                let on = unsafe { CStr::from_ptr(found.f_mntonname.as_ptr()) };
                let mount = PathBuf::from(std::ffi::OsStr::from_bytes(on.to_bytes()));
                (mount.starts_with(root) && mount != root)
                    .then(|| (mount, local(found).err().unwrap_or_default()))
            })
            .collect()
    }
}

#[cfg(target_os = "linux")]
use linux::watch;

#[cfg(target_os = "linux")]
mod linux {
    //! Watching with inotify, as rustix has it: a watch for each directory.

    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::mem::MaybeUninit;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use anyhow::{Context, Result};
    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    use rustix::io::Errno;

    use super::{GATHER, Tell, unwatched};
    use crate::report::Report;

    /// What a directory's watch is told of: what is made, changed, deleted
    /// or moved in it, and its own deletion or move; not what is opened or
    /// read, as each round's own reading would be taken for a change.
    fn mask() -> WatchFlags {
        WatchFlags::ATTRIB
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::DELETE_SELF
            | WatchFlags::MODIFY
            | WatchFlags::MOVE_SELF
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::DONT_FOLLOW
            | WatchFlags::ONLYDIR
    }

    /// The limit on watches was reached.
    struct Full(PathBuf);

    /// The directories watched, by their watch.
    struct Tree {
        inotify: OwnedFd,
        paths: HashMap<i32, PathBuf>,
        /// With `-x`, the device of the root's filesystem, the only one
        /// watched.
        device: Option<u64>,
        /// At most this many watches, as a test says the limit is.
        limit: Option<usize>,
    }

    impl Tree {
        /// Watches `top`, and each directory under it, as deep as it goes,
        /// not following links; with `deep` false, `top` alone. One gone
        /// meanwhile, or that cannot be read, is let be: there is nothing
        /// of it to copy either.
        fn add(&mut self, top: &Path, deep: bool) -> Result<(), Full> {
            let mut directories = vec![top.to_path_buf()];
            while let Some(directory) = directories.pop() {
                if self.limit.is_some_and(|limit| self.paths.len() >= limit) {
                    return Err(Full(directory));
                }
                match inotify::add_watch(&self.inotify, &directory, mask()) {
                    Ok(watch) => {
                        self.paths.insert(watch, directory.clone());
                    }
                    Err(Errno::NOSPC) => return Err(Full(directory)),
                    Err(_) => continue,
                }
                if !deep {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        continue;
                    }
                    if let Some(device) = self.device
                        && !entry
                            .metadata()
                            .is_ok_and(|metadata| metadata.dev() == device)
                    {
                        continue;
                    }
                    directories.push(entry.path());
                }
            }
            Ok(())
        }
    }

    /// Watches `watched`, each directory in it with `deep`, saying each
    /// change with `tell`, until what it watches is gone, or the limit on
    /// watches is reached, which it says.
    pub fn watch(watched: &Path, deep: bool, one_file_system: bool, tell: Tell) -> Result<()> {
        let device = if one_file_system {
            Some(std::fs::metadata(watched).context("look at SOURCE")?.dev())
        } else {
            None
        };
        let mut tree = Tree {
            inotify: inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)
                .context("start inotify")?,
            paths: HashMap::new(),
            device,
            limit: crate::inject::watches(),
        };
        if let Err(Full(at)) = tree.add(watched, deep) {
            unwatched(tell, &at, limit());
            return Ok(());
        }
        tell(&Report::Watching)?;
        let mut buffer = vec![MaybeUninit::<u8>::uninit(); 64 * 1024];
        let events = tree.inotify.try_clone().context("start inotify")?;
        let mut reader = inotify::Reader::new(&events, &mut buffer);
        // A change not said yet, and when the last was.
        let mut pending = false;
        let mut said = Instant::now() - GATHER;
        loop {
            // Wait for more, or for the moment a change gathered is due.
            let timeout = pending.then(|| GATHER.saturating_sub(said.elapsed()));
            let timeout = timeout.map(|left| rustix::event::Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            });
            let mut fds = [rustix::event::PollFd::new(
                &events,
                rustix::event::PollFlags::IN,
            )];
            match rustix::event::poll(&mut fds, timeout.as_ref()) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => return Err(error).context("wait for inotify"),
            }
            let mut added = Vec::new();
            let mut gone = false;
            let mut again = false;
            loop {
                let event = match reader.next() {
                    Ok(event) => event,
                    Err(Errno::AGAIN | Errno::INTR) => break,
                    Err(error) => return Err(error).context("read inotify"),
                };
                pending = true;
                let flags = event.events();
                if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                    // Lost, a directory made meanwhile among them: each is
                    // watched again.
                    again = true;
                    continue;
                }
                if flags.contains(ReadFlags::IGNORED) {
                    tree.paths.remove(&event.wd());
                    continue;
                }
                let Some(directory) = tree.paths.get(&event.wd()) else {
                    continue;
                };
                if flags.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF)
                    && directory == watched
                {
                    gone = true;
                    continue;
                }
                // A directory made, moved in, or let in: watched, with all
                // it holds already.
                if deep
                    && flags.contains(ReadFlags::ISDIR)
                    && flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO | ReadFlags::ATTRIB)
                    && let Some(name) = event.file_name()
                {
                    added.push(directory.join(OsStr::from_bytes(name.to_bytes())));
                }
            }
            if gone {
                tell(&Report::Changed)?;
                unwatched(
                    tell,
                    watched,
                    "it was deleted or moved, and its watches with it".to_owned(),
                );
                return Ok(());
            }
            for directory in added {
                if let Err(Full(at)) = tree.add(&directory, true) {
                    tell(&Report::Changed)?;
                    unwatched(tell, &at, limit());
                    return Ok(());
                }
            }
            if again && let Err(Full(at)) = tree.add(watched, deep) {
                tell(&Report::Changed)?;
                unwatched(tell, &at, limit());
                return Ok(());
            }
            if pending && said.elapsed() >= GATHER {
                tell(&Report::Changed)?;
                pending = false;
                said = Instant::now();
            }
        }
    }

    /// Why the limit on watches stops the watching.
    fn limit() -> String {
        let set = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .map(|limit| format!(", {}", limit.trim()))
            .unwrap_or_default();
        format!(
            "the limit on inotify watches, fs.inotify.max_user_watches{set}, was reached, a watch for each directory"
        )
    }
}

#[cfg(target_os = "macos")]
use macos::watch;

#[cfg(target_os = "macos")]
mod macos {
    //! Watching with FSEvents: one for the whole tree.

    use std::path::Path;
    use std::time::Instant;

    use anyhow::{Context, Result};
    use notify::event::{AccessKind, AccessMode, EventKind};
    use notify::{RecursiveMode, Watcher};

    use super::{GATHER, Tell, unwatched};
    use crate::report::Report;

    /// Watches `watched`, all it holds with `deep`, saying each change with
    /// `tell`.
    pub fn watch(watched: &Path, deep: bool, _one_file_system: bool, tell: Tell) -> Result<()> {
        let (events_in, events) = crossbeam_channel::unbounded();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = events_in.send(event);
        })
        .context("start FSEvents")?;
        let mode = if deep {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(watched, mode).context("watch SOURCE")?;
        tell(&Report::Watching)?;
        let mut pending = false;
        let mut said = Instant::now() - GATHER;
        loop {
            let event = if pending {
                events.recv_timeout(GATHER.saturating_sub(said.elapsed()))
            } else {
                events
                    .recv()
                    .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
            };
            match event {
                Ok(Ok(event)) => {
                    // What is opened or read is no change: each round's own
                    // reading would be taken for one.
                    let read = matches!(
                        event.kind,
                        EventKind::Access(
                            AccessKind::Open(_)
                                | AccessKind::Read
                                | AccessKind::Close(AccessMode::Read)
                                | AccessKind::Close(AccessMode::Execute)
                        )
                    );
                    if !read || event.need_rescan() {
                        pending = true;
                    }
                }
                Ok(Err(error)) => {
                    if let notify::ErrorKind::MaxFilesWatch = error.kind {
                        tell(&Report::Changed)?;
                        unwatched(tell, watched, format!("{error}"));
                        return Ok(());
                    }
                    pending = true;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if pending && said.elapsed() >= GATHER {
                tell(&Report::Changed)?;
                pending = false;
                said = Instant::now();
            }
        }
    }
}
