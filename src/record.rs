//! The record stream: the files the Source sends, one after another.
//!
//! The stream is one continuous run of bytes, carried by data packets with no
//! regard for where a record begins or ends. Each record is
//!
//! ```text
//! [header length: u32][header][content: size bytes][trailer]
//! ```
//!
//! The header and trailer are postcard-encoded. The trailer carries the
//! content's BLAKE3 hash, and why the Source could not read it whole, if it
//! could not.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

use crate::manifest::{FileId, Timestamp};
use crate::profile::{self, State};

/// The largest header or trailer a reader accepts.
const MAX_FRAME: u32 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Header {
    /// A file's content follows: exactly `size` bytes, then a [`Trailer`].
    /// `size` and `mtime` are as the Source found the file when it opened it,
    /// which may no longer be the manifest's: the content is read up to that
    /// size, and what the file gains after is left for the next run, which
    /// finds its mtime changed.
    File {
        id: FileId,
        size: u64,
        mtime: Timestamp,
    },
    /// The stream is complete.
    End,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trailer {
    pub hash: [u8; 32],
    /// Why the Source could not read the file whole, if it could not: it
    /// shrank, or a read failed. The content is padded to its size, to keep
    /// the stream in step, and the Destination discards it.
    pub unread: Option<String>,
}

fn write_framed<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(value).context("encode a record")?;
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    Ok(())
}

fn read_framed<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length).context("read a record")?;
    let length = u32::from_le_bytes(length);
    if length > MAX_FRAME {
        bail!("a record header of {length} bytes exceeds the limit");
    }
    let mut bytes = vec![0; length as usize];
    reader.read_exact(&mut bytes).context("read a record")?;
    postcard::from_bytes(&bytes).context("decode a record")
}

pub fn write_header(writer: &mut impl Write, header: &Header) -> Result<()> {
    write_framed(writer, header)
}

pub fn write_trailer(writer: &mut impl Write, trailer: &Trailer) -> Result<()> {
    write_framed(writer, trailer)
}

pub fn read_header(reader: &mut impl Read) -> Result<Header> {
    read_framed(reader)
}

pub fn read_trailer(reader: &mut impl Read) -> Result<Trailer> {
    read_framed(reader)
}

/// Reads the record stream from the payloads of data packets, as another
/// thread receives them. The stream ends when the sender is dropped.
pub struct ChannelReader {
    receiver: Receiver<Vec<u8>>,
    /// What a thread waiting for bytes waits for.
    waiting: State,
    buffer: Vec<u8>,
    position: usize,
}

impl ChannelReader {
    /// A reader of `receiver`, whose thread is in `waiting` while it waits.
    pub fn new(receiver: Receiver<Vec<u8>>, waiting: State) -> Self {
        Self {
            receiver,
            waiting,
            buffer: Vec::new(),
            position: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.position == self.buffer.len() {
            match profile::wait(self.waiting, || self.receiver.recv()) {
                Ok(bytes) => {
                    self.buffer = bytes;
                    self.position = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let count = out.len().min(self.buffer.len() - self.position);
        out[..count].copy_from_slice(&self.buffer[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_split_anywhere_reads_back() {
        let mut stream = Vec::new();
        let header = Header::File {
            id: 7,
            size: 5,
            mtime: Timestamp {
                seconds: 1,
                nanoseconds: 2,
            },
        };
        write_header(&mut stream, &header).unwrap();
        stream.extend_from_slice(b"hello");
        let trailer = Trailer {
            hash: *blake3::hash(b"hello").as_bytes(),
            unread: None,
        };
        write_trailer(&mut stream, &trailer).unwrap();
        write_header(&mut stream, &Header::End).unwrap();

        // Packets that split the stream at every possible place.
        let (sender, receiver) = crossbeam_channel::unbounded();
        for chunk in stream.chunks(3) {
            sender.send(chunk.to_vec()).unwrap();
        }
        drop(sender);
        let mut reader = ChannelReader::new(receiver, State::Bytes);
        assert_eq!(read_header(&mut reader).unwrap(), header);
        let mut content = [0; 5];
        reader.read_exact(&mut content).unwrap();
        assert_eq!(&content, b"hello");
        assert_eq!(read_trailer(&mut reader).unwrap(), trailer);
        assert_eq!(read_header(&mut reader).unwrap(), Header::End);
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
    }
}
