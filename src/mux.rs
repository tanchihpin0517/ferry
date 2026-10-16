//! The one writer of a connection, which decides which packet goes next.
//!
//! Every thread of a peer hands its packets to a pump thread, in one of three
//! queues, each written in the order it was handed over:
//!
//! - **messages**: needs, results, reports, and control messages, such as an
//!   error or the Destination being done; few and small;
//! - **paced messages**, handed over with [`Mux::message_paced`]: the
//!   manifest, and a dry run's listing; many, as fast as a scan goes;
//! - **data**: the record stream, in packets of at most [`DATA_PACKET`]
//!   bytes, so that nothing waits behind more than one of them.
//!
//! Messages go first. Between the other two, data goes first while the data
//! queue is at least a quarter full, [`DATA_FIRST`] packets: then the sender
//! has files to send, and the manifest, which a scan makes faster than a
//! connection carries it, would otherwise hold them back until it was all
//! sent. Below that, the manifest goes first, so that the Destination finds
//! more to need before the sender runs out of what it already needs.
//!
//! The data queue is bounded, so that a sender faster than the connection
//! waits, rather than piling the stream up in memory. So are paced messages:
//! the others, few and small, never wait, so that nothing waits on a thread
//! that waits for room.

use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, anyhow};

use crate::profile::{self, State};
use crate::protocol::{self, Message};
use crate::report::Gauge;

/// The most record-stream bytes in one data packet.
pub const DATA_PACKET: usize = 64 * 1024;

/// How many data packets may wait to be written: 16 MiB.
const DATA_QUEUE: usize = 256;

/// How many data packets must wait for data to go before the manifest.
const DATA_FIRST: usize = DATA_QUEUE / 4;

/// How many bytes of messages may wait to be written before a paced message
/// waits for room. Small in unit tests, to reach it.
const PACED_BYTES: usize = if cfg!(test) { 4096 } else { 8 << 20 };

/// The bytes of messages waiting to be written, and a way to wait for fewer.
#[derive(Default)]
struct Queued {
    bytes: Mutex<usize>,
    fewer: Condvar,
}

/// Where a peer's threads hand their packets to the pump. Each packet is
/// encoded whole.
#[derive(Clone)]
pub struct Mux {
    messages: Sender<Vec<u8>>,
    paced: Sender<Vec<u8>>,
    data: SyncSender<Vec<u8>>,
    /// Rung after each handover, so the pump wakes for any queue.
    bell: Sender<()>,
    /// How full the data queue is, for the dashboard.
    gauge: Gauge,
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
    messages_gauge: Gauge,
    paced_gauge: Gauge,
    queued: Arc<Queued>,
}

pub fn channel() -> (Mux, Outlet) {
    let (messages, messages_out) = mpsc::channel();
    let (paced, paced_out) = mpsc::channel();
    let (data, data_out) = mpsc::sync_channel(DATA_QUEUE);
    let (bell, bell_out) = mpsc::channel();
    let gauge = Gauge::new("data", DATA_QUEUE);
    let messages_gauge = Gauge::new("messages", 0);
    let paced_gauge = Gauge::of_bytes("manifest", PACED_BYTES);
    let queued = Arc::new(Queued::default());
    (
        Mux {
            messages,
            paced,
            data,
            bell,
            gauge: gauge.clone(),
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
            messages_gauge,
            paced_gauge,
            queued,
        },
    )
}

fn closed() -> anyhow::Error {
    anyhow!("the connection's writer stopped")
}

impl Mux {
    /// Hands over a packet that keeps its order among the messages.
    pub fn message<M: Message>(&self, message: &M) -> Result<()> {
        self.messages_gauge.up();
        self.hand_over(&self.messages, protocol::encode(message)?)
    }

    /// Hands over a packet that keeps its order among the paced messages,
    /// waiting first while [`PACED_BYTES`] of messages wait to be written,
    /// so that a sender faster than the connection waits, rather than piling
    /// messages up in memory.
    pub fn message_paced<M: Message>(&self, message: &M) -> Result<()> {
        let packet = protocol::encode(message)?;
        let mut bytes = self.queued.bytes.lock().expect("not poisoned");
        if *bytes > PACED_BYTES {
            let _paced = profile::enter(State::Paced);
            while *bytes > PACED_BYTES {
                bytes = self.queued.fewer.wait(bytes).expect("not poisoned");
            }
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

    /// Hands over the next bytes of the record stream, as data packets.
    /// Waits while the data queue is full.
    pub fn data(&self, bytes: &[u8]) -> Result<()> {
        for piece in bytes.chunks(DATA_PACKET) {
            self.gauge.up();
            let packet = protocol::encode_data(piece)?;
            profile::wait(State::DataQueue, || self.data.send(packet)).map_err(|_| closed())?;
            self.ring()?;
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
        if *bytes <= PACED_BYTES {
            self.queued.fewer.notify_all();
        }
        Some(packet)
    }

    /// The next data packet, if any.
    fn data(&self) -> Option<Vec<u8>> {
        let packet = self.data.try_recv().ok()?;
        self.gauge.down();
        Some(packet)
    }
}

impl Drop for Outlet {
    /// Paced messages wait for room, which comes as messages are written:
    /// once nothing writes them, none waits for ever.
    fn drop(&mut self) {
        *self.queued.bytes.lock().expect("not poisoned") = 0;
        self.queued.fewer.notify_all();
    }
}

/// Writes the packets handed to the `Mux`, until every `Mux` is dropped and
/// every queue is empty. Flushes whenever there is nothing left to write.
pub fn pump(writer: &mut impl Write, outlet: Outlet) -> Result<()> {
    let _thread = profile::thread("pump");
    loop {
        let data_first = outlet.gauge.level() >= DATA_FIRST;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Control, Needs, PacketType, Results};

    #[test]
    fn messages_go_first_in_order_and_data_is_split() {
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
    }

    /// With data enough waiting, data goes before the manifest, until it is
    /// below [`DATA_FIRST`] packets; messages go before both.
    #[test]
    fn data_goes_before_the_manifest_while_enough_of_it_waits() {
        let (mux, outlet) = channel();
        let packets = DATA_FIRST + 4;
        mux.data(&vec![7; packets * DATA_PACKET]).unwrap();
        mux.message_paced(&Results::Resend(vec![1])).unwrap();
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
    }

    /// Paced messages stop being handed over once [`PACED_BYTES`] wait, and
    /// go on as the pump writes them, in order.
    #[test]
    fn paced_messages_wait_for_room() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

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
            std::thread::sleep(Duration::from_millis(200));
            // About PACED_BYTES' worth, then it waits.
            let waiting = handed.load(Ordering::SeqCst);
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
    }

    /// With no pump to make room, a paced message fails rather than waiting
    /// for ever.
    #[test]
    fn a_paced_message_does_not_wait_for_a_pump_that_is_gone() {
        let (mux, outlet) = channel();
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
                }
                unreachable!()
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(outlet);
            assert!(sender.join().unwrap() > 0);
        });
    }
}
