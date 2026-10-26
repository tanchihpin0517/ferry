//! The one writer of a connection, which decides which packet goes next.
//!
//! Every thread of a peer hands its packets to a pump thread, in one of three
//! queues, each written in the order it was handed over:
//!
//! - **messages**: needs, results, reports, and control messages, such as an
//!   error or the Destination being done; few and small;
//! - **paced messages**, handed over with [`Mux::message_paced`]: the
//!   manifest, and a dry run's listing; many, as fast as a scan goes;
//! - **data**: the blocks of files, in packets of at most [`DATA_PACKET`]
//!   bytes, so that nothing waits behind more than one of them.
//!
//! Messages go first. Between the other two, data goes first while the data
//! queue is at least a quarter full, [`DATA_FIRST`] packets: then the sender
//! has files to send, and the manifest, which a scan makes faster than a
//! connection carries it, would otherwise hold them back until it was all
//! sent. Below that, the manifest goes first, so that the Destination finds
//! more to need before the sender runs out of what it already needs.
//!
//! Where a transfer has more streams than one, each of the others has a
//! lane of its own, which carries blocks only, written by a pump of its own:
//! each block goes whole to the stream with the fewest data packets waiting,
//! the first among them; all else goes on the first stream.
//!
//! What the Source sends of the data is bounded by the data credit the
//! Destination gives; the data queue is bounded too, more tightly, so that
//! what waits here, where the pump copies it, stays in the cache, and a
//! sender faster than the connection waits. So are paced messages: the
//! others, few and small, never wait, so that nothing waits on a thread that
//! waits for room.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::profile::{self, State};
use crate::protocol::{self, Message};
use crate::report::Gauge;
use crate::stop::Stopped;

/// The most bytes of blocks in one data packet.
pub const DATA_PACKET: usize = 64 * 1024;

/// How many data packets may wait to be written: 16 MiB.
const DATA_QUEUE: usize = 256;

/// How many data packets must wait for data to go before the manifest.
const DATA_FIRST: usize = DATA_QUEUE / 4;

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

/// Where a peer's threads hand their packets to the pump. Each packet is
/// encoded whole.
#[derive(Clone)]
pub struct Mux {
    messages: Sender<Vec<u8>>,
    paced: Sender<Vec<u8>>,
    data: Sender<Vec<u8>>,
    /// Rung after each handover, so the pump wakes for any queue.
    bell: Sender<()>,
    /// How full the data queues are, all streams', for the dashboard.
    gauge: Gauge,
    /// How full the first stream's data queue is.
    first: Gauge,
    /// The other streams' lanes.
    lanes: Arc<[Lanes]>,
    /// How many messages, and how many bytes of paced ones, wait.
    messages_gauge: Gauge,
    paced_gauge: Gauge,
    queued: Arc<Queued>,
}

/// The pump's ends of the queues.
pub struct Outlet {
    messages: Receiver<Vec<u8>>,
    paced: Receiver<Vec<u8>>,
    data: Receiver<Vec<u8>>,
    bell: Receiver<()>,
    gauge: Gauge,
    first: Gauge,
    messages_gauge: Gauge,
    paced_gauge: Gauge,
    queued: Arc<Queued>,
}

/// A stream's lane, as the threads that hand blocks over have it.
struct Lanes {
    data: Sender<Vec<u8>>,
    /// How full it is.
    level: Gauge,
}

/// A stream's lane, as its pump has it: blocks only.
pub struct Lane {
    data: Receiver<Vec<u8>>,
    level: Gauge,
    /// All the streams' data, for the dashboard.
    gauge: Gauge,
}

pub fn channel() -> (Mux, Outlet) {
    channel_paced(PACED_BYTES)
}

/// As [`channel`], with paced messages waiting for room once `limit` bytes of
/// messages wait to be written.
pub fn channel_paced(limit: usize) -> (Mux, Outlet) {
    let (mux, outlet, _) = channel_with_lanes(limit, 0);
    (mux, outlet)
}

/// As [`channel_paced`], with `lanes` more streams, which carry blocks: one
/// lane each, for a pump of its own; see [`pump_lane`].
pub fn channel_with_lanes(limit: usize, lanes: usize) -> (Mux, Outlet, Vec<Lane>) {
    channel_named(limit, ["data", "messages", "manifest"], lanes)
}

/// As [`channel`], for a server's reports on its control connection: its
/// queues named apart from its peer's.
pub fn channel_for_reports() -> (Mux, Outlet) {
    let (mux, outlet, _) = channel_named(
        PACED_BYTES,
        ["reports' data", "reports", "reports listed"],
        0,
    );
    (mux, outlet)
}

/// As [`channel_with_lanes`], its queues' gauges named `names`: the data,
/// the messages, and the paced messages.
fn channel_named(limit: usize, names: [&str; 3], lanes: usize) -> (Mux, Outlet, Vec<Lane>) {
    let [data_name, messages_name, paced_name] = names;
    let (messages, messages_out) = crossbeam_channel::unbounded();
    let (paced, paced_out) = crossbeam_channel::unbounded();
    let (data, data_out) = crossbeam_channel::bounded(DATA_QUEUE);
    let (bell, bell_out) = crossbeam_channel::unbounded();
    let gauge = Gauge::new(data_name, DATA_QUEUE * (1 + lanes));
    let first = Gauge::new(format!("{data_name} on the first stream"), DATA_QUEUE);
    let (lanes_in, lanes_out): (Vec<Lanes>, Vec<Lane>) = (0..lanes)
        .map(|index| {
            let (data, data_out) = crossbeam_channel::bounded(DATA_QUEUE);
            let level = Gauge::new(format!("{data_name} on stream {}", index + 1), DATA_QUEUE);
            (
                Lanes {
                    data,
                    level: level.clone(),
                },
                Lane {
                    data: data_out,
                    level,
                    gauge: gauge.clone(),
                },
            )
        })
        .unzip();
    let messages_gauge = Gauge::new(messages_name, 0);
    let paced_gauge = Gauge::of_bytes(paced_name, limit);
    let queued = Arc::new(Queued {
        bytes: Mutex::new(0),
        fewer: Condvar::new(),
        limit,
        closed: AtomicBool::new(false),
    });
    (
        Mux {
            messages,
            paced,
            data,
            bell,
            gauge: gauge.clone(),
            first: first.clone(),
            lanes: lanes_in.into(),
            messages_gauge: messages_gauge.clone(),
            paced_gauge: paced_gauge.clone(),
            queued: Arc::clone(&queued),
        },
        Outlet {
            messages: messages_out,
            paced: paced_out,
            data: data_out,
            bell: bell_out,
            gauge,
            first,
            messages_gauge,
            paced_gauge,
            queued,
        },
        lanes_out,
    )
}

/// The pump is gone: why is the pump's to say, so this is only a
/// consequence.
fn closed() -> anyhow::Error {
    Stopped.into()
}

impl Mux {
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
        self.packet_paced(protocol::encode(message)?)
    }

    /// As [`Mux::message_paced`], a packet already encoded, such as one of
    /// the manifest's stream.
    pub fn packet_paced(&self, packet: Vec<u8>) -> Result<()> {
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
        self.ring()
    }

    /// Hands over the next bytes of the blocks of files, as data packets,
    /// on the first stream. Waits while the data queue is full.
    pub fn data(&self, bytes: &[u8]) -> Result<()> {
        for piece in bytes.chunks(DATA_PACKET) {
            self.gauge.up();
            self.first.up();
            let packet = protocol::encode_data(piece)?;
            profile::wait(State::DataQueue, || self.data.send(packet)).map_err(|_| closed())?;
            self.ring()?;
        }
        Ok(())
    }

    /// Hands over a block, whole, as data packets, on the stream with the
    /// fewest waiting, the first among them. Waits while its queue is full.
    pub fn block(&self, block: &[u8]) -> Result<()> {
        let fewest = self
            .lanes
            .iter()
            .enumerate()
            .min_by_key(|(index, lane)| (lane.level.level(), *index))
            .filter(|(_, lane)| lane.level.level() < self.first.level());
        let Some((_, lane)) = fewest else {
            return self.data(block);
        };
        for piece in block.chunks(DATA_PACKET) {
            self.gauge.up();
            lane.level.up();
            let packet = protocol::encode_data(piece)?;
            profile::wait(State::DataQueue, || lane.data.send(packet)).map_err(|_| closed())?;
        }
        Ok(())
    }

    /// How full the data queue is.
    pub fn data_gauge(&self) -> Gauge {
        self.gauge.clone()
    }

    fn ring(&self) -> Result<()> {
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

    /// The next data packet, if any.
    fn data(&self) -> Option<Vec<u8>> {
        let packet = self.data.try_recv().ok()?;
        self.gauge.down();
        self.first.down();
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

/// Writes the packets handed to the `Mux`, until every `Mux` is dropped and
/// every queue is empty. Flushes whenever there is nothing left to write. Its
/// thread is profiled as the pump's, by whoever starts it.
pub fn pump(writer: &mut impl Write, outlet: Outlet) -> Result<()> {
    loop {
        let data_first = outlet.first.level() >= DATA_FIRST;
        let packet = outlet
            .message(&outlet.messages)
            .inspect(|_| {
                outlet.messages_gauge.down();
            })
            .or_else(|| data_first.then(|| outlet.data()).flatten())
            .or_else(|| {
                outlet.message(&outlet.paced).inspect(|packet| {
                    outlet.paced_gauge.sub(packet.len() as u64);
                })
            })
            .or_else(|| outlet.data())
            .ok_or(());
        match packet {
            Ok(packet) => profile::wait(State::WriteConnection, || writer.write_all(&packet))
                .context("write a packet")?,
            Err(_) => {
                profile::wait(State::WriteConnection, || writer.flush()).context("send packets")?;
                // Wait for the next handover; once every `Mux` is gone and
                // nothing is left, the pump is done.
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

/// Writes the blocks handed to a stream's lane, until every `Mux` is
/// dropped and the lane is empty. Flushes whenever there is nothing left to
/// write. Its thread is profiled as a pump's, by whoever starts it.
pub fn pump_lane(writer: &mut impl Write, lane: Lane) -> Result<()> {
    loop {
        let packet = match lane.data.try_recv() {
            Ok(packet) => packet,
            Err(_) => {
                profile::wait(State::WriteConnection, || writer.flush()).context("send packets")?;
                match profile::wait(State::Work, || lane.data.recv()) {
                    Ok(packet) => packet,
                    // Every `Mux` is gone, and nothing is left.
                    Err(_) => return Ok(()),
                }
            }
        };
        lane.level.down();
        lane.gauge.down();
        profile::wait(State::WriteConnection, || writer.write_all(&packet))
            .context("write a packet")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Control, Needs, PacketType, Results};

    #[test]
    fn messages_go_first_in_order_and_data_is_split() {
        crate::test_support::within(
            "messages go first in order and data is split",
            crate::test_support::PATIENCE,
            move || {
                let (mux, outlet) = channel();
                mux.data(&vec![7; DATA_PACKET + 10]).unwrap();
                mux.message(&Needs::End).unwrap();
                mux.message(&Control::Done).unwrap();
                drop(mux);
                let mut written = Vec::new();
                pump(&mut written, outlet).unwrap();

                let mut reader = written.as_slice();
                let mut kinds = Vec::new();
                let mut data = 0;
                while let Some(packet) = protocol::receive(&mut reader).unwrap() {
                    kinds.push(packet.kind);
                    if packet.kind == PacketType::Data {
                        let payload = packet.into_payload();
                        assert!(payload.len() <= DATA_PACKET);
                        data += payload.len();
                    }
                }
                assert_eq!(
                    kinds,
                    [
                        PacketType::Needs,
                        PacketType::Control,
                        PacketType::Data,
                        PacketType::Data
                    ]
                );
                assert_eq!(data, DATA_PACKET + 10);
            },
        );
    }

    /// With data enough waiting, data goes before the manifest, until it is
    /// below [`DATA_FIRST`] packets; messages go before both.
    #[test]
    fn data_goes_before_the_manifest_while_enough_of_it_waits() {
        crate::test_support::within(
            "data goes before the manifest while enough of it waits",
            crate::test_support::PATIENCE,
            move || {
                let (mux, outlet) = channel();
                let packets = DATA_FIRST + 4;
                mux.data(&vec![7; packets * DATA_PACKET]).unwrap();
                mux.message_paced(&Results::Failed {
                    id: 1,
                    message: String::new(),
                })
                .unwrap();
                mux.message(&Control::Done).unwrap();
                drop(mux);
                let mut written = Vec::new();
                pump(&mut written, outlet).unwrap();

                let mut reader = written.as_slice();
                let mut kinds = Vec::new();
                while let Some(packet) = protocol::receive(&mut reader).unwrap() {
                    kinds.push(packet.kind);
                }
                let before = packets - (DATA_FIRST - 1);
                let mut expected = vec![PacketType::Control];
                expected.extend([PacketType::Data].repeat(before));
                expected.push(PacketType::Results);
                expected.extend([PacketType::Data].repeat(packets - before));
                assert_eq!(kinds, expected);
            },
        );
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

                let (mux, outlet) = channel();
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
                let (mux, outlet) = channel();
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
