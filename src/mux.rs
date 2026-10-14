//! The one writer of a connection, which decides which packet goes next.
//!
//! Every thread of a peer hands its packets to a pump thread, in one of three
//! queues, and the pump writes them in order of priority:
//!
//! 1. **control**: errors and warnings, which must never wait behind data;
//! 2. **messages**: the manifest, needs, results, and the report that the
//!    Destination is done, in the order they were handed over;
//! 3. **data**: the record stream, in packets of at most [`DATA_PACKET`]
//!    bytes, so that nothing waits behind more than one of them.
//!
//! The data queue is bounded, so that a sender faster than the connection
//! waits, rather than piling the stream up in memory.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

use anyhow::{Context, Result, anyhow};

use crate::protocol::{self, Message};

/// The most record-stream bytes in one data packet.
pub const DATA_PACKET: usize = 64 * 1024;

/// How many data packets may wait to be written.
const DATA_QUEUE: usize = 64;

/// Where a peer's threads hand their packets to the pump. Each packet is
/// encoded whole.
#[derive(Clone)]
pub struct Mux {
    control: Sender<Vec<u8>>,
    messages: Sender<Vec<u8>>,
    data: SyncSender<Vec<u8>>,
    /// Rung after each handover, so the pump wakes for any queue.
    bell: Sender<()>,
    /// Data packets handed over and not yet written.
    waiting: Arc<AtomicUsize>,
}

/// The pump's ends of the queues.
pub struct Outlet {
    control: Receiver<Vec<u8>>,
    messages: Receiver<Vec<u8>>,
    data: Receiver<Vec<u8>>,
    bell: Receiver<()>,
    waiting: Arc<AtomicUsize>,
}

pub fn channel() -> (Mux, Outlet) {
    let (control, control_out) = mpsc::channel();
    let (messages, messages_out) = mpsc::channel();
    let (data, data_out) = mpsc::sync_channel(DATA_QUEUE);
    let (bell, bell_out) = mpsc::channel();
    let waiting = Arc::new(AtomicUsize::new(0));
    (
        Mux {
            control,
            messages,
            data,
            bell,
            waiting: Arc::clone(&waiting),
        },
        Outlet {
            control: control_out,
            messages: messages_out,
            data: data_out,
            bell: bell_out,
            waiting,
        },
    )
}

fn closed() -> anyhow::Error {
    anyhow!("the connection's writer stopped")
}

impl Mux {
    /// Hands over a packet that goes ahead of everything else.
    pub fn control<M: Message>(&self, message: &M) -> Result<()> {
        self.control
            .send(protocol::encode(message)?)
            .map_err(|_| closed())?;
        self.ring()
    }

    /// Hands over a packet that keeps its order among the messages.
    pub fn message<M: Message>(&self, message: &M) -> Result<()> {
        self.messages
            .send(protocol::encode(message)?)
            .map_err(|_| closed())?;
        self.ring()
    }

    /// Hands over the next bytes of the record stream, as data packets.
    /// Waits while the data queue is full.
    pub fn data(&self, bytes: &[u8]) -> Result<()> {
        for piece in bytes.chunks(DATA_PACKET) {
            self.waiting.fetch_add(1, Ordering::Relaxed);
            self.data
                .send(protocol::encode_data(piece)?)
                .map_err(|_| closed())?;
            self.ring()?;
        }
        Ok(())
    }

    /// How full the data queue is, from 0 to 1: kept full, the connection is
    /// the bottleneck.
    pub fn data_backlog(&self) -> f64 {
        (self.waiting.load(Ordering::Relaxed) as f64 / DATA_QUEUE as f64).min(1.0)
    }

    fn ring(&self) -> Result<()> {
        self.bell.send(()).map_err(|_| closed())
    }
}

/// Writes the packets handed to the `Mux`, until every `Mux` is dropped and
/// every queue is empty. Flushes whenever there is nothing left to write.
pub fn pump(writer: &mut impl Write, outlet: Outlet) -> Result<()> {
    loop {
        let packet = outlet
            .control
            .try_recv()
            .or_else(|_| outlet.messages.try_recv())
            .or_else(|_| {
                let packet = outlet.data.try_recv();
                if packet.is_ok() {
                    outlet.waiting.fetch_sub(1, Ordering::Relaxed);
                }
                packet
            });
        match packet {
            Ok(packet) => writer.write_all(&packet).context("write a packet")?,
            Err(_) => {
                writer.flush().context("send packets")?;
                // Wait for the next handover; once every `Mux` is gone and
                // nothing is left, the pump is done.
                if outlet.bell.recv().is_err() {
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
    use crate::protocol::{Control, Needs, PacketType};

    #[test]
    fn control_goes_first_and_data_is_split() {
        let (mux, outlet) = channel();
        mux.data(&vec![7; DATA_PACKET + 10]).unwrap();
        mux.message(&Needs::End).unwrap();
        mux.control(&Control::Done).unwrap();
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
                PacketType::Control,
                PacketType::Needs,
                PacketType::Data,
                PacketType::Data
            ]
        );
        assert_eq!(data, DATA_PACKET + 10);
    }
}
