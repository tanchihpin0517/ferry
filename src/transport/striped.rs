//! Messages over several paths: each message is a piece, numbered in the
//! order it was sent, and goes to the path with the shortest queue; the far
//! side puts the pieces back in order.
//!
//! On each path, before any piece, a header says how many paths the
//! connection has, so that the far side knows how many to wait for. A piece
//! is its number, eight bytes little-endian, then the message as a packet.
//!
//! Memory stays bounded on both sides. Each path's queue is short, so a
//! sender faster than every path waits. The receiver holds only so many
//! pieces that came early; beyond that, a path's reader waits, unless its
//! piece is the next one due. That one is always taken, and each path's
//! pieces come in order, so the next piece due is always at the head of some
//! path: the receiver never waits for a piece behind a waiting reader.

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};

use crate::protocol::{self, Packet};

/// How many pieces may wait to go out on one path.
const PATH_QUEUE: usize = 8;

/// How many pieces that came early the receiver holds.
const EARLY: usize = 256;

/// Writes the header that starts a path: how many paths there are.
pub fn write_header(writer: &mut impl Write, paths: usize) -> Result<()> {
    let paths = u32::try_from(paths).context("too many paths")?;
    writer.write_all(&paths.to_le_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Reads the header that starts a path.
pub fn read_header(reader: &mut impl Read) -> Result<usize> {
    let mut paths = [0u8; 4];
    reader
        .read_exact(&mut paths)
        .context("read the start of a path")?;
    match u32::from_le_bytes(paths) {
        0 => bail!("a path says its connection has no paths"),
        paths => Ok(paths as usize),
    }
}

/// The sending side of a connection over `writers`, one for each path.
pub fn sending<W: Write + Send + 'static>(writers: Vec<W>) -> Sending {
    let paths = writers
        .into_iter()
        .map(|writer| {
            let (queue, pieces) = mpsc::sync_channel::<(u64, Packet)>(PATH_QUEUE);
            let waiting = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&waiting);
            let thread = std::thread::spawn(move || -> Result<()> {
                let mut writer = BufWriter::new(writer);
                for (number, packet) in pieces {
                    writer.write_all(&number.to_le_bytes())?;
                    protocol::forward(&mut writer, &packet)?;
                    // What waits is written before the path would idle.
                    if counted.fetch_sub(1, Ordering::Relaxed) == 1 {
                        writer.flush()?;
                    }
                }
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
    queue: SyncSender<(u64, Packet)>,
    /// Pieces handed to the path and not yet written.
    waiting: Arc<AtomicUsize>,
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
            .min_by_key(|path| path.waiting.load(Ordering::Relaxed))
            .expect("a connection has a path");
        path.waiting.fetch_add(1, Ordering::Relaxed);
        path.queue
            .send((number, packet))
            .map_err(|_| anyhow!("a path closed"))
    }

    /// Closes every path once what waits on it is written, and says whether
    /// it all was.
    pub fn close(self) -> Result<()> {
        let mut result = Ok(());
        for Path { queue, thread, .. } in self.paths {
            drop(queue);
            let written = thread.join().expect("a path's writer panicked");
            if result.is_ok() {
                result = written;
            }
        }
        result
    }
}

/// The receiving side of a connection over `readers`, one for each path.
pub fn receiving<R: Read + Send + 'static>(readers: Vec<R>) -> Receiving {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            early: BTreeMap::new(),
            next: 0,
            open: readers.len(),
            failed: None,
            draining: false,
        }),
        changed: Condvar::new(),
    });
    let threads = readers
        .into_iter()
        .map(|reader| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || read_path(BufReader::new(reader), &shared))
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
    state: Mutex<State>,
    /// Rung when a piece comes, a path ends, or one is taken.
    changed: Condvar,
}

struct State {
    /// Pieces that came before the next one due, by number.
    early: BTreeMap<u64, Packet>,
    /// The number of the next piece due.
    next: u64,
    /// Paths not yet ended.
    open: usize,
    failed: Option<String>,
    /// Pieces are no longer wanted, only read.
    draining: bool,
}

/// Reads one path's pieces until it ends, handing each to the receiver.
fn read_path(mut reader: impl Read, shared: &Shared) {
    let ended = loop {
        let mut number = [0u8; 8];
        match read_fully(&mut reader, &mut number) {
            Ok(false) => break Ok(()),
            Ok(true) => {}
            Err(error) => break Err(error),
        }
        let number = u64::from_le_bytes(number);
        let packet = match protocol::receive(&mut reader) {
            Ok(Some(packet)) => packet,
            Ok(None) => break Err(anyhow!("a path ended inside a piece")),
            Err(error) => break Err(error),
        };
        let mut state = shared.state.lock().expect("not poisoned");
        while !state.draining && number != state.next && state.early.len() >= EARLY {
            state = shared.changed.wait(state).expect("not poisoned");
        }
        if !state.draining {
            state.early.insert(number, packet);
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

impl Receiving {
    /// The next message, in the order sent, or `None` once every path has
    /// ended with nothing missing.
    pub fn recv(&mut self) -> Result<Option<Packet>> {
        let mut state = self.shared.state.lock().expect("not poisoned");
        loop {
            let next = state.next;
            if let Some(packet) = state.early.remove(&next) {
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
            state = self.shared.changed.wait(state).expect("not poisoned");
        }
    }

    /// Whether the next message is already here, so that receiving it would
    /// not wait.
    pub fn ready(&self) -> bool {
        let state = self.shared.state.lock().expect("not poisoned");
        state.early.contains_key(&state.next)
    }

    /// Reads whatever is left on every path, without keeping it, so that the
    /// far end never waits to write.
    pub fn drain(mut self) {
        self.stop();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }

    fn stop(&self) {
        let mut state = self.shared.state.lock().expect("not poisoned");
        state.draining = true;
        state.early.clear();
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
    }

    #[test]
    fn a_path_cut_short_is_an_error_not_an_end() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        // Piece 1 alone: piece 0 never comes.
        writer.write_all(&1u64.to_le_bytes()).unwrap();
        protocol::send_data(&mut writer, b"late").unwrap();
        drop(writer);
        let mut receiving = receiving(vec![reader]);
        assert!(receiving.recv().is_err());
    }

    #[test]
    fn the_header_says_how_many_paths() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, 3).unwrap();
        assert_eq!(read_header(&mut bytes.as_slice()).unwrap(), 3);
        assert!(read_header(&mut [0u8, 0, 0, 0].as_slice()).is_err());
    }
}
