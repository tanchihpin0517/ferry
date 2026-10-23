use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

use crate::frame;
use crate::fsys;
use crate::manifest::{Entry, FileId, Kind, Manifest};
use crate::mux::{self};
use crate::protocol::{self, Control, Needs, PacketType, Results};
use crate::record::{self, Header};
use crate::report::Transfer;

use super::apply::*;
use super::*;
use crate::manifest::Timestamp;
use crate::record::Trailer;

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
        entries: vec![file.clone()],
        more: false,
    };
    protocol::send(&mut packets, &directory).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let mut stream = Vec::new();
    let header = Header::File {
        id: 1,
        size: 5,
        mtime: file.mtime,
    };
    record::write_header(&mut stream, &header).unwrap();
    stream.extend_from_slice(b"hello");
    let trailer = Trailer {
        hash: *blake3::hash(b"hello").as_bytes(),
        unread: unread.map(str::to_owned),
    };
    record::write_trailer(&mut stream, &trailer).unwrap();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    packets
}

/// A manifest of files at the root, each its name, content, and the hash
/// the Source says it has, then the record stream of them, as a Source
/// sends them. The stream is cut after `keep` bytes, its end marker not
/// sent, if `keep` is given; `then` comes last.
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
        entries: entries.clone(),
        more: false,
    };
    protocol::send(&mut packets, &directory).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let mut stream = Vec::new();
    for (file, (_, content, hash)) in entries.iter().zip(files) {
        let header = Header::File {
            id: file.id,
            size: file.size,
            mtime: file.mtime,
        };
        record::write_header(&mut stream, &header).unwrap();
        stream.extend_from_slice(content);
        let trailer = Trailer {
            hash: *hash,
            unread: None,
        };
        record::write_trailer(&mut stream, &trailer).unwrap();
    }
    match keep {
        Some(keep) => stream.truncate(keep),
        None => record::write_header(&mut stream, &Header::End).unwrap(),
    }
    for frame in stream.chunks(frame::FRAME) {
        protocol::send_data(&mut packets, &frame::encode(frame, true, 3).unwrap()).unwrap();
    }
    if let Some(then) = then {
        protocol::send(&mut packets, &then).unwrap();
    }
    packets
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
        fsync: false,
        delete: false,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
        let answers = run_reporting(&root, &files_packets(&files, None, None), true);
        let results = decoded::<Results>(&answers);
        assert!(
            matches!(&results[..], [Results::Failed { id: 1, message }]
                if message.contains("hash does not match")),
            "{results:?}"
        );
        assert_eq!(decoded::<Control>(&answers), [Control::Done]);
        let failed: u64 = decoded::<crate::report::Report>(&answers)
            .iter()
            .filter_map(|report| match report {
                crate::report::Report::Counts(counts) => Some(counts.failed),
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

#[test]
fn a_stream_cut_inside_a_record_leaves_no_temporary_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let big = vec![7u8; 1_000_000];
    let files: [(&[u8], &[u8], [u8; 32]); 2] = [
        (b"a", b"hello", *blake3::hash(b"hello").as_bytes()),
        (b"big", &big, *blake3::hash(&big).as_bytes()),
    ];
    // The first record whole, the second cut short, as when the
    // connection drops.
    let packets = files_packets(&files, Some(300_000), None);
    let error = run_failing(&root, &packets);
    assert!(format!("{error:#}").contains("record"), "{error:#}");
    assert_eq!(temporaries(&root), Vec::<String>::new());
}

#[test]
fn records_whole_then_the_source_failing_leave_no_temporary_file() {
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
    // Each record whole, written and waiting for its batch, then the
    // Source fails before its stream ends.
    let whole = {
        let mut length = 0;
        let mut stream = Vec::new();
        for (index, (_, content, hash)) in files.iter().enumerate() {
            stream.clear();
            record::write_header(
                &mut stream,
                &Header::File {
                    id: index as FileId + 1,
                    size: content.len() as u64,
                    mtime: entry(1, b"x", Kind::File, 0).mtime,
                },
            )
            .unwrap();
            stream.extend_from_slice(content);
            record::write_trailer(
                &mut stream,
                &Trailer {
                    hash: *hash,
                    unread: None,
                },
            )
            .unwrap();
            length += stream.len();
        }
        length
    };
    let packets = files_packets(
        &files,
        Some(whole),
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
    run_reporting(root, packets, false)
}

/// As [`run_on`], with reports or without.
fn run_reporting(root: &Path, packets: &[u8], report: bool) -> Vec<(PacketType, Vec<u8>)> {
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: false,
        report,
        protect: Vec::new(),
        in_flight_entries: 0,
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
    let answers = run_reporting(&root, &packets(None), true);
    let reported: Vec<Vec<Transfer>> = decoded::<crate::report::Report>(&answers)
        .into_iter()
        .filter_map(|report| match report {
            crate::report::Report::Transferring(files) => Some(files),
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: true,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: true,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: true,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: true,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
        one_file_system: false,
    };
    run(&mut &packets[..], &mut Vec::new(), &options).unwrap();
    assert_eq!(std::fs::read(root.join("alias")).unwrap(), b"kept");
    assert_eq!(std::fs::read(root.join("locked")).unwrap(), b"kept");
    assert!(!root.join("extra").exists());
}

/// Runs the Destination, syncing, on a manifest of `messages` and an
/// empty record stream, beside a file outside its root and with one
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    for dry_run in [true, false] {
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run,
            modify_window: 0,
            numeric_ids: true,
            fsync: false,
            delete: true,
            report: false,
            protect: Vec::new(),
            in_flight_entries: 0,
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
    let (mux, _outlet) = mux::channel();
    delete_extras(
        &listed,
        b"d",
        &[(b"extra".to_vec(), false)],
        &state,
        &mut Directories::new(None, None),
        &mux,
        &Stop::new(None),
    )
    .unwrap();
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
    let (mux, _outlet) = mux::channel();
    // Gone already, as another may have deleted it: it cannot be.
    let extras = [
        (b"already-gone".to_vec(), false),
        (b"extra".to_vec(), false),
        (b"gone".to_vec(), true),
    ];
    delete_extras(
        &listed,
        b"d",
        &extras,
        &state,
        &mut Directories::new(None, None),
        &mux,
        &Stop::new(None),
    )
    .unwrap();
    assert_eq!(state.failures.load(Ordering::Relaxed), 1);
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
        fsync: false,
        delete: false,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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

/// A frame that says it is compressed, whose body is not.
fn corrupt_frame() -> Vec<u8> {
    let mut frame = vec![1];
    frame.extend_from_slice(&100u32.to_le_bytes());
    frame.extend_from_slice(&16u32.to_le_bytes());
    frame.extend_from_slice(&[0xab; 16]);
    frame
}

/// A record stream whose first record's header cannot be read as one.
fn undecodable_record() -> Vec<u8> {
    let mut stream = 3u32.to_le_bytes().to_vec();
    stream.extend_from_slice(&[0xff; 3]);
    frame::encode(&stream, false, 3).unwrap()
}

/// A record stream that begins with a record of an ID never asked for.
fn unneeded_record() -> Vec<u8> {
    let mut stream = Vec::new();
    record::write_header(
        &mut stream,
        &Header::File {
            id: 99,
            size: 0,
            mtime: entry(99, b"x", Kind::File, 0).mtime,
        },
    )
    .unwrap();
    frame::encode(&stream, false, 3).unwrap()
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
        fsync: false,
        delete: true,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
        (undecodable_record(), "decode a record"),
        (corrupt_frame(), "decompress a frame"),
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

/// The Source's manifest, then `frames`, as data packets.
fn manifest_then(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    protocol::send(
        &mut packets,
        &part(b"", vec![entry(1, b"a", Kind::File, 5)], false),
    )
    .unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    for frame in frames {
        protocol::send_data(&mut packets, frame).unwrap();
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
        fsync: false,
        delete: false,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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

/// Many frames behind the cause, the main thread waits for room for them
/// as the stage that failed stops the rest: what it then meets is not taken
/// for the cause, which is said once.
#[test]
fn the_cause_is_said_however_much_comes_after_it() {
    let mut stream = Vec::new();
    record::write_header(
        &mut stream,
        &Header::File {
            id: 1,
            size: 5,
            mtime: entry(1, b"a", Kind::File, 5).mtime,
        },
    )
    .unwrap();
    stream.extend_from_slice(b"hello");
    record::write_trailer(
        &mut stream,
        &Trailer {
            hash: *blake3::hash(b"hello").as_bytes(),
            unread: None,
        },
    )
    .unwrap();
    let filler = frame::encode(&[0; 4096], false, 3).unwrap();
    for (frames, cause) in [
        (
            [
                vec![frame::encode(&stream, true, 3).unwrap(), corrupt_frame()],
                vec![filler.clone(); 60],
            ]
            .concat(),
            "decompress a frame",
        ),
        (
            [vec![unneeded_record()], vec![filler.clone(); 60]].concat(),
            "the Source sent unneeded ID 99",
        ),
    ] {
        let (error, controls) = failing_with_errors(&manifest_then(&frames));
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
            fsync: false,
            delete: false,
            report: false,
            protect: Vec::new(),
            in_flight_entries: 0,
            one_file_system: false,
        };
        let error = run(&mut &packets[..], &mut Vec::new(), &options).unwrap_err();
        assert!(
            format!("{error:#}").contains("the connection closed before the manifest ended"),
            "{error:#}"
        );
    }
}

/// A record stream that ends early says where: before a record, or in the
/// middle of one, by its file.
#[test]
fn a_record_stream_that_ends_early_says_where() {
    let big = vec![7u8; 1_000_000];
    let files: [(&[u8], &[u8], [u8; 32]); 2] = [
        (b"a", b"hello", *blake3::hash(b"hello").as_bytes()),
        (b"big", &big, *blake3::hash(&big).as_bytes()),
    ];
    let whole_first = {
        let mut stream = Vec::new();
        record::write_header(
            &mut stream,
            &Header::File {
                id: 1,
                size: 5,
                mtime: entry(1, b"a", Kind::File, 5).mtime,
            },
        )
        .unwrap();
        stream.extend_from_slice(b"hello");
        record::write_trailer(
            &mut stream,
            &Trailer {
                hash: *blake3::hash(b"hello").as_bytes(),
                unread: None,
            },
        )
        .unwrap();
        stream.len()
    };
    for (keep, said) in [
        (300_000, "the record stream ended in the middle of big"),
        (whole_first, "the record stream ended before its end"),
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
    protocol::send_data(&mut packets, &corrupt_frame()).unwrap();
    let error = run_failing(&root, &packets);
    assert!(
        format!("{error:#}").contains("decompress a frame"),
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

/// A needed file whose record never comes, nor word that the Source could
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: false,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut then, &frame::encode(&stream, true, 3).unwrap()).unwrap();
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
        fsync: false,
        delete: false,
        report: false,
        protect: Vec::new(),
        in_flight_entries: 0,
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
    let mut stream = Vec::new();
    record::write_header(
        &mut stream,
        &Header::File {
            id: 1,
            size: 5,
            mtime: entry(1, b"a", Kind::File, 5).mtime,
        },
    )
    .unwrap();
    stream.extend_from_slice(b"hello");
    record::write_trailer(
        &mut stream,
        &Trailer {
            hash: *blake3::hash(b"hello").as_bytes(),
            unread: None,
        },
    )
    .unwrap();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut then, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let answers = run_gated(&root, first, then, file_1_needed);
    assert_eq!(decoded::<Control>(&answers), [Control::Done]);
    assert_eq!(std::fs::read(root.join("a")).unwrap(), b"hello");
}

/// A directory that cannot be finished, here gone before its turn, fails
/// alone: reported as its entry, and the directory after it is finished all
/// the same.
#[test]
fn a_directory_that_cannot_be_finished_fails_alone() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("b")).unwrap();
    let state = State::new(false);
    let dirs = crate::dirs::DirCache::new(root.as_os_str(), crate::dirs::KEPT);
    let (mux, outlet) = mux::channel();
    let stop = crate::stop::Stop::new(None);
    let (finishing, done) = crossbeam_channel::unbounded();
    let mut b = entry(2, b"b", Kind::Directory, 0);
    b.mode = 0o750;
    finishing
        .send((b"gone".to_vec(), entry(1, b"gone", Kind::Directory, 0)))
        .unwrap();
    finishing.send((b"b".to_vec(), b)).unwrap();
    drop(finishing);
    finish_as_done(&state, &dirs, &mux, &stop, done, true).unwrap();
    assert_eq!(state.failures.load(std::sync::atomic::Ordering::Relaxed), 1);
    let finished = std::fs::metadata(root.join("b")).unwrap();
    assert_eq!(finished.permissions().mode() & 0o7777, 0o750);
    assert_eq!(finished.mtime(), 1_600_000_000);
    drop(mux);
    let mut sent = Vec::new();
    mux::pump(&mut sent, outlet).unwrap();
    let mut sent = &sent[..];
    let mut failed = Vec::new();
    while let Some(packet) = protocol::receive(&mut sent).unwrap() {
        if packet.kind == PacketType::Results
            && let Results::Failed { id, .. } = packet.decode::<Results>().unwrap()
        {
            failed.push(id);
        }
    }
    assert_eq!(failed, [1]);
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
    );
    assert!(set.is_ok());
    assert_eq!(*tried.borrow(), ["owner", "mode", "times"]);
    let warnings = state.warnings.lock().unwrap();
    let kinds: Vec<_> = warnings.kinds.keys().copied().collect();
    use super::warnings::Warning;
    assert_eq!(kinds, [Warning::Owner, Warning::Times]);
    drop(warnings);

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
    let warnings = state.warnings.lock().unwrap();
    use super::warnings::Warning;
    assert!(warnings.kinds.contains_key(&Warning::Setid));
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

/// The directory a root is in, made durable when the root is made, is as
/// its path names it.
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

/// A second record of one ID, as only a Source gone wrong sends, fails the
/// transfer at once, saying so, rather than wait for a target never to come.
#[test]
fn a_second_record_of_one_id_fails_the_transfer() {
    let mut stream = Vec::new();
    for _ in 0..2 {
        record::write_header(
            &mut stream,
            &Header::File {
                id: 1,
                size: 5,
                mtime: entry(1, b"a", Kind::File, 5).mtime,
            },
        )
        .unwrap();
        stream.extend_from_slice(b"hello");
        record::write_trailer(
            &mut stream,
            &Trailer {
                hash: *blake3::hash(b"hello").as_bytes(),
                unread: None,
            },
        )
        .unwrap();
    }
    record::write_header(&mut stream, &Header::End).unwrap();
    let frames = [frame::encode(&stream, true, 3).unwrap()];
    let (error, _) = failing_with_errors(&manifest_then(&frames));
    let error = format!("{error:#}");
    assert!(error.contains("the Source sent ID 1 twice"), "{error}");
}

/// Directories are finished many at once, but each only once every one
/// under it is: here each of 20 chains of 4 levels is finished to no access
/// at all, which would keep what is under a directory from being finished,
/// were it finished first.
#[test]
fn a_directory_is_finished_only_after_all_under_it() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let (done, finishing) = crossbeam_channel::unbounded();
    let mut chains = Vec::new();
    for chain in 0..20 {
        let levels: Vec<String> = (1..=4)
            .map(|depth| format!("c{chain:02}{}", "/d".repeat(depth - 1)))
            .collect();
        std::fs::create_dir_all(root.join(levels.last().unwrap())).unwrap();
        // Done deepest first, as the applier lets them go.
        for path in levels.iter().rev() {
            let mut directory = entry(1, b"d", Kind::Directory, 0);
            directory.mode = 0o000;
            done.send((path.as_bytes().to_vec(), directory)).unwrap();
        }
        chains.push(levels);
    }
    drop(done);
    let state = State::new(true);
    let (mux, _outlet) = mux::channel();
    let dirs = crate::dirs::DirCache::new(root.as_os_str(), crate::dirs::KEPT);
    finish::finish_as_done(&state, &dirs, &mux, &Stop::new(None), finishing, false).unwrap();
    assert_eq!(state.failures.load(Ordering::Relaxed), 0);
    // Let in again, from the top, to be looked at and removed.
    for levels in &chains {
        for path in levels {
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
}
