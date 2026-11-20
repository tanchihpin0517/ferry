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
//! mark, the handshake, a port's secret: it is being prepared. Then it is
//! ready, [`Wire::ready`], its ends handed to what carries the pipes of its
//! kind of connection, from then the only thing on it; see
//! [`super::pipes`]. A connection that ends before each of its pipes did
//! broke: what reads it then fails, and the transfer with it. No other wire
//! takes its place: a host has one SSH connection, and one dropped fails the
//! transfer, as with rsync. Whether the far end is there at all is not the
//! wire's to judge, but the monitors', by their heartbeat; see
//! [`crate::monitor`]. A ready wire's scope ends as it is taken for broken,
//! or let go.

use std::fs::File;
use std::io::{self, BufRead, IoSlice, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustix::event::PollFlags;
use rustix::fs::{FileType, OFlags};
use rustix::net::Shutdown;

use super::child::Child;
use super::join::Silent;
use super::scope::{Ended, Scope, Waited};
use crate::profile::State;

/// How much a wire reads ahead before its packets, as for a line, or the
/// start mark.
const RAW_BUFFER: usize = 8 * 1024;

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
    pub(crate) fn socket(socket: UnixStream, scope: &Scope) -> io::Result<Self> {
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
        let socket = crate::fsys::fstat(&input)?.kind == FileType::Socket;
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

/// What a broken stream's error says before why, once: as the wire says
/// it, and as the pipes say it of a wire.
pub(super) const BROKE: &str = "the stream broke: ";

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

    /// Reads what comes next into `into`, waiting for it, as [`read_within`]
    /// does: 0 at its end; silent past its deadline.
    fn more(&self, into: &mut [u8]) -> io::Result<usize> {
        let deadline = self.deadline.map(|(deadline, _)| deadline);
        read_within(&self.file, &self.scope, deadline, into).map_err(|unread| match unread {
            Unread::Ended(error) | Unread::Failed(error) => error,
            Unread::TimedOut => {
                let silence = self
                    .deadline
                    .map(|(_, silence)| silence)
                    .unwrap_or_default();
                io::Error::new(io::ErrorKind::TimedOut, Silent(silence))
            }
        })
    }
}

/// What a read of a wire came to, short of what came.
enum Unread {
    /// Its scope ended, and nothing had come: the scope's end.
    Ended(io::Error),
    /// Its deadline passed, and nothing had come.
    TimedOut,
    /// Reading it failed.
    Failed(io::Error),
}

/// Reads what comes on `file` next into `into`, waiting for it within
/// `scope`, until `deadline`, if any: what read, 0 at its end. Once the wait
/// ends with the scope, what came already is still taken, as it takes no
/// waiting, so that the last a far end said before it ended, which ended the
/// scope, is not lost, as a server's answer is the last it says; then the
/// scope's end is the error, whatever else the read meets. A wire is read so
/// before it is ready, and after.
fn read_within(
    file: &File,
    scope: &Scope,
    deadline: Option<Instant>,
    into: &mut [u8],
) -> Result<usize, Unread> {
    loop {
        let ended = match scope.wait(file, PollFlags::IN, deadline, State::ReadConnection) {
            Ok(Waited::Ready) => None,
            Ok(Waited::TimedOut) => return Err(Unread::TimedOut),
            Err(error) if Ended::of(&error).is_some() => Some(error),
            Err(error) => return Err(Unread::Failed(error)),
        };
        let unread = match (&mut &*file).read(into) {
            Ok(read) if read > 0 => return Ok(read),
            Ok(read) => match ended {
                None => return Ok(read),
                Some(_) => None,
            },
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                None
            }
            Err(error) => Some(error),
        };
        match (ended, unread) {
            (Some(ended), _) => return Err(Unread::Ended(ended)),
            (None, Some(error)) => return Err(Unread::Failed(error)),
            (None, None) => {}
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
        write_within(file, &self.scope, &[IoSlice::new(bytes)])
            .map_err(|unwritten| match unwritten {
                Unwritten::Ended(error) | Unwritten::Failed(error) => error,
            })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a write of a wire came to, short of what was written.
enum Unwritten {
    /// Its scope ended: the scope's end.
    Ended(io::Error),
    /// Writing it failed.
    Failed(io::Error),
}

/// Writes some of `parts`, in order, in one write, on `file`, waiting for
/// room within `scope`: how many bytes went. A wire is written so before it
/// is ready, and after.
fn write_within(file: &File, scope: &Scope, parts: &[IoSlice]) -> Result<usize, Unwritten> {
    loop {
        scope.check().map_err(Unwritten::Ended)?;
        match (&mut &*file).write_vectored(parts) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                match scope.wait(file, PollFlags::OUT, None, State::WriteConnection) {
                    // The scope's end is seen as it looks again.
                    Err(error) if Ended::of(&error).is_none() => {
                        return Err(Unwritten::Failed(error));
                    }
                    _ => {}
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Unwritten::Failed(error)),
            Ok(written) => return Ok(written),
        }
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

/// A ready wire's writing end: what is written goes as it is, each write
/// waiting for room, until the wire is taken for broken.
pub struct WireOut {
    /// None once it is shut.
    file: Option<File>,
    shared: Arc<Shared>,
}

impl WireOut {
    /// Writes some of `parts`, in order, waiting for room, until the wire is
    /// taken for broken.
    fn write_some(&mut self, parts: &[IoSlice]) -> io::Result<usize> {
        let shared = &*self.shared;
        let broke = |kind| io::Error::new(kind, format!("{BROKE}{}", shared.why()));
        if shared.scope.ended().is_some() {
            return Err(broke(io::ErrorKind::BrokenPipe));
        }
        let file = self.file.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "the stream's end is written")
        })?;
        match write_within(file, &shared.scope, parts) {
            Ok(written) => {
                shared.written.fetch_add(written as u64, Ordering::Relaxed);
                Ok(written)
            }
            Err(Unwritten::Ended(_)) => Err(broke(io::ErrorKind::BrokenPipe)),
            Err(Unwritten::Failed(error)) => {
                shared.take_for_broken(&error.to_string());
                Err(broke(error.kind()))
            }
        }
    }

    /// Writes all of `parts`, one after another, waiting for room: as many
    /// at once as a write takes, rather than each copied into one first.
    pub fn write_all(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        let mut slices: Vec<IoSlice> = parts
            .iter()
            .filter(|part| !part.is_empty())
            .map(|part| IoSlice::new(part))
            .collect();
        let mut slices = &mut slices[..];
        while !slices.is_empty() {
            let written = self.write_some(slices)?;
            IoSlice::advance_slices(&mut slices, written);
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
        io::Error::new(kind, format!("{BROKE}{}", self.shared.why()))
    }

    /// Reads what comes next into `into`, waiting for it, as [`read_within`]
    /// does: what read, 0 at the stream's end; once the stream is taken for
    /// broken, or let go, or its scope ends, why.
    fn more(&self, into: &mut [u8]) -> io::Result<usize> {
        match read_within(&self.file, &self.shared.scope, None, into) {
            Ok(read) => {
                if read > 0 {
                    self.shared.heard(read);
                }
                Ok(read)
            }
            Err(Unread::Ended(_) | Unread::TimedOut) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("{BROKE}{}", self.shared.why()),
            )),
            Err(Unread::Failed(error)) => Err(self.failed(error)),
        }
    }

    /// The stream failed under the reader.
    fn failed(&self, error: io::Error) -> io::Error {
        self.shared.take_for_broken(&error.to_string());
        io::Error::new(error.kind(), format!("{BROKE}{}", self.shared.why()))
    }
}

#[cfg(test)]
mod tests;
