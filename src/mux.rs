//! The one writer of a connection, which decides which packet goes next.
//!
//! Every thread of a peer hands its packets to a pump thread, in one of two
//! queues, and the pump writes them in order of priority:
//!
//! 1. **messages**: the manifest, needs, results, reports, and control
//!    messages, such as an error or the Destination being done, in the order
//!    they were handed over;
//! 2. **data**: the record stream, in packets of at most [`DATA_PACKET`]
//!    bytes, so that nothing waits behind more than one of them.
//!
//! No message goes ahead of the others: an error waits its turn among them.
//!
//! The data queue is bounded, so that a sender faster than the connection
//! waits, rather than piling the stream up in memory. So are messages handed
//! over with [`Mux::message_paced`], as the walk hands over the manifest:
//! the rest, few and small, never wait, so that nothing waits on a thread
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

/// How many data packets may wait to be written.
const DATA_QUEUE: usize = 64;

/// How many bytes of messages may wait to be written before a paced message
/// waits for room. Small in unit tests, to reach it.
const PACED_BYTES: usize = if cfg!(test) { 4096 } else { 64 << 20 };

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
    data: SyncSender<Vec<u8>>,
    /// Rung after each handover, so the pump wakes for any queue.
    bell: Sender<()>,
    /// How full the data queue is, for the dashboard.
    gauge: Gauge,
    queued: Arc<Queued>,
}

/// The pump's ends of the queues.
pub struct Outlet {
    messages: Receiver<Vec<u8>>,
    data: Receiver<Vec<u8>>,
    bell: Receiver<()>,
    gauge: Gauge,
    queued: Arc<Queued>,
}

pub fn channel() -> (Mux, Outlet) {
    let (messages, messages_out) = mpsc::channel();
    let (data, data_out) = mpsc::sync_channel(DATA_QUEUE);
    let (bell, bell_out) = mpsc::channel();
    let gauge = Gauge::new(DATA_QUEUE);
    let queued = Arc::new(Queued::default());
    (
        Mux {
            messages,
            data,
            bell,
            gauge: gauge.clone(),
            queued: Arc::clone(&queued),
        },
        Outlet {
            messages: messages_out,
            data: data_out,
            bell: bell_out,
            gauge,
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
        self.hand_over(protocol::encode(message)?)
    }

    /// As [`Mux::message`], but waits first while [`PACED_BYTES`] of
    /// messages wait to be written, so that a sender faster than the
    /// connection waits, rather than piling messages up in memory.
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
        self.hand_over(packet)
    }

    fn hand_over(&self, packet: Vec<u8>) -> Result<()> {
        let length = packet.len();
        *self.queued.bytes.lock().expect("not poisoned") += length;
        if self.messages.send(packet).is_err() {
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
        let packet = match outlet.messages.try_recv() {
            Ok(packet) => {
                let mut bytes = outlet.queued.bytes.lock().expect("not poisoned");
                *bytes -= packet.len();
                if *bytes <= PACED_BYTES {
                    outlet.queued.fewer.notify_all();
                }
                Ok(packet)
            }
            Err(_) => {
                let packet = outlet.data.try_recv();
                if packet.is_ok() {
                    outlet.gauge.down();
                }
                packet
            }
        };
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
