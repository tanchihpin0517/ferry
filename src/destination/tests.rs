use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

use crate::block::{self, ByteBuf, Check, Data};
use crate::fsys;
use crate::manifest::{Entry, FileId, Kind, Manifest};
use crate::protocol::{self, Answer, Control, PacketType};
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

/// A record of a whole file, `id`, of `content`, with the hash the Source
/// says it has, or why it could not read it whole, and none of it.
fn whole_file(id: FileId, content: &[u8], check: Check) -> Data<ByteBuf> {
    let bytes = match check {
        Check::Hash(_) => content.to_vec(),
        Check::Unread(_) => Vec::new(),
    };
    Data::File {
        id,
        mtime: entry(id, b"x", Kind::File, 0).mtime,
        check,
        bytes: ByteBuf(bytes),
    }
}

/// The records of whole files, each its ID, content, and the hash the
/// Source says it has, or why it could not read it whole, as the Source
/// writes them: packets, one after another.
fn wholes(files: &[(FileId, &[u8], Check)]) -> Vec<u8> {
    let mut packets = Vec::new();
    for (id, content, check) in files {
        protocol::send(&mut packets, &whole_file(*id, content, check.clone())).unwrap();
    }
    packets
}

/// The segment of `content`, a file's, from `offset`, as the Source writes
/// it, with its chaining value: a packet.
fn segment_of(id: FileId, content: &[u8], offset: u64) -> Vec<u8> {
    let end = (offset + block::SEGMENT).min(content.len() as u64);
    let bytes = &content[offset as usize..end as usize];
    let segment = Data::Segment {
        id,
        offset,
        size: content.len() as u64,
        mtime: entry(id, b"x", Kind::File, 0).mtime,
        check: Check::Hash(block::chaining_value(offset, bytes)),
        bytes: ByteBuf(bytes.to_vec()),
    };
    protocol::encode(&segment).unwrap()
}

/// The records a Source writes of files, each its ID, content, and the hash
/// it says the content has: those up to a segment whole, larger ones a
/// segment at a time; and the hashes of those, which go apart.
fn records_of(files: &[(FileId, &[u8], [u8; 32])]) -> (Vec<u8>, Vec<Data<ByteBuf>>) {
    let mut data = Vec::new();
    let mut hashes = Vec::new();
    for &(id, content, hash) in files {
        if content.len() as u64 <= block::SEGMENT {
            data.extend(wholes(&[(id, content, Check::Hash(hash))]));
            continue;
        }
        for index in 0..block::segments(content.len() as u64) {
            data.extend(segment_of(id, content, index * block::SEGMENT));
        }
        hashes.push(Data::Hash { file: id, hash });
    }
    (data, hashes)
}

/// The records in `data`, as the Source writes them.
fn send_data(packets: &mut Vec<u8>, data: &[u8]) {
    packets.extend_from_slice(data);
}

/// The data's end, as the Source writes it.
fn data_end(packets: &mut Vec<u8>) {
    protocol::send(packets, &Data::<ByteBuf>::End).unwrap();
}

/// The data's end, then its pipe's, as the Source ends it, before the rest
/// has.
fn end_data(packets: &mut Vec<u8>) {
    data_end(packets);
    packets.extend_from_slice(&[PacketType::Data as u8, 0, 0, 0, 0]);
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
    send_part(&mut packets, 0, vec![file], true);
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let check = match unread {
        Some(why) => Check::Unread(why.to_owned()),
        None => hash(b"hello"),
    };
    send_data(&mut packets, &wholes(&[(1, b"hello", check)]));
    data_end(&mut packets);
    packets
}

/// A manifest of files at the root, each its name, content, and the hash
/// the Source says it has, then the records of them, as a Source writes
/// them, the hashes of those in segments, and the data's end. The records
/// are cut after `keep` bytes, as a stream cut, and neither those hashes
/// nor anything more sent, if `keep` is given; `then` comes last.
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
    send_part(&mut packets, 0, entries, true);
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let (mut data, hashes) = records_of(&with_ids(files));
    if let Some(keep) = keep {
        data.truncate(keep);
    }
    send_data(&mut packets, &data);
    if keep.is_none() {
        for hash in &hashes {
            protocol::send(&mut packets, hash).unwrap();
        }
        data_end(&mut packets);
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
        batch_entries: 0,
        listers: 1,
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
        let results = failures(&answers);
        assert!(
            matches!(&results[..], [Answer::Failed { id: 1, message }]
                if message.contains("hash does not match")),
            "{results:?}"
        );
        assert!(done(&answers));
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

/// Records cut short, as when the connection drops, leave no temporary
/// file: neither of a record cut, nor of a file whose segments did not all
/// come.
#[test]
fn records_cut_short_leave_no_temporary_file() {
    let big = vec![7u8; block::SEGMENT as usize + 1_000];
    let files: [(&[u8], &[u8], [u8; 32]); 2] = [
        (b"a", b"hello", *blake3::hash(b"hello").as_bytes()),
        (b"big", &big, *blake3::hash(&big).as_bytes()),
    ];
    let (data, _) = records_of(&with_ids(&files));
    // Inside the first record, and inside the second segment.
    for keep in [10, data.len() - 500] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        let packets = files_packets(&files, Some(keep), None);
        let error = run_failing(&root, &packets);
        assert!(
            format!("{error:#}").contains("the stream broke"),
            "{error:#}"
        );
        assert_eq!(temporaries(&root), Vec::<String>::new());
    }
}

/// Each record whole, then the Source failing, its pipes ending without the
/// data's end, as those of a Source that failed end: its failure is the
/// cause, not files that never came, and no temporary file is left.
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
    // Each record whole, its files written and waiting for their batch,
    // then the Source fails before the data ends.
    let (whole, _) = records_of(&with_ids(&files));
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
        batch_entries: 0,
        listers: 1,
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

/// What the Destination said failed, among its answers.
fn failures(answers: &[(PacketType, Vec<u8>)]) -> Vec<Answer> {
    decoded::<Answer>(answers)
        .into_iter()
        .filter(|answer| matches!(answer, Answer::Failed { .. } | Answer::NotDeleted { .. }))
        .collect()
}

/// Whether the Destination said it was done, last, and never failed.
fn done(answers: &[(PacketType, Vec<u8>)]) -> bool {
    decoded::<Control>(answers).is_empty()
        && decoded::<Answer>(answers).last() == Some(&Answer::Done)
}

#[test]
fn a_file_is_written() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let answers = run_on(&root, &packets(None));
    assert_eq!(failures(&answers), []);
    assert!(done(&answers));
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
    for (dir, entries) in [
        (0, vec![entry(1, b"d", Kind::Directory, 0)]),
        (1, Vec::new()),
    ] {
        send_part(&mut packets, dir, entries, true);
    }
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        batch_entries: 0,
        listers: 1,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    data_end(&mut packets);
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

/// A directory in two batches, synced: only what neither batch has is
/// deleted, found once its last batch is here.
#[test]
fn a_directory_in_batches_deletes_only_what_no_batch_has() {
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
    for record in part(
        0,
        vec![
            entry(1, b"first", Kind::File, 5),
            entry(2, b"second", Kind::File, 6),
        ],
        true,
    ) {
        send(&mut packets, &record);
    }
    send(&mut packets, &Manifest::End);
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        batch_entries: 1,
        listers: 1,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    data_end(&mut packets);
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
        },
    )
    .unwrap();
    send_part(
        &mut packets,
        0,
        vec![entry(1, b"locked", Kind::Unreadable, 0)],
        true,
    );
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        batch_entries: 0,
        listers: 1,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    data_end(&mut packets);
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
    send_part(
        &mut packets,
        0,
        vec![entry(1, b"locked", Kind::Unreadable, 0)],
        true,
    );
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: true,
        report: Reporter::none(),
        protect: Vec::new(),
        batch_entries: 0,
        listers: 1,
        data_credit: 0,
        one_file_system: false,
    };
    data_end(&mut packets);
    run(&mut &packets[..], &mut Vec::new(), &options).unwrap();
    assert_eq!(std::fs::read(root.join("alias")).unwrap(), b"kept");
    assert_eq!(std::fs::read(root.join("locked")).unwrap(), b"kept");
    assert!(!root.join("extra").exists());
}

/// Runs the Destination, syncing, on a manifest of `messages` and an
/// end of the data alone, beside a file outside its root and with one
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
    data_end(&mut packets);
    for dry_run in [true, false] {
        let options = Options {
            root: root.as_os_str().as_bytes().to_vec(),
            dry_run,
            modify_window: 0,
            numeric_ids: true,
            delete: true,
            report: Reporter::none(),
            protect: Vec::new(),
            batch_entries: 0,
            listers: 1,
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

/// The records of `entries` of the directory `dir`: after its `In`, and
/// before its `Listed`, if `last`.
fn part(dir: FileId, entries: Vec<Entry>, last: bool) -> Vec<Manifest> {
    let mut records = vec![Manifest::In { dir }];
    records.extend(entries.into_iter().map(Manifest::Entry));
    if last {
        records.push(Manifest::Listed { dir });
    }
    records
}

/// Sends the records of `entries` of the directory `dir`, as [`part`].
fn send_part(packets: &mut Vec<u8>, dir: FileId, entries: Vec<Entry>, last: bool) {
    for record in part(dir, entries, last) {
        protocol::send(packets, &record).unwrap();
    }
}

/// The root, then `parts` one after another, then the end.
fn whole(parts: &[Vec<Manifest>]) -> Vec<Manifest> {
    let mut records = vec![root_directory()];
    records.extend(parts.iter().flatten().cloned());
    records.push(Manifest::End);
    records
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
        refused(&whole(&[part(0, entries, true)]));
    }
}

/// The Destination's `inside`, as it has it, so that nothing it asks for
/// is done before what is tested is refused, however soon it is applied.
fn inside() -> Vec<Manifest> {
    part(1, vec![entry(3, b"keep", Kind::File, 4)], true)
}

#[test]
fn a_manifest_with_a_directory_not_announced_is_refused() {
    // A file, one never named, and the root again, once it ended.
    for dir in [2, 9, 0] {
        refused(&whole(&[
            part(0, the_rest(1), true),
            inside(),
            part(dir, vec![entry(4, b"x", Kind::File, 0)], true),
        ]));
    }
    // Before the root.
    refused(&[part(0, the_rest(1), true), vec![Manifest::End]].concat());
    // An entry in no directory: before any In, or once it ended.
    refused(&whole(&[vec![Manifest::Entry(entry(
        1,
        b"keep",
        Kind::File,
        4,
    ))]]));
    refused(&whole(&[
        part(0, the_rest(1), true),
        vec![Manifest::Entry(entry(3, b"x", Kind::File, 0))],
    ]));
}

#[test]
fn a_manifest_with_a_directory_sent_twice_is_refused() {
    refused(&whole(&[part(0, the_rest(1), true), inside(), inside()]));
    refused(&whole(&[
        part(0, the_rest(1), true),
        inside(),
        vec![Manifest::Unlistable { dir: 1 }],
    ]));
    // Unlistable once some of it came.
    refused(&whole(&[
        part(0, the_rest(1), true),
        part(1, vec![entry(3, b"keep", Kind::File, 4)], false),
        vec![Manifest::Unlistable { dir: 1 }],
    ]));
}

#[test]
fn a_manifest_that_ends_before_its_directories_do_is_refused() {
    refused(&whole(&[part(0, the_rest(1), true)]));
    refused(&whole(&[
        part(0, the_rest(1), false),
        part(1, vec![entry(3, b"keep", Kind::File, 4)], true),
    ]));
}

#[test]
fn a_manifest_out_of_order_is_refused() {
    // Within a batch.
    let mut entries = the_rest(1);
    entries.reverse();
    refused(&whole(&[part(0, entries, true)]));
    // A name twice.
    let entries = vec![
        entry(1, b"keep", Kind::File, 4),
        entry(2, b"keep", Kind::File, 4),
    ];
    refused(&whole(&[part(0, entries, true)]));
    // Across Ins.
    refused(&whole(&[
        part(0, vec![entry(1, b"keep", Kind::File, 4)], false),
        part(0, vec![entry(2, b"inside", Kind::Directory, 0)], true),
    ]));
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
    refused(&whole(&[part(0, entries, true)]));
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
    let failures = failures_of(&extras, results);
    assert!(failures.is_empty());
    tell_deleted(b"d", failures, &mut Vec::new()).unwrap();
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
    let failures = failures_of(&extras, results);
    assert!(failures.len() < extras.len());
    let mut commands = Vec::new();
    tell_deleted(b"d", failures, &mut commands).unwrap();
    let failed: Vec<&Answer> = commands
        .iter()
        .filter_map(|command| match command {
            Command::Fail { result, .. } => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        matches!(failed[..], [Answer::NotDeleted { path, .. }] if path == b"d/already-gone"),
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
        batch_entries: 0,
        listers: 1,
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
    let results = failures(&answers);
    assert!(
        matches!(
            &results[..],
            [Answer::Failed { id: 1, message }]
                if message == "the Source could not read it whole: it shrank while it was read"
        ),
        "{results:?}"
    );
    assert!(done(&answers));
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
    packet.kind == PacketType::Answer && matches!(packet.decode::<Answer>(), Ok(Answer::File(1)))
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

/// A packet of the data whose record does not decode as one.
fn undecodable_record() -> Vec<u8> {
    let mut packet = vec![PacketType::Data as u8];
    packet.extend_from_slice(&3u32.to_le_bytes());
    packet.extend_from_slice(&[0xff; 3]);
    packet
}

/// A segment out of its place: at a byte that is no segment's start.
fn misplaced_segment() -> Vec<u8> {
    let segment = Data::Segment {
        id: 1,
        offset: 1,
        size: 3 * block::SEGMENT,
        mtime: entry(1, b"x", Kind::File, 0).mtime,
        check: Check::Hash([0; 32]),
        bytes: ByteBuf(vec![0; block::SEGMENT as usize]),
    };
    protocol::encode(&segment).unwrap()
}

/// A record of a file never asked for.
fn unneeded_record() -> Vec<u8> {
    wholes(&[(99, b"", hash(b""))])
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
    send_part(
        &mut first,
        0,
        vec![entry(1, b"later", Kind::Directory, 0)],
        true,
    );
    send_data(&mut first, &data);
    let mut then = Vec::new();
    send_part(&mut then, 1, Vec::new(), true);
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
        batch_entries: 0,
        listers: 1,
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
        (undecodable_record(), "decode a record of the data"),
        (misplaced_segment(), "a segment at 1 of"),
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
        let after: Vec<Answer> = decoded(&answers[error_at..]);
        assert!(
            after
                .iter()
                .all(|answer| !matches!(answer, Answer::File(_))),
            "{after:?}"
        );
    }
}

/// The Source's manifest, then `records`, as data packets.
fn manifest_then(records: &[Vec<u8>]) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    send_part(&mut packets, 0, vec![entry(1, b"a", Kind::File, 5)], true);
    protocol::send(&mut packets, &Manifest::End).unwrap();
    for record in records {
        send_data(&mut packets, record);
    }
    data_end(&mut packets);
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
        batch_entries: 0,
        listers: 1,
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

/// Many records behind the cause, the main thread waits for room for them
/// as the stage that failed stops the rest: what it then meets is not taken
/// for the cause, which is said once.
#[test]
fn the_cause_is_said_however_much_comes_after_it() {
    let good = wholes(&[(1, b"hello", hash(b"hello"))]);
    let filler = wholes(&[(1, &[0; 4096], hash(&[0; 4096]))]);
    for (records, cause) in [
        (
            [vec![good, misplaced_segment()], vec![filler.clone(); 60]].concat(),
            "a segment at 1 of",
        ),
        (
            [vec![unneeded_record()], vec![filler.clone(); 60]].concat(),
            "the Source sent unneeded ID 99",
        ),
    ] {
        let (error, controls) = failing_with_errors(&manifest_then(&records));
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
    send_part(&mut packets, 0, vec![entry(1, b"a", Kind::File, 5)], false);
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
            batch_entries: 0,
            listers: 1,
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

/// A file in segments, its hash before its segments, and a small file
/// after them, are made, as the records come, in the order written.
#[test]
fn segments_after_their_hash_make_the_file() {
    let content = crate::test_support::Random::new(43).bytes(2 * block::SEGMENT as usize + 1000);
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    let entries = vec![
        entry(1, b"big", Kind::File, content.len() as u64),
        entry(2, b"small", Kind::File, 5),
    ];
    send_part(&mut packets, 0, entries, true);
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let hash = Data::<ByteBuf>::Hash {
        file: 1,
        hash: *blake3::hash(&content).as_bytes(),
    };
    protocol::send(&mut packets, &hash).unwrap();
    for index in 0..3 {
        send_data(
            &mut packets,
            &segment_of(1, &content, index * block::SEGMENT),
        );
    }
    send_data(
        &mut packets,
        &wholes(&[(2, b"hello", super::tests::hash(b"hello"))]),
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    data_end(&mut packets);
    let answers = run_on(&root, &packets);
    assert_eq!(failures(&answers), []);
    assert!(done(&answers));
    assert!(std::fs::read(root.join("big")).unwrap() == content);
    assert_eq!(std::fs::read(root.join("small")).unwrap(), b"hello");
}

/// Records that end early, as a stream cut, between them or in the middle
/// of a file whose segments did not all come, fail the transfer.
#[test]
fn records_that_end_early_fail_the_transfer() {
    let big = vec![7u8; block::SEGMENT as usize + 1_000];
    let files: [(&[u8], &[u8], [u8; 32]); 2] = [
        (b"a", b"hello", *blake3::hash(b"hello").as_bytes()),
        (b"big", &big, *blake3::hash(&big).as_bytes()),
    ];
    let (data, _) = records_of(&with_ids(&files));
    let whole_first = wholes(&[(1, b"hello", hash(b"hello"))]).len();
    for keep in [data.len() - 500, whole_first + 3] {
        let temp = tempfile::tempdir().unwrap();
        let error = run_failing(
            &temp.path().join("destination"),
            &files_packets(&files, Some(keep), None),
        );
        assert!(
            format!("{error:#}").contains("the stream broke"),
            "{error:#}"
        );
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
    send_part(
        &mut packets,
        0,
        vec![entry(1, b"locked", Kind::Directory, 0)],
        true,
    );
    send_part(&mut packets, 1, vec![entry(2, b"f", Kind::File, 5)], true);
    protocol::send(&mut packets, &Manifest::End).unwrap();
    send_data(&mut packets, &misplaced_segment());
    let error = run_failing(&root, &packets);
    assert!(
        format!("{error:#}").contains("a segment at 1 of"),
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
    send_part(&mut packets, 0, vec![entry(1, b"a", Kind::File, 5)], true);
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        batch_entries: 0,
        listers: 1,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    data_end(&mut packets);
    let summary = run(&mut &packets[..], &mut output, &options).unwrap();
    assert_eq!(summary.failed, 1);
    let mut reader = output.as_slice();
    let answers: Answers = std::iter::from_fn(|| protocol::receive(&mut reader).unwrap())
        .map(|packet| (packet.kind, packet.into_payload()))
        .collect();
    let results = failures(&answers);
    assert!(
        matches!(&results[..], [Answer::Failed { id: 1, message }]
            if message == "the Source never sent it"),
        "{results:?}"
    );
    assert!(done(&answers));
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
    send_part(&mut first, 0, vec![entry(1, b"a", Kind::File, 5)], true);
    protocol::send(&mut first, &Manifest::End).unwrap();
    // As a Source says so: once the file is asked for.
    let mut then = Vec::new();
    protocol::send(
        &mut then,
        &Data::<ByteBuf>::Unopened {
            file: 1,
            path: b"a".to_vec(),
        },
    )
    .unwrap();
    data_end(&mut then);
    let answers = run_gated(&root, first, then, file_1_needed);
    assert_eq!(failures(&answers), []);
    assert!(done(&answers));
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
        batch_entries: 0,
        listers: 1,
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
    send_part(&mut first, 0, vec![entry(1, b"a", Kind::File, 5)], true);
    let mut then = Vec::new();
    protocol::send(&mut then, &Manifest::End).unwrap();
    send_data(&mut then, &wholes(&[(1, b"hello", hash(b"hello"))]));
    data_end(&mut then);
    let answers = run_gated(&root, first, then, file_1_needed);
    assert!(done(&answers));
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
    let hello = wholes(&[(1, b"hello", hash(b"hello"))]);
    let packets = manifest_then(&[hello.clone(), hello]);
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
    send_part(&mut packets, 0, chains, true);
    let mut id = 21;
    let mut levels = Vec::new();
    for chain in 0..20 {
        let mut path = format!("c{chain:02}");
        let mut dir = 1 + chain;
        levels.push(path.clone());
        for _ in 1..4 {
            send_part(&mut packets, dir, vec![closed(id, b"d")], true);
            dir = id;
            id += 1;
            path.push_str("/d");
            levels.push(path.clone());
        }
        send_part(&mut packets, dir, Vec::new(), true);
    }
    protocol::send(&mut packets, &Manifest::End).unwrap();
    data_end(&mut packets);
    let answers = run_on(&root, &packets);
    let failed: Vec<Answer> = failures(&answers);
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
/// go of their entries, as Answer::Compared says those of it are asked for.
#[test]
fn compared_never_covers_a_file_not_asked_for() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("destination");
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    // Twenty directories to make, of three files each, numbered in the
    // manifest's order: depth first, by name.
    let directories: Vec<Entry> = (0..20u64)
        .map(|n| entry(1 + n, format!("d{n:02}").as_bytes(), Kind::Directory, 0))
        .collect();
    send_part(&mut packets, 0, directories, true);
    let mut id = 21;
    for n in 0..20 {
        let files: Vec<Entry> = (0..3)
            .map(|f| {
                id += 1;
                entry(id - 1, format!("f{f}").as_bytes(), Kind::File, 1)
            })
            .collect();
        send_part(&mut packets, 1 + n, files, true);
    }
    protocol::send(&mut packets, &Manifest::End).unwrap();
    // No file comes: each fails as never sent, which does not matter here.
    let options = Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        delete: false,
        report: Reporter::none(),
        protect: Vec::new(),
        batch_entries: 0,
        listers: 1,
        data_credit: 0,
        one_file_system: false,
    };
    let mut output = Vec::new();
    data_end(&mut packets);
    run(&mut &packets[..], &mut output, &options).unwrap();
    let mut reader = output.as_slice();
    let mut needs = Vec::new();
    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
        if packet.kind == PacketType::Answer {
            needs.push(packet.decode::<Answer>().unwrap());
        }
    }
    let mut asked = std::collections::BTreeSet::new();
    let mut compared = std::collections::BTreeSet::new();
    for need in &needs {
        match need {
            Answer::File(id) => drop(asked.insert(*id)),
            Answer::Compared { first, last } => {
                for file in (*first..=*last).filter(|file| (21..id).contains(file)) {
                    assert!(
                        asked.contains(&file),
                        "told {first} to {last} compared before {file} was asked for: {needs:?}"
                    );
                }
                for entry in *first..=*last {
                    assert!(compared.insert(entry), "{entry} told twice: {needs:?}");
                }
            }
            _ => {}
        }
    }
    // Every entry is told compared, the root's too.
    assert_eq!(compared, (0..id).collect(), "{needs:?}");
    // Directories to make are among what is asked for too.
    assert!(
        (21..id).all(|file| asked.contains(&file)),
        "every file asked for once its directory was made: {asked:?}"
    );
}

/// The manifest of one file, `big`, of `size` bytes, and what a Source sends
/// of it: `records`, and its hash, before them or after, if any, then the
/// end.
fn big_packets(size: u64, records: &[Vec<u8>], hash: Option<([u8; 32], bool)>) -> Vec<u8> {
    let mut packets = Vec::new();
    protocol::send(&mut packets, &root_directory()).unwrap();
    send_part(
        &mut packets,
        0,
        vec![entry(1, b"big", Kind::File, size)],
        true,
    );
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let hash = hash.map(|(hash, first)| (Data::<ByteBuf>::Hash { file: 1, hash }, first));
    if let Some((hash, true)) = &hash {
        protocol::send(&mut packets, hash).unwrap();
    }
    for record in records {
        send_data(&mut packets, record);
    }
    if let Some((hash, false)) = &hash {
        protocol::send(&mut packets, hash).unwrap();
    }
    data_end(&mut packets);
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
        let records: Vec<Vec<u8>> = order.iter().map(|&index| segments[index].clone()).collect();
        let packets = big_packets(content.len() as u64, &records, Some((hash, first)));
        let answers = run_on(&root, &packets);
        assert_eq!(failures(&answers), [], "{order:?}");
        assert!(done(&answers));
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
    for (records, hash) in [
        (good, [7; 32]),
        (changed, *blake3::hash(&content).as_bytes()),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("destination");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("big"), b"old").unwrap();
        let packets = big_packets(content.len() as u64, &records, Some((hash, false)));
        let answers = run_on(&root, &packets);
        let results = failures(&answers);
        assert!(
            matches!(&results[..], [Answer::Failed { id: 1, message }]
                if message == "the content's hash does not match the Source's"),
            "{results:?}"
        );
        assert!(done(&answers));
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
    let unread = protocol::encode(&Data::Segment {
        id: 1,
        offset: block::SEGMENT,
        size: content.len() as u64,
        mtime: entry(1, b"x", Kind::File, 0).mtime,
        check: Check::Unread("it shrank while it was read".to_owned()),
        bytes: ByteBuf(Vec::new()),
    })
    .unwrap();
    let hash = *blake3::hash(&content).as_bytes();
    for (records, hash, said) in [
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
        let answers = run_on(&root, &big_packets(content.len() as u64, &records, hash));
        let results = failures(&answers);
        assert!(
            matches!(&results[..], [Answer::Failed { id: 1, message }] if message == said),
            "{results:?}"
        );
        assert!(done(&answers));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}

/// What the Destination holds of the files' bytes stays within its bound,
/// which takes the place of the credit it once gave: a record that would go
/// past it
/// waits until enough is let go of, one alone always fits, and a wait ends
/// as the Destination stops.
#[test]
fn what_is_held_of_the_files_bytes_stays_within_its_bound() {
    crate::test_support::within(
        "what is held of the files' bytes stays within its bound",
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
    send_part(&mut first, 0, entries, true);
    protocol::send(&mut first, &Manifest::End).unwrap();
    let packed: Vec<(FileId, &[u8], Check)> = files
        .iter()
        .map(|&(id, _, content)| (id, content, hash(content)))
        .collect();
    send_data(&mut first, &wholes(&packed));
    // The data ends only once they have their names.
    let mut then = Vec::new();
    end_data(&mut then);
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
        batch_entries: 0,
        listers: 1,
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

/// The extras that could not be deleted, each with why.
fn failures_of(
    extras: &[(Vec<u8>, bool)],
    results: Vec<io::Result<()>>,
) -> Vec<((Vec<u8>, bool), io::Error)> {
    extras
        .iter()
        .cloned()
        .zip(results)
        .filter_map(|(extra, result)| result.err().map(|error| (extra, error)))
        .collect()
}
