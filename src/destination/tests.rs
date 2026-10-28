use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

use crate::block::{self, Body, Check, PackFile, Segment};
use crate::fsys;
use crate::manifest::{Entry, FileId, Kind, Manifest};
use crate::mux::{self};
use crate::protocol::{self, BlocksEnd, Control, FileHash, Needs, PacketType, Results};
use crate::report::{Report, Reporter, Transfer};

use super::apply::*;
use super::decider::{Command, tell_deleted};
use super::*;
use crate::manifest::Timestamp;

pub(super) fn entry(id: FileId, name: &[u8], kind: Kind, size: u64) -> Entry {
    Entry {
        id,
        name: name.to_vec(),
        kind: kind.clone(),
        size,
        mtime: Timestamp {
            seconds: 1_600_000_000,
            nanoseconds: 0,
        },
        mode: if kind == Kind::Directory {
            0o755
        } else {
            0o644
        },
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

/// The hash the Source sends of `content`.
fn hash(content: &[u8]) -> Check {
    Check::Hash(*blake3::hash(content).as_bytes())
}

/// A pack of whole files, each its ID, content, and the hash the Source
/// says it has, or why it could not read it whole, as the Source encodes
/// it, compressed or not.
fn pack_of(files: &[(FileId, &[u8], Check)], compress: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    let files = files
        .iter()
        .map(|(id, content, check)| {
            let len = match check {
                Check::Hash(_) => {
                    payload.extend_from_slice(content);
                    content.len() as u32
                }
                Check::Unread(_) => 0,
            };
            PackFile {
                file: *id,
                len,
                mtime: entry(*id, b"x", Kind::File, 0).mtime,
                check: check.clone(),
            }
        })
        .collect();
    block::encode(Body::Pack { files }, &payload, compress, 3).unwrap()
}

/// A pack, compressed.
fn pack(files: &[(FileId, &[u8], Check)]) -> Vec<u8> {
    pack_of(files, true)
}

/// The segment of `content`, a file's, from `offset`, as the Source encodes
/// it, with its chaining value.
fn segment_of(id: FileId, content: &[u8], offset: u64) -> Vec<u8> {
    let end = (offset + block::SEGMENT).min(content.len() as u64);
    let bytes = &content[offset as usize..end as usize];
    let segment = Segment {
        file: id,
        offset,
        len: bytes.len() as u32,
        size: content.len() as u64,
        mtime: entry(id, b"x", Kind::File, 0).mtime,
        check: Check::Hash(block::chaining_value(offset, bytes)),
    };
    block::encode(Body::Segment(segment), bytes, false, 3).unwrap()
}

/// The blocks a Source sends of files, each its ID, content, and the hash
/// it says the content has: those up to a segment in a pack, larger ones a
/// segment at a time; and the hashes of those, which go apart.
fn blocks_of(files: &[(FileId, &[u8], [u8; 32])]) -> (Vec<u8>, Vec<FileHash>) {
    let small: Vec<(FileId, &[u8], Check)> = files
        .iter()
        .filter(|(_, content, _)| content.len() as u64 <= block::SEGMENT)
        .map(|&(id, content, hash)| (id, content, Check::Hash(hash)))
        .collect();
    let mut data = if small.is_empty() {
        Vec::new()
    } else {
        pack(&small)
    };
    let mut hashes = Vec::new();
    for &(id, content, hash) in files {
        if content.len() as u64 > block::SEGMENT {
            for index in 0..block::segments(content.len() as u64) {
                data.extend(segment_of(id, content, index * block::SEGMENT));
            }
            hashes.push(FileHash { file: id, hash });
        }
    }
    number(&mut data, 0);
    (data, hashes)
}

/// Numbers the blocks one after another in `data`, from `first`, as a
/// Source does as it sends them, but for one cut short; returns the number
/// after the last.
fn number(data: &mut [u8], first: u64) -> u64 {
    let (mut at, mut next) = (0, first);
    while at + 16 <= data.len() {
        let length = |from: usize| {
            u32::from_le_bytes(data[at + from..at + from + 4].try_into().unwrap()) as usize
        };
        let whole = 16 + length(0) + length(4);
        if at + whole > data.len() {
            break;
        }
        block::number(&mut data[at..], next);
        next += 1;
        at += whole;
    }
    next
}

/// How many whole blocks are in `data`.
fn blocks_in(data: &[u8]) -> u64 {
    number(&mut data.to_vec(), 0)
}

/// The Source's word that it sent `blocks` blocks.
fn send_end(packets: &mut Vec<u8>, blocks: u64) {
    protocol::send(packets, &BlocksEnd { blocks }).unwrap();
}

/// The blocks in `data`, as data packets, as a Source cuts them.
fn send_blocks(packets: &mut Vec<u8>, data: &[u8]) {
    for piece in data.chunks(mux::DATA_PACKET) {
        protocol::send_data(packets, piece).unwrap();
    }
}

/// What a Source would send for one file, `hello`, read whole or, with
/// why, not.
fn packets(unread: Option<&str>) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    )
    .unwrap();
    let file = entry(1, b"f", Kind::File, 5);
    let directory = Manifest::Directory {
        path: Vec::new(),
        entries: vec![file],
        more: false,
    };
    protocol::send(&mut packets, &directory).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let check = match unread {
        Some(why) => Check::Unread(why.to_owned()),
        None => hash(b"hello"),
    };
    protocol::send_data(&mut packets, &pack(&[(1, b"hello", check)])).unwrap();
    send_end(&mut packets, 1);
    packets
}

/// A manifest of files at the root, each its name, content, and the hash
/// the Source says it has, then the blocks of them, as a Source sends them,
/// and the hashes of those in segments. The blocks are cut after `keep`
/// bytes, and neither those hashes nor the end sent, if `keep` is given;
/// `then` comes last.
fn files_packets(
    files: &[(&[u8], &[u8], [u8; 32])],
    keep: Option<usize>,
    then: Option<Control>,
) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    )
    .unwrap();
    let entries: Vec<Entry> = files
        .iter()
        .enumerate()
        .map(|(index, (name, content, _))| {
            entry(index as FileId + 1, name, Kind::File, content.len() as u64)
        })
        .collect();
    let directory = Manifest::Directory {
        path: Vec::new(),
        entries,
        more: false,
    };
    protocol::send(&mut packets, &directory).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let (mut data, hashes) = blocks_of(&with_ids(files));
    if let Some(keep) = keep {
        data.truncate(keep);
    }
    send_blocks(&mut packets, &data);
    if keep.is_none() {
        for hash in &hashes {
            protocol::send(&mut packets, hash).unwrap();
        }
        send_end(&mut packets, blocks_in(&data));
    }
    if let Some(then) = then {
        protocol::send(&mut packets, &then).unwrap();
    }
    packets
}

/// Files by name, numbered from 1 as [`files_packets`] does.
fn with_ids<'a>(files: &[(&[u8], &'a [u8], [u8; 32])]) -> Vec<(FileId, &'a [u8], [u8; 32])> {
    files
        .iter()
        .enumerate()
        .map(|(index, &(_, content, hash))| (index as FileId + 1, content, hash))
        .collect()
}

/// The names in `root` that are temporary files.
fn temporaries(root: &Path) -> Vec<String> {
    std::fs::read_dir(root)
        .map(|names| {
            names
                .map(|name| name.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".ferry-"))
                .collect()
        })
        .unwrap_or_default()
}

/// Runs the Destination on `packets`, which it fails on, and returns why.
fn run_failing(root: &Path, packets: &[u8]) -> anyhow::Error {
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    run(&mut &packets[..], &mut Vec::new(), &options).unwrap_err()
}

/// Content that does not match the hash the Source sent is never put in
/// place: the file fails, its temporary file goes, and a file already
/// there is left as it was.
#[test]
fn content_that_does_not_match_its_hash_fails_and_leaves_the_old_file() {
    for old in [Some(&b"old!!"[..]), None] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir(&root).unwrap();
        if let Some(old) = old {
            std::fs::write(root.join("f"), old).unwrap();
        }
        let files: [(&[u8], &[u8], [u8; 32]); 1] =
            [(b"f", b"hellO", *blake3::hash(b"hello").as_bytes())];
        let (answers, reports) = run_reporting(&root, &files_packets(&files, None, None));
        let results = decoded::<Results>(&answers);
        assert!(
            matches!(&results[..], [Results::Failed { id: 1, message }]
                if message.contains("hash does not match")),
            "{results:?}"
        );
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        let failed: u64 = reports
            .iter()
            .filter_map(|report| match report {
                Report::Counts(counts) => Some(counts.failed),
                _ => None,
            })
            .sum();
        assert_eq!(failed, 1);
        match old {
            Some(old) => {
                assert_eq!(std::fs::read(root.join("f")).unwrap(), old);
                assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            }
            None => assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0),
        }
    }
}

/// Blocks cut short, as when the connection drops, leave no temporary
/// file: neither of a block cut, nor of a file whose segments did not all
/// come.
#[test]
fn blocks_cut_short_leave_no_temporary_file() {
    let big = vec![7u8; block::SEGMENT as usize + 1_000];
    let files: [(&[u8], &[u8], [u8; 32]); 2] = [
        (b"a", b"hello", *blake3::hash(b"hello").as_bytes()),
        (b"big", &big, *blake3::hash(&big).as_bytes()),
    ];
    let (data, _) = blocks_of(&with_ids(&files));
    // Inside the pack, and inside the second segment.
    for keep in [10, data.len() - 500] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let packets = files_packets(&files, Some(keep), None);
        let error = run_failing(&root, &packets);
        assert!(format!("{error:#}").contains("the data ended"), "{error:#}");
        assert_eq!(temporaries(&root), Vec::<String>::new());
    }
}

#[test]
fn blocks_whole_then_the_source_failing_leave_no_temporary_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let contents: Vec<Vec<u8>> = (0..5).map(|index| vec![index; 100]).collect();
    let names: Vec<Vec<u8>> = (0..5)
        .map(|index| format!("f{index}").into_bytes())
        .collect();
    let files: Vec<(&[u8], &[u8], [u8; 32])> = names
        .iter()
        .zip(&contents)
        .map(|(name, content)| {
            (
                name.as_slice(),
                content.as_slice(),
                *blake3::hash(content).as_bytes(),
            )
        })
        .collect();
    // Each block whole, its files written and waiting for their batch, then
    // the Source fails before the blocks end.
    let (whole, _) = blocks_of(&with_ids(&files));
    let packets = files_packets(
        &files,
        Some(whole.len()),
        Some(Control::Error {
            message: "the scan failed".to_owned(),
        }),
    );
    let error = run_failing(&root, &packets);
    assert!(
        format!("{error:#}").contains("the Source failed"),
        "{error:#}"
    );
    assert_eq!(temporaries(&root), Vec::<String>::new());
}

/// Runs the Destination on `packets`, and returns what it sent back.
fn run_on(root: &Path, packets: &[u8]) -> Vec<(PacketType, Vec<u8>)> {
    run_with(root, packets, Reporter::none())
}

/// As [`run_on`], and returns what it reported, too: none of it among what
/// it sent back.
fn run_reporting(root: &Path, packets: &[u8]) -> (Vec<(PacketType, Vec<u8>)>, Vec<Report>) {
    let (out, reports) = crossbeam_channel::unbounded();
    let answers = run_with(root, packets, Reporter::new(out));
    assert_eq!(decoded::<Report>(&answers), []);
    (answers, reports.try_iter().collect())
}

/// As [`run_on`], reporting to `report`.
fn run_with(root: &Path, packets: &[u8], report: Reporter) -> Vec<(PacketType, Vec<u8>)> {
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report,
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    run(&mut &packets[..], &mut output, &options).unwrap();
    let mut answers = Vec::new();
    let mut reader = output.as_slice();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        let kind = packet.kind;
        answers.push((kind, packet.into_payload()));
    }
    answers
}

fn decoded<M: protocol::Message>(answers: &[(PacketType, Vec<u8>)]) -> Vec<M> {
    answers
        .iter()
        .filter(|(kind, _)| *kind == M::TYPE)
        .map(|(_, payload)| postcard::from_bytes(payload).unwrap())
        .collect()
}

#[test]
fn a_file_is_written() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let answers = run_on(&root, &packets(None));
    assert_eq!(decoded::<Results>(&answers), []);
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
    assert_eq!(std::fs::read(root.join("f")).unwrap(), b"hello");
}

/// Whatever files are reported as they are written, none is once the
/// stream ends.
#[test]
fn the_files_being_written_are_reported_until_none_is() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let (_, reports) = run_reporting(&root, &packets(None));
    let reported: Vec<Vec<Transfer>> = reports
        .into_iter()
        .filter_map(|report| match report {
            Report::Transferring(files) => Some(files),
            _ => None,
        })
        .collect();
    for files in &reported[..reported.len().saturating_sub(1)] {
        for file in files {
            assert_eq!((file.id, file.path.as_str(), file.size), (1, "f", 5));
            assert!(file.done <= 5);
        }
    }
    assert!(reported.last().is_none_or(Vec::is_empty), "{reported:?}");
}

/// The largest packet the Destination sends, syncing an empty `d/` onto
/// one with `extras` files of long names: all of them deleted.
fn largest_packet_deleting(extras: usize) -> usize {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir_all(root.join("d")).unwrap();
    for i in 0..extras {
        let name = format!("{i:06}{}", "x".repeat(244));
        std::fs::File::create(root.join("d").join(name)).unwrap();
    }
    let mut packets = Vec::new();
    protocol::send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    )
    .unwrap();
    for (path, entries) in [
        (&b""[..], vec![entry(1, b"d", Kind::Directory, 0)]),
        (&b"d"[..], Vec::new()),
    ] {
        let part = Manifest::Directory {
            path: path.to_vec(),
            entries,
            more: false,
        };
        protocol::send(&mut packets, &part).unwrap();
    }
    protocol::send(&mut packets, &Manifest::End).unwrap();
    send_end(&mut packets, 0);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    run(&mut &packets[..], &mut output, &options).unwrap();
    assert_eq!(std::fs::read_dir(root.join("d")).unwrap().count(), 0);
    let mut sent = &output[..];
    let mut largest = 0;
    while let Some(packet) = protocol::receive(&mut sent).unwrap() {
        largest = largest.max(packet.into_payload().len());
    }
    largest
}

/// No message the Destination sends grows with what `sync` deletes: the
/// largest is as large with 2,000 extras in a directory as with 100, as it
/// was not when it named them all in one.
#[test]
fn no_message_grows_with_the_extras_deleted() {
    let few = largest_packet_deleting(100);
    let many = largest_packet_deleting(2_000);
    assert_eq!(few, many, "100 extras: {few} bytes; 2,000: {many} bytes");
}

/// A directory in two parts, synced: only what neither part has is
/// deleted, found once its last part is here.
#[test]
fn a_directory_in_parts_deletes_only_what_no_part_has() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    // What the parts have is here already, as they say it is: none of it
    // is needed.
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    for name in ["first", "second", "extra"] {
        std::fs::write(root.join(name), name).unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(root.join(name))
            .unwrap();
        file.set_modified(mtime).unwrap();
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
    }
    let mut packets = Vec::new();
    let send =
        |packets: &mut Vec<u8>, message: &Manifest| protocol::send(packets, message).unwrap();
    send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    );
    send(
        &mut packets,
        &Manifest::Directory {
            path: Vec::new(),
            entries: vec![entry(1, b"first", Kind::File, 5)],
            more: true,
        },
    );
    send(
        &mut packets,
        &Manifest::Directory {
            path: Vec::new(),
            entries: vec![entry(2, b"second", Kind::File, 6)],
            more: false,
        },
    );
    send(&mut packets, &Manifest::End);
    send_end(&mut packets, 0);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    run(&mut &packets[..], &mut output, &options).unwrap();
    assert_eq!(
        tree(&root),
        [
            (root.join("first"), Some(b"first".to_vec())),
            (root.join("second"), Some(b"second".to_vec())),
        ]
    );
}

/// What the Source listed but could not read is not deleted, nor
/// anything under it; what it does not have, beside it, is.
#[test]
fn sync_deletes_beside_what_the_source_could_not_read_but_not_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir_all(root.join("locked/deeper")).unwrap();
    std::fs::write(root.join("locked/deeper/file"), "kept").unwrap();
    std::fs::write(root.join("extra"), "extra").unwrap();
    let mut packets = Vec::new();
    protocol::send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    )
    .unwrap();
    protocol::send(
        &mut packets,
        &Manifest::Problem {
            path: b"locked".to_vec(),
            file: None,
        },
    )
    .unwrap();
    protocol::send(
        &mut packets,
        &Manifest::Directory {
            path: Vec::new(),
            entries: vec![entry(1, b"locked", Kind::Unreadable, 0)],
            more: false,
        },
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    send_end(&mut packets, 0);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    let summary = run(&mut &packets[..], &mut output, &options).unwrap();
    assert_eq!(summary.problems, 1);
    assert_eq!(
        std::fs::read(root.join("locked/deeper/file")).unwrap(),
        b"kept"
    );
    assert!(!root.join("extra").exists());
}

/// An extra that is what the Source could not read, under another name
/// the filesystem takes for the same, is kept: a hard link stands for a
/// filesystem that ignores case. Another extra beside it is deleted.
#[test]
fn an_extra_that_is_what_the_source_could_not_read_is_kept() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("locked"), "kept").unwrap();
    std::fs::hard_link(root.join("locked"), root.join("alias")).unwrap();
    std::fs::write(root.join("extra"), "extra").unwrap();
    let mut packets = Vec::new();
    protocol::send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    )
    .unwrap();
    protocol::send(
        &mut packets,
        &Manifest::Directory {
            path: Vec::new(),
            entries: vec![entry(1, b"locked", Kind::Unreadable, 0)],
            more: false,
        },
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    send_end(&mut packets, 0);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    run(&mut &packets[..], &mut Vec::new(), &options).unwrap();
    assert_eq!(std::fs::read(root.join("alias")).unwrap(), b"kept");
    assert_eq!(std::fs::read(root.join("locked")).unwrap(), b"kept");
    assert!(!root.join("extra").exists());
}

/// Runs the Destination, syncing, on a manifest of `messages` and an
/// end of the blocks alone, beside a file outside its root and with one
/// inside; checks that the run is refused as invalid, and that neither
/// file, nor anything else, changed.
fn refused(messages: &[Manifest]) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir_all(root.join("inside")).unwrap();
    std::fs::write(root.join("inside/keep"), "keep").unwrap();
    std::fs::write(root.join("keep"), "keep").unwrap();
    std::fs::write(temp.path().join("outside"), "outside").unwrap();
    let before = tree(temp.path());
    let mut packets = Vec::new();
    for message in messages {
        protocol::send(&mut packets, message).unwrap();
    }
    send_end(&mut packets, 0);
    for dry_run in [true, false] {
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run,
            modify_window: 0,
            numeric_ids: true,
            delete: true,
            report: Reporter::none(),
            protect: Vec::new(),
            in_flight_entries: 0,
            data_credit: 0,
            one_file_system: false,
        };
        let mut output = Vec::new();
        let error = run(&mut &packets[..], &mut output, &options)
            .expect_err("an invalid manifest was taken");
        assert!(
            format!("{error:#}").contains("invalid manifest"),
            "{error:#}"
        );
        assert_eq!(tree(temp.path()), before, "{messages:?}");
        // Said at most once, here or by the server.
        let answers: Vec<(PacketType, Vec<u8>)> = {
            let mut reader = output.as_slice();
            std::iter::from_fn(|| protocol::receive(&mut reader).unwrap())
                .map(|packet| (packet.kind, packet.into_payload()))
                .collect()
        };
        assert!(decoded::<Control>(&answers).len() <= 1);
    }
}

/// Every path under `root`, with what each file holds.
fn tree(root: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    let mut found = Vec::new();
    let mut waiting = vec![root.to_path_buf()];
    while let Some(directory) = waiting.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                waiting.push(path.clone());
                found.push((path, None));
            } else {
                let content = std::fs::read(&path).ok();
                found.push((path, content));
            }
        }
    }
    found.sort();
    found
}

fn root_directory() -> Manifest {
    Manifest::Root(entry(0, b"", Kind::Directory, 0))
}

fn part(path: &[u8], entries: Vec<Entry>, more: bool) -> Manifest {
    Manifest::Directory {
        path: path.to_vec(),
        entries,
        more,
    }
}

/// The names of the Destination's own tree, "inside" and "keep", sent
/// as they are, so that only what is tested is wrong.
fn the_rest(first: FileId) -> Vec<Entry> {
    vec![
        entry(first, b"inside", Kind::Directory, 0),
        entry(first + 1, b"keep", Kind::File, 4),
    ]
}

#[test]
fn a_manifest_with_a_name_that_is_not_one_is_refused() {
    for name in [
        &b".."[..],
        b".",
        b"",
        b"../outside",
        b"inside/keep",
        b"a\0b",
    ] {
        let mut entries = vec![entry(
            1,
            name,
            Kind::Symlink {
                target: b"x".to_vec(),
            },
            0,
        )];
        entries.extend(the_rest(2));
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for (id, entry) in entries.iter_mut().enumerate() {
            entry.id = id as FileId + 1;
        }
        refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
    }
}

#[test]
fn a_manifest_with_a_directory_not_announced_is_refused() {
    for path in [&b".."[..], b"nowhere", b"keep", b"inside/../.."] {
        refused(&[
            root_directory(),
            part(b"", the_rest(1), false),
            part(path, vec![entry(3, b"x", Kind::File, 0)], false),
            Manifest::End,
        ]);
    }
    // Before the root.
    refused(&[part(b"", the_rest(1), false), Manifest::End]);
}

#[test]
fn a_manifest_with_a_directory_sent_twice_is_refused() {
    // Its first time is as the Destination has it, so that nothing it asks
    // for is done before the second is refused, however soon it is applied.
    let inside = || part(b"inside", vec![entry(3, b"keep", Kind::File, 4)], false);
    refused(&[
        root_directory(),
        part(b"", the_rest(1), false),
        inside(),
        inside(),
        Manifest::End,
    ]);
    refused(&[
        root_directory(),
        part(b"", the_rest(1), false),
        part(b"", vec![], false),
        Manifest::End,
    ]);
}

#[test]
fn a_manifest_out_of_order_is_refused() {
    // Within a part.
    let mut entries = the_rest(1);
    entries.reverse();
    refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
    // A name twice.
    let entries = vec![
        entry(1, b"keep", Kind::File, 4),
        entry(2, b"keep", Kind::File, 4),
    ];
    refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
    // Between parts.
    refused(&[
        root_directory(),
        part(b"", vec![entry(1, b"keep", Kind::File, 4)], true),
        part(b"", vec![entry(2, b"inside", Kind::Directory, 0)], false),
        Manifest::End,
    ]);
}

#[test]
fn a_manifest_with_another_root_or_ids_out_of_turn_is_refused() {
    refused(&[root_directory(), root_directory(), Manifest::End]);
    refused(&[
        Manifest::Root(entry(0, b"named", Kind::Directory, 0)),
        Manifest::End,
    ]);
    refused(&[
        Manifest::Root(entry(5, b"", Kind::Directory, 0)),
        Manifest::End,
    ]);
    let mut entries = the_rest(1);
    entries[1].id = 7;
    refused(&[root_directory(), part(b"", entries, false), Manifest::End]);
}

/// An extra is deleted from the directory the comparison listed, however
/// its path has changed since: here, moved away, and another made where
/// it was, whose own file of that name is kept.
#[test]
fn an_extra_is_deleted_from_the_directory_listed_not_its_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/extra"), "listed").unwrap();
    let listed = fsys::open_path_from(&root, b"d").unwrap();
    std::fs::rename(root.join("d"), root.join("moved")).unwrap();
    std::fs::create_dir(root.join("d")).unwrap();
    std::fs::write(root.join("d/extra"), "another").unwrap();

    let state = State::new(true);
    let stop = Stop::new(None);
    let extras = [(b"extra".to_vec(), false)];
    let results = delete_extras(&listed, &extras, &state.keep, &stop);
    assert!(tell_deleted(b"d", &extras, results, &mut Vec::new()).unwrap());
    assert!(!root.join("moved/extra").exists());
    assert_eq!(std::fs::read(root.join("d/extra")).unwrap(), b"another");
}

/// An extra that cannot be deleted is counted as failed, and the rest
/// are deleted all the same.
#[test]
fn an_extra_that_cannot_be_deleted_fails_and_the_rest_are_deleted() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir_all(root.join("d/gone/deeper")).unwrap();
    std::fs::write(root.join("d/extra"), "x").unwrap();
    let listed = fsys::open_path_from(&root, b"d").unwrap();

    let state = State::new(true);
    // Gone already, as another may have deleted it: it cannot be.
    let extras = [
        (b"already-gone".to_vec(), false),
        (b"extra".to_vec(), false),
        (b"gone".to_vec(), true),
    ];
    let stop = Stop::new(None);
    let results = delete_extras(&listed, &extras, &state.keep, &stop);
    let mut commands = Vec::new();
    assert!(tell_deleted(b"d", &extras, results, &mut commands).unwrap());
    let failed: Vec<&Results> = commands
        .iter()
        .filter_map(|command| match command {
            Command::Fail { result, .. } => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        matches!(failed[..], [Results::NotDeleted { path, .. }] if path == b"d/already-gone"),
        "{failed:?}"
    );
    assert_eq!(
        std::fs::read_dir(root.join("d")).unwrap().count(),
        0,
        "the rest are deleted"
    );
}

/// The Source fails part way, as a scan that fails does: the
/// Destination stops with its reason, rather than waiting for the rest.
#[test]
fn a_source_that_fails_stops_the_destination() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let mut packets = Vec::new();
    protocol::send(
        &mut packets,
        &Manifest::Root(entry(0, b"", Kind::Directory, 0)),
    )
    .unwrap();
    protocol::send(
        &mut packets,
        &Control::Error {
            message: "the scan failed".to_owned(),
        },
    )
    .unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: true,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    let error = run(&mut &packets[..], &mut output, &options).unwrap_err();
    assert!(
        format!("{error:#}").contains("the Source failed: the scan failed"),
        "{error:#}"
    );
    // The Source knows: it is not told back, nor by the server.
    assert!(error.is::<crate::stop::Reported>());
    let mut reader = output.as_slice();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        assert_ne!(packet.kind, PacketType::Control);
    }
}

/// A file the Source could not read whole fails, and is not asked for
/// again: the next run finds it changed.
#[test]
fn a_file_the_source_could_not_read_whole_fails_and_leaves_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let answers = run_on(&root, &packets(Some("it shrank while it was read")));
    let results = decoded::<Results>(&answers);
    assert!(
        matches!(
            &results[..],
            [Results::Failed { id: 1, message }]
                if message == "the Source could not read it whole: it shrank while it was read"
        ),
        "{results:?}"
    );
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}

/// What a Destination sent, packet by packet.
type Answers = Vec<(PacketType, Vec<u8>)>;

/// What a Destination writes, kept where the test can see it, which opens
/// `gate` once a packet among it `opens` it.
struct Watched {
    written: Arc<Mutex<Vec<u8>>>,
    gate: Arc<Gate>,
    opens: fn(&protocol::Packet) -> bool,
}

/// A `Control::Error`.
fn an_error(packet: &protocol::Packet) -> bool {
    packet.kind == PacketType::Control
        && matches!(packet.decode::<Control>(), Ok(Control::Error { .. }))
}

/// The file with ID 1 asked for; the root, missing, may be asked for
/// before it.
fn file_1_needed(packet: &protocol::Packet) -> bool {
    packet.kind == PacketType::Needs
        && matches!(packet.decode::<Needs>(), Ok(Needs::Files(ids)) if ids.contains(&1))
}

/// Opened once, and waited for.
#[derive(Default)]
struct Gate {
    opened: Mutex<bool>,
    open: std::sync::Condvar,
}

impl Write for Watched {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut written = self.written.lock().unwrap();
        written.extend_from_slice(bytes);
        let mut reader = written.as_slice();
        while let Ok(Some(packet)) = protocol::receive(&mut reader) {
            if (self.opens)(&packet) {
                *self.gate.opened.lock().unwrap() = true;
                self.gate.open.notify_all();
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A Source's packets: `first`, then, once the gate opens, `then`.
struct Gated {
    first: Vec<u8>,
    then: Vec<u8>,
    at: usize,
    gate: Arc<Gate>,
}

impl Read for Gated {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.first.len() {
            let opened = self.gate.opened.lock().unwrap();
            let (opened, waited) = self
                .gate
                .open
                .wait_timeout_while(opened, std::time::Duration::from_secs(20), |opened| {
                    !*opened
                })
                .unwrap();
            assert!(!waited.timed_out(), "what the gate waits for never came");
            drop(opened);
            self.first = std::mem::take(&mut self.then);
            self.at = 0;
        }
        let count = out.len().min(self.first.len() - self.at);
        out[..count].copy_from_slice(&self.first[self.at..self.at + count]);
        self.at += count;
        Ok(count)
    }
}

/// A block of `header` and `payload`, encoded as they are.
fn raw_block(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut block = (header.len() as u32).to_le_bytes().to_vec();
    block.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    block.extend_from_slice(&0u64.to_le_bytes());
    block.extend_from_slice(header);
    block.extend_from_slice(payload);
    block
}

/// A block that says it is compressed, whose payload is not.
fn corrupt_block() -> Vec<u8> {
    let header = block::Header {
        codec: block::Codec::Zstd,
        raw_len: 100,
        body: Body::Pack {
            files: vec![PackFile {
                file: 1,
                len: 100,
                mtime: entry(1, b"x", Kind::File, 0).mtime,
                check: Check::Hash([0; 32]),
            }],
        },
    };
    raw_block(&postcard::to_stdvec(&header).unwrap(), &[0xab; 16])
}

/// A block whose header cannot be read as one.
fn undecodable_block() -> Vec<u8> {
    raw_block(&[0xff; 3], &[0; 16])
}

/// A block of a file never asked for.
fn unneeded_block() -> Vec<u8> {
    pack_of(&[(99, b"", hash(b""))], false)
}

/// Syncs into a root whose `later` holds files the Source does not have, as
/// a Source sends: the root, `data` once the root's part is sent, then,
/// once the Destination has said it failed, `later` and the manifest's end.
/// Returns why it failed, what it sent, and what is left in `later`.
fn sync_failing_before_later(data: Vec<u8>) -> (anyhow::Error, Answers, Vec<String>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir_all(root.join("later")).unwrap();
    for index in 0..5 {
        std::fs::write(root.join(format!("later/extra{index}")), "extra").unwrap();
    }
    let mut first = Vec::new();
    protocol::send(&mut first, &root_directory()).unwrap();
    protocol::send(
        &mut first,
        &part(b"", vec![entry(1, b"later", Kind::Directory, 0)], false),
    )
    .unwrap();
    protocol::send_data(&mut first, &data).unwrap();
    let mut then = Vec::new();
    protocol::send(&mut then, &part(b"later", Vec::new(), false)).unwrap();
    protocol::send(&mut then, &Manifest::End).unwrap();
    let gate = Arc::new(Gate::default());
    let written = Arc::new(Mutex::new(Vec::new()));
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut reader = Gated {
        first,
        then,
        at: 0,
        gate: Arc::clone(&gate),
    };
    let mut writer = Watched {
        written: Arc::clone(&written),
        gate,
        opens: an_error,
    };
    let error = run(&mut reader, &mut writer, &options).unwrap_err();
    let written = written.lock().unwrap();
    let mut reader = written.as_slice();
    let mut answers = Vec::new();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        answers.push((packet.kind, packet.into_payload()));
    }
    let mut left: Vec<String> = std::fs::read_dir(root.join("later"))
        .unwrap()
        .map(|name| name.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    (error, answers, left)
}

/// Once the Destination has failed and said so, nothing more of the
/// manifest is applied: what is extra in a directory compared after is not
/// deleted, nor anything asked for.
#[test]
fn nothing_more_of_the_manifest_is_applied_once_the_destination_has_failed() {
    for (data, cause) in [
        (undecodable_block(), "decode a block's header"),
        (corrupt_block(), "decompress a block"),
    ] {
        let (error, answers, left) = sync_failing_before_later(data);
        assert!(format!("{error:#}").contains(cause), "{error:#}");
        assert_eq!(left, ["extra0", "extra1", "extra2", "extra3", "extra4"]);
        let controls = decoded::<Control>(&answers);
        assert!(
            matches!(&controls[..], [Control::Error { message }] if message.contains(cause)),
            "{controls:?}"
        );
        let error_at = answers
            .iter()
            .position(|(kind, _)| *kind == PacketType::Control)
            .unwrap();
        let after: Vec<Needs> = decoded(&answers[error_at..]);
        assert!(
            after.iter().all(|needs| !matches!(needs, Needs::Files(_))),
            "{after:?}"
        );
    }
}

/// The Source's manifest, then `blocks`, as data packets.
fn manifest_then(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    protocol::send(
        &mut packets,
        &part(b"", vec![entry(1, b"a", Kind::File, 5)], false),
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    for (sequence, block) in blocks.iter().enumerate() {
        let mut block = block.clone();
        block::number(&mut block, sequence as u64);
        protocol::send_data(&mut packets, &block).unwrap();
    }
    packets
}

/// Runs the Destination on `packets`, which it fails on, and returns why,
/// and the `Control::Error`s it sent.
fn failing_with_errors(packets: &[u8]) -> (anyhow::Error, Vec<Control>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    let error = run(&mut &packets[..], &mut output, &options).unwrap_err();
    let mut reader = output.as_slice();
    let mut controls = Vec::new();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        if packet.kind == PacketType::Control {
            controls.push(packet.decode::<Control>().unwrap());
        }
    }
    (error, controls)
}

/// Many blocks behind the cause, the main thread waits for room for them
/// as the stage that failed stops the rest: what it then meets is not taken
/// for the cause, which is said once.
#[test]
fn the_cause_is_said_however_much_comes_after_it() {
    let good = pack(&[(1, b"hello", hash(b"hello"))]);
    let filler = pack_of(&[(1, &[0; 4096], hash(&[0; 4096]))], false);
    for (blocks, cause) in [
        (
            [vec![good, corrupt_block()], vec![filler.clone(); 60]].concat(),
            "decompress a block",
        ),
        (
            [vec![unneeded_block()], vec![filler.clone(); 60]].concat(),
            "the Source sent unneeded ID 99",
        ),
    ] {
        let (error, controls) = failing_with_errors(&manifest_then(&blocks));
        let error = format!("{error:#}");
        assert!(error.contains(cause), "{error}");
        assert!(!error.contains("stopped"), "{error}");
        assert!(
            matches!(&controls[..], [Control::Error { message }] if message.contains(cause)),
            "{controls:?}"
        );
    }
}

/// The connection closing before the manifest's end fails, a dry run's as
/// well as a copy's: what was listed so far is not all there is.
#[test]
fn a_connection_that_closes_before_the_manifest_ends_fails() {
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    protocol::send(
        &mut packets,
        &part(b"", vec![entry(1, b"a", Kind::File, 5)], true),
    )
    .unwrap();
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let options = Options {
            root: temp
                .path()
                .join("destination")
                .as_os_str()
                .as_bytes()
                .to_vec(),
            dry_run,
            modify_window: 0,
            numeric_ids: true,
            delete: false,
            report: Reporter::none(),
            protect: Vec::new(),
            in_flight_entries: 0,
            data_credit: 0,
            one_file_system: false,
        };
        let error = run(&mut &packets[..], &mut Vec::new(), &options).unwrap_err();
        assert!(
            format!("{error:#}").contains("the connection closed before the manifest ended"),
            "{error:#}"
        );
    }
}

/// Blocks that come out of the order the Source numbered them, as no pipe
/// brings them but a Source gone wrong might send them, its word that they
/// ended before the last of them came, and the hash of a file in segments
/// before any of its segments, still make the files: the blocks are put
/// back in order, and have ended once as many as the Source said came.
#[test]
fn blocks_out_of_order_and_their_end_before_the_last_make_the_files() {
    let content = crate::test_support::Random::new(43).bytes(2 * block::SEGMENT as usize + 1000);
    let mut blocks = vec![pack(&[(2, b"hello", hash(b"hello"))])];
    blocks.extend((0..3).map(|index| segment_of(1, &content, index * block::SEGMENT)));
    for (sequence, block) in blocks.iter_mut().enumerate() {
        block::number(block, sequence as u64);
    }
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    let entries = vec![
        entry(1, b"big", Kind::File, content.len() as u64),
        entry(2, b"small", Kind::File, 5),
    ];
    protocol::send(&mut packets, &part(b"", entries, false)).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let hash = FileHash {
        file: 1,
        hash: *blake3::hash(&content).as_bytes(),
    };
    protocol::send(&mut packets, &hash).unwrap();
    send_blocks(&mut packets, &blocks[3]);
    send_end(&mut packets, 4);
    for index in [1, 0, 2] {
        send_blocks(&mut packets, &blocks[index]);
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let answers = run_on(&root, &packets);
    assert_eq!(decoded::<Results>(&answers), []);
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
    assert!(std::fs::read(root.join("big")).unwrap() == content);
    assert_eq!(std::fs::read(root.join("small")).unwrap(), b"hello");
}

/// A block numbered as one that came, or at or past as many as the Source
/// says it sent, before it says or after, or past any a Source can number,
/// is refused; and blocks that end short of as many as it said, as a
/// stream cut, end the transfer.
#[test]
fn blocks_numbered_wrong_or_short_of_their_end_are_refused() {
    let hello = pack(&[(1, b"hello", hash(b"hello"))]);
    let numbered = |sequence: u64| {
        let mut block = hello.clone();
        block::number(&mut block, sequence);
        block
    };
    let sent = |send: &dyn Fn(&mut Vec<u8>)| {
        let mut packets = manifest_then(&[]);
        send(&mut packets);
        packets
    };
    for (packets, said) in [
        (
            sent(&|packets| {
                protocol::send_data(packets, &numbered(0)).unwrap();
                protocol::send_data(packets, &numbered(0)).unwrap();
            }),
            "the Source sent block 0 twice",
        ),
        (
            sent(&|packets| {
                send_end(packets, 1);
                protocol::send_data(packets, &numbered(1)).unwrap();
            }),
            "the Source sent block 1 of the 1 it said it sent",
        ),
        (
            sent(&|packets| {
                protocol::send_data(packets, &numbered(5)).unwrap();
                send_end(packets, 2);
            }),
            "the Source sent block 5 of the 2 it said it sent",
        ),
        // As the nightly fuzz sent on 2026-10-27: the next place after it
        // overflowed in the pool.
        (
            sent(&|packets| {
                protocol::send_data(packets, &numbered(u64::MAX)).unwrap();
            }),
            "past any it can number",
        ),
        (
            sent(&|packets| {
                send_end(packets, 1);
                send_end(packets, 1);
            }),
            "the Source said twice that its blocks ended",
        ),
        (
            sent(&|packets| {
                protocol::send_data(packets, &numbered(0)).unwrap();
                send_end(packets, 2);
            }),
            "the data ended before its end",
        ),
    ] {
        let (error, _) = failing_with_errors(&packets);
        assert!(format!("{error:#}").contains(said), "{said}: {error:#}");
    }
}

/// Blocks that end early say where: between blocks, or in the middle of a
/// file whose segments did not all come, by its name.
#[test]
fn blocks_that_end_early_say_where() {
    let big = vec![7u8; block::SEGMENT as usize + 1_000];
    let files: [(&[u8], &[u8], [u8; 32]); 2] = [
        (b"a", b"hello", *blake3::hash(b"hello").as_bytes()),
        (b"big", &big, *blake3::hash(&big).as_bytes()),
    ];
    let (data, _) = blocks_of(&with_ids(&files));
    let whole_first = pack(&[(1, b"hello", hash(b"hello"))]).len();
    for (keep, said) in [
        (data.len() - 500, "the data ended in the middle of big"),
        (whole_first, "the data ended before its end"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let error = run_failing(
            &temp.path().join("destination"),
            &files_packets(&files, Some(keep), None),
        );
        assert_eq!(format!("{error:#}"), said);
    }
}

/// A directory let into, to write a file in it, gets its own permissions
/// back when the transfer fails before it is finished.
#[test]
fn a_directory_let_into_gets_its_permissions_back_when_the_transfer_fails() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir_all(root.join("locked")).unwrap();
    std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    protocol::send(
        &mut packets,
        &part(b"", vec![entry(1, b"locked", Kind::Directory, 0)], false),
    )
    .unwrap();
    protocol::send(
        &mut packets,
        &part(b"locked", vec![entry(2, b"f", Kind::File, 5)], false),
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    protocol::send_data(&mut packets, &corrupt_block()).unwrap();
    let error = run_failing(&root, &packets);
    assert!(
        format!("{error:#}").contains("decompress a block"),
        "{error:#}"
    );
    let mode = std::fs::metadata(root.join("locked"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(mode, 0o500);
}

/// A needed file whose bytes never come, nor word that the Source could
/// not open it, fails, rather than the transfer passing without it; its
/// directory is finished all the same.
#[test]
fn a_needed_file_that_never_comes_fails() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    protocol::send(
        &mut packets,
        &part(b"", vec![entry(1, b"a", Kind::File, 5)], false),
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    send_end(&mut packets, 0);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    let summary = run(&mut &packets[..], &mut output, &options).unwrap();
    assert_eq!(summary.failed, 1);
    let mut reader = output.as_slice();
    let answers: Answers = std::iter::from_fn(|| protocol::receive(&mut reader).unwrap())
        .map(|packet| (packet.kind, packet.into_payload()))
        .collect();
    let results = decoded::<Results>(&answers);
    assert!(
        matches!(&results[..], [Results::Failed { id: 1, message }]
            if message == "the Source never sent it"),
        "{results:?}"
    );
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
    assert!(!root.join("a").exists());
    // Finished: its mtime is the Source's.
    let mtime = std::fs::metadata(&root).unwrap().modified().unwrap();
    assert_eq!(
        mtime,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000)
    );
}

/// A needed file the Source said it could not open is not failed again
/// when the stream ends without it: the Source reported it.
#[test]
fn a_needed_file_the_source_could_not_open_is_not_failed_again() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let mut first = Vec::new();
    protocol::send(&mut first, &root_directory()).unwrap();
    protocol::send(
        &mut first,
        &part(b"", vec![entry(1, b"a", Kind::File, 5)], false),
    )
    .unwrap();
    protocol::send(&mut first, &Manifest::End).unwrap();
    // As a Source says so: once the file is asked for.
    let mut then = Vec::new();
    protocol::send(
        &mut then,
        &Manifest::Problem {
            path: b"a".to_vec(),
            file: Some(1),
        },
    )
    .unwrap();
    send_end(&mut then, 0);
    let answers = run_gated(&root, first, then, file_1_needed);
    assert_eq!(decoded::<Results>(&answers), []);
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
}

/// Runs the Destination on `first`, then, once a packet it sends `opens`
/// the gate, on `then`, and returns what it sent.
fn run_gated(
    root: &Path,
    first: Vec<u8>,
    then: Vec<u8>,
    opens: fn(&protocol::Packet) -> bool,
) -> Answers {
    let gate = Arc::new(Gate::default());
    let written = Arc::new(Mutex::new(Vec::new()));
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    let mut reader = Gated {
        first,
        then,
        at: 0,
        gate: Arc::clone(&gate),
    };
    let mut writer = Watched {
        written: Arc::clone(&written),
        gate,
        opens,
    };
    run(&mut reader, &mut writer, &options).unwrap();
    let written = written.lock().unwrap();
    let mut reader = written.as_slice();
    std::iter::from_fn(|| protocol::receive(&mut reader).unwrap())
        .map(|packet| (packet.kind, packet.into_payload()))
        .collect()
}

/// What a directory needs is asked for as soon as it is compared, not once
/// the next packet comes: here, the manifest's end comes only after.
#[test]
fn a_directory_compared_is_applied_before_the_next_packet() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let mut first = Vec::new();
    protocol::send(&mut first, &root_directory()).unwrap();
    protocol::send(
        &mut first,
        &part(b"", vec![entry(1, b"a", Kind::File, 5)], false),
    )
    .unwrap();
    let mut then = Vec::new();
    protocol::send(&mut then, &Manifest::End).unwrap();
    protocol::send_data(&mut then, &pack(&[(1, b"hello", hash(b"hello"))])).unwrap();
    send_end(&mut then, 1);
    let answers = run_gated(&root, first, then, file_1_needed);
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
    assert_eq!(std::fs::read(root.join("a")).unwrap(), b"hello");
}

/// A finisher sets a directory's permissions and mtime, and fails on one
/// it cannot open, which the decider says fails alone.
#[test]
fn a_finisher_finishes_a_directory_or_says_why_not() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("b")).unwrap();
    let state = State::new(false);
    let dirs = crate::dirs::DirCache::new(root.as_os_str(), crate::dirs::KEPT);
    let mut b = entry(2, b"b", Kind::Directory, 0);
    b.mode = 0o750;
    let mut warned = Vec::new();
    let gone = entry(1, b"gone", Kind::Directory, 0);
    assert!(
        finish::finish_directory(&state, &dirs, b"gone", b"gone/", &gone, &mut warned).is_err()
    );
    finish::finish_directory(&state, &dirs, b"b", b"b/", &b, &mut warned).unwrap();
    assert!(warned.is_empty());
    let finished = std::fs::metadata(root.join("b")).unwrap();
    assert_eq!(finished.permissions().mode() & 0o7777, 0o750);
    assert_eq!(finished.mtime(), 1_600_000_000);
}

/// An owner, permissions, or mtime this process may not set, or the
/// filesystem does not keep, is a warning, by kind, and the rest is still
/// set; any other error is returned, once each is tried.
#[test]
fn what_cannot_be_set_is_a_warning_and_the_rest_is_set() {
    use std::cell::RefCell;
    let state = State::new(false);
    let mut wanted = entry(1, b"f", Kind::File, 0);
    // A group this process is in, which it may set on what it owns.
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    wanted.uid = uid;
    wanted.gid = gid;
    let denied = || io::Error::from_raw_os_error(libc::EPERM);
    let tried = RefCell::new(Vec::new());
    let mut warned = Vec::new();
    let set = set_attributes(
        &state,
        b"f",
        (uid, gid.wrapping_add(1)),
        &wanted,
        Setters {
            chown: |_, _| {
                tried.borrow_mut().push("owner");
                Err(denied())
            },
            chmod: Some(|_| {
                tried.borrow_mut().push("mode");
                Ok(())
            }),
            utimes: |_| {
                tried.borrow_mut().push("times");
                Err(io::Error::from_raw_os_error(libc::EINVAL))
            },
        },
        &mut warned,
    );
    assert!(set.is_ok());
    assert_eq!(*tried.borrow(), ["owner", "mode", "times"]);
    let kinds: Vec<_> = warned.iter().map(|(kind, _)| *kind).collect();
    use super::warnings::Warning;
    assert_eq!(kinds, [Warning::Owner, Warning::Times]);
    // Warned of by whoever was told, never by setting them.
    assert!(state.warnings.lock().unwrap().kinds.is_empty());

    // Another error fails the entry, but the mtime is still tried.
    tried.borrow_mut().clear();
    let set = set_attributes(
        &state,
        b"f",
        (uid, gid),
        &wanted,
        Setters {
            chown: |_, _| Ok(()),
            chmod: Some(|_| Err(io::Error::from_raw_os_error(libc::EIO))),
            utimes: |_| {
                tried.borrow_mut().push("times");
                Ok(())
            },
        },
        &mut warned,
    );
    assert_eq!(set.unwrap_err().raw_os_error(), Some(libc::EIO));
    assert_eq!(*tried.borrow(), ["times"]);
}

/// A user that could not be set takes setuid away, and a group, setgid: the
/// file would otherwise run as root here, which the Source never let it. An
/// owner with no ID here, as in a user namespace, is one that cannot be set.
#[test]
fn an_owner_not_set_takes_setuid_and_setgid_away() {
    let mut state = State::new(false);
    state.ownership = Arc::new(crate::owners::Ownership::as_root());
    let mut wanted = entry(1, b"f", Kind::File, 0);
    (wanted.mode, wanted.uid, wanted.gid) = (0o6755, 1000, 1000);
    let warned = std::cell::RefCell::new(Vec::new());
    let mode_after = |current: (u32, u32), chown: Option<i32>| {
        let mode = std::cell::Cell::new(None);
        let set = set_attributes(
            &state,
            b"f",
            current,
            &wanted,
            Setters {
                chown: |_, _| {
                    chown.map_or(Ok(()), |errno| Err(io::Error::from_raw_os_error(errno)))
                },
                chmod: Some(|set| {
                    mode.set(Some(set));
                    Ok(())
                }),
                utimes: |_| Ok(()),
            },
            &mut warned.borrow_mut(),
        );
        assert!(set.is_ok());
        mode.get().unwrap()
    };
    assert_eq!(mode_after((0, 0), None), 0o6755);
    assert_eq!(mode_after((0, 0), Some(libc::EPERM)), 0o0755);
    assert_eq!(mode_after((0, 0), Some(libc::EINVAL)), 0o0755);
    // Only the group was to be set.
    assert_eq!(mode_after((1000, 0), Some(libc::EPERM)), 0o4755);
    // Only the user.
    assert_eq!(mode_after((0, 1000), Some(libc::EPERM)), 0o2755);
    use super::warnings::Warning;
    assert!(
        warned
            .borrow()
            .iter()
            .any(|(kind, _)| *kind == Warning::Setid)
    );
}

/// What the filesystem refuses for want of privilege or support may be left
/// unset; a time it cannot hold, or an owner with no ID here, too; nothing
/// else.
#[test]
fn only_privilege_and_support_make_an_attribute_unsettable() {
    let error = io::Error::from_raw_os_error;
    assert!(unsettable(&error(libc::EPERM), false));
    assert!(unsettable(&error(libc::EOPNOTSUPP), false));
    assert!(unsettable(&error(libc::ENOTSUP), false));
    assert!(unsettable(&error(libc::EINVAL), true));
    assert!(!unsettable(&error(libc::EINVAL), false));
    assert!(!unsettable(&error(libc::EIO), false));
    assert!(!unsettable(&error(libc::ENOSPC), false));
}

/// A full drive, a quota reached, or a read-only one ends the transfer, as
/// every entry after would fail the same way; any other error, as one that
/// cannot be read back, fails its entry alone.
#[test]
fn a_full_or_read_only_drive_ends_the_transfer() {
    let error = |errno| anyhow::Error::from(io::Error::from_raw_os_error(errno)).context("write f");
    assert!(ends_the_transfer(&error(libc::ENOSPC)));
    assert!(ends_the_transfer(&error(libc::EDQUOT)));
    assert!(ends_the_transfer(&error(libc::EROFS)));
    assert!(!ends_the_transfer(&error(libc::EIO)));
    assert!(!ends_the_transfer(&error(libc::EACCES)));
}

/// The directory a root is in, where a root that is a file is written, is
/// as its path names it.
#[test]
fn the_directory_a_root_is_in_is_as_its_path_names_it() {
    for (root, parent) in [
        ("a/b", "a"),
        ("b", "."),
        ("b/", "."),
        ("/b", "/"),
        ("a/b/", "a"),
    ] {
        assert_eq!(
            super::parent_of(Path::new(root)),
            Path::new(parent),
            "{root}"
        );
    }
}

/// A file sent twice, as only a Source gone wrong sends, fails the transfer
/// at once, saying so, rather than wait for a target never to come.
#[test]
fn a_file_sent_twice_fails_the_transfer() {
    let hello = pack(&[(1, b"hello", hash(b"hello"))]);
    let mut packets = manifest_then(&[hello.clone(), hello]);
    send_end(&mut packets, 2);
    let (error, _) = failing_with_errors(&packets);
    let error = format!("{error:#}");
    assert!(error.contains("the Source sent ID 1 twice"), "{error}");
}

/// Directories are finished many at once, but each only once every one
/// under it is: here each of 20 chains of 4 levels is finished to no access
/// at all, which would keep what is under a directory from being finished,
/// were it finished first. Every directory ends as the Source has it.
#[test]
fn a_directory_is_finished_only_after_all_under_it() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let closed = |id: FileId, name: &[u8]| {
        let mut directory = entry(id, name, Kind::Directory, 0);
        directory.mode = 0o000;
        directory
    };
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    // Numbered in the manifest's order: depth first, by name.
    let chains: Vec<Entry> = (0..20u64)
        .map(|chain| closed(1 + chain, format!("c{chain:02}").as_bytes()))
        .collect();
    protocol::send(&mut packets, &part(b"", chains, false)).unwrap();
    let mut id = 21;
    let mut levels = Vec::new();
    for chain in 0..20 {
        let mut path = format!("c{chain:02}");
        levels.push(path.clone());
        for _ in 1..4 {
            protocol::send(
                &mut packets,
                &part(path.as_bytes(), vec![closed(id, b"d")], false),
            )
            .unwrap();
            id += 1;
            path.push_str("/d");
            levels.push(path.clone());
        }
        protocol::send(&mut packets, &part(path.as_bytes(), Vec::new(), false)).unwrap();
    }
    protocol::send(&mut packets, &Manifest::End).unwrap();
    send_end(&mut packets, 0);
    let answers = run_on(&root, &packets);
    let failed: Vec<Results> = decoded(&answers);
    assert!(failed.is_empty(), "{failed:?}");
    // Let in again, from the top, to be looked at and removed.
    for path in &levels {
        let path = root.join(path);
        let mode = std::fs::symlink_metadata(&path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o000, "{}", path.display());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The files of a directory to be made are asked for once it is made, which
/// its parent's part does: until then, the Source is never told it may let
/// go of their entries, as Needs::Through says those up to it are asked for.
#[test]
fn needs_through_never_passes_a_file_not_asked_for() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    // Twenty directories to make, of three files each, numbered in the
    // manifest's order: depth first, by name.
    let directories: Vec<Entry> = (0..20u64)
        .map(|n| entry(1 + n, format!("d{n:02}").as_bytes(), Kind::Directory, 0))
        .collect();
    protocol::send(&mut packets, &part(b"", directories, false)).unwrap();
    let mut id = 21;
    for n in 0..20 {
        let files: Vec<Entry> = (0..3)
            .map(|f| {
                id += 1;
                entry(id - 1, format!("f{f}").as_bytes(), Kind::File, 1)
            })
            .collect();
        protocol::send(
            &mut packets,
            &part(format!("d{n:02}").as_bytes(), files, false),
        )
        .unwrap();
    }
    protocol::send(&mut packets, &Manifest::End).unwrap();
    // No file comes: each fails as never sent, which does not matter here.
    send_end(&mut packets, 0);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        // Told every 2 entries.
        in_flight_entries: 4,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    run(&mut &packets[..], &mut output, &options).unwrap();
    let mut reader = output.as_slice();
    let mut needs = Vec::new();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        if packet.kind == PacketType::Needs {
            needs.push(packet.decode::<Needs>().unwrap());
        }
    }
    let mut asked = std::collections::BTreeSet::new();
    let mut throughs = 0;
    for need in &needs {
        match need {
            Needs::Files(ids) => asked.extend(ids.iter().copied()),
            Needs::Through(through) => {
                throughs += 1;
                for file in 21..id {
                    assert!(
                        file > *through || asked.contains(&file),
                        "told through {through} before {file} was asked for: {needs:?}"
                    );
                }
            }
            _ => {}
        }
    }
    assert!(throughs > 0, "{needs:?}");
    // Directories to make are among what is asked for too.
    assert!(
        (21..id).all(|file| asked.contains(&file)),
        "every file asked for once its directory was made: {asked:?}"
    );
}

/// The manifest of one file, `big`, of `size` bytes, and what a Source sends
/// of it: `blocks`, and its hash, before them or after, if any, then the
/// end.
fn big_packets(size: u64, blocks: &[Vec<u8>], hash: Option<([u8; 32], bool)>) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    protocol::send(
        &mut packets,
        &part(b"", vec![entry(1, b"big", Kind::File, size)], false),
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let hash = hash.map(|(hash, first)| (FileHash { file: 1, hash }, first));
    if let Some((hash, true)) = &hash {
        protocol::send(&mut packets, hash).unwrap();
    }
    for (sequence, block) in blocks.iter().enumerate() {
        let mut block = block.clone();
        block::number(&mut block, sequence as u64);
        send_blocks(&mut packets, &block);
    }
    if let Some((hash, false)) = &hash {
        protocol::send(&mut packets, hash).unwrap();
    }
    send_end(&mut packets, blocks.len() as u64);
    packets
}

/// A file larger than a segment is written a segment at a time, each where
/// it goes, whatever order they come in, and its hash before them or after.
#[test]
fn a_large_file_is_written_in_segments_in_any_order() {
    let content = crate::test_support::Random::new(41).bytes(2 * block::SEGMENT as usize + 1000);
    let hash = *blake3::hash(&content).as_bytes();
    let segments: Vec<Vec<u8>> = (0..3)
        .map(|index| segment_of(1, &content, index * block::SEGMENT))
        .collect();
    for (order, first) in [([0, 1, 2], false), ([2, 0, 1], true)] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let blocks: Vec<Vec<u8>> = order.iter().map(|&index| segments[index].clone()).collect();
        let packets = big_packets(content.len() as u64, &blocks, Some((hash, first)));
        let answers = run_on(&root, &packets);
        assert_eq!(decoded::<Results>(&answers), [], "{order:?}");
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert!(
            std::fs::read(root.join("big")).unwrap() == content,
            "{order:?}"
        );
    }
}

/// A file in segments whose hash, merged from theirs, is not the Source's,
/// or one of whose segments is not as its chaining value says, fails: a
/// file already there is left as it was, and nothing else is.
#[test]
fn a_large_file_not_as_read_fails_and_leaves_the_old_file() {
    let content = crate::test_support::Random::new(43).bytes(block::SEGMENT as usize + 1000);
    let good = vec![
        segment_of(1, &content, 0),
        segment_of(1, &content, block::SEGMENT),
    ];
    // The second segment's last byte changed, and its chaining value not.
    let mut changed = good.clone();
    let last = changed[1].len() - 1;
    changed[1][last] ^= 1;
    for (blocks, hash) in [
        (good, [7; 32]),
        (changed, *blake3::hash(&content).as_bytes()),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("big"), b"old").unwrap();
        let packets = big_packets(content.len() as u64, &blocks, Some((hash, false)));
        let answers = run_on(&root, &packets);
        let results = decoded::<Results>(&answers);
        assert!(
            matches!(&results[..], [Results::Failed { id: 1, message }]
                if message == "the content's hash does not match the Source's"),
            "{results:?}"
        );
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert_eq!(std::fs::read(root.join("big")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }
}

/// A file in segments one of which the Source could not read whole fails,
/// said why; one whose segments did not all come by the end fails, as never
/// sent whole. Neither leaves anything.
#[test]
fn a_large_file_not_sent_whole_fails_and_leaves_nothing() {
    let content = crate::test_support::Random::new(47).bytes(block::SEGMENT as usize + 1000);
    let unread = {
        let segment = Segment {
            file: 1,
            offset: block::SEGMENT,
            len: 0,
            size: content.len() as u64,
            mtime: entry(1, b"x", Kind::File, 0).mtime,
            check: Check::Unread("it shrank while it was read".to_owned()),
        };
        block::encode(Body::Segment(segment), &[], false, 3).unwrap()
    };
    let hash = *blake3::hash(&content).as_bytes();
    for (blocks, hash, said) in [
        (
            vec![segment_of(1, &content, 0), unread.clone()],
            None,
            "the Source could not read it whole: it shrank while it was read",
        ),
        (
            vec![unread, segment_of(1, &content, 0)],
            None,
            "the Source could not read it whole: it shrank while it was read",
        ),
        (
            vec![segment_of(1, &content, 0)],
            Some((hash, false)),
            "the Source never sent all of it",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let answers = run_on(&root, &big_packets(content.len() as u64, &blocks, hash));
        let results = decoded::<Results>(&answers);
        assert!(
            matches!(&results[..], [Results::Failed { id: 1, message }] if message == said),
            "{results:?}"
        );
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}

/// What the Destination holds of the blocks stays within its bound, which
/// takes the place of the credit it once gave: a block that would go past it
/// waits until enough is let go of, one alone always fits, and a wait ends
/// as the Destination stops.
#[test]
fn what_is_held_of_the_blocks_stays_within_its_bound() {
    crate::test_support::within(
        "what is held of the blocks stays within its bound",
        crate::test_support::PATIENCE,
        || {
            let holding = Arc::new(super::Holding::new(100));
            let stop = crate::stop::Stop::new(None);
            let first = holding.take(60, &stop).unwrap();
            let second = holding.take(40, &stop).unwrap();
            std::thread::scope(|scope| {
                let waiting = scope.spawn(|| holding.take(10, &stop).map(drop));
                std::thread::sleep(std::time::Duration::from_millis(50));
                assert!(!waiting.is_finished(), "held past the bound");
                drop(first);
                waiting.join().unwrap().unwrap();
            });
            drop(second);
            // Past the bound alone, it fits once nothing else is held.
            let large = holding.take(1000, &stop).unwrap();
            std::thread::scope(|scope| {
                let waiting = scope.spawn(|| holding.take(1, &stop).map(drop));
                std::thread::sleep(std::time::Duration::from_millis(50));
                stop.fail(anyhow::anyhow!("failed"));
                let error = waiting.join().unwrap().unwrap_err();
                assert!(error.is::<crate::stop::Stopped>());
            });
            drop(large);
        },
    );
}

/// A Source's packets: `first`, then, once `ready` holds, or 10 s on,
/// `then`, as a Source still reading a large file sends nothing for a
/// while; how long it waited goes to `waited`, for the test to judge, so
/// that a Destination that never makes it ready still ends.
struct Paused {
    first: Vec<u8>,
    then: Vec<u8>,
    at: usize,
    ready: Box<dyn Fn() -> bool + Send>,
    waited: Arc<Mutex<Option<std::time::Duration>>>,
}

impl Read for Paused {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.first.len() && !self.then.is_empty() {
            let started = std::time::Instant::now();
            while !(self.ready)() && started.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            *self.waited.lock().unwrap() = Some(started.elapsed());
            self.first = std::mem::take(&mut self.then);
            self.at = 0;
        }
        let count = out.len().min(self.first.len() - self.at);
        out[..count].copy_from_slice(&self.first[self.at..self.at + count]);
        self.at += count;
        Ok(count)
    }
}

/// Files written take their names within a second, though their batch is
/// far from full and more is still to come, as while a large file is sent:
/// here the Source sends three small files, then nothing more until they
/// have their names, where before they took them only at the end.
#[test]
fn files_written_take_their_names_while_more_is_to_come() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let files: [(FileId, &[u8], &[u8]); 3] = [(1, b"a", b"1"), (2, b"b", b"22"), (3, b"c", b"333")];
    let mut first = Vec::new();
    protocol::send(&mut first, &root_directory()).unwrap();
    let entries = files
        .iter()
        .map(|&(id, name, content)| entry(id, name, Kind::File, content.len() as u64))
        .collect();
    protocol::send(&mut first, &part(b"", entries, false)).unwrap();
    protocol::send(&mut first, &Manifest::End).unwrap();
    let packed: Vec<(FileId, &[u8], Check)> = files
        .iter()
        .map(|&(id, _, content)| (id, content, hash(content)))
        .collect();
    protocol::send_data(&mut first, &pack(&packed)).unwrap();
    let mut then = Vec::new();
    send_end(&mut then, 1);
    let named = root.clone();
    let waited = Arc::new(Mutex::new(None));
    let mut reader = Paused {
        first,
        then,
        at: 0,
        ready: Box::new(move || ["a", "b", "c"].iter().all(|name| named.join(name).exists())),
        waited: Arc::clone(&waited),
    };
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        in_flight_entries: 0,
        data_credit: 0,
        one_file_system: false,
    };
    run(&mut reader, &mut Vec::new(), &options).unwrap();
    let waited = waited.lock().unwrap().expect("the Source paused");
    assert!(waited < std::time::Duration::from_secs(5), "{waited:?}");
    for (_, name, content) in files {
        let name = std::str::from_utf8(name).unwrap();
        assert_eq!(std::fs::read(root.join(name)).unwrap(), content);
    }
}
