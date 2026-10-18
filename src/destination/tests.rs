use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::frame;
use crate::fsys;
use crate::manifest::{Entry, FileId, Kind, Manifest};
use crate::mux::{self};
use crate::protocol::{self, Control, Needs, PacketType, Results};
use crate::record::{self, Header};
use crate::report::Transfer;

use super::*;
use super::{apply::*, write::*};
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

/// A directory in two parts, synced: only what neither part has is
/// deleted, found once its last part is here.
#[test]
fn a_directory_in_parts_deletes_only_what_no_part_has() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    for name in ["first", "second", "extra"] {
        std::fs::write(root.join(name), name).unwrap();
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
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: true,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete: true,
        report: false,
        protect: Vec::new(),
    };
    let mut output = Vec::new();
    run(&mut &packets[..], &mut output, &options).unwrap();
    let mut reader = output.as_slice();
    let mut deletes = Vec::new();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        if packet.kind == PacketType::Needs
            && let Needs::Deletes(more) = packet.decode::<Needs>().unwrap()
        {
            deletes.extend(more);
        }
    }
    assert_eq!(deletes, [(b"extra".to_vec(), false)]);
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
        };
        let error = run(&mut &packets[..], &mut Vec::new(), &options)
            .expect_err("an invalid manifest was taken");
        assert!(
            format!("{error:#}").contains("invalid manifest"),
            "{error:#}"
        );
        assert_eq!(tree(temp.path()), before, "{messages:?}");
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
    refused(&[
        root_directory(),
        part(b"", the_rest(1), false),
        part(b"inside", vec![], false),
        part(b"inside", vec![], false),
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

    let state = Mutex::new(State::new(true));
    let (mux, _outlet) = mux::channel();
    delete_extras(&listed, b"d", &[(b"d/extra".to_vec(), false)], &state, &mux).unwrap();
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

    let state = Mutex::new(State::new(true));
    let (mux, _outlet) = mux::channel();
    // Gone already, as another may have deleted it: it cannot be.
    let extras = [
        (b"d/already-gone".to_vec(), false),
        (b"d/extra".to_vec(), false),
        (b"d/gone".to_vec(), true),
    ];
    delete_extras(&listed, b"d", &extras, &state, &mux).unwrap();
    assert_eq!(state.lock().unwrap().failures, 1);
    assert_eq!(
        std::fs::read_dir(root.join("d")).unwrap().count(),
        0,
        "the rest are deleted"
    );
}

/// Directories are done as the manifest leaves them, and all their files
/// are settled, deepest first; one never listed, once its parent is
/// left; one to be let go, without being finished. Nothing stays open.
#[test]
fn directories_are_finished_as_they_are_done_deepest_first() {
    let mut state = State::new(true);
    let (finishing, done) = mpsc::channel();
    state.finishing = Some(finishing);
    let directory = |id: FileId, name: &[u8]| entry(id, name, Kind::Directory, 0);
    let finished = |done: &mpsc::Receiver<(Vec<u8>, Entry)>| -> Vec<String> {
        done.try_iter()
            .map(|(path, _)| String::from_utf8(path).unwrap())
            .collect()
    };
    // The root, with a/ and b/ in it; a/ with a file and c/; b/ never
    // listed.
    state.announce(Vec::new(), directory(0, b""));
    state.mark_to_finish(b"");
    state.enter(b"");
    state.announce(b"a".to_vec(), directory(1, b"a"));
    state.announce(b"b".to_vec(), directory(2, b"b"));
    state.mark_to_finish(b"a");
    state.mark_to_finish(b"b");
    state.enter(b"a");
    state.file_needed(b"a");
    state.announce(b"a/c".to_vec(), directory(3, b"c"));
    state.enter(b"a/c");
    // Left, nothing under it, nothing to finish: let go.
    state.enter(b"a/d-not-announced");
    assert_eq!(finished(&done), Vec::<String>::new());
    assert!(!state.open.contains_key(b"a/c".as_slice()));
    // The manifest is over; a/ waits for its file.
    state.leave_all();
    assert_eq!(finished(&done), ["b"]);
    assert!(state.open.contains_key(b"a".as_slice()));
    assert!(state.open.contains_key(b"".as_slice()));
    state.file_settled(b"a");
    assert_eq!(finished(&done), ["a", ""]);
    assert!(
        state.open.is_empty(),
        "{:?}",
        state.open.keys().collect::<Vec<_>>()
    );
}

/// A directory whose needed file the Source could not open is finished
/// once it says so, not at the end; an ID it does not wait for, or
/// said again, is nothing.
#[test]
fn a_directory_is_finished_once_its_file_the_source_could_not_open_is_settled() {
    let mut state = State::new(true);
    let (finishing, done) = mpsc::channel();
    state.finishing = Some(finishing);
    let directory = |id: FileId, name: &[u8]| entry(id, name, Kind::Directory, 0);
    state.announce(Vec::new(), directory(0, b""));
    state.enter(b"");
    state.announce(b"a".to_vec(), directory(1, b"a"));
    state.announce(b"b".to_vec(), directory(2, b"b"));
    state.mark_to_finish(b"a");
    state.enter(b"a");
    state.file_needed(b"a");
    state.targets.insert(
        3,
        Target {
            directory: b"a".to_vec(),
            entry: entry(3, b"locked", Kind::File, 1),
            is_root: false,
        },
    );
    // The manifest has left a/, which waits for its file.
    state.enter(b"b");
    let state = Mutex::new(state);
    settled(&state, 99);
    assert!(done.try_recv().is_err());
    settled(&state, 3);
    let finished: Vec<Vec<u8>> = done.try_iter().map(|(path, _)| path).collect();
    assert_eq!(finished, [b"a".to_vec()]);
    settled(&state, 3);
    assert!(done.try_recv().is_err());
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
    };
    let mut output = Vec::new();
    let error = run(&mut &packets[..], &mut output, &options).unwrap_err();
    assert!(
        format!("{error:#}").contains("the Source failed: the scan failed"),
        "{error:#}"
    );
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
