//! Where a peer's threads write what they send the other peer: each packet
//! whole, on the pipe its kind goes on, as [`Mux`] routes it. Until the
//! pipes carry records rather than packets, the packets are the peers' own:
//!
//! - **control**, both ways: an error, which never waits;
//! - **manifest**, the Source's: the manifest, plain or compressed;
//! - **data**, the Source's: the blocks of files, in packets of at most
//!   [`DATA_PACKET`] bytes, the hashes of files sent in segments, what the
//!   owner could not open, and how many blocks there were;
//! - **answers**, the Destination's: needs, results, and that it is done.
//!
//! How each pipe is sent, and when its writer waits, is the transport's;
//! see [`crate::transport::pipes`].
//!
//! [`Queues`] is the one writer of a server's control connection, until it
//! is on pipes too: what the peer reports goes on it, in two queues, each
//! written in the order it was handed over: **messages**, few and small;
//! and **paced messages**, a dry run's listing, which wait for room once
//! [`PACED_BYTES`] wait to be written. Messages go first.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::profile::{self, State};
use crate::protocol::{self, Control, Message, PacketType};
use crate::report::Gauge;
use crate::stop::Stopped;
use crate::transport::pipes::{PipeWriter, Stopper};

/// The most bytes of blocks in one data packet.
pub const DATA_PACKET: usize = 64 * 1024;

/// Where the packets of one pipe are written, each whole, whichever thread
/// writes it.
pub trait Out: Send + Sync {
    fn write_all(&self, bytes: &[u8]) -> io::Result<()>;
}

impl Out for PipeWriter {
    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        PipeWriter::write_all(self, bytes)
    }
}

/// Packets handed on to whoever writes them, as for one stream of packets
/// rather than pipes: as the tests and the fuzzer run a peer.
impl Out for Sender<Vec<u8>> {
    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.send(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "no one writes the packets"))
    }
}

/// A pipe's bytes as they come, packet by packet, or why they stopped: for
/// a peer run over one stream of packets rather than pipes, as the tests and
/// the fuzzer run one.
pub struct Piped {
    packets: Receiver<io::Result<Vec<u8>>>,
    packet: Vec<u8>,
    at: usize,
}

impl Piped {
    pub fn new(packets: Receiver<io::Result<Vec<u8>>>) -> Self {
        Self {
            packets,
            packet: Vec::new(),
            at: 0,
        }
    }
}

impl Read for Piped {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        while self.at == self.packet.len() {
            match self.packets.recv() {
                Ok(packet) => (self.packet, self.at) = (packet?, 0),
                Err(_) => return Ok(0),
            }
        }
        let taken = into.len().min(self.packet.len() - self.at);
        into[..taken].copy_from_slice(&self.packet[self.at..self.at + taken]);
        self.at += taken;
        Ok(taken)
    }
}

/// Where a peer's threads write their packets, each on the pipe its kind
/// goes on: clones write the same pipes.
#[derive(Clone)]
pub struct Mux(Arc<Routes>);

struct Routes {
    control: Box<dyn Out>,
    /// The Destination's.
    answers: Option<Box<dyn Out>>,
    /// The Source's.
    manifest: Option<Box<dyn Out>>,
    data: Option<Box<dyn Out>>,
    /// How much of the data waits to be sent, for the dashboard.
    data_gauge: Option<Gauge>,
    /// Each stops reading one of the pipes the peer reads, as it stops.
    stoppers: Vec<Stopper>,
}

impl Mux {
    /// The Source's: control, the manifest and the data, and how full the
    /// data's pipe is; `stoppers` stop reading what it reads.
    pub fn for_source(
        control: impl Out + 'static,
        manifest: impl Out + 'static,
        data: impl Out + 'static,
        data_gauge: Option<Gauge>,
        stoppers: Vec<Stopper>,
    ) -> Self {
        Self(Arc::new(Routes {
            control: Box::new(control),
            answers: None,
            manifest: Some(Box::new(manifest)),
            data: Some(Box::new(data)),
            data_gauge,
            stoppers,
        }))
    }

    /// The Destination's: control and the answers; `stoppers` stop reading
    /// what it reads.
    pub fn for_destination(
        control: impl Out + 'static,
        answers: impl Out + 'static,
        stoppers: Vec<Stopper>,
    ) -> Self {
        Self(Arc::new(Routes {
            control: Box::new(control),
            answers: Some(Box::new(answers)),
            manifest: None,
            data: None,
            data_gauge: None,
            stoppers,
        }))
    }

    /// The pipe `kind` goes on.
    fn route(&self, kind: PacketType) -> Result<&dyn Out> {
        let routes = &*self.0;
        let pipe = match kind {
            PacketType::Control => Some(&routes.control),
            PacketType::Needs | PacketType::Results => routes.answers.as_ref(),
            PacketType::Manifest | PacketType::ManifestStream => routes.manifest.as_ref(),
            PacketType::Data | PacketType::FileHash | PacketType::BlocksEnd => routes.data.as_ref(),
            _ => None,
        };
        match pipe {
            Some(pipe) => Ok(&**pipe),
            None => bail!("no pipe of this peer's carries a {kind:?} packet"),
        }
    }

    /// Writes `packet`, whole, on `pipe`.
    fn write(pipe: &dyn Out, packet: &[u8]) -> Result<()> {
        pipe.write_all(packet).context("write a packet")
    }

    /// Writes a message on the pipe its kind goes on: an error on control,
    /// which never waits; others wait while their pipe can take no more.
    pub fn message<M: Message>(&self, message: &M) -> Result<()> {
        Self::write(self.route(M::TYPE)?, &protocol::encode(message)?)
    }

    /// As [`Mux::message`]: the manifest, which waits for room on its pipe.
    pub fn message_paced<M: Message>(&self, message: &M) -> Result<()> {
        self.message(message)
    }

    /// As [`Mux::message_paced`], a packet of the manifest already encoded,
    /// such as one of its compressed stream.
    pub fn packet_paced(&self, packet: Vec<u8>) -> Result<()> {
        Self::write(self.route(PacketType::ManifestStream)?, &packet)
    }

    /// Writes a message on the data's pipe, among the blocks: what the
    /// Source could not open, once it was needed.
    pub fn data_message<M: Message>(&self, message: &M) -> Result<()> {
        Self::write(self.route(PacketType::Data)?, &protocol::encode(message)?)
    }

    /// The Destination is done: said on the answers, after every result.
    pub fn done(&self) -> Result<()> {
        Self::write(
            self.route(PacketType::Results)?,
            &protocol::encode(&Control::Done)?,
        )
    }

    /// Writes a block, whole, as data packets, waiting while the data's
    /// pipe can take no more.
    pub fn block(&self, block: &[u8]) -> Result<()> {
        let data = self.route(PacketType::Data)?;
        for piece in block.chunks(DATA_PACKET) {
            Self::write(data, &protocol::encode_data(piece)?)?;
        }
        Ok(())
    }

    /// How full the data's pipe is.
    pub fn data_gauge(&self) -> Option<Gauge> {
        self.0.data_gauge.clone()
    }

    /// Stops reading every pipe the peer reads, as it stops: what waits for
    /// one returns, and what comes is let go of.
    pub fn stop_reading(&self) {
        for stopper in &self.0.stoppers {
            stopper.stop();
        }
    }
}

/// How many bytes of messages may wait to be written before a paced message
/// waits for room. Small in unit tests, to reach it.
pub const PACED_BYTES: usize = if cfg!(test) { 4096 } else { 8 << 20 };

/// The bytes of messages waiting to be written, and a way to wait for fewer.
struct Queued {
    bytes: Mutex<usize>,
    fewer: Condvar,
    /// How many may wait before a paced message waits for room.
    limit: usize,
    /// The pump is gone: set while `bytes` is locked, so that a paced
    /// message waiting for room sees it as it wakes.
    closed: AtomicBool,
}

/// Where a server's threads hand what the peer reports to the pump of its
/// control connection. Each packet is encoded whole.
#[derive(Clone)]
pub struct Queues {
    messages: Sender<Vec<u8>>,
    paced: Sender<Vec<u8>>,
    /// Rung after each handover, so the pump wakes for any queue.
    bell: Sender<()>,
    /// How many messages, and how many bytes of paced ones, wait.
    messages_gauge: Gauge,
    paced_gauge: Gauge,
    queued: Arc<Queued>,
}

/// The pump's ends of the queues.
pub struct Outlet {
    messages: Receiver<Vec<u8>>,
    paced: Receiver<Vec<u8>>,
    bell: Receiver<()>,
    messages_gauge: Gauge,
    paced_gauge: Gauge,
    queued: Arc<Queued>,
}

/// The queues of a server's reports on its control connection.
pub fn channel_for_reports() -> (Queues, Outlet) {
    channel_named(PACED_BYTES, ["reports", "reports listed"])
}

/// Queues whose gauges are named `names`: the messages, and the paced
/// messages, which wait for room once `limit` bytes of messages wait.
fn channel_named(limit: usize, names: [&str; 2]) -> (Queues, Outlet) {
    let [messages_name, paced_name] = names;
    let (messages, messages_out) = crossbeam_channel::unbounded();
    let (paced, paced_out) = crossbeam_channel::unbounded();
    let (bell, bell_out) = crossbeam_channel::unbounded();
    let messages_gauge = Gauge::new(messages_name, 0);
    let paced_gauge = Gauge::of_bytes(paced_name, limit);
    let queued = Arc::new(Queued {
        bytes: Mutex::new(0),
        fewer: Condvar::new(),
        limit,
        closed: AtomicBool::new(false),
    });
    (
        Queues {
            messages,
            paced,
            bell,
            messages_gauge: messages_gauge.clone(),
            paced_gauge: paced_gauge.clone(),
            queued: Arc::clone(&queued),
        },
        Outlet {
            messages: messages_out,
            paced: paced_out,
            bell: bell_out,
            messages_gauge,
            paced_gauge,
            queued,
        },
    )
}

/// The pump is gone: why is the pump's to say, so this is only a
/// consequence.
fn closed() -> anyhow::Error {
    Stopped.into()
}

impl Queues {
    /// Hands over a packet that keeps its order among the messages.
    pub fn message<M: Message>(&self, message: &M) -> Result<()> {
        self.messages_gauge.up();
        self.hand_over(&self.messages, protocol::encode(message)?)
    }

    /// Hands over a packet that keeps its order among the paced messages,
    /// waiting first while its limit of message bytes wait to be written,
    /// so that a sender faster than the connection waits, rather than piling
    /// messages up in memory.
    pub fn message_paced<M: Message>(&self, message: &M) -> Result<()> {
        let packet = protocol::encode(message)?;
        let mut bytes = self.queued.bytes.lock().expect("not poisoned");
        let closed = || self.queued.closed.load(Ordering::Relaxed);
        let limit = self.queued.limit;
        if *bytes > limit && !closed() {
            let _paced = profile::enter(State::Paced);
            while *bytes > limit && !closed() {
                bytes = self.queued.fewer.wait(bytes).expect("not poisoned");
            }
        }
        // Once the pump is gone, nothing more is handed over: its queues
        // may not be gone yet, and what went in would wait for room for ever.
        if closed() {
            return Err(self::closed());
        }
        drop(bytes);
        self.paced_gauge.add(packet.len() as u64);
        self.hand_over(&self.paced, packet)
    }

    fn hand_over(&self, queue: &Sender<Vec<u8>>, packet: Vec<u8>) -> Result<()> {
        let length = packet.len();
        *self.queued.bytes.lock().expect("not poisoned") += length;
        if queue.send(packet).is_err() {
            // The pump is gone, and may have let go of the count already.
            let mut bytes = self.queued.bytes.lock().expect("not poisoned");
            *bytes = bytes.saturating_sub(length);
            return Err(closed());
        }
        self.bell.send(()).map_err(|_| closed())
    }
}

impl Outlet {
    /// The next message in `queue`, if any, made room for.
    fn message(&self, queue: &Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        let packet = queue.try_recv().ok()?;
        let mut bytes = self.queued.bytes.lock().expect("not poisoned");
        *bytes -= packet.len();
        if *bytes <= self.queued.limit {
            self.queued.fewer.notify_all();
        }
        Some(packet)
    }
}

impl Drop for Outlet {
    /// Paced messages wait for room, which comes as messages are written:
    /// once nothing writes them, none waits for ever.
    fn drop(&mut self) {
        let mut bytes = self.queued.bytes.lock().expect("not poisoned");
        self.queued.closed.store(true, Ordering::Relaxed);
        *bytes = 0;
        drop(bytes);
        self.queued.fewer.notify_all();
    }
}

/// Writes the packets handed to the `Queues`, until every `Queues` is
/// dropped and every queue is empty. Flushes whenever there is nothing left
/// to write. Its thread is profiled as the pump's, by whoever starts it.
pub fn pump(writer: &mut impl Write, outlet: Outlet) -> Result<()> {
    loop {
        let packet = outlet
            .message(&outlet.messages)
            .inspect(|_| {
                outlet.messages_gauge.down();
            })
            .or_else(|| {
                outlet.message(&outlet.paced).inspect(|packet| {
                    outlet.paced_gauge.sub(packet.len() as u64);
                })
            });
        match packet {
            Some(packet) => profile::wait(State::WriteConnection, || writer.write_all(&packet))
                .context("write a packet")?,
            None => {
                profile::wait(State::WriteConnection, || writer.flush()).context("send packets")?;
                // Wait for the next handover; once every `Queues` is gone
                // and nothing is left, the pump is done.
                if profile::wait(State::Work, || outlet.bell.recv()).is_err() {
                    return Ok(());
                }
                continue;
            }
        }
        // Each handover rang once; take this packet's ring.
        let _ = outlet.bell.try_recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::protocol::{BlocksEnd, FileHash, Needs, Results};

    /// Each kind of packet goes on its pipe, whole: a block in data packets
    /// of at most [`DATA_PACKET`] bytes; the Destination's being done on the
    /// answers, after them.
    #[test]
    fn each_packet_goes_on_its_pipe() {
        let pipe = || crossbeam_channel::unbounded::<Vec<u8>>();
        let kinds = |out: &Receiver<Vec<u8>>| -> Vec<PacketType> {
            out.try_iter()
                .map(|packet| protocol::receive(&mut &packet[..]).unwrap().unwrap().kind)
                .collect()
        };
        let ((control, control_out), (manifest, manifest_out), (data, data_out)) =
            (pipe(), pipe(), pipe());
        let source = Mux::for_source(control, manifest, data, None, Vec::new());
        source.message(&Manifest::End).unwrap();
        source
            .message(&FileHash {
                file: 1,
                hash: [0; 32],
            })
            .unwrap();
        source.block(&vec![7; DATA_PACKET + 10]).unwrap();
        source.message(&BlocksEnd { blocks: 1 }).unwrap();
        source
            .message(&Control::Error {
                message: "failed".to_owned(),
            })
            .unwrap();
        assert!(source.message(&Needs::End).is_err());
        assert_eq!(kinds(&manifest_out), [PacketType::Manifest]);
        assert_eq!(
            kinds(&data_out),
            [
                PacketType::FileHash,
                PacketType::Data,
                PacketType::Data,
                PacketType::BlocksEnd
            ]
        );
        assert_eq!(kinds(&control_out), [PacketType::Control]);
        let ((control, control_out), (answers, answers_out)) = (pipe(), pipe());
        let destination = Mux::for_destination(control, answers, Vec::new());
        destination.message(&Needs::End).unwrap();
        destination
            .message(&Results::Failed {
                id: 1,
                message: String::new(),
            })
            .unwrap();
        destination.done().unwrap();
        assert!(destination.block(b"x").is_err());
        assert_eq!(
            kinds(&answers_out),
            [PacketType::Needs, PacketType::Results, PacketType::Control]
        );
        assert!(control_out.try_recv().is_err());
    }

    /// Paced messages stop being handed over once [`PACED_BYTES`] wait, and
    /// go on as the pump writes them, in order.
    #[test]
    fn paced_messages_wait_for_room() {
        crate::test_support::within(
            "paced messages wait for room",
            crate::test_support::PATIENCE,
            move || {
                use std::sync::atomic::{AtomicUsize, Ordering};

                let (mux, outlet) = channel_for_reports();
                let handed = AtomicUsize::new(0);
                let count = 64;
                std::thread::scope(|scope| {
                    let sender = scope.spawn(|| {
                        for index in 0..count {
                            mux.message_paced(&Results::Failed {
                                id: index,
                                message: "x".repeat(512),
                            })
                            .unwrap();
                            handed.fetch_add(1, Ordering::SeqCst);
                        }
                        drop(mux);
                    });
                    // About PACED_BYTES' worth, then it waits: however long that
                    // takes a busy machine.
                    let waiting = settled(&handed, PACED_BYTES / 600);
                    assert!(
                        waiting < count as usize,
                        "{waiting} handed over without a pump"
                    );
                    assert!(waiting >= PACED_BYTES / 600, "only {waiting} handed over");

                    let mut written = Vec::new();
                    pump(&mut written, outlet).unwrap();
                    sender.join().unwrap();
                    let mut reader = written.as_slice();
                    let mut ids = Vec::new();
                    while let Some(packet) = protocol::receive(&mut reader).unwrap() {
                        if let Results::Failed { id, .. } = packet.decode::<Results>().unwrap() {
                            ids.push(id);
                        }
                    }
                    assert_eq!(ids, (0..count).collect::<Vec<_>>());
                });
            },
        );
    }

    /// Waits until `count` is at least `at_least`, and has stayed put a while:
    /// a sender that has handed over that much and waits for room.
    fn settled(count: &std::sync::atomic::AtomicUsize, at_least: usize) -> usize {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let started = Instant::now();
        let mut last = count.load(Ordering::SeqCst);
        let mut since = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "{last} handed over, not {at_least}"
            );
            std::thread::sleep(Duration::from_millis(5));
            let now = count.load(Ordering::SeqCst);
            if now != last {
                (last, since) = (now, Instant::now());
            } else if now >= at_least && since.elapsed() >= Duration::from_millis(50) {
                return now;
            }
        }
    }

    /// With no pump to make room, a paced message fails rather than waiting
    /// for ever.
    #[test]
    fn a_paced_message_does_not_wait_for_a_pump_that_is_gone() {
        crate::test_support::within(
            "a paced message does not wait for a pump that is gone",
            crate::test_support::PATIENCE,
            move || {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let (mux, outlet) = channel_for_reports();
                let handed = AtomicUsize::new(0);
                std::thread::scope(|scope| {
                    let sender = scope.spawn(|| {
                        for index in 0.. {
                            let message = Results::Failed {
                                id: index,
                                message: "x".repeat(512),
                            };
                            if mux.message_paced(&message).is_err() {
                                return index;
                            }
                            handed.fetch_add(1, Ordering::SeqCst);
                        }
                        unreachable!()
                    });
                    // Gone only once the sender waits for room.
                    settled(&handed, PACED_BYTES / 600);
                    drop(outlet);
                    assert!(sender.join().unwrap() > 0);
                });
            },
        );
    }
}
