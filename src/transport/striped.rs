//! Messages over several paths: each message is a piece, numbered in the
//! order it was sent, and goes to the path with the shortest queue; the far
//! side puts the pieces back in order.
//!
//! On each path, before any piece, a header says how many paths the
//! connection has, so that the far side knows how many to wait for, and
//! whether this one is a path or a heartbeat channel; see
//! [`super::heartbeat`]. A piece is its number, eight bytes little-endian,
//! then the message as a packet. After its last piece, a path ends with an
//! end marker, [`END`] in place of a number: a path that ends without it was
//! cut short, as when its SSH connection dropped, even if it was cut between
//! two pieces, and fails the connection.
//!
//! Memory stays bounded on both sides. Each path's queue is short, so a
//! sender faster than every path waits. The receiver holds only so many
//! pieces that came early, and so many bytes of them; beyond that, a path's
//! reader waits, unless its piece is the next one due. That one is always taken, and each path's
//! pieces come in order, so the next piece due is always at the head of some
//! path: the receiver never waits for a piece behind a waiting reader.

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::Sender;

use crate::profile::{self, Gauge};
use crate::protocol::{self, Packet};

/// How many pieces may wait to go out on one path.
const PATH_QUEUE: usize = 8;

/// How many pieces that came early the receiver holds.
const EARLY: usize = 256;

/// How many bytes of pieces that came early the receiver holds, whatever
/// their number: a piece may be 16 MiB, and 256 of them 4 GiB. A data
/// packet is a frame, some 2 MiB at most, so this is some 28 of them; one
/// piece is held however large, if none other is.
const EARLY_BYTES: usize = if cfg!(test) { 1 << 20 } else { 64 << 20 };

/// The end marker, written after a path's last piece in place of a number.
pub const END: u64 = u64::MAX;

/// What joins the far side: a path, a heartbeat channel, or the channel the
/// control connection goes on once the server has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Path,
    Heartbeat,
    Control,
}

/// Writes the header that starts a path or a heartbeat channel: how many
/// paths there are, and which this is.
pub fn write_header(writer: &mut impl Write, paths: usize, kind: Kind) -> Result<()> {
    let paths = u32::try_from(paths).context("too many paths")?;
    writer.write_all(&paths.to_le_bytes())?;
    writer.write_all(&[match kind {
        Kind::Path => 0,
        Kind::Heartbeat => 1,
        Kind::Control => 2,
    }])?;
    writer.flush()?;
    Ok(())
}

/// Reads the header that starts a path or a heartbeat channel.
pub fn read_header(reader: &mut impl Read) -> Result<(usize, Kind)> {
    let mut header = [0u8; 5];
    reader
        .read_exact(&mut header)
        .context("read the start of a path")?;
    let paths = match u32::from_le_bytes(header[..4].try_into().expect("four bytes")) {
        0 => bail!("a path says its connection has no paths"),
        paths => paths as usize,
    };
    let kind = match header[4] {
        0 => Kind::Path,
        1 => Kind::Heartbeat,
        2 => Kind::Control,
        other => bail!("a path says it is of kind {other}"),
    };
    Ok((paths, kind))
}

/// A gauge's name: `name`, after `label` if there is one.
fn gauged(label: &str, name: &str) -> String {
    if label.is_empty() {
        name.to_owned()
    } else {
        format!("{label} {name}")
    }
}

/// The sending side of a connection over `writers`, one for each path.
pub fn sending<W: Write + Send + 'static>(writers: Vec<W>) -> Sending {
    sending_as(writers, "", false)
}

/// As [`sending`], with each path's queue gauged under a name that starts
/// with `label`, such as which connection it is; and when `cut`, the last
/// path is cut short before its first piece, as a dropped connection would
/// cut it: for `ferry test`.
pub fn sending_as<W: Write + Send + 'static>(writers: Vec<W>, label: &str, cut: bool) -> Sending {
    let last = writers.len().saturating_sub(1);
    let paths = writers
        .into_iter()
        .enumerate()
        .map(|(index, writer)| {
            let cut = cut && index == last;
            let (queue, pieces) = crossbeam_channel::bounded::<(u64, Packet)>(PATH_QUEUE);
            let waiting = Gauge::new(
                gauged(label, &format!("send path {}", index + 1)),
                PATH_QUEUE,
            );
            let counted = waiting.clone();
            let thread = std::thread::spawn(move || -> Result<()> {
                let _thread = profile::thread("path writer");
                let mut writer = BufWriter::new(writer);
                let mut pieces = pieces.into_iter();
                while let Some((number, packet)) =
                    profile::wait(profile::State::Work, || pieces.next())
                {
                    let _writing = profile::enter(profile::State::WriteConnection);
                    if cut {
                        // Ends the path with no end marker, losing this piece.
                        return Ok(());
                    }
                    writer.write_all(&number.to_le_bytes())?;
                    protocol::forward(&mut writer, &packet)?;
                    // What waits is written before the path would idle.
                    if counted.down() == 0 {
                        writer.flush()?;
                    }
                }
                writer.write_all(&END.to_le_bytes())?;
                writer.flush()?;
                Ok(())
            });
            Path {
                queue,
                waiting,
                thread,
            }
        })
        .collect();
    Sending { paths, next: 0 }
}

/// The sending half of a connection. Dropping it closes every path, once
/// what waits on them is written.
pub struct Sending {
    paths: Vec<Path>,
    next: u64,
}

struct Path {
    queue: Sender<(u64, Packet)>,
    /// Pieces handed to the path and not yet written.
    waiting: Gauge,
    thread: JoinHandle<Result<()>>,
}

impl Sending {
    /// Sends a message, numbered, on the path with the shortest queue. Waits
    /// while that queue is full.
    pub fn send(&mut self, packet: Packet) -> Result<()> {
        let number = self.next;
        self.next += 1;
        let path = self
            .paths
            .iter()
            .min_by_key(|path| path.waiting.level())
            .expect("a connection has a path");
        path.waiting.up();
        profile::wait(profile::State::WriteConnection, || {
            path.queue.send((number, packet))
        })
        .map_err(|_| anyhow!("a path closed"))
    }

    /// Closes every path once what waits on it is written, and says whether
    /// it all was.
    pub fn close(self) -> Result<()> {
        let mut result = Ok(());
        for Path { queue, thread, .. } in self.paths {
            drop(queue);
            let written = thread
                .join()
                .unwrap_or_else(|_| Err(anyhow!("internal error in a path's writer")));
            if result.is_ok() {
                result = written;
            }
        }
        result
    }
}

/// The receiving side of a connection over `readers`, one for each path.
pub fn receiving<R: Read + Send + 'static>(readers: Vec<R>) -> Receiving {
    receiving_as(readers, "")
}

/// As [`receiving`], with the pieces received and not yet taken, which wait
/// for the ones before them or to be taken, gauged under a name that starts
/// with `label`, such as which connection it is.
pub fn receiving_as<R: Read + Send + 'static>(readers: Vec<R>, label: &str) -> Receiving {
    receiving_failed_as(readers, label, None)
}

/// As [`receiving_as`], failed with `failed` if given, before any path is
/// read: what a path then says, such as that it ended, does not stand for
/// why.
pub fn receiving_failed_as<R: Read + Send + 'static>(
    readers: Vec<R>,
    label: &str,
    failed: Option<String>,
) -> Receiving {
    let shared = Arc::new(Shared {
        early_gauge: Gauge::new(gauged(label, "receive"), EARLY),
        state: Mutex::new(State {
            early: BTreeMap::new(),
            early_bytes: 0,
            waiting: Vec::new(),
            next: 0,
            open: readers.len(),
            failed,
            draining: false,
        }),
        changed: Condvar::new(),
    });
    let threads = readers
        .into_iter()
        .map(|reader| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _thread = profile::thread("path reader");
                read_path(BufReader::new(reader), &shared)
            })
        })
        .collect();
    Receiving { shared, threads }
}

/// The receiving half of a connection.
pub struct Receiving {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

struct Shared {
    /// How many pieces are received and not yet taken.
    early_gauge: Gauge,
    state: Mutex<State>,
    /// Rung when a piece comes, a path ends, or one is taken.
    changed: Condvar,
}

struct State {
    /// Pieces that came before the next one due, by number.
    early: BTreeMap<u64, Packet>,
    /// How many bytes they hold.
    early_bytes: usize,
    /// The numbers of the pieces at the head of the paths whose readers
    /// wait for room.
    waiting: Vec<u64>,
    /// The number of the next piece due.
    next: u64,
    /// Paths not yet ended.
    open: usize,
    failed: Option<String>,
    /// Pieces are no longer wanted, only read.
    draining: bool,
}

/// Reads one path's pieces until its end marker, handing each to the
/// receiver.
fn read_path(mut reader: impl Read, shared: &Shared) {
    let ended = loop {
        let mut number = [0u8; 8];
        let reading = profile::enter(profile::State::ReadConnection);
        match read_fully(&mut reader, &mut number) {
            Ok(false) => break Err(anyhow!("a path ended before its end")),
            Ok(true) => {}
            Err(error) => break Err(error),
        }
        let number = u64::from_le_bytes(number);
        if number == END {
            break Ok(());
        }
        let packet = match protocol::receive(&mut reader) {
            Ok(Some(packet)) => packet,
            Ok(None) => break Err(anyhow!("a path ended inside a piece")),
            Err(error) => break Err(error),
        };
        drop(reading);
        let size = packet.size();
        let full = |state: &State| {
            !state.draining
                && number != state.next
                && !state.early.is_empty()
                && (state.early.len() >= EARLY || state.early_bytes + size > EARLY_BYTES)
        };
        let mut state = shared.state.lock().expect("not poisoned");
        if full(&state) {
            let _waiting = profile::enter(profile::State::Receiver);
            // The receiver is told, as it may then know that the next piece
            // can never come.
            state.waiting.push(number);
            shared.changed.notify_all();
            while full(&state) {
                state = shared.changed.wait(state).expect("not poisoned");
            }
            let at = state
                .waiting
                .iter()
                .position(|&waiting| waiting == number)
                .expect("pushed");
            state.waiting.swap_remove(at);
        }
        if !state.draining {
            state.early.insert(number, packet);
            state.early_bytes += size;
            shared.early_gauge.set(state.early.len());
        }
        shared.changed.notify_all();
    };
    let mut state = shared.state.lock().expect("not poisoned");
    state.open -= 1;
    if let Err(error) = ended {
        state.failed.get_or_insert(format!("{error:#}"));
    }
    shared.changed.notify_all();
}

/// Fills `buffer`, or says there was nothing more to read.
fn read_fully(reader: &mut impl Read, buffer: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => bail!("a path ended inside a piece"),
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("read a path"),
        }
    }
    Ok(true)
}

/// Fails a connection's receiving half from outside, as the heartbeats do.
#[derive(Clone)]
pub struct Failer(Arc<Shared>);

impl Failer {
    /// Fails the connection with `message`, unless it failed already: what
    /// is not here yet will not come.
    pub fn fail(&self, message: String) {
        let mut state = self.0.state.lock().expect("not poisoned");
        state.failed.get_or_insert(message);
        self.0.changed.notify_all();
    }
}

impl Receiving {
    /// A way to fail this connection from another thread.
    pub fn failer(&self) -> Failer {
        Failer(Arc::clone(&self.shared))
    }

    /// The next message, in the order sent, or `None` once every path has
    /// ended with nothing missing.
    pub fn recv(&mut self) -> Result<Option<Packet>> {
        let mut state = self.shared.state.lock().expect("not poisoned");
        loop {
            let next = state.next;
            if let Some(packet) = state.early.remove(&next) {
                state.early_bytes -= packet.size();
                self.shared.early_gauge.set(state.early.len());
                state.next += 1;
                self.shared.changed.notify_all();
                return Ok(Some(packet));
            }
            if let Some(failed) = &state.failed {
                bail!("{failed}");
            }
            if state.open == 0 {
                if state.early.is_empty() {
                    return Ok(None);
                }
                bail!("the paths ended with piece {next} missing");
            }
            // Each path's pieces come in order, so one whose reader waits
            // with another at its head never brings the next: with every
            // open path so, it never comes. One whose head the next has
            // become since will bring it, once it is woken.
            if state.waiting.len() == state.open && !state.waiting.contains(&next) {
                bail!("piece {next} can never come: every path waits behind it");
            }
            state = profile::wait(profile::State::ReadConnection, || {
                self.shared.changed.wait(state).expect("not poisoned")
            });
        }
    }

    /// Whether the next message is already here, so that receiving it would
    /// not wait.
    pub fn ready(&self) -> bool {
        let state = self.shared.state.lock().expect("not poisoned");
        state.early.contains_key(&state.next)
    }

    /// Reads whatever is left on every path, without keeping it, so that the
    /// far end never waits to write. The readers go on in the background,
    /// until their paths end: one whose SSH connection is gone may never end,
    /// since its master holds its pipes.
    pub fn drain(mut self) {
        self.stop();
        self.threads.clear();
    }

    fn stop(&self) {
        let mut state = self.shared.state.lock().expect("not poisoned");
        state.draining = true;
        state.early.clear();
        state.early_bytes = 0;
        self.shared.changed.notify_all();
    }
}

impl Drop for Receiving {
    fn drop(&mut self) {
        // Readers left waiting for room would wait forever.
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::PipeReader;
    use std::time::Duration;

    fn packet(bytes: &[u8]) -> Packet {
        let encoded = protocol::encode_data(bytes).unwrap();
        protocol::receive(&mut encoded.as_slice()).unwrap().unwrap()
    }

    /// A reader that sleeps before each read: a slow path.
    struct Slow(PipeReader);

    impl Read for Slow {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(2));
            self.0.read(buffer)
        }
    }

    #[test]
    fn messages_come_out_in_order_over_paths_one_of_them_slow() {
        crate::test_support::within(
            "messages come out in order over paths one of them slow",
            crate::test_support::PATIENCE,
            move || {
                let mut readers: Vec<Box<dyn Read + Send>> = Vec::new();
                let mut writers = Vec::new();
                for path in 0..4 {
                    let (reader, writer) = std::io::pipe().unwrap();
                    if path == 1 {
                        readers.push(Box::new(Slow(reader)));
                    } else {
                        readers.push(Box::new(reader));
                    }
                    writers.push(writer);
                }
                let mut sending = sending(writers);
                let mut receiving = receiving(readers);
                let sent: Vec<Vec<u8>> = (0..2_000u32)
                    .map(|index| vec![index as u8; (index as usize * 389) % 5_000])
                    .collect();
                let expected = sent.clone();
                let sender = std::thread::spawn(move || {
                    for bytes in &sent {
                        sending.send(packet(bytes)).unwrap();
                    }
                    sending.close().unwrap();
                });
                let mut received = Vec::new();
                while let Some(packet) = receiving.recv().unwrap() {
                    received.push(packet.into_payload());
                }
                sender.join().unwrap();
                assert_eq!(received, expected);
            },
        );
    }

    /// Behind a path that holds back the next piece due, the receiver holds
    /// no more bytes of what came early than its budget, however few the
    /// pieces; then, the piece in, all come in order.
    #[test]
    fn what_comes_early_is_held_to_a_budget_of_bytes() {
        crate::test_support::within(
            "what comes early is held to a budget of bytes",
            crate::test_support::PATIENCE,
            move || {
                const PIECE: usize = 100_000;
                let (held, mut held_writer) = std::io::pipe().unwrap();
                let (fast, mut fast_writer) = std::io::pipe().unwrap();
                let pieces: Vec<Vec<u8>> = (0..40u8).map(|index| vec![index; PIECE]).collect();
                let early = pieces.clone();
                let writer = std::thread::spawn(move || {
                    for (number, bytes) in early.iter().enumerate().skip(1) {
                        fast_writer
                            .write_all(&(number as u64).to_le_bytes())
                            .unwrap();
                        protocol::send_data(&mut fast_writer, bytes).unwrap();
                    }
                    fast_writer.write_all(&END.to_le_bytes()).unwrap();
                });
                let mut receiving = receiving(vec![held, fast]);
                // Until the fast path's reader waits, at its budget.
                let mut most = 0;
                for _ in 0..100 {
                    std::thread::sleep(Duration::from_millis(10));
                    most = most.max(receiving.shared.state.lock().unwrap().early_bytes);
                }
                assert!(most <= EARLY_BYTES, "{most} bytes held");
                assert!(most > EARLY_BYTES - 2 * PIECE, "{most} bytes held");
                held_writer.write_all(&0u64.to_le_bytes()).unwrap();
                protocol::send_data(&mut held_writer, &pieces[0]).unwrap();
                held_writer.write_all(&END.to_le_bytes()).unwrap();
                let mut received = Vec::new();
                while let Some(packet) = receiving.recv().unwrap() {
                    received.push(packet.into_payload());
                }
                writer.join().unwrap();
                assert_eq!(received, pieces);
            },
        );
    }

    /// Pieces of any size, spread over up to four paths in any way, each
    /// path's in order, as a sender spreads them, come out in order.
    #[test]
    fn pieces_spread_any_way_come_out_in_order() {
        let mut random = crate::test_support::Random::new(17);
        for _ in 0..50 {
            let paths = 1 + random.below(4) as usize;
            let count = random.below(300) as usize;
            let mut spread = vec![Vec::new(); paths];
            let mut sent = Vec::new();
            for number in 0..count as u64 {
                let length = random.below(3_000) as usize;
                let payload = random.bytes(length);
                let path = &mut spread[random.below(paths as u64) as usize];
                path.extend_from_slice(&number.to_le_bytes());
                protocol::send_data(path, &payload).unwrap();
                sent.push(payload);
            }
            let readers = spread
                .into_iter()
                .map(|mut path| {
                    path.extend_from_slice(&END.to_le_bytes());
                    std::io::Cursor::new(path)
                })
                .collect();
            let mut receiving = receiving(readers);
            let mut received = Vec::new();
            while let Some(packet) = receiving.recv().unwrap() {
                received.push(packet.into_payload());
            }
            assert_eq!(received, sent);
        }
    }

    /// Behind a piece that never comes, as a far side gone wrong may hold
    /// back, the receiver fails once every path still open waits for room,
    /// rather than wait with them for ever.
    #[test]
    fn a_piece_that_can_never_come_fails_the_connection() {
        crate::test_support::within(
            "a piece that can never come fails the connection",
            Duration::from_secs(10),
            move || {
                let mut ahead = Vec::new();
                for number in 1..=300u64 {
                    ahead.extend_from_slice(&number.to_le_bytes());
                    protocol::send_data(&mut ahead, b"early").unwrap();
                }
                ahead.extend_from_slice(&END.to_le_bytes());
                let ended = END.to_le_bytes().to_vec();
                let mut receiving = receiving(vec![
                    std::io::Cursor::new(ahead),
                    std::io::Cursor::new(ended),
                ]);
                let Err(error) = receiving.recv() else {
                    panic!("a piece that never comes must fail the connection");
                };
                assert!(error.to_string().contains("piece 0"), "{error}");
            },
        );
    }

    #[test]
    fn a_path_cut_short_is_an_error_not_an_end() {
        crate::test_support::within(
            "a path cut short is an error not an end",
            crate::test_support::PATIENCE,
            move || {
                let (reader, mut writer) = std::io::pipe().unwrap();
                // Piece 1 alone: piece 0 never comes.
                writer.write_all(&1u64.to_le_bytes()).unwrap();
                protocol::send_data(&mut writer, b"late").unwrap();
                drop(writer);
                let mut receiving = receiving(vec![reader]);
                assert!(receiving.recv().is_err());
            },
        );
    }

    #[test]
    fn a_path_cut_between_pieces_is_an_error_not_an_end() {
        crate::test_support::within(
            "a path cut between pieces is an error not an end",
            crate::test_support::PATIENCE,
            move || {
                // Two paths: the first ends properly, the second is cut after its
                // only piece, with no end marker, and piece 1, which it would have
                // carried next, never comes. Nothing is missing yet when it ends.
                let (first, mut first_writer) = std::io::pipe().unwrap();
                let (second, mut second_writer) = std::io::pipe().unwrap();
                second_writer.write_all(&0u64.to_le_bytes()).unwrap();
                protocol::send_data(&mut second_writer, b"zero").unwrap();
                drop(second_writer);
                let mut receiving = receiving(vec![first, second]);
                assert_eq!(receiving.recv().unwrap().unwrap().into_payload(), b"zero");
                let Err(error) = receiving.recv() else {
                    panic!("a path cut short must fail the connection");
                };
                assert!(error.to_string().contains("before its end"), "{error}");
                first_writer.write_all(&END.to_le_bytes()).unwrap();
            },
        );
    }

    #[test]
    fn a_cut_path_ends_without_its_marker() {
        crate::test_support::within(
            "a cut path ends without its marker",
            crate::test_support::PATIENCE,
            move || {
                let (reader, writer) = std::io::pipe().unwrap();
                let mut sending = sending_as(vec![writer], "", true);
                let mut receiving = receiving(vec![reader]);
                let _ = sending.send(packet(b"lost"));
                let _ = sending.close();
                assert!(receiving.recv().is_err());
            },
        );
    }

    #[test]
    fn a_failer_fails_a_receiver_waiting_for_more() {
        crate::test_support::within(
            "a failer fails a receiver waiting for more",
            crate::test_support::PATIENCE,
            move || {
                let (reader, _writer) = std::io::pipe().unwrap();
                let mut receiving = receiving(vec![reader]);
                let failer = receiving.failer();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    failer.fail("gone".to_owned());
                });
                let Err(error) = receiving.recv() else {
                    panic!("a failed connection must fail");
                };
                assert_eq!(error.to_string(), "gone");
            },
        );
    }

    #[test]
    fn the_header_says_how_many_paths_and_which_kind() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, 3, Kind::Heartbeat).unwrap();
        assert_eq!(
            read_header(&mut bytes.as_slice()).unwrap(),
            (3, Kind::Heartbeat)
        );
        assert!(read_header(&mut [0u8, 0, 0, 0, 0].as_slice()).is_err());
        assert!(read_header(&mut [1u8, 0, 0, 0, 7].as_slice()).is_err());
    }
}
