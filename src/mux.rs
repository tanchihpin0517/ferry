//! Where a peer's threads write what they send the other peer: each packet
//! whole, on the pipe its kind goes on, as [`Mux`] routes it, or records,
//! on the pipe that carries them. Until every pipe carries records rather
//! than packets, the packets are the peers' own:
//!
//! - **control**, both ways: an error, which never waits;
//! - **manifest**, the Source's: the manifest's records, plain or
//!   compressed;
//! - **data**, the Source's: the blocks of files, in packets of at most
//!   [`DATA_PACKET`] bytes, the hashes of files sent in segments, what the
//!   owner could not open, and how many blocks there were;
//! - **answers**, the Destination's: its answers' records, what it needs,
//!   what it compared, what failed, and that it is done.
//!
//! How each pipe is sent, and when its writer waits, is the transport's;
//! see [`crate::transport::pipes`].

use std::io::{self, Read};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::protocol::{self, Answer, Message, PacketType};
use crate::report::Gauge;
use crate::transport::pipes::{PipeReader, PipeWriter, Stopper};

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

/// A pipe's bytes, as they come, for one who reads them as a pipe, as the
/// tests and the fuzzer read a peer's records.
impl Out for Sender<io::Result<Vec<u8>>> {
    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.send(Ok(bytes.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "no one reads the pipe"))
    }
}

/// Where a peer reads one of its pipes: its bytes, and what they took on the
/// wire, compressed or not, so far.
pub trait In: Read + Send {
    fn wire(&self) -> u64;
}

impl<T: In + ?Sized> In for Box<T> {
    fn wire(&self) -> u64 {
        (**self).wire()
    }
}

impl In for PipeReader {
    fn wire(&self) -> u64 {
        PipeReader::wire(self)
    }
}

impl In for Piped {
    fn wire(&self) -> u64 {
        self.read
    }
}

/// A pipe's bytes as they come, packet by packet, or why they stopped: for
/// a peer run over one stream of packets rather than pipes, as the tests and
/// the fuzzer run one.
pub struct Piped {
    packets: Receiver<io::Result<Vec<u8>>>,
    packet: Vec<u8>,
    at: usize,
    /// Bytes read, ever.
    read: u64,
}

impl Piped {
    pub fn new(packets: Receiver<io::Result<Vec<u8>>>) -> Self {
        Self {
            packets,
            packet: Vec::new(),
            at: 0,
            read: 0,
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
        self.read += taken as u64;
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

    /// Writes the next bytes of the manifest's records, waiting while its
    /// pipe can take no more.
    pub fn manifest(&self, bytes: &[u8]) -> io::Result<()> {
        match &self.0.manifest {
            Some(pipe) => pipe.write_all(bytes),
            None => Err(io::Error::other("this peer writes no manifest")),
        }
    }

    /// Writes a message on the data's pipe, among the blocks: what the
    /// Source could not open, once it was needed.
    pub fn data_message<M: Message>(&self, message: &M) -> Result<()> {
        Self::write(self.route(PacketType::Data)?, &protocol::encode(message)?)
    }

    /// Writes `answers` on the answers' pipe, as records, at once, waiting
    /// while it can take no more.
    pub fn answers(&self, answers: impl IntoIterator<Item = Answer>) -> Result<()> {
        let Some(pipe) = &self.0.answers else {
            bail!("this peer writes no answers");
        };
        let mut bytes = Vec::new();
        for answer in answers {
            protocol::put_record(&mut bytes, &answer)?;
        }
        Self::write(&**pipe, &bytes)
    }

    /// The Destination is done: said on the answers, after every other.
    pub fn done(&self) -> Result<()> {
        self.answers([Answer::Done])
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::protocol::{BlocksEnd, Control, FileHash};

    /// Each kind of packet goes on its pipe, whole: a block in data packets
    /// of at most [`DATA_PACKET`] bytes; the manifest's records on its pipe,
    /// as they are, and no packet; the Destination's answers as records,
    /// its being done after them.
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
        source.manifest(b"records").unwrap();
        assert!(source.message(&Manifest::End).is_err());
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
        assert!(source.answers([Answer::NeedsEnd]).is_err());
        assert_eq!(manifest_out.try_iter().collect::<Vec<_>>(), [b"records"]);
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
        let answers = [
            Answer::NeedsEnd,
            Answer::Failed {
                id: 1,
                message: String::new(),
            },
        ];
        destination.answers(answers.clone()).unwrap();
        destination.done().unwrap();
        assert!(destination.block(b"x").is_err());
        assert!(destination.manifest(b"x").is_err());
        let written: Vec<u8> = answers_out.try_iter().flatten().collect();
        let mut reader = written.as_slice();
        let read: Vec<Answer> =
            std::iter::from_fn(|| protocol::read_record(&mut reader).unwrap()).collect();
        assert_eq!(read, [&answers[..], &[Answer::Done]].concat());
        assert!(control_out.try_recv().is_err());
    }
}
