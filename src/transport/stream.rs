//! A stream of packets over one connection, as each end has it: the stream
//! between a transfer's two servers, which the client makes and hands to
//! both, a server's control connection past its handshake, and the link
//! between the client's monitor and a host's. It carries the packets as they
//! are, and adds one of its own, which no one reading it sees: its end,
//! which its writer sends once done. Its reader takes it for broken once it
//! ends without its end: what reads it then fails, and the transfer with
//! it. No other stream takes its place: a host has one SSH connection, and
//! one dropped fails the transfer, as with rsync. Whether the far end is
//! there at all is not the stream's to judge, but the monitors', by their
//! heartbeat; see [`crate::monitor`].
//!
//! Both its ends are made not to block: its reader waits for what comes,
//! and its writer for room, within the stream's scope, which it ends as it
//! takes the stream for broken, or as it is let go, and which ends as the
//! scope it was made in does, as a host's once it is lost: so whatever waits
//! on it stops at once, even on a pipe, which cannot be shut down as a
//! socket is.
//!
//! What the reader makes of what comes, and where the writer is among the
//! packets it writes, are [`Deframer`]'s and [`Boundaries`]', which do no
//! I/O: so any bytes, however they come, can go through them, as they do in
//! the fuzzer.

use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::PollFlags;
use rustix::fs::{FileType, OFlags};
use rustix::net::Shutdown;

use super::scope::{Ended, Scope};
use super::start;
use crate::profile::State;
use crate::protocol::PacketType;

/// A packet's header: its type, then its payload's length, four bytes,
/// little-endian.
pub const HEADER: usize = 5;

/// How much the reader reads ahead, for the headers of packets, and small
/// packets.
const BUFFER: usize = 64 * 1024;

/// What is left of a packet that is read straight into the reader's
/// caller, rather than through its buffer.
const STRAIGHT: usize = 16 * 1024;

/// A stream's two ends, as handed over.
pub struct Ends {
    /// What it is read from, and written to: one socket, both ways, or two
    /// pipes.
    pub input: File,
    pub output: File,
    /// Kept while the stream is in use: the socket of a process that handed
    /// the stream over, which waits for it to close.
    pub held: Option<UnixStream>,
    /// What comes on it begins with the start mark, after what a shell may
    /// have printed at the far end, all of which is skipped.
    pub marked: bool,
    /// What was read from it already, before it was handed over, as by a
    /// buffered reader that read on past a handshake: read first.
    pub before: Vec<u8>,
}

impl Ends {
    /// A socket, read and written both, its packets from the first byte.
    pub fn socket(socket: UnixStream) -> io::Result<Self> {
        Ok(Self {
            input: File::from(OwnedFd::from(socket.try_clone()?)),
            output: File::from(OwnedFd::from(socket)),
            held: None,
            marked: false,
            before: Vec::new(),
        })
    }
}

/// What the stream's reader, its writer, and whoever asks what it carried
/// share.
struct Shared {
    /// Ended once the stream is taken for broken, or let go of, saying why
    /// as first said: whoever finds it broken after says so too, whichever
    /// of the peer's threads says it first; and whatever waits on it, a
    /// writer for room or the reader for what comes, stops.
    scope: Scope,
    /// When anything last came, as nanoseconds since `started`, and whether
    /// anything has.
    heard: AtomicU64,
    anything: AtomicBool,
    started: Instant,
    /// Bytes read, and written.
    read: AtomicU64,
    written: AtomicU64,
    /// Its socket again, to shut down once it breaks; none for pipes, which
    /// cannot be, and whose writing end must close for the other side to
    /// read to its end.
    shut: Option<File>,
    _held: Option<UnixStream>,
}

impl Shared {
    /// Takes the stream for broken, for `why` unless it was already: shut
    /// down both ways, so that the other side sees it so, and a writer here
    /// waiting for room stops.
    fn take_for_broken(&self, why: &str) {
        self.scope.end(why);
        if let Some(socket) = &self.shut {
            let _ = rustix::net::shutdown(socket, Shutdown::Both);
        }
    }

    /// Why the stream was taken for broken, as first said.
    fn why(&self) -> String {
        self.scope
            .ended()
            .map_or_else(|| "it was let go".to_owned(), |ended| ended.0)
    }

    fn heard(&self, bytes: usize) {
        let now = self.started.elapsed().as_nanos() as u64;
        self.heard.store(now, Ordering::Relaxed);
        self.anything.store(true, Ordering::Relaxed);
        self.read.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// When anything last came, or the stream started.
    fn last_heard(&self) -> Instant {
        self.started + Duration::from_nanos(self.heard.load(Ordering::Relaxed))
    }
}

/// What the stream carried, for the dashboard: clones see the same.
#[derive(Clone)]
pub struct Carried(Arc<Shared>);

impl Carried {
    /// The bytes it carried both ways, and how long ago anything came on
    /// it, if anything has.
    pub fn now(&self) -> (u64, Option<Duration>) {
        let shared = &self.0;
        let bytes = shared.read.load(Ordering::Relaxed) + shared.written.load(Ordering::Relaxed);
        let heard = shared
            .anything
            .load(Ordering::Relaxed)
            .then(|| shared.last_heard().elapsed());
        (bytes, heard)
    }

    /// Lets go of the stream, as once what it carries is known to be over:
    /// its reader, waiting, stops at once, rather than once nothing came for
    /// the silence it is allowed.
    pub fn let_go(&self) {
        self.0.take_for_broken("it was let go");
    }
}

/// Starts a stream on its `ends`, within `scope`: its reader, which skips to
/// the start mark first on one marked; its writer; and what it carried. Once
/// the scope ends, so does the stream, and the stream ends it as it breaks,
/// or is let go.
pub fn start(ends: Ends, scope: Scope) -> io::Result<(Reading, Writing, Carried)> {
    let Ends {
        input,
        output,
        held,
        marked,
        before,
    } = ends;
    for end in [&input, &output] {
        let flags = rustix::fs::fcntl_getfl(end)?;
        rustix::fs::fcntl_setfl(end, flags | OFlags::NONBLOCK)?;
    }
    let socket = FileType::from_raw_mode(rustix::fs::fstat(&input)?.st_mode) == FileType::Socket;
    let shared = Arc::new(Shared {
        scope,
        heard: AtomicU64::new(0),
        anything: AtomicBool::new(false),
        started: Instant::now(),
        read: AtomicU64::new(0),
        written: AtomicU64::new(0),
        shut: if socket {
            Some(input.try_clone()?)
        } else {
            None
        },
        _held: held,
    });
    let out = Arc::new(Mutex::new(Out {
        file: Some(output),
        boundaries: Boundaries::default(),
    }));
    let mut deframer = Deframer::default();
    deframer.already(&before);
    shared
        .read
        .fetch_add(before.len() as u64, Ordering::Relaxed);
    let mut reading = Reading {
        deframer,
        source: Source {
            file: input,
            shared: Arc::clone(&shared),
        },
    };
    if marked {
        let skipped = start::skip(&mut Raw(&mut reading))?;
        if !skipped.found {
            return Err(reading.source.broke(
                io::ErrorKind::ConnectionAborted,
                "the stream ended before it started",
            ));
        }
    }
    Ok((
        reading,
        Writing {
            out,
            shared: Arc::clone(&shared),
        },
        Carried(shared),
    ))
}

/// The stream's writing end, under its lock.
struct Out {
    /// None once the stream's end is written.
    file: Option<File>,
    /// Where the peer is among the packets it writes.
    boundaries: Boundaries,
}

/// Where a peer is among the packets it writes, followed through what it
/// writes, however that is cut: between two, the stream's end may go.
#[derive(Default)]
pub struct Boundaries {
    /// The header of the packet being written, as much of it as was, and
    /// what is left of its payload.
    header: [u8; HEADER],
    filled: usize,
    left: usize,
}

impl Boundaries {
    /// Between two packets: the end may go.
    pub fn between(&self) -> bool {
        self.filled == 0 && self.left == 0
    }

    /// Follows the packets through `bytes`, just written.
    pub fn follow(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.left > 0 {
                let taken = self.left.min(bytes.len());
                self.left -= taken;
                bytes = &bytes[taken..];
                continue;
            }
            let taken = (HEADER - self.filled).min(bytes.len());
            self.header[self.filled..self.filled + taken].copy_from_slice(&bytes[..taken]);
            self.filled += taken;
            bytes = &bytes[taken..];
            if self.filled == HEADER {
                self.filled = 0;
                self.left =
                    u32::from_le_bytes(self.header[1..].try_into().expect("four bytes")) as usize;
            }
        }
    }
}

impl Out {
    /// Writes some of `bytes`, waiting for room, until the stream is taken
    /// for broken.
    fn write_some(&mut self, bytes: &[u8], shared: &Shared) -> io::Result<usize> {
        loop {
            if shared.scope.ended().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("the stream broke: {}", shared.why()),
                ));
            }
            let file = self.file.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "the stream's end is written")
            })?;
            match (&mut &*file).write(bytes) {
                Ok(written) => {
                    shared.written.fetch_add(written as u64, Ordering::Relaxed);
                    return Ok(written);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // Woken once there is room, or the stream is taken for
                    // broken, which the next look says.
                    let _ = shared
                        .scope
                        .wait(file, PollFlags::OUT, None, State::WriteConnection);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    shared.take_for_broken(&error.to_string());
                    return Err(io::Error::new(
                        error.kind(),
                        format!("the stream broke: {}", shared.why()),
                    ));
                }
            }
        }
    }

    fn write_all(&mut self, mut bytes: &[u8], shared: &Shared) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self.write_some(bytes, shared)?;
            self.boundaries.follow(&bytes[..written]);
            bytes = &bytes[written..];
        }
        Ok(())
    }
}

/// The stream's writing end, as the peer writes its packets on it.
pub struct Writing {
    out: Arc<Mutex<Out>>,
    shared: Arc<Shared>,
}

impl Write for Writing {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut out = self.out.lock().expect("not poisoned");
        let written = out.write_some(bytes, &self.shared)?;
        out.boundaries.follow(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Writing {
    /// Ends the stream this way, once the peer is done: its end goes, and
    /// nothing after it, so that the other side reads to its end, which it
    /// takes for this peer's being done.
    pub fn finish(self) -> io::Result<()> {
        let mut out = self.out.lock().expect("not poisoned");
        if !out.boundaries.between() {
            return Err(io::Error::other("the stream ended inside a packet"));
        }
        out.write_all(&[PacketType::StreamEnd as u8, 0, 0, 0, 0], &self.shared)?;
        let file = out.file.take().expect("not yet ended");
        // A socket is shut for writing, as its reading end stays open; a
        // pipe ends as its writing end closes.
        let _ = rustix::net::shutdown(&file, Shutdown::Write);
        Ok(())
    }
}

impl Drop for Writing {
    /// Gone without its end, as when the server stops: the stream is taken
    /// for broken, so that the other side sees it so.
    fn drop(&mut self) {
        let mut out = self.out.lock().expect("not poisoned");
        if out.file.take().is_some() {
            self.shared
                .take_for_broken("its writer went without its end");
        }
    }
}

/// The stream's reading end, as the peer reads its packets from it: the
/// stream's own packets taken out.
pub struct Reading {
    deframer: Deframer,
    source: Source,
}

impl Read for Reading {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let Self { deframer, source } = self;
        match deframer.read(into, &mut |into| source.more(into)) {
            Ok(read) => Ok(read),
            Err(Fault::Read(error)) => Err(error),
            Err(Fault::Wrong(kind, why)) => Err(source.broke(kind, why)),
        }
    }
}

/// What the reader reads the stream from.
struct Source {
    file: File,
    shared: Arc<Shared>,
}

impl Source {
    /// Takes the stream for broken, and says why. One that ended is said so
    /// as aborted, never as an end: what reads packets takes that for the
    /// end between two of them.
    fn broke(&self, kind: io::ErrorKind, why: &str) -> io::Error {
        self.shared.take_for_broken(why);
        io::Error::new(kind, why.to_owned())
    }

    /// Waits until something can be read, or until the stream is taken for
    /// broken, or let go, or its scope ends.
    fn wait(&self) -> io::Result<()> {
        match self
            .shared
            .scope
            .wait(&self.file, PollFlags::IN, None, State::ReadConnection)
        {
            Ok(_) => Ok(()),
            Err(error) if Ended::of(&error).is_some() => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("the stream broke: {}", self.shared.why()),
            )),
            Err(error) => Err(error),
        }
    }

    /// Reads what comes next into `into`, waiting for it: what read, 0 at
    /// the stream's end. Once the wait ends with the scope, what came
    /// already is still taken, as it takes no waiting, so that the last a
    /// worker said before it ended, which ended its scope, is not lost; then
    /// the scope's end is the error, whatever else the read meets.
    fn more(&self, into: &mut [u8]) -> io::Result<usize> {
        loop {
            let waited = self.wait();
            match (&mut &self.file).read(into) {
                Ok(read) if read > 0 => {
                    self.shared.heard(read);
                    return Ok(read);
                }
                Ok(read) => {
                    waited?;
                    return Ok(read);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    waited?;
                }
                Err(error) => {
                    waited?;
                    return Err(self.failed(error));
                }
            }
        }
    }

    /// The stream failed under the reader.
    fn failed(&self, error: io::Error) -> io::Error {
        self.shared.take_for_broken(&error.to_string());
        io::Error::new(
            error.kind(),
            format!("the stream broke: {}", self.shared.why()),
        )
    }
}

/// What the reader makes of the stream's bytes: the peer's packets, handed
/// on as they are; the stream's own, taken out; its end; and why it broke.
/// It reads them through a buffer of its own, from what it is given to read
/// from, and does no I/O itself.
pub struct Deframer {
    buffer: Box<[u8]>,
    /// What is in the buffer, not yet taken.
    start: usize,
    end: usize,
    /// What is left of the packet being read, header and all, to hand on.
    left: usize,
    /// The other side's end came: the stream ends there, whether what
    /// carries it does or not, as a pipe a joining process still holds.
    ended: bool,
}

/// Why the deframer goes no further.
#[derive(Debug)]
pub enum Fault {
    /// What it reads from failed, and said why.
    Read(io::Error),
    /// The stream's bytes are wrong, or end too soon: an error of this kind,
    /// for this reason.
    Wrong(io::ErrorKind, &'static str),
}

impl From<io::Error> for Fault {
    fn from(error: io::Error) -> Self {
        Self::Read(error)
    }
}

impl Default for Deframer {
    fn default() -> Self {
        Self {
            buffer: vec![0; BUFFER].into_boxed_slice(),
            start: 0,
            end: 0,
            left: 0,
            ended: false,
        }
    }
}

impl Deframer {
    /// Takes `bytes`, read before the deframer was, as the first to come.
    pub fn already(&mut self, bytes: &[u8]) {
        if bytes.len() > self.buffer.len() - self.end {
            let mut buffer = vec![0; self.end + bytes.len()].into_boxed_slice();
            buffer[..self.end].copy_from_slice(&self.buffer[..self.end]);
            self.buffer = buffer;
        }
        self.buffer[self.end..self.end + bytes.len()].copy_from_slice(bytes);
        self.end += bytes.len();
    }

    /// Reads the peer's next bytes into `into`, as [`Read::read`] does: 0
    /// for an empty `into`, and once the stream's end came. `more` reads
    /// what comes next on the stream into what it is given, never empty,
    /// waiting for it: 0 at the end of what carries the stream.
    pub fn read(
        &mut self,
        into: &mut [u8],
        more: &mut impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> Result<usize, Fault> {
        if into.is_empty() || self.ended {
            return Ok(0);
        }
        loop {
            if self.left > 0 {
                let buffered = self.end - self.start;
                if buffered > 0 {
                    let taken = buffered.min(self.left).min(into.len());
                    into[..taken].copy_from_slice(&self.buffer[self.start..self.start + taken]);
                    self.start += taken;
                    self.left -= taken;
                    return Ok(taken);
                }
                // A large part of a packet is read straight to the peer.
                let wanted = self.left.min(into.len());
                if wanted >= STRAIGHT {
                    let read = more(&mut into[..wanted])?;
                    if read == 0 {
                        return Err(cut_short());
                    }
                    self.left -= read;
                    return Ok(read);
                }
                if self.fill(more)? == 0 {
                    return Err(cut_short());
                }
                continue;
            }
            // A packet's header, to see whose it is.
            while self.end - self.start < HEADER {
                if self.fill(more)? == 0 {
                    if self.start != self.end {
                        return Err(cut_short());
                    }
                    return Err(Fault::Wrong(
                        io::ErrorKind::ConnectionAborted,
                        "the stream ended before the other side was done",
                    ));
                }
            }
            let header = &self.buffer[self.start..self.start + HEADER];
            let kind = header[0];
            let length = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
            if kind == PacketType::StreamEnd as u8 {
                if length != 0 {
                    return Err(Fault::Wrong(
                        io::ErrorKind::InvalidData,
                        "a packet of the stream's own with a payload",
                    ));
                }
                // The end: nothing after it is read.
                self.start += HEADER;
                self.ended = true;
                return Ok(0);
            }
            self.left = HEADER + length;
        }
    }

    /// Reads more into the buffer, from `more`: what read, 0 at the end of
    /// what carries the stream. It is called only with room in the buffer:
    /// for a header, of which it holds less, or with nothing in it.
    fn fill(&mut self, more: &mut impl FnMut(&mut [u8]) -> io::Result<usize>) -> io::Result<usize> {
        if self.start == self.end {
            (self.start, self.end) = (0, 0);
        } else if self.end == self.buffer.len() {
            self.buffer.copy_within(self.start..self.end, 0);
            (self.start, self.end) = (0, self.end - self.start);
        }
        let read = more(&mut self.buffer[self.end..])?;
        self.end += read;
        Ok(read)
    }

    /// What is in the buffer, reading more if nothing is: what came before
    /// the stream's packets, to skip.
    fn fill_buf(
        &mut self,
        more: &mut impl FnMut(&mut [u8]) -> io::Result<usize>,
    ) -> io::Result<&[u8]> {
        if self.start == self.end {
            self.fill(more)?;
        }
        Ok(&self.buffer[self.start..self.end])
    }

    fn consume(&mut self, used: usize) {
        self.start += used;
    }
}

fn cut_short() -> Fault {
    Fault::Wrong(
        io::ErrorKind::ConnectionAborted,
        "the stream ended inside a packet",
    )
}

/// The reader as it is, its packets as they come, to skip to the start
/// mark first.
struct Raw<'a>(&'a mut Reading);

impl Read for Raw<'_> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let taken = available.len().min(into.len());
        into[..taken].copy_from_slice(&available[..taken]);
        self.consume(taken);
        Ok(taken)
    }
}

impl BufRead for Raw<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let Reading { deframer, source } = &mut *self.0;
        deframer.fill_buf(&mut |into| source.more(into))
    }

    fn consume(&mut self, used: usize) {
        self.0.deframer.consume(used);
    }
}

#[cfg(test)]
mod tests;
