use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::test_support::{PATIENCE, within};
use crate::transport::scope::Scope;

/// The two ends of a stream of `wires` wires, each pair a socket pair, the
/// Source's and the Destination's, within `scope`.
fn pair_within(wires: usize, windows: Windows, scope: &Scope) -> (Connection, Connection) {
    let (mut source, mut destination) = (Vec::new(), Vec::new());
    for _ in 0..wires {
        let (one, other) = UnixStream::pair().unwrap();
        source.push(Wire::socket(one, scope).unwrap());
        destination.push(Wire::socket(other, scope).unwrap());
    }
    (
        Connection::open(source, &stream(true, windows, None)).unwrap(),
        Connection::open(destination, &stream(false, windows, None)).unwrap(),
    )
}

fn pair(wires: usize, windows: Windows) -> (Connection, Connection) {
    pair_within(wires, windows, &Scope::new().unwrap())
}

/// The two ends of a stream of `wires` wires, its pipes compressed at
/// level 1.
fn compressed_pair(wires: usize, windows: Windows) -> (Connection, Connection) {
    let scope = Scope::new().unwrap();
    let (mut source, mut destination) = (Vec::new(), Vec::new());
    for _ in 0..wires {
        let (one, other) = UnixStream::pair().unwrap();
        source.push(Wire::socket(one, &scope).unwrap());
        destination.push(Wire::socket(other, &scope).unwrap());
    }
    (
        Connection::open(source, &stream(true, windows, Some(1))).unwrap(),
        Connection::open(destination, &stream(false, windows, Some(1))).unwrap(),
    )
}

/// Bytes that do not compress.
fn noise(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Windows small enough for a test to fill.
fn small() -> Windows {
    Windows {
        manifest: 64 * 1024,
        data: 256 * 1024,
        answers: 64 * 1024,
    }
}

/// Bytes that differ along their length, so that one out of place shows.
fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|at| seed.wrapping_add((at % 251) as u8))
        .collect()
}

/// Reads `reader` to its end.
fn all(mut reader: PipeReader) -> io::Result<Vec<u8>> {
    let mut read = Vec::new();
    reader.read_to_end(&mut read)?;
    Ok(read)
}

/// Waits until `count` has stayed put a while, at least `at_least`: a writer
/// that waits.
fn settled(count: &AtomicUsize, at_least: usize) -> usize {
    let started = Instant::now();
    let mut last = count.load(Ordering::SeqCst);
    let mut since = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{last} written"
        );
        std::thread::sleep(Duration::from_millis(5));
        let now = count.load(Ordering::SeqCst);
        if now != last {
            (last, since) = (now, Instant::now());
        } else if now >= at_least && since.elapsed() >= Duration::from_millis(100) {
            return now;
        }
    }
}

/// Each pipe carries what was written, in order, however it was cut, data
/// over several wires; each reads to its end once its writer finished; and
/// once all went both ways, neither end is broken.
#[test]
fn each_pipe_carries_what_was_written_in_order_data_on_every_wire() {
    within(
        "each pipe carries what was written, in order, data on every wire",
        PATIENCE,
        || {
            let (mut source, mut destination) = pair(3, small());
            let data = pattern(5 << 20, 1);
            let manifest = pattern(300_000, 2);
            let answers = pattern(200_000, 3);
            let (data_in, manifest_in, answers_in) = (
                destination.reader(DATA),
                destination.reader(MANIFEST),
                destination.writer(ANSWERS),
            );
            let answers_out = source.reader(ANSWERS);
            // Cut however: a byte, a frame and more, a chunk and more.
            let sizes = [1, 7, 65_537, 3 << 20, 100];
            let (data_out, manifest_out) = (source.writer(DATA), source.writer(MANIFEST));
            std::thread::scope(|scope| {
                let reading_data = scope.spawn(|| all(data_in).unwrap());
                let reading_manifest = scope.spawn(|| all(manifest_in).unwrap());
                let reading_answers = scope.spawn(|| all(answers_out).unwrap());
                let writing_data = scope.spawn(|| {
                    let mut at = 0;
                    for size in sizes.iter().cycle() {
                        let end = (at + size).min(data.len());
                        data_out.write_all(&data[at..end]).unwrap();
                        at = end;
                        if at == data.len() {
                            break;
                        }
                    }
                });
                for piece in manifest.chunks(4096) {
                    manifest_out.write_all(piece).unwrap();
                }
                answers_in.write_all(&answers).unwrap();
                destination.finish().unwrap();
                assert_eq!(reading_answers.join().unwrap(), answers);
                writing_data.join().unwrap();
                source.finish().unwrap();
                assert_eq!(reading_manifest.join().unwrap(), manifest);
                assert_eq!(reading_data.join().unwrap(), data);
                assert!(source.shared.lock().broken.is_none());
                assert!(destination.shared.lock().broken.is_none());
            });
        },
    );
}

/// A writer waits once the window and its send buffer are used up, and goes
/// on as the far program reads.
#[test]
fn a_writer_waits_for_window_and_goes_on_as_the_far_end_reads() {
    within(
        "a writer waits for window and goes on as the far end reads",
        PATIENCE,
        || {
            let (source, mut destination) = pair(1, small());
            let manifest = pattern(1 << 20, 4);
            let written = AtomicUsize::new(0);
            let writer = source.writer(MANIFEST);
            std::thread::scope(|scope| {
                let writing = scope.spawn(|| {
                    for piece in manifest.chunks(1024) {
                        writer.write_all(piece).unwrap();
                        written.fetch_add(piece.len(), Ordering::SeqCst);
                    }
                });
                // The window and the send buffer, 64 KiB each, and no more.
                let waiting = settled(&written, 64 * 1024);
                assert!(waiting <= 2 * 64 * 1024 + 1024, "{waiting} written");
                let mut reader = destination.reader(MANIFEST);
                let mut read = vec![0; manifest.len()];
                reader.read_exact(&mut read).unwrap();
                assert_eq!(read, manifest);
                writing.join().unwrap();
            });
        },
    );
}

/// A pipe no one reads holds back none of the others: its writer waits, and
/// the others go on, control too.
#[test]
fn a_pipe_no_one_reads_holds_back_no_other() {
    within("a pipe no one reads holds back no other", PATIENCE, || {
        let (source, mut destination) = pair(2, small());
        let stuck = AtomicBool::new(false);
        let manifest = source.writer(MANIFEST);
        let data = pattern(3 << 20, 6);
        let writer = source.writer(DATA);
        let reader = destination.reader(DATA);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // Far more than the window: it waits.
                let _ = manifest.write_all(&pattern(4 << 20, 5));
                stuck.store(true, Ordering::SeqCst);
            });
            let reading = scope.spawn(|| {
                let mut reader = reader;
                let mut read = vec![0; data.len()];
                reader.read_exact(&mut read).unwrap();
                read
            });
            writer.write_all(&data).unwrap();
            assert_eq!(reading.join().unwrap(), data);
            source.writer(CONTROL).write_all(b"error").unwrap();
            let mut control = destination.reader(CONTROL);
            let mut read = [0; 5];
            control.read_exact(&mut read).unwrap();
            assert_eq!(&read, b"error");
            assert!(!stuck.load(Ordering::SeqCst));
            // Let go of, the stuck writer stops.
            drop(destination);
        });
        assert!(stuck.load(Ordering::SeqCst));
    });
}

/// A reader stopped returns as at the pipe's end, and what comes after is
/// let go of, its window given back: a writer waiting for window goes on.
#[test]
fn a_reader_stopped_lets_go_of_what_comes_and_its_writer_goes_on() {
    within(
        "a reader stopped lets go of what comes and its writer goes on",
        PATIENCE,
        || {
            let (source, mut destination) = pair(1, small());
            let writer = source.writer(MANIFEST);
            let mut reader = destination.reader(MANIFEST);
            let stopper = reader.stopper();
            let written = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                let writing = scope.spawn(|| {
                    for piece in pattern(1 << 20, 9).chunks(4096) {
                        writer.write_all(piece).unwrap();
                        written.fetch_add(piece.len(), Ordering::SeqCst);
                    }
                });
                let reading = scope.spawn(move || reader.read(&mut [0; 1]).map(|_| reader));
                let mut reader = reading.join().unwrap().unwrap();
                // Far less than written: the writer waits for window.
                assert!(settled(&written, 64 * 1024) < 1 << 20);
                stopper.stop();
                writing.join().unwrap();
                assert_eq!(reader.read(&mut [0; 16]).unwrap(), 0);
            });
        },
    );
}

/// A sync returns once the far program has read all written before it, not
/// before, data across several wires as well; and with the connection let go
/// of, at once.
#[test]
fn a_sync_returns_once_the_far_program_has_read_all_before_it() {
    within(
        "a sync returns once the far program has read all before it",
        PATIENCE,
        || {
            for (pipe, length) in [(MANIFEST, 50_000), (DATA, 3 << 20)] {
                let (source, mut destination) = pair(3, small());
                let synced = AtomicBool::new(false);
                let bytes = pattern(length, 7);
                let writer = source.writer(pipe);
                std::thread::scope(|scope| {
                    let syncing = scope.spawn(|| {
                        writer.write_all(&bytes).unwrap();
                        writer.sync().unwrap();
                        synced.store(true, Ordering::SeqCst);
                    });
                    let mut reader = destination.reader(pipe);
                    let mut read = vec![0; length - 1];
                    reader.read_exact(&mut read).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    assert!(!synced.load(Ordering::SeqCst), "synced before all was read");
                    let mut last = [0];
                    reader.read_exact(&mut last).unwrap();
                    syncing.join().unwrap();
                    assert!(synced.load(Ordering::SeqCst));
                });
            }
            // One the far end never reads returns as the connection is let
            // go of.
            let (source, destination) = pair(1, small());
            std::thread::scope(|scope| {
                let writer = source.writer(MANIFEST);
                let syncing = scope.spawn(move || {
                    writer.write_all(b"never read").unwrap();
                    writer.sync()
                });
                std::thread::sleep(Duration::from_millis(50));
                drop(destination);
                assert!(syncing.join().unwrap().is_err());
            });
        },
    );
}

/// Compressed, each pipe carries what was written, in order, as it was:
/// many small writes, one far larger than a frame of what compresses, and
/// one of what does not, through windows of a few frames and of no more than
/// one, both ways; the far end reads them however it asks, a byte or a
/// mebibyte at a time; and what compresses takes far fewer bytes on the
/// wire than were written.
#[test]
fn a_compressed_pipe_carries_what_was_written_and_compresses_it() {
    within(
        "a compressed pipe carries what was written, and compresses it",
        PATIENCE,
        || {
            for windows in [
                small(),
                Windows {
                    manifest: 4096,
                    data: 256 * 1024,
                    answers: 2048,
                },
            ] {
                let (mut source, mut destination) = compressed_pair(1, windows);
                let mut written = Vec::new();
                for record in 0..2_000u32 {
                    written.extend_from_slice(format!("record {record}\n").as_bytes());
                }
                written.extend(pattern(1 << 20, 3));
                written.extend(noise(300_000, 9));
                std::thread::scope(|scope| {
                    let manifest = source.writer(MANIFEST);
                    let answers = destination.writer(ANSWERS);
                    let (to_write, back) = (&written, &written);
                    scope.spawn(move || {
                        for piece in to_write.chunks(37) {
                            manifest.write_all(piece).unwrap();
                        }
                        manifest.close();
                    });
                    scope.spawn(move || {
                        answers.write_all(back).unwrap();
                        answers.close();
                    });
                    let mut manifest = destination.reader(MANIFEST);
                    let mut read = Vec::new();
                    let mut byte = [0];
                    for _ in 0..1000 {
                        manifest.read_exact(&mut byte).unwrap();
                        read.push(byte[0]);
                    }
                    let mut big = vec![0; 1 << 20];
                    loop {
                        let count = manifest.read(&mut big).unwrap();
                        if count == 0 {
                            break;
                        }
                        read.extend_from_slice(&big[..count]);
                    }
                    assert!(read == written, "the manifest came otherwise");
                    assert!(all(source.reader(ANSWERS)).unwrap() == written);
                });
                // The records and the pattern compress, each way, though in
                // small flushes where the window holds a frame alone; the
                // noise does not: less than half goes.
                let carried: u64 = source.carried().iter().map(|carried| carried.now().0).sum();
                assert!(
                    carried < written.len() as u64,
                    "{carried} bytes went both ways for {} written each way",
                    written.len()
                );
                source.finish().unwrap();
                destination.finish().unwrap();
            }
        },
    );
}

/// Compressed, the data's chunks come back in order, as they were, over
/// several wires: what compresses, far more than its window, and what does
/// not, each chunk alone; written in pieces of every size, and read so.
#[test]
fn compressed_chunks_come_back_in_order_over_every_wire() {
    within(
        "compressed chunks come back in order over every wire",
        PATIENCE,
        || {
            let (source, mut destination) = compressed_pair(3, small());
            let mut written = pattern(24 << 20, 1);
            written.extend(noise(3 << 20, 4));
            written.extend(pattern(5 << 20, 2));
            std::thread::scope(|scope| {
                let data = source.writer(DATA);
                let to_write = &written;
                scope.spawn(move || {
                    let mut at = 0;
                    for size in (0..).map(|step: usize| 1 + step * 7919 % (3 << 20)) {
                        let end = (at + size).min(to_write.len());
                        data.write_all(&to_write[at..end]).unwrap();
                        at = end;
                        if at == to_write.len() {
                            break;
                        }
                    }
                    data.close();
                });
                let mut reader = destination.reader(DATA);
                let mut read = Vec::new();
                let mut buffer = vec![0; 100_003];
                loop {
                    let count = reader.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    read.extend_from_slice(&buffer[..count]);
                }
                assert!(read == written, "the data came otherwise");
            });
            let carried: u64 = destination
                .carried()
                .iter()
                .map(|carried| carried.now().0)
                .sum();
            assert!(
                carried < (3 << 20) + (2 << 20),
                "{carried} bytes went for {} written",
                written.len()
            );
            source.finish().unwrap();
            destination.finish().unwrap();
        },
    );
}

/// Compressed, a sync returns once the far program has read all written
/// before it, decompressed, not before.
#[test]
fn a_sync_on_a_compressed_pipe_returns_once_all_is_read() {
    within(
        "a sync on a compressed pipe returns once all is read",
        PATIENCE,
        || {
            let (source, mut destination) = compressed_pair(1, small());
            let synced = AtomicBool::new(false);
            let bytes = pattern(200_000, 5);
            let writer = source.writer(MANIFEST);
            std::thread::scope(|scope| {
                let syncing = scope.spawn(|| {
                    writer.write_all(&bytes).unwrap();
                    writer.sync().unwrap();
                    synced.store(true, Ordering::SeqCst);
                });
                let mut reader = destination.reader(MANIFEST);
                let mut read = vec![0; bytes.len() - 1];
                reader.read_exact(&mut read).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                assert!(!synced.load(Ordering::SeqCst), "synced before all was read");
                let mut last = [0];
                reader.read_exact(&mut last).unwrap();
                syncing.join().unwrap();
                assert!(synced.load(Ordering::SeqCst));
            });
        },
    );
}

/// A writer waiting for window stops as the connection's scope ends, saying
/// why.
#[test]
fn a_waiting_writer_stops_as_the_scope_ends() {
    within("a waiting writer stops as the scope ends", PATIENCE, || {
        let scope = Scope::new().unwrap();
        let (source, _destination) = pair_within(1, small(), &scope);
        let writer = source.writer(MANIFEST);
        let writing = std::thread::spawn(move || writer.write_all(&pattern(4 << 20, 8)));
        std::thread::sleep(Duration::from_millis(100));
        scope.end("the host was lost");
        let error = writing.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("the host was lost"), "{error}");
    });
}

/// A connection let go of before all went both ways is broken, as the far
/// end sees it: what reads there fails, past what came.
#[test]
fn a_connection_let_go_of_early_is_broken_at_the_far_end() {
    within("a connection let go of early is broken", PATIENCE, || {
        let (source, mut destination) = pair(1, small());
        source.writer(MANIFEST).write_all(b"part").unwrap();
        let mut reader = destination.reader(MANIFEST);
        let mut read = [0; 4];
        reader.read_exact(&mut read).unwrap();
        drop(source);
        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert!(error.to_string().contains("the stream broke"), "{error}");
    });
}

/// Bytes a wire read before it was ready go first, taken as frames.
#[test]
fn what_a_wire_read_before_it_was_ready_goes_first() {
    within(
        "what a wire read before it was ready goes first",
        PATIENCE,
        || {
            let (one, other) = UnixStream::pair().unwrap();
            let scope = Scope::new().unwrap();
            let mut early = Wire::socket(other, &scope).unwrap();
            let mut frames = Outgoing::of(CONTROL, Kind::Bytes, b"first".to_vec()).bytes();
            frames.extend(Outgoing::of(CONTROL, Kind::Bytes, b" then".to_vec()).bytes());
            let mut sender = one;
            sender.write_all(&frames[..8]).unwrap();
            // Read ahead, as reading to a start mark does.
            std::io::BufRead::fill_buf(&mut early).unwrap();
            sender.write_all(&frames[8..]).unwrap();
            let mut connection =
                Connection::open(vec![early], &stream(false, small(), None)).unwrap();
            let mut read = [0; 10];
            connection.reader(CONTROL).read_exact(&mut read).unwrap();
            assert_eq!(&read, b"first then");
        },
    );
}

// The core, without threads.

/// A frame of `kind` on `pipe` with `content`, as sent.
fn frame(pipe: u8, kind: Kind, content: &[u8]) -> Vec<u8> {
    Outgoing::of(pipe, kind, content.to_vec()).bytes()
}

/// A chunk numbered `number` of `content`, in one frame.
fn chunk(number: u64, content: &[u8]) -> Vec<u8> {
    let mut bytes = number.to_le_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&(content.len() as u32).to_le_bytes());
    bytes.extend_from_slice(content);
    frame(DATA, Kind::Bytes, &bytes)
}

/// The Destination's core, of `wires` wires, and its inbounds.
fn destination_core(wires: usize) -> (Core, Vec<Inbound>) {
    (
        Core::new(&stream(false, small(), None), wires),
        (0..wires).map(Inbound::new).collect(),
    )
}

/// What `core` holds to read on `pipe`.
fn readable(core: &mut Core, pipe: u8) -> Vec<u8> {
    core.read_all(pipe)
}

/// Why `bytes`, come on wire `wire`, are wrong.
fn fault(core: &mut Core, inbound: &mut Inbound, bytes: &[u8]) -> &'static str {
    match inbound.take(bytes, core) {
        Err(Fault(why)) => why,
        Ok(_) => panic!("taken"),
    }
}

/// Chunks come back in their numbers' order, whichever wire each came on,
/// however the bytes were cut; the data ends once its end and every chunk
/// before it came.
#[test]
fn chunks_go_back_in_order_and_the_data_ends_with_every_one() {
    let (mut core, mut inbound) = destination_core(2);
    let second = chunk(1, b"second");
    for byte in &second {
        inbound[1].take(&[*byte], &mut core).unwrap();
    }
    assert_eq!(readable(&mut core, DATA), b"");
    inbound[0].take(&chunk(0, b"first,"), &mut core).unwrap();
    inbound[0]
        .take(&frame(DATA, Kind::End, &3u64.to_le_bytes()), &mut core)
        .unwrap();
    assert_eq!(readable(&mut core, DATA), b"first,second");
    assert!(!core.pipes[usize::from(DATA)].complete());
    inbound[1].take(&chunk(2, b"!"), &mut core).unwrap();
    assert_eq!(readable(&mut core, DATA), b"!");
    assert!(core.pipes[usize::from(DATA)].complete());
}

/// A chunk's number comes off the wire: one repeated, past the data's end,
/// past any a sender can number, or further ahead than the window holds, is
/// the far end's fault; so is an end short of the chunks that came.
#[test]
fn a_chunk_numbered_wrong_is_a_fault() {
    let ahead = u64::from(small().data) / (CHUNK_HEADER as u64 + 1) + 1;
    for (chunks, why) in [
        (vec![chunk(0, b"a"), chunk(0, b"a")], "a chunk twice"),
        (vec![chunk(1, b"a"), chunk(1, b"a")], "a chunk twice"),
        (
            vec![frame(DATA, Kind::End, &1u64.to_le_bytes()), chunk(1, b"a")],
            "a chunk past the data's end",
        ),
        (
            vec![chunk(u64::MAX, b"a")],
            "a chunk past any a sender can number",
        ),
        (
            vec![chunk(ahead, b"a")],
            "a chunk further ahead than the window holds",
        ),
        (
            vec![chunk(4, b"a"), frame(DATA, Kind::End, &4u64.to_le_bytes())],
            "the data's end short of the chunks that came",
        ),
    ] {
        let (mut core, mut inbound) = destination_core(1);
        let (last, before) = chunks.split_last().unwrap();
        for bytes in before {
            inbound[0].take(bytes, &mut core).unwrap();
        }
        assert_eq!(fault(&mut core, &mut inbound[0], last), why);
    }
}

/// What a frame says is checked: its pipe, its kind, its length; bytes past
/// a window; more than 16 MiB unread on control; window given back that was
/// never taken; anything but data on a wire but the first; bytes after a
/// pipe's end.
#[test]
fn a_frame_that_says_what_cannot_be_is_a_fault() {
    let mut long = frame(CONTROL, Kind::Bytes, b"");
    long[2..].copy_from_slice(&(MOST as u32 + 1).to_le_bytes());
    // The manifest's window is a frame's most.
    let past_window = vec![frame(MANIFEST, Kind::Bytes, &vec![0; MOST])];
    for (frames, wire, why) in [
        (
            vec![frame(9, Kind::Bytes, b"x")],
            0,
            "a frame of a pipe there is none of",
        ),
        (
            vec![vec![CONTROL, 9, 0, 0, 0, 0]],
            0,
            "a frame of no kind there is",
        ),
        (vec![long], 0, "a frame longer than any is"),
        (
            vec![frame(ANSWERS, Kind::Bytes, b"x")],
            0,
            "bytes on a pipe the other side does not write",
        ),
        (
            [past_window, vec![frame(MANIFEST, Kind::Bytes, b"x")]].concat(),
            0,
            "bytes past a pipe's window",
        ),
        (
            vec![frame(ANSWERS, Kind::Window, &1u32.to_le_bytes())],
            0,
            "more window given back than was taken",
        ),
        (
            vec![frame(MANIFEST, Kind::Window, &1u32.to_le_bytes())],
            0,
            "window given back on a pipe with none of this side's",
        ),
        (
            vec![frame(CONTROL, Kind::Bytes, b"x")],
            1,
            "a frame on a wire but the first, not of data",
        ),
        (
            vec![
                frame(MANIFEST, Kind::End, b""),
                frame(MANIFEST, Kind::Bytes, b"x"),
            ],
            0,
            "bytes on a pipe after its end",
        ),
        (
            vec![frame(ANSWERS, Kind::Synced, &0u32.to_le_bytes())],
            0,
            "the answer to a sync not asked",
        ),
    ] {
        let (mut core, mut inbound) = destination_core(2);
        let (last, before) = frames.split_last().unwrap();
        for bytes in before {
            inbound[wire].take(bytes, &mut core).unwrap();
        }
        assert_eq!(fault(&mut core, &mut inbound[wire], last), why);
    }
    // Control has no window, but a guard.
    let (mut core, mut inbound) = destination_core(1);
    let full = frame(CONTROL, Kind::Bytes, &vec![0; MOST]);
    for _ in 0..CONTROL_GUARD / MOST {
        inbound[0].take(&full, &mut core).unwrap();
    }
    assert_eq!(
        fault(
            &mut core,
            &mut inbound[0],
            &frame(CONTROL, Kind::Bytes, b"x")
        ),
        "more than 16 MiB unread on the control pipe"
    );
    // Read, there is room again.
    readable(&mut core, CONTROL);
    inbound[0].take(&full, &mut core).unwrap();
}

/// A wire that ends inside a frame or a chunk, or the first before every
/// pipe ended both ways, or the last before every chunk came, is broken.
#[test]
fn a_wire_that_ends_early_is_broken() {
    let (mut core, mut inbound) = destination_core(2);
    inbound[0]
        .take(&frame(CONTROL, Kind::Bytes, b"x")[..3], &mut core)
        .unwrap();
    assert_eq!(
        core.wire_ended(0, &inbound[0]),
        Err(Fault("a wire ended inside a frame"))
    );
    let cut = &chunk(0, b"abc")[HEADER..HEADER + 10];
    inbound[1]
        .take(&frame(DATA, Kind::Bytes, cut), &mut core)
        .unwrap();
    assert_eq!(
        core.wire_ended(1, &inbound[1]),
        Err(Fault("a wire ended inside a chunk"))
    );
    let (mut core, inbound) = destination_core(2);
    assert_eq!(
        core.wire_ended(0, &inbound[0]),
        Err(Fault("the stream ended before the other side was done"))
    );
}

/// The first wire's pump sends window given back and syncs first, then
/// control, then answers; of the manifest and data, data while a quarter of
/// its buffer is full, else the manifest first.
#[test]
fn the_first_wire_sends_in_order_of_rank() {
    let mut core = Core::new(&stream(true, Windows::default(), None), 1);
    let queue = |core: &mut Core, pipe: u8, length: usize| {
        let state = &mut core.pipes[usize::from(pipe)];
        state.sending.push_back(Piece {
            bytes: Arc::new(vec![0; length]),
            start: 0,
            end: length,
        });
        state.sending_bytes += length;
    };
    queue(&mut core, MANIFEST, 10);
    queue(&mut core, DATA, 10);
    queue(&mut core, CONTROL, 10);
    core.urgent.push_back(Outgoing::of(
        ANSWERS,
        Kind::Window,
        1u32.to_le_bytes().to_vec(),
    ));
    let mut sending = Sending::default();
    let mut order = Vec::new();
    while let Some(next) = core.next(0, &mut sending) {
        order.push((next.header[0], next.header[1]));
    }
    assert_eq!(
        order,
        [
            (ANSWERS, Kind::Window as u8),
            (CONTROL, Kind::Bytes as u8),
            (MANIFEST, Kind::Bytes as u8),
            (DATA, Kind::Bytes as u8),
        ]
    );
    queue(&mut core, MANIFEST, 10);
    queue(&mut core, DATA, DATA_BUFFER / 4);
    let first = core.next(0, &mut sending).unwrap();
    assert_eq!(first.header[0], DATA);
}

/// Window comes back as the program reads, a quarter at a time; a sync is
/// answered once the program read as far as it says, at once if it had.
#[test]
fn window_comes_back_as_read_and_a_sync_once_read_so_far() {
    let (mut core, mut inbound) = destination_core(1);
    let quarter = small().manifest as usize / 4;
    inbound[0]
        .take(
            &frame(MANIFEST, Kind::Bytes, &vec![0; quarter - 1]),
            &mut core,
        )
        .unwrap();
    let mut sync = 7u32.to_le_bytes().to_vec();
    sync.extend_from_slice(&(quarter as u64).to_le_bytes());
    inbound[0]
        .take(&frame(MANIFEST, Kind::Sync, &sync), &mut core)
        .unwrap();
    readable(&mut core, MANIFEST);
    assert!(core.urgent.is_empty(), "given back or answered too soon");
    inbound[0]
        .take(&frame(MANIFEST, Kind::Bytes, b"x"), &mut core)
        .unwrap();
    readable(&mut core, MANIFEST);
    let sent: Vec<(u8, u8, Vec<u8>)> = core
        .urgent
        .drain(..)
        .map(|frame| {
            let bytes = frame.bytes();
            (bytes[0], bytes[1], bytes[HEADER..].to_vec())
        })
        .collect();
    assert_eq!(
        sent,
        [
            (MANIFEST, Kind::Synced as u8, 7u32.to_le_bytes().to_vec()),
            (
                MANIFEST,
                Kind::Window as u8,
                (quarter as u32).to_le_bytes().to_vec()
            ),
        ]
    );
    // Already read so far: answered at once.
    let mut sync = 8u32.to_le_bytes().to_vec();
    sync.extend_from_slice(&1u64.to_le_bytes());
    inbound[0]
        .take(&frame(MANIFEST, Kind::Sync, &sync), &mut core)
        .unwrap();
    assert_eq!(core.urgent.len(), 1);
}

/// All is drained only once each pipe's end, and the last frame of each
/// chunk, is written on the wire, not as soon as a pump takes it: a server
/// says it is done only then, and the other side may let go of the
/// connection as soon as it hears so.
#[test]
fn drained_once_written_not_once_taken() {
    let mut core = Core::new(&stream(true, Windows::default(), None), 1);
    let state = &mut core.pipes[usize::from(DATA)];
    state.sending.push_back(Piece {
        bytes: Arc::new(vec![1; 10]),
        start: 0,
        end: 10,
    });
    state.sending_bytes = 10;
    for pipe in &mut core.pipes {
        pipe.closed = true;
    }
    let mut sending = Sending::default();
    let mut taken = Vec::new();
    while let Some(frame) = core.next(0, &mut sending) {
        taken.push(Carrying::of(&frame, &sending));
        assert!(!core.drained(), "drained with frames not written");
    }
    for carried in taken {
        assert!(!core.drained());
        core.written(carried);
    }
    assert!(core.drained());
}

/// A server's control connection, the server's end and the client's, on a
/// socket pair, its reports' window `reports`.
fn control_pair(reports: u32) -> (Connection, Connection) {
    let (one, other) = UnixStream::pair().unwrap();
    let scope = Scope::new().unwrap();
    (
        Connection::open(
            vec![Wire::socket(one, &scope).unwrap()],
            &control_connection(true, reports, None),
        )
        .unwrap(),
        Connection::open(
            vec![Wire::socket(other, &scope).unwrap()],
            &control_connection(false, reports, None),
        )
        .unwrap(),
    )
}

/// On a control connection, reports waiting for window, as a dry run's
/// listing does while the client shows it, hold back nothing on control:
/// the metrics and the error still come.
#[test]
fn reports_waiting_hold_back_neither_metrics_nor_errors() {
    within(
        "reports waiting hold back neither metrics nor errors",
        PATIENCE,
        || {
            let (server, mut client) = control_pair(64 * 1024);
            let reports = server.writer(REPORTS);
            let control = server.writer(CONTROL);
            let stuck = AtomicBool::new(false);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let _ = reports.write_all(&pattern(4 << 20, 10));
                    stuck.store(true, Ordering::SeqCst);
                });
                std::thread::sleep(Duration::from_millis(100));
                for said in [&b"metrics"[..], b"metrics", b"error"] {
                    control.write_all(said).unwrap();
                }
                let mut read = [0; 19];
                client.reader(CONTROL).read_exact(&mut read).unwrap();
                assert_eq!(&read, b"metricsmetricserror");
                assert!(!stuck.load(Ordering::SeqCst), "the reports did not wait");
                drop(client);
            });
        },
    );
}

/// The server's being done, on control, may come before its last reports:
/// the client reads each pipe to its end, and has every report.
#[test]
fn done_before_the_last_reports_leaves_none_unread() {
    within(
        "done before the last reports leaves none unread",
        PATIENCE,
        || {
            let (server, mut client) = control_pair(64 * 1024);
            let reports = pattern(1 << 20, 11);
            let (mut control_in, reports_in) = (client.reader(CONTROL), client.reader(REPORTS));
            std::thread::scope(|scope| {
                let reporting = scope.spawn(|| server.writer(REPORTS).write_all(&reports).unwrap());
                let writing = scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(50));
                    server.writer(CONTROL).write_all(b"done").unwrap();
                    reporting.join().unwrap();
                    server.finish().unwrap();
                });
                // Done comes while most reports wait for window.
                let mut done = [0; 4];
                control_in.read_exact(&mut done).unwrap();
                assert_eq!(&done, b"done");
                assert_eq!(all(reports_in).unwrap(), reports);
                writing.join().unwrap();
                assert_eq!(control_in.read(&mut [0; 1]).unwrap(), 0);
            });
            client.finish().unwrap();
        },
    );
}

/// Each wire's pump takes chunks of the data of its own, whichever is free:
/// the data goes on every wire, each chunk whole on one.
#[test]
fn every_wire_takes_chunks_of_the_data() {
    let mut core = Core::new(&stream(true, Windows::default(), None), 3);
    let state = &mut core.pipes[usize::from(DATA)];
    state.sending.push_back(Piece {
        bytes: Arc::new(vec![1; 3 * CHUNK]),
        start: 0,
        end: 3 * CHUNK,
    });
    state.sending_bytes = 3 * CHUNK;
    let mut sendings: Vec<Sending> = (0..3).map(|_| Sending::default()).collect();
    for (wire, sending) in sendings.iter_mut().enumerate() {
        let frame = core.next(wire, sending).expect("a chunk for each wire");
        assert_eq!(frame.header[0], DATA);
        let number = u64::from_le_bytes(frame.bytes()[HEADER..HEADER + 8].try_into().unwrap());
        assert_eq!(number, wire as u64);
    }
    assert_eq!(core.pipes[usize::from(DATA)].sending_bytes, 0);
}
