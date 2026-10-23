//! What a Source gone wrong, or ill-willed, may send a Destination: a
//! manifest and blocks made of names a peer must never use, such as `..`,
//! `a/b`, or a symlink out, of IDs out of turn, of content whose hash may
//! not match, and of hashes of files never sent in segments. Whatever comes, the Destination never panics,
//! and nothing outside its root changes: a tree beside it, and the symlinks
//! inside it that point there, are as they were.
#![no_main]

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use arbitrary::{Result, Unstructured};
use ferry::block::{self, Body, Check, PackFile};
use ferry::destination::{self, Options};
use ferry::manifest::{Entry, FileId, Kind, Manifest, Timestamp};
use ferry::protocol::{self, FileHash};
use libfuzzer_sys::fuzz_target;

/// The names a Source may send, good and bad.
const NAMES: &[&[u8]] = &[
    b"a", b"b", b"c", b"..", b".", b"", b"out", b"abs", b"a/b", b"x\0y", b"kept",
];

/// The directories its parts may be of.
const PATHS: &[&[u8]] = &[
    b"", b"a", b"b", b"a/c", b"out", b"abs", b"..", b"a/..", b"/tmp",
];

/// Where a symlink may point.
const TARGETS: &[&[u8]] = &[b"../outside", b"/", b"a", b"../../outside/secret", b""];

fn entry(u: &mut Unstructured, id: FileId) -> Result<Entry> {
    let kind = match u.int_in_range(0..=5)? {
        0 | 1 => Kind::File,
        2 => Kind::Directory,
        3 => Kind::Symlink {
            target: u.choose(TARGETS)?.to_vec(),
        },
        4 => Kind::Fifo,
        _ => Kind::Unreadable,
    };
    Ok(Entry {
        // Mostly in turn, now and then not.
        id: if u.ratio(1, 16)? { u.arbitrary()? } else { id },
        name: u.choose(NAMES)?.to_vec(),
        kind,
        size: u.int_in_range(0..=64)?,
        mtime: Timestamp {
            seconds: u.int_in_range(0..=2_000_000_000)?,
            nanoseconds: if u.ratio(1, 16)? { u.arbitrary()? } else { 0 },
        },
        mode: u.int_in_range(0..=0o7777)?,
        uid: 0,
        gid: 0,
    })
}

/// A manifest, then the blocks of its files, as packets.
fn packets(u: &mut Unstructured) -> Result<Vec<u8>> {
    let mut packets = Vec::new();
    let mut next: FileId = 1;
    let mut files = Vec::new();
    let root_kind = if u.ratio(7, 8)? {
        Kind::Directory
    } else {
        Kind::File
    };
    let root = Entry {
        kind: root_kind,
        ..entry(u, 0)?
    };
    let root = Entry {
        id: 0,
        name: Vec::new(),
        ..root
    };
    if root.kind == Kind::File {
        files.push((0, root.size));
    }
    let _ = protocol::send(&mut packets, &Manifest::Root(root));
    for _ in 0..u.int_in_range(0..=6)? {
        let mut entries = Vec::new();
        for _ in 0..u.int_in_range(0..=4)? {
            let made = entry(u, next)?;
            next += 1;
            if made.kind == Kind::File {
                files.push((made.id, made.size));
            }
            entries.push(made);
        }
        let part = Manifest::Directory {
            path: u.choose(PATHS)?.to_vec(),
            entries,
            more: u.ratio(1, 8)?,
        };
        let _ = protocol::send(&mut packets, &part);
    }
    if u.ratio(7, 8)? {
        let _ = protocol::send(&mut packets, &Manifest::End);
    }
    let mut payload = Vec::new();
    let mut entries = Vec::new();
    for (id, size) in files {
        if u.ratio(1, 4)? {
            continue;
        }
        let content: Vec<u8> = (0..size).map(|index| index as u8).collect();
        let check = if u.ratio(1, 16)? {
            Check::Unread("it shrank while it was read".to_owned())
        } else if u.ratio(1, 8)? {
            Check::Hash([0; 32])
        } else {
            Check::Hash(*blake3::hash(&content).as_bytes())
        };
        let len = match check {
            Check::Hash(_) => {
                payload.extend_from_slice(&content);
                size as u32
            }
            Check::Unread(_) => 0,
        };
        entries.push(PackFile {
            file: id,
            len,
            mtime: Timestamp {
                seconds: 1_600_000_000,
                nanoseconds: 0,
            },
            check,
        });
        if u.ratio(1, 16)? {
            let hash = FileHash {
                file: id,
                hash: [0; 32],
            };
            let _ = protocol::send(&mut packets, &hash);
        }
    }
    if !entries.is_empty()
        && let Ok(encoded) =
            block::encode(Body::Pack { files: entries }, &payload, u.arbitrary()?, 1)
    {
        let _ = protocol::send_data(&mut packets, &encoded);
    }
    if u.ratio(7, 8)? {
        let _ = protocol::send_data(&mut packets, &block::END);
    }
    Ok(packets)
}

/// Every path under `root`, with its content, or its target, as it is.
fn tree(root: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut waiting = vec![root.to_path_buf()];
    while let Some(path) = waiting.pop() {
        let key = path.as_os_str().as_bytes().to_vec();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path).unwrap_or_default();
            found.insert(key, target.as_os_str().as_bytes().to_vec());
        } else if metadata.is_dir() {
            found.insert(key, b"/".to_vec());
            if let Ok(names) = std::fs::read_dir(&path) {
                waiting.extend(names.flatten().map(|name| name.path()));
            }
        } else {
            found.insert(key, std::fs::read(&path).unwrap_or_default());
        }
    }
    found
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(packets) = packets(&mut u) else {
        return;
    };
    let delete = u.arbitrary().unwrap_or(false);
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "secret").unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("kept"), "kept").unwrap();
    std::os::unix::fs::symlink("../outside", root.join("out")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("abs")).unwrap();
    let before = tree(&outside);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
        one_file_system: false,
    };
    let _ = destination::run(&mut &packets[..], &mut Vec::new(), &options);
    assert_eq!(tree(&outside), before, "outside the root changed");
    let beside: Vec<_> = std::fs::read_dir(temp.path())
        .unwrap()
        .flatten()
        .map(|name| name.file_name())
        .collect();
    assert!(beside.len() <= 2, "{beside:?} beside the root");
});
