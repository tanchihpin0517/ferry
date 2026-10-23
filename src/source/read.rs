//! The Source's readers: each opens a file it is handed, and reads it whole,
//! if it fits in a pack, or reads a segment of a larger one, hashing each
//! piece as it is read.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use anyhow::Result;

use crate::block::{self, Check, PackFile};
use crate::dirs::DirCache;
use crate::fsys;
use crate::manifest::{FileId, Timestamp};
use crate::stop::Stop;

use super::owner::Read;

/// How many bytes a file is read in at a time.
const READ_CHUNK: usize = 256 * 1024;

/// What a reader is handed.
pub(super) enum Job {
    /// A file to open, and read whole if it fits in a pack.
    Open { id: FileId, path: Vec<u8> },
    /// A segment of a file open, to read.
    Segment {
        id: FileId,
        file: Arc<File>,
        offset: u64,
        len: usize,
    },
}

/// Does what a reader is handed.
pub(super) fn read(job: Job, root: &OsStr, dirs: &DirCache, stop: &Stop) -> Read {
    match job {
        Job::Open { id, path } => open(root, dirs, id, path, stop),
        Job::Segment {
            id,
            file,
            offset,
            len,
        } => {
            let mut hasher = block::SegmentHasher::new(offset);
            let content = match read_part(&file, offset, len, stop, |bytes| hasher.update(bytes)) {
                Ok(content) => content,
                // The Source stopping, which nothing read then is sent for.
                Err(error) => Content {
                    bytes: Vec::new(),
                    unread: Some(format!("{error:#}")),
                },
            };
            Read::Segment {
                id,
                offset,
                content,
                value: hasher.finish(),
            }
        }
    }
}

/// Opens a file to send: one that fits in a pack is read whole, and hashed;
/// a larger one is left open, to be read a segment at a time.
fn open(root: &OsStr, dirs: &DirCache, id: FileId, path: Vec<u8>, stop: &Stop) -> Read {
    let (file, stat) = match open_source_file(root, dirs, &path) {
        Ok(opened) => opened,
        Err(error) => return Read::Unreadable { id, path, error },
    };
    #[allow(clippy::unnecessary_cast)]
    let (size, mtime) = (stat.st_size as u64, fsys::mtime(&stat));
    if size > block::SEGMENT {
        return Read::Large {
            id,
            file: Arc::new(file),
            size,
            mtime,
        };
    }
    match read_whole(&file, id, size, mtime, stop) {
        Ok((entry, bytes)) => Read::Small { entry, bytes },
        // Reading errors mark the file: this is the Source stopping, which
        // the owner sees before it would report it.
        Err(error) => Read::Unreadable {
            id,
            path,
            error: io::Error::other(error),
        },
    }
}

/// What was read of a file: its bytes, or, if they could not be read
/// whole, why, and none of them.
pub(super) struct Content {
    pub(super) bytes: Vec<u8>,
    pub(super) unread: Option<String>,
}

/// Reads `len` bytes of `file` from `offset`: as many as it had there when
/// it was opened, as rsync does, what it gained since being left for the
/// next run, which finds its mtime changed. Each piece is hashed as it is
/// read, while it is fresh in the cache. Of one that shrank, or whose
/// reading failed, nothing is sent but why, which fails it on the
/// Destination: nothing is sent again.
pub(super) fn read_part(
    file: &File,
    offset: u64,
    len: usize,
    stop: &Stop,
    mut hash: impl FnMut(&[u8]),
) -> Result<Content> {
    let mut bytes = vec![0; len];
    let mut done = 0;
    let unread = |why: String| Content {
        bytes: Vec::new(),
        unread: Some(why),
    };
    while done < len {
        // A large file stops being read as the Source stops.
        stop.check()?;
        let wanted = READ_CHUNK.min(len - done);
        match file.read_at(&mut bytes[done..done + wanted], offset + done as u64) {
            Ok(0) => return Ok(unread("it shrank while it was read".to_owned())),
            Ok(count) => {
                hash(&bytes[done..done + count]);
                done += count;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Ok(unread(format!("reading it failed: {error}"))),
        }
    }
    Ok(Content {
        bytes,
        unread: None,
    })
}

/// Reads a file of `size` bytes whole, or as much of it as it can, for a
/// pack.
pub(super) fn read_whole(
    file: &File,
    id: FileId,
    size: u64,
    mtime: Timestamp,
    stop: &Stop,
) -> Result<(PackFile, Vec<u8>)> {
    let mut hasher = blake3::Hasher::new();
    let content = read_part(file, 0, size as usize, stop, |bytes| {
        hasher.update(bytes);
    })?;
    let (len, check) = match content.unread {
        None => (
            content.bytes.len() as u32,
            Check::Hash(*hasher.finalize().as_bytes()),
        ),
        Some(why) => (0, Check::Unread(why)),
    };
    let entry = PackFile {
        file: id,
        len,
        mtime,
        check,
    };
    Ok((entry, content.bytes))
}

/// Opens a regular file of the tree by its path relative to the root, never
/// following a symlink below the root, and returns it with its status as
/// opened. The empty path is the root itself; any other is opened in its
/// directory from `dirs`, opened once from the root.
fn open_source_file(
    root: &OsStr,
    dirs: &DirCache,
    path: &[u8],
) -> io::Result<(File, rustix::fs::Stat)> {
    if path.is_empty() {
        return fsys::open_regular_root(root);
    }
    let (directory, name) = match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&b""[..], path),
    };
    fsys::open_regular_file(&*dirs.open(directory)?, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirs;
    use crate::manifest::Kind;

    /// As rsync does: a file is read up to the size it had when opened; one
    /// that grew since is cut there, and of one that shrank nothing goes
    /// but why: it is not padded, as each block says what it holds.
    #[test]
    fn a_file_is_read_up_to_the_size_it_had_when_opened() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        std::fs::write(&path, b"hello world").unwrap();
        let mtime = Timestamp {
            seconds: 1,
            nanoseconds: 0,
        };
        let read = |size: u64| {
            let file = File::open(&path).unwrap();
            read_whole(&file, 1, size, mtime, &Stop::new(None)).unwrap()
        };
        let hash = |bytes: &[u8]| Check::Hash(*blake3::hash(bytes).as_bytes());
        // Grown since: only what it had.
        let (entry, bytes) = read(5);
        assert_eq!(
            (entry.len, entry.check, bytes),
            (5, hash(b"hello"), b"hello".to_vec())
        );
        let (entry, bytes) = read(11);
        assert_eq!(entry.check, hash(b"hello world"));
        assert_eq!(bytes, b"hello world");
        // Shrunk since: none of it, and why.
        let (entry, bytes) = read(16);
        assert_eq!(
            (entry.len, entry.check, bytes),
            (
                0,
                Check::Unread("it shrank while it was read".to_owned()),
                Vec::new()
            )
        );
        // A segment, from where it begins, hashed as it is read.
        let file = File::open(&path).unwrap();
        let mut read = Vec::new();
        let content = read_part(&file, 6, 5, &Stop::new(None), |bytes| {
            read.extend_from_slice(bytes)
        })
        .unwrap();
        assert_eq!((content.bytes, content.unread), (b"world".to_vec(), None));
        assert_eq!(read, b"world");
    }

    /// A segment of a file is read where it is, and its chaining value made
    /// as it is: the same as made of its bytes at once.
    #[test]
    fn a_segment_is_read_with_its_chaining_value() {
        let temp = tempfile::tempdir().unwrap();
        let content = crate::test_support::Random::new(3).bytes(block::SEGMENT as usize + 1000);
        std::fs::write(temp.path().join("f"), &content).unwrap();
        let file = Arc::new(File::open(temp.path().join("f")).unwrap());
        let root = temp.path().as_os_str();
        let job = Job::Segment {
            id: 4,
            file,
            offset: block::SEGMENT,
            len: 1000,
        };
        let Read::Segment {
            id,
            offset,
            content: read,
            value,
        } = read_job(job, root)
        else {
            panic!("not a segment");
        };
        assert_eq!((id, offset, read.unread), (4, block::SEGMENT, None));
        assert_eq!(read.bytes, &content[block::SEGMENT as usize..]);
        assert_eq!(value, block::chaining_value(block::SEGMENT, &read.bytes));
    }

    fn read_job(job: Job, root: &OsStr) -> Read {
        read(
            job,
            root,
            &DirCache::new(root, dirs::KEPT),
            &Stop::new(None),
        )
    }

    fn mkfifo(path: &std::path::Path) {
        let directory = fsys::open_root(path.parent().unwrap().as_os_str()).unwrap();
        let name = path.file_name().unwrap().as_encoded_bytes();
        fsys::make_special(directory, name, &Kind::Fifo, 0o644).unwrap();
    }

    /// A file the scan found, made a FIFO before it is read, is reported as
    /// unreadable at once, rather than waited on for ever for a writer.
    #[test]
    fn a_file_made_a_fifo_since_the_scan_is_unreadable_rather_than_waited_on() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().as_os_str().to_owned();
        let fifo = temp.path().join("f");
        mkfifo(&fifo);

        let (done, prepared) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let job = Job::Open {
                id: 1,
                path: b"f".to_vec(),
            };
            let _ = done.send(read_job(job, &root));
        });
        let ready = match prepared.recv_timeout(Duration::from_secs(5)) {
            Ok(ready) => ready,
            Err(_) => {
                // Let the stuck reader go, so the test fails rather than hangs.
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo);
                panic!("opening a FIFO waited for a writer");
            }
        };
        match ready {
            Read::Unreadable { id, path, error } => {
                assert_eq!((id, path.as_slice()), (1, &b"f"[..]));
                assert_eq!(error.to_string(), "it is no longer a regular file");
            }
            _ => panic!("a FIFO was read"),
        }
    }

    /// A file made a directory since the scan is reported as unreadable, not
    /// sent as its size in zeros.
    #[test]
    fn a_file_made_a_directory_since_the_scan_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("d")).unwrap();
        let job = Job::Open {
            id: 1,
            path: b"d".to_vec(),
        };
        match read_job(job, temp.path().as_os_str()) {
            Read::Unreadable { error, .. } => {
                assert_eq!(error.to_string(), "it is no longer a regular file");
            }
            _ => panic!("a directory was read"),
        }
    }

    /// A root that is one file is opened as the files below a root are, but
    /// through a symlink the user named.
    #[test]
    fn a_root_that_is_a_file_is_read_through_a_symlink_but_not_a_fifo() {
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, b"content").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let root_file = Job::Open {
            id: 0,
            path: Vec::new(),
        };
        match read_job(root_file, link.as_os_str()) {
            Read::Small { entry, bytes } => {
                assert_eq!((entry.len, bytes), (7, b"content".to_vec()));
            }
            _ => panic!("the root was not read"),
        }

        let fifo = temp.path().join("fifo");
        mkfifo(&fifo);
        let root = fifo.into_os_string();
        let ready =
            crate::test_support::within("opening a FIFO root", Duration::from_secs(5), move || {
                let job = Job::Open {
                    id: 0,
                    path: Vec::new(),
                };
                matches!(read_job(job, &root), Read::Unreadable { .. })
            });
        assert!(ready, "a FIFO root was read");
    }
}
