//! The stream between a transfer's two servers, as each server has it: one
//! stream, which the client makes and hands to both, and which carries the
//! peers' packets as they are. It adds two packets of its own, which no peer
//! sees: a keepalive, which its writer sends once nothing went on it for
//! [`KEEPALIVE`], and its end, which its writer sends once its peer is done.
//! Its reader takes it for broken once nothing came on it for [`SILENCE`],
//! or once it ends without its end: the peer reading it then fails, and the
//! transfer with it. No other stream takes its place: a host has one SSH
//! connection, and one dropped fails the transfer, as with rsync. See the
//! draft, `docs/drafts/architecture-redesign.md`, "2026-10-25 重新決定".
//!
//! Both its ends are made not to block: its reader waits for what comes for
//! at most what is left of [`SILENCE`], and its writer, waiting for room,
//! stops once the stream is taken for broken, even on a pipe, which cannot
//! be shut down as a socket is.

use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{FileType, OFlags};
use rustix::net::Shutdown;

use super::start;
use crate::profile::{self, State};
use crate::protocol::PacketType;

/// How long nothing may be written on the stream before its writer says it
/// is alive.
pub const KEEPALIVE: Duration = Duration::from_secs(5);

/// How long nothing may come on the stream before it is taken for broken.
pub const SILENCE: Duration = Duration::from_secs(20);

/// A packet's header: its type, then its payload's length, four bytes,
/// little-endian.
const HEADER: usize = 5;

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
}

impl Ends {
    /// A socket, read and written both, its packets from the first byte.
    pub fn socket(socket: UnixStream) -> io::Result<Self> {
        Ok(Self {
            input: File::from(OwnedFd::from(socket.try_clone()?)),
            output: File::from(OwnedFd::from(socket)),
            held: None,
            marked: false,
        })
    }
}

/// What the stream's reader, its writer, its keeper, and whoever asks what
/// it carried share.
struct Shared {
    /// Taken for broken, or done with: a writer waiting for room stops.
    broken: AtomicBool,
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
    /// Takes the stream for broken: shut down both ways, so that the other
    /// side sees it so, and a writer here waiting for room stops.
    fn take_for_broken(&self) {
        self.broken.store(true, Ordering::SeqCst);
        if let Some(socket) = &self.shut {
            let _ = rustix::net::shutdown(socket, Shutdown::Both);
        }
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
}

/// Starts a stream on its `ends`: its reader, which skips to the start mark
/// first on one marked; its writer; what it carried; and its keeper, a
/// thread that keeps it alive while nothing goes on it.
pub fn start(ends: Ends) -> io::Result<(Reading, Writing, Carried)> {
    let silence = crate::inject::silence().unwrap_or(SILENCE);
    start_with(ends, KEEPALIVE.min(silence / 4), silence)
}

/// [`start`], with the times given: for tests.
fn start_with(
    ends: Ends,
    keepalive: Duration,
    silence: Duration,
) -> io::Result<(Reading, Writing, Carried)> {
    let Ends {
        input,
        output,
        held,
        marked,
    } = ends;
    for end in [&input, &output] {
        let flags = rustix::fs::fcntl_getfl(end)?;
        rustix::fs::fcntl_setfl(end, flags | OFlags::NONBLOCK)?;
    }
    let socket = FileType::from_raw_mode(rustix::fs::fstat(&input)?.st_mode) == FileType::Socket;
    let shared = Arc::new(Shared {
        broken: AtomicBool::new(false),
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
        header: [0; HEADER],
        filled: 0,
        left: 0,
        last: Instant::now(),
    }));
    {
        let (out, shared) = (Arc::clone(&out), Arc::clone(&shared));
        std::thread::spawn(move || keep(&out, &shared, keepalive));
    }
    let mut reading = Reading {
        file: input,
        buffer: vec![0; BUFFER].into_boxed_slice(),
        start: 0,
        end: 0,
        left: 0,
        ended: false,
        silence,
        shared: Arc::clone(&shared),
    };
    if marked {
        let skipped = start::skip(&mut Raw(&mut reading))?;
        if !skipped.found {
            return Err(reading.broke(
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

/// The keeper: once nothing was written on the stream for `keepalive`,
/// between two packets, it writes a keepalive, until the stream is done
/// with, or broken. A writer busy writing is left to it.
fn keep(out: &Mutex<Out>, shared: &Shared, keepalive: Duration) {
    let _thread = profile::thread("stream keeper");
    let every = (keepalive / 4).max(Duration::from_millis(10));
    loop {
        std::thread::sleep(every);
        if shared.broken.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut out) = out.try_lock() else {
            continue;
        };
        if out.file.is_none() {
            return;
        }
        if out.between() && out.last.elapsed() >= keepalive {
            let keepalive = [PacketType::Keepalive as u8, 0, 0, 0, 0];
            if out.write_all(&keepalive, shared).is_err() {
                return;
            }
        }
    }
}

/// The stream's writing end, under its lock: what the writer and the keeper
/// share.
struct Out {
    /// None once the stream's end is written.
    file: Option<File>,
    /// The header of the packet being written, as much of it as was, and
    /// what is left of its payload.
    header: [u8; HEADER],
    filled: usize,
    left: usize,
    /// When anything was last written.
    last: Instant,
}

impl Out {
    /// Between two packets: a keepalive, or the end, may go.
    fn between(&self) -> bool {
        self.filled == 0 && self.left == 0
    }

    /// Follows the packets through `bytes`, just written.
    fn follow(&mut self, mut bytes: &[u8]) {
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

    /// Writes some of `bytes`, waiting for room, until the stream is taken
    /// for broken.
    fn write_some(&mut self, bytes: &[u8], shared: &Shared) -> io::Result<usize> {
        loop {
            if shared.broken.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the stream broke",
                ));
            }
            let file = self.file.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "the stream's end is written")
            })?;
            match (&mut &*file).write(bytes) {
                Ok(written) => {
                    self.last = Instant::now();
                    shared.written.fetch_add(written as u64, Ordering::Relaxed);
                    return Ok(written);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // Woken now and then, to stop once the stream is taken
                    // for broken.
                    let timeout = Timespec {
                        tv_sec: 1,
                        tv_nsec: 0,
                    };
                    let mut polled = [PollFd::new(file, PollFlags::OUT)];
                    let _ = profile::wait(State::WriteConnection, || {
                        rustix::event::poll(&mut polled, Some(&timeout))
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    shared.take_for_broken();
                    return Err(io::Error::new(
                        error.kind(),
                        format!("the stream broke: {error}"),
                    ));
                }
            }
        }
    }

    fn write_all(&mut self, mut bytes: &[u8], shared: &Shared) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self.write_some(bytes, shared)?;
            self.follow(&bytes[..written]);
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
        out.follow(&bytes[..written]);
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
        if !out.between() {
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
            self.shared.take_for_broken();
        }
    }
}

/// The stream's reading end, as the peer reads its packets from it: the
/// stream's own packets taken out.
pub struct Reading {
    file: File,
    buffer: Box<[u8]>,
    /// What is in the buffer, not yet taken.
    start: usize,
    end: usize,
    /// What is left of the packet being read, header and all, to hand on.
    left: usize,
    /// The other side's end came: nothing may come after it.
    ended: bool,
    silence: Duration,
    shared: Arc<Shared>,
}

impl Reading {
    /// Takes the stream for broken, and says why. One that ended is said so
    /// as aborted, never as an end: what reads packets takes that for the
    /// end between two of them.
    fn broke(&self, kind: io::ErrorKind, why: &str) -> io::Error {
        self.shared.take_for_broken();
        io::Error::new(kind, why.to_owned())
    }

    /// Waits until something can be read, for at most what is left of the
    /// silence the stream is allowed.
    fn wait(&self) -> io::Result<()> {
        loop {
            let left = self
                .silence
                .saturating_sub(self.shared.last_heard().elapsed());
            if left.is_zero() {
                return Err(self.broke(
                    io::ErrorKind::TimedOut,
                    &format!(
                        "nothing came on the stream for {} s",
                        self.silence.as_secs()
                    ),
                ));
            }
            let timeout = Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            };
            let mut polled = [PollFd::new(&self.file, PollFlags::IN)];
            match profile::wait(State::ReadConnection, || {
                rustix::event::poll(&mut polled, Some(&timeout))
            }) {
                Ok(0) => {}
                Ok(_) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Reads what comes into `into`: what read, 0 at the stream's end.
    fn read_into(file: &File, into: &mut [u8]) -> io::Result<Option<usize>> {
        match (&mut &*file).read(into) {
            Ok(read) => Ok(Some(read)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Reads more into the buffer, waiting for it: what read, 0 at the
    /// stream's end.
    fn fill(&mut self) -> io::Result<usize> {
        if self.start == self.end {
            (self.start, self.end) = (0, 0);
        } else if self.end == self.buffer.len() {
            self.buffer.copy_within(self.start..self.end, 0);
            (self.start, self.end) = (0, self.end - self.start);
        }
        loop {
            self.wait()?;
            match Self::read_into(&self.file, &mut self.buffer[self.end..]) {
                Ok(Some(read)) => {
                    self.end += read;
                    if read > 0 {
                        self.shared.heard(read);
                    }
                    return Ok(read);
                }
                Ok(None) => {}
                Err(error) => return Err(self.failed(error)),
            }
        }
    }

    /// The stream failed under the reader.
    fn failed(&self, error: io::Error) -> io::Error {
        self.shared.take_for_broken();
        io::Error::new(error.kind(), format!("the stream broke: {error}"))
    }

    fn cut_short(&self) -> io::Error {
        self.broke(
            io::ErrorKind::ConnectionAborted,
            "the stream ended inside a packet",
        )
    }
}

impl Read for Reading {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if into.is_empty() {
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
                    loop {
                        self.wait()?;
                        match Self::read_into(&self.file, &mut into[..wanted]) {
                            Ok(Some(0)) => return Err(self.cut_short()),
                            Ok(Some(read)) => {
                                self.shared.heard(read);
                                self.left -= read;
                                return Ok(read);
                            }
                            Ok(None) => {}
                            Err(error) => return Err(self.failed(error)),
                        }
                    }
                }
                if self.fill()? == 0 {
                    return Err(self.cut_short());
                }
                continue;
            }
            // A packet's header, to see whose it is.
            while self.end - self.start < HEADER {
                if self.fill()? == 0 {
                    if self.start != self.end {
                        return Err(self.cut_short());
                    }
                    if self.ended {
                        return Ok(0);
                    }
                    return Err(self.broke(
                        io::ErrorKind::ConnectionAborted,
                        "the stream ended before the other side was done",
                    ));
                }
            }
            let header = &self.buffer[self.start..self.start + HEADER];
            let kind = header[0];
            let length = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
            if kind == PacketType::Keepalive as u8 || kind == PacketType::StreamEnd as u8 {
                if length != 0 {
                    return Err(self.broke(
                        io::ErrorKind::InvalidData,
                        "a packet of the stream's own with a payload",
                    ));
                }
                self.start += HEADER;
                if kind == PacketType::StreamEnd as u8 {
                    self.ended = true;
                }
                continue;
            }
            if self.ended {
                return Err(self.broke(
                    io::ErrorKind::InvalidData,
                    "a packet came after the stream's end",
                ));
            }
            self.left = HEADER + length;
        }
    }
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
        let reading = &mut *self.0;
        if reading.start == reading.end {
            reading.fill()?;
        }
        Ok(&reading.buffer[reading.start..reading.end])
    }

    fn consume(&mut self, used: usize) {
        self.0.start += used;
    }
}

#[cfg(test)]
mod tests;
