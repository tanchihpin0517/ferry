//! A wire: one connection, as each end has it, the only way Ferry reads or
//! writes one. The client's to a host's first session, to a server, to the
//! monitor on a host; a server's control connection, which is its standard
//! input and output; the stream between a transfer's two servers, which the
//! client makes and hands to both; and the link between the client's
//! monitor and a host's. How it was made, as a channel SSH forwards, a port
//! given its secret, a joining process, or a pipe or socket on this
//! machine, makes no difference once it is a wire. See
//! [Monitors](../../docs/design/architecture.md#monitors).
//!
//! Every wait on it is within its scope: its reader waits for what comes,
//! and its writer for room, until the scope ends, as a host's does once it
//! is lost; so whatever waits on it stops at once, even on a pipe, which
//! cannot be shut down as a socket is. Its ends are made not to block for
//! that, and taken back as they were when it is handed to a program.
//!
//! First, a wire carries bytes as they are: what a shell printed, the start
//! mark, the handshake, a port's secret. Then it goes on the stream layer,
//! [`Wire::stream`], which carries the peers' packets as they are, and adds
//! one of its own, which no one reading it sees: its end, which its writer
//! sends once done. Its reader takes it for broken once it ends without its
//! end: what reads it then fails, and the transfer with it. No other wire
//! takes its place: a host has one SSH connection, and one dropped fails the
//! transfer, as with rsync. Whether the far end is there at all is not the
//! wire's to judge, but the monitors', by their heartbeat; see
//! [`crate::monitor`]. The stream layer ends the wire's scope as it takes
//! the wire for broken, or as it is let go.
//!
//! What the reader makes of what comes, and where the writer is among the
//! packets it writes, are [`Deframer`]'s and [`Boundaries`]', which do no
//! I/O: so any bytes, however they come, can go through them, as they do in
//! the fuzzer.

use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::PollFlags;
use rustix::fs::{FileType, OFlags};
use rustix::net::Shutdown;

use super::child::Child;
use super::join::Silent;
use super::scope::{Ended, Scope, Waited};
use crate::profile::State;
use crate::protocol::PacketType;

/// A packet's header: its type, then its payload's length, four bytes,
/// little-endian.
pub const HEADER: usize = 5;

/// How much the reader reads ahead, for the headers of packets, and small
/// packets.
const BUFFER: usize = 64 * 1024;

/// How much a wire reads ahead before its packets, as for a line, or the
/// start mark.
const RAW_BUFFER: usize = 8 * 1024;

/// What is left of a packet that is read straight into the reader's
/// caller, rather than through its buffer.
const STRAIGHT: usize = 16 * 1024;

/// A connection, as each end has it: what it is read from and written to,
/// within its scope. It reads and writes bytes as they are, until it goes
/// on the stream layer.
pub struct Wire {
    input: Input,
    output: Output,
    /// Kept while the wire is in use: the socket of a process that handed
    /// it over, which waits for it to close.
    held: Option<UnixStream>,
    /// The process that carries it, the client's: `ssh`, or a joining
    /// process.
    carrier: Option<Child>,
}

impl Wire {
    /// The wire that `input` and `output` are, one socket both ways or two
    /// pipes, within `scope`: they are made not to block.
    pub(super) fn new(
        input: File,
        output: File,
        held: Option<UnixStream>,
        scope: &Scope,
    ) -> io::Result<Self> {
        for end in [&input, &output] {
            blocking(end, false)?;
        }
        Ok(Self {
            input: Input {
                file: input,
                scope: scope.clone(),
                buffer: Vec::new(),
                start: 0,
                deadline: None,
            },
            output: Output {
                file: Some(output),
                scope: scope.clone(),
            },
            held,
            carrier: None,
        })
    }

    /// The wire that this process's standard input and output are, within
    /// `scope`: a server's control connection.
    pub fn standard(scope: &Scope) -> io::Result<Self> {
        Self::new(
            File::from(io::stdin().as_fd().try_clone_to_owned()?),
            File::from(io::stdout().as_fd().try_clone_to_owned()?),
            None,
            scope,
        )
    }

    /// The wire that `socket` is, both ways.
    pub(super) fn socket(socket: UnixStream, scope: &Scope) -> io::Result<Self> {
        let input = File::from(OwnedFd::from(socket.try_clone()?));
        Self::new(input, File::from(OwnedFd::from(socket)), None, scope)
    }

    /// The wire that `stream`, a connection to a port, is, both ways.
    pub(super) fn tcp(stream: TcpStream, scope: &Scope) -> io::Result<Self> {
        let input = File::from(OwnedFd::from(stream.try_clone()?));
        Self::new(input, File::from(OwnedFd::from(stream)), None, scope)
    }

    /// The wire that the standard input and output of `carrier`, piped, are:
    /// the process carries it.
    pub(super) fn carried(mut carrier: Child, scope: &Scope) -> io::Result<Self> {
        let input = File::from(OwnedFd::from(carrier.stdout.take().expect("piped")));
        let output = File::from(OwnedFd::from(carrier.stdin.take().expect("piped")));
        let mut wire = Self::new(input, output, None, scope)?;
        wire.carrier = Some(carrier);
        Ok(wire)
    }

    /// The process that carries it, for the caller to end.
    pub(super) fn take_carrier(&mut self) -> Option<Child> {
        self.carrier.take()
    }

    /// The scope it waits in.
    pub(super) fn scope(&self) -> &Scope {
        &self.input.scope
    }

    /// From now on, what is read fails with [`Silent`] once nothing more
    /// came for `silence`, from now; or never, with none: as for a
    /// connection that has not said what it is for.
    pub fn within(&mut self, silence: Option<Duration>) {
        self.input.deadline = silence.map(|silence| (Instant::now() + silence, silence));
    }

    /// Skips to the start mark, past what a shell may have printed at the
    /// far end: one that ends before it never started.
    pub fn started(&mut self) -> io::Result<()> {
        if super::start::skip(self)?.found {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "the stream ended before it started",
        ))
    }

    /// Closes the way to the far end: it reads to its end. A socket is shut
    /// for writing, as its reading end stays open.
    pub(super) fn close_output(&mut self) {
        self.output.close();
    }

    /// The reading half and the writing half, for a thread each, as a
    /// session's lines are read while commands are written.
    pub fn split(self) -> (Input, Output) {
        (self.input, self.output)
    }

    /// Ends a wire that is not to be used: its scope ends, so that whoever
    /// waits on it stops; a socket is shut down; what carries it is ended.
    pub(super) fn end(self) {
        self.input.scope.end(LET_GO);
        let _ = rustix::net::shutdown(&self.input.file, Shutdown::Both);
        drop(self.input);
        drop(self.output);
        if let Some(carrier) = self.carrier {
            carrier.kill();
            carrier.wait();
        }
    }

    /// Hands the wire to the program `command` starts, as its standard
    /// input and output, as they were given, blocking: what is left of it is
    /// what must be kept until the program is done with it. One read past
    /// what was its own to read cannot be.
    pub fn give_to(self, command: &mut Command) -> io::Result<Held> {
        if !self.input.buffered().is_empty() {
            return Err(io::Error::other(
                "a connection read past what was its own cannot be handed over",
            ));
        }
        let output = self.output.file.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection's output is closed",
            )
        })?;
        for end in [&self.input.file, &output] {
            blocking(end, true)?;
        }
        command
            .stdin(Stdio::from(OwnedFd::from(self.input.file)))
            .stdout(Stdio::from(OwnedFd::from(output)));
        Ok(Held { _kept: self.held })
    }

    /// Goes on the stream layer: its reader, its writer, and what it
    /// carried. What it read ahead goes first. Once the wire's scope ends, so
    /// does the stream, and the stream ends it as it breaks, or is let go.
    pub fn stream(self) -> io::Result<(Reading, Writing, Carried)> {
        let scope = self.input.scope.clone();
        self.stream_in(scope)
    }

    /// As [`Wire::stream`], within `scope` rather than its own.
    pub(super) fn stream_in(self, scope: Scope) -> io::Result<(Reading, Writing, Carried)> {
        let (mut input, output, carried) = self.ready_in(scope)?;
        let mut deframer = Deframer::default();
        deframer.already(&std::mem::take(&mut input.before));
        Ok((
            Reading {
                deframer,
                source: input,
            },
            Writing {
                out: Arc::new(Mutex::new(Out {
                    wire: output,
                    boundaries: Boundaries::default(),
                })),
                shared: Arc::clone(&carried.0),
            },
            carried,
        ))
    }

    /// Is ready, its two ends handed to what carries the pipes on it: its
    /// reading end, what it read ahead first; its writing end; and what it
    /// carried. Once the wire's scope ends, so do they, and they end it as
    /// the wire breaks, or is let go. See [`super::pipes`].
    pub fn ready(self) -> io::Result<(WireIn, WireOut, Carried)> {
        let scope = self.input.scope.clone();
        self.ready_in(scope)
    }

    /// As [`Wire::ready`], within `scope` rather than its own.
    pub(super) fn ready_in(self, scope: Scope) -> io::Result<(WireIn, WireOut, Carried)> {
        let before = self.input.buffered().to_vec();
        let input = self.input.file;
        let output = self.output.file.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection's output is closed",
            )
        })?;
        let socket =
            FileType::from_raw_mode(rustix::fs::fstat(&input)?.st_mode) == FileType::Socket;
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
            _held: self.held,
        });
        shared
            .read
            .fetch_add(before.len() as u64, Ordering::Relaxed);
        Ok((
            WireIn {
                file: input,
                before,
                shared: Arc::clone(&shared),
            },
            WireOut {
                file: Some(output),
                shared: Arc::clone(&shared),
            },
            Carried(shared),
        ))
    }
}

impl Read for Wire {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.input.read(into)
    }
}

impl BufRead for Wire {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.input.fill_buf()
    }

    fn consume(&mut self, used: usize) {
        self.input.consume(used);
    }
}

impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What is left of a wire handed to a program: kept until the program is
/// done with it, as a joining process waits for it to close.
pub struct Held {
    _kept: Option<UnixStream>,
}

/// Why a wire, or a stream on it, stopped being read as it was let go.
pub(super) const LET_GO: &str = "the connection was let go";

/// Makes `end` block, or not.
fn blocking(end: &File, blocks: bool) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(end)?;
    let flags = if blocks {
        flags - OFlags::NONBLOCK
    } else {
        flags | OFlags::NONBLOCK
    };
    rustix::fs::fcntl_setfl(end, flags)?;
    Ok(())
}

/// What a wire is read from, before its packets: what comes waited for
/// within its scope, and its deadline, if it has one. Read, it reads no
/// more than it is asked for, so that what comes after is left for whoever
/// it is handed to; it reads ahead only as a [`BufRead`].
pub struct Input {
    file: File,
    scope: Scope,
    /// What was read ahead, from `start` on.
    buffer: Vec<u8>,
    start: usize,
    /// When what is read fails, and the silence that says so.
    deadline: Option<(Instant, Duration)>,
}

impl Input {
    /// What was read ahead, not yet taken.
    fn buffered(&self) -> &[u8] {
        &self.buffer[self.start..]
    }

    /// Reads what comes next into `into`, waiting for it: 0 at its end.
    fn more(&self, into: &mut [u8]) -> io::Result<usize> {
        loop {
            let deadline = self.deadline.map(|(deadline, _)| deadline);
            match self
                .scope
                .wait(&self.file, PollFlags::IN, deadline, State::ReadConnection)?
            {
                Waited::Ready => {}
                Waited::TimedOut => {
                    let silence = self
                        .deadline
                        .map(|(_, silence)| silence)
                        .unwrap_or_default();
                    return Err(io::Error::new(io::ErrorKind::TimedOut, Silent(silence)));
                }
            }
            match (&self.file).read(into) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                read => return read,
            }
        }
    }
}

impl Read for Input {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let buffered = self.buffered();
        if buffered.is_empty() {
            return self.more(into);
        }
        let taken = buffered.len().min(into.len());
        into[..taken].copy_from_slice(&buffered[..taken]);
        self.consume(taken);
        Ok(taken)
    }
}

impl BufRead for Input {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.buffered().is_empty() {
            let mut buffer = std::mem::take(&mut self.buffer);
            buffer.resize(RAW_BUFFER, 0);
            let read = self.more(&mut buffer);
            buffer.truncate(*read.as_ref().unwrap_or(&0));
            (self.buffer, self.start) = (buffer, 0);
            read?;
        }
        Ok(self.buffered())
    }

    fn consume(&mut self, used: usize) {
        self.start = (self.start + used).min(self.buffer.len());
    }
}

/// What a wire is written to, before its packets: room waited for within
/// its scope.
pub struct Output {
    /// None once closed.
    file: Option<File>,
    scope: Scope,
}

impl Output {
    /// Closes it: the far end reads to its end. A socket is shut for
    /// writing, as its reading end stays open.
    fn close(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = rustix::net::shutdown(&file, Shutdown::Write);
        }
    }
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let file = self.file.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection's output is closed",
            )
        })?;
        loop {
            self.scope.check()?;
            match (&mut &*file).write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.scope
                        .wait(file, PollFlags::OUT, None, State::WriteConnection)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                written => return written,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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

    /// Takes the wire for broken, for `why` unless it was already: shut down
    /// both ways, so that the far end sees it so, and every wait on it ends.
    pub(super) fn take_for_broken(&self, why: &str) {
        self.0.take_for_broken(why);
    }

    /// Ends the wire's scope, for `why` unless it ended already: every wait
    /// on it ends, its reader's too, without shutting it.
    pub(super) fn end_scope(&self, why: &str) {
        self.0.scope.end(why);
    }
}

/// The stream's writing end, under its lock.
struct Out {
    wire: WireOut,
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
    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self.wire.write_some(bytes)?;
            self.boundaries.follow(&bytes[..written]);
            bytes = &bytes[written..];
        }
        Ok(())
    }
}

/// A ready wire's writing end: what is written goes as it is, each write
/// waiting for room, until the wire is taken for broken.
pub struct WireOut {
    /// None once it is shut.
    file: Option<File>,
    shared: Arc<Shared>,
}

impl WireOut {
    /// Writes some of `bytes`, waiting for room, until the wire is taken
    /// for broken.
    fn write_some(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let shared = &*self.shared;
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

    /// Writes all of `bytes`, waiting for room.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self.write_some(bytes)?;
            bytes = &bytes[written..];
        }
        Ok(())
    }

    /// Shuts the way to the far end, which reads to its end: a socket is
    /// shut for writing, as its reading end stays open; a pipe ends as its
    /// writing end closes. False if it was shut already.
    pub fn shut(&mut self) -> bool {
        let Some(file) = self.file.take() else {
            return false;
        };
        let _ = rustix::net::shutdown(&file, Shutdown::Write);
        true
    }

    /// Takes the wire for broken, for `why` unless it was already.
    pub fn take_for_broken(&self, why: &str) {
        self.shared.take_for_broken(why);
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
        let written = out.wire.write_some(bytes)?;
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
        out.write_all(&[PacketType::StreamEnd as u8, 0, 0, 0, 0])?;
        out.wire.shut();
        Ok(())
    }
}

impl Drop for Writing {
    /// Gone without its end, as when the server stops: the stream is taken
    /// for broken, so that the other side sees it so.
    fn drop(&mut self) {
        let mut out = self.out.lock().expect("not poisoned");
        if out.wire.file.take().is_some() {
            self.shared
                .take_for_broken("its writer went without its end");
        }
    }
}

/// The stream's reading end, as the peer reads its packets from it: the
/// stream's own packets taken out.
pub struct Reading {
    deframer: Deframer,
    source: WireIn,
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

/// A ready wire's reading end: what comes, as it comes, what was read ahead
/// of it first, each read waiting for it, until the wire is taken for
/// broken, or let go, or its scope ends.
pub struct WireIn {
    file: File,
    /// What was read ahead before the wire was ready, to go first.
    before: Vec<u8>,
    shared: Arc<Shared>,
}

impl WireIn {
    /// Reads what comes next into `into`, never empty, waiting for it: what
    /// read, 0 at the wire's end. See [`WireIn::more`].
    pub fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if !self.before.is_empty() {
            let taken = self.before.len().min(into.len());
            into[..taken].copy_from_slice(&self.before[..taken]);
            self.before.drain(..taken);
            return Ok(taken);
        }
        self.more(into)
    }

    /// Takes the wire for broken, for `why` unless it was already: the
    /// error to say, of `kind`, saying why as first said.
    pub fn take_for_broken(&self, kind: io::ErrorKind, why: &str) -> io::Error {
        self.shared.take_for_broken(why);
        io::Error::new(kind, format!("the stream broke: {}", self.shared.why()))
    }

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
}

fn cut_short() -> Fault {
    Fault::Wrong(
        io::ErrorKind::ConnectionAborted,
        "the stream ended inside a packet",
    )
}

#[cfg(test)]
mod tests;
