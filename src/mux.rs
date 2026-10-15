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
//! waits, rather than piling the stream up in memory.

use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

use anyhow::{Context, Result, anyhow};

use crate::protocol::{self, Message};
use crate::report::Gauge;

/// The most record-stream bytes in one data packet.
pub const DATA_PACKET: usize = 64 * 1024;

/// How many data packets may wait to be written.
const DATA_QUEUE: usize = 64;

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
}

/// The pump's ends of the queues.
pub struct Outlet {
    messages: Receiver<Vec<u8>>,
    data: Receiver<Vec<u8>>,
    bell: Receiver<()>,
    gauge: Gauge,
}

pub fn channel() -> (Mux, Outlet) {
    let (messages, messages_out) = mpsc::channel();
    let (data, data_out) = mpsc::sync_channel(DATA_QUEUE);
    let (bell, bell_out) = mpsc::channel();
    let gauge = Gauge::new(DATA_QUEUE);
    (
        Mux {
            messages,
            data,
            bell,
            gauge: gauge.clone(),
        },
        Outlet {
            messages: messages_out,
            data: data_out,
            bell: bell_out,
            gauge,
        },
    )
}

fn closed() -> anyhow::Error {
    anyhow!("the connection's writer stopped")
}

impl Mux {
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
            self.gauge.up();
            self.data
                .send(protocol::encode_data(piece)?)
                .map_err(|_| closed())?;
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

/// Writes the packets handed to the `Mux`, until every `Mux` is dropped and
/// every queue is empty. Flushes whenever there is nothing left to write.
pub fn pump(writer: &mut impl Write, outlet: Outlet) -> Result<()> {
    loop {
        let packet = outlet.messages.try_recv().or_else(|_| {
            let packet = outlet.data.try_recv();
            if packet.is_ok() {
                outlet.gauge.down();
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
}
