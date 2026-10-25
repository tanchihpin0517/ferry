//! The session's driver, and its streams' executors: one thread tells the
//! session what happened, as it comes, and does what the session says; for
//! each stream, one thread reads its frames, and one writes them. The
//! session does no I/O; these do nothing else. See the draft,
//! `docs/drafts/architecture-redesign.md`, sections 3 and 5.
//!
//! A stream comes as its two ends, numbered by whoever made it, the client,
//! with the same number at both servers, and never numbered so again. Its
//! reader hands the driver each frame as it comes, so that a stream is
//! always read dry: what comes is bounded by the peers' protocol, never by a
//! reader waiting. Its writer writes what the session hands it, and says how
//! much it wrote, once that is written out. A stream broken is shut down
//! both ways at once, whatever its writer still has, so that its other end
//! sees it broken too, and its threads end; one ended is closed once its
//! writer has written all it was handed.
//!
//! What the session delivers goes to the peer, as do its giving up and its
//! end. That a stream broke, and why when one broke the session's rules,
//! goes to whoever follows the streams: the client.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use rustix::net::Shutdown;

use super::frame::{self, Unread};
use super::session::{Bytes, Command, Event, FRAGMENT, Frame, Session, StreamId};
use crate::profile::{self, State};

/// How many bytes a stream's reader and writer each buffer: two fragments.
const BUFFER: usize = 2 * FRAGMENT;

/// How many things heard or asked the session is told, at most, before it
/// is told that nothing more is to come now: so that what is owed the other
/// side goes now and then, even while frames keep coming.
const BEFORE_IDLE: usize = 256;

/// A stream's two ends, as handed over.
pub struct Ends {
    /// What its frames are read from, and written to: one socket, both
    /// ways, or two pipes.
    pub input: File,
    pub output: File,
    /// Kept while the stream is in use, and let go once it is closed: the
    /// socket of a process that handed the stream over, which waits for it
    /// to close.
    pub held: Option<UnixStream>,
}

impl Ends {
    /// A socket, read and written both.
    pub fn socket(socket: UnixStream) -> io::Result<Self> {
        Ok(Self {
            input: File::from(OwnedFd::from(socket.try_clone()?)),
            output: File::from(OwnedFd::from(socket)),
            held: None,
        })
    }
}

/// What the session hands the peer.
#[derive(Debug)]
pub enum Delivery {
    /// The other peer's next control message.
    Control(Bytes),
    /// A block of the other peer's.
    Block(Bytes),
    /// The other peer is done: all it sent is delivered, and nothing more
    /// comes of it.
    Ended,
    /// The session gave up: no stream was joined in the grace. Nothing more
    /// comes.
    Lost,
    /// The session ended: all each side sent is had.
    Closed,
}

/// What the session tells whoever follows the streams.
#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    /// A stream broke: another may be joined in its place.
    Broke(StreamId),
    /// Why a stream was taken for broken, when it broke the session's
    /// rules: a fault of the other side's, or of whoever joined it.
    Warn(String),
}

/// What the driver is asked, by the peer or by whoever hands it streams.
enum Ask {
    Join(StreamId, Ends),
    Tell(Event),
}

/// What a stream's reader or writer says.
enum Said {
    Frame(StreamId, Frame),
    /// The stream ended or failed; with why, if what came is not a frame.
    Broken(StreamId, Option<String>),
    Wrote(StreamId, usize),
    /// Its writer is done: it wrote all it was handed and closed its end,
    /// or failed.
    Finished(StreamId),
}

/// Where the peer, and whoever hands the session its streams, reach the
/// driver. Once every handle is dropped, the driver shuts each stream down,
/// and ends.
#[derive(Clone)]
pub struct Handle {
    asks: Sender<Ask>,
}

impl Handle {
    /// Hands the session a stream, numbered as at the other server.
    pub fn join(&self, stream: StreamId, ends: Ends) {
        let _ = self.asks.send(Ask::Join(stream, ends));
    }

    /// The peer's control message, for the other peer.
    pub fn control(&self, message: Vec<u8>) {
        let _ = self.asks.send(Ask::Tell(Event::Control(message)));
    }

    /// The peer's block, for the other peer.
    pub fn block(&self, bytes: Bytes) {
        let _ = self.asks.send(Ask::Tell(Event::Block(bytes)));
    }

    /// The peer is done: once all it sent is had, the streams end.
    pub fn close(&self) {
        let _ = self.asks.send(Ask::Tell(Event::Close));
    }
}

/// Starts the driver of the session `id`, which waits a grace from now for
/// its first stream. Returns where to reach it, what it delivers to the
/// peer, what it tells the client, and its thread.
pub fn start(id: [u8; 16]) -> (Handle, Receiver<Delivery>, Receiver<Notice>, JoinHandle<()>) {
    let (asks, asked) = crossbeam_channel::unbounded();
    let (deliver, deliveries) = crossbeam_channel::unbounded();
    let (notify, notices) = crossbeam_channel::unbounded();
    let session = Session::new(id, Instant::now());
    let thread = std::thread::spawn(move || {
        let _thread = profile::thread("session");
        let (said, heard) = crossbeam_channel::unbounded();
        let mut driver = Driver {
            session,
            streams: HashMap::new(),
            said,
            deliver,
            notify,
        };
        driver.run(&asked, &heard);
    });
    (Handle { asks }, deliveries, notices, thread)
}

/// The driver's thread: the session, and each stream's executors.
struct Driver {
    session: Session,
    streams: HashMap<StreamId, Executors>,
    /// Where each stream's reader and writer say what they did.
    said: Sender<Said>,
    deliver: Sender<Delivery>,
    notify: Sender<Notice>,
}

/// A stream's executors, as the driver holds them.
struct Executors {
    /// Where its writer takes its frames; none once it is closed.
    frames: Option<Sender<Frame>>,
    /// Its ends again, to shut them down should it break.
    shut: [File; 2],
    _held: Option<UnixStream>,
    /// Its writer is done.
    finished: bool,
}

/// What the driver's thread takes next.
enum Next {
    Heard(Said),
    Asked(Ask),
    Tick,
    Gone,
}

impl Driver {
    fn run(&mut self, asked: &Receiver<Ask>, heard: &Receiver<Said>) {
        loop {
            // What came is told first, then that nothing more has: what is
            // owed the other side then goes in one frame. What the streams
            // say and what is asked are taken in turn, so that neither waits
            // while the other keeps coming: the peer's control messages, as
            // credit, while blocks come.
            for _ in 0..BEFORE_IDLE {
                let heard = heard.try_recv().map(|said| self.hear(said)).is_ok();
                let asked = match asked.try_recv() {
                    Ok(ask) => {
                        self.ask(ask);
                        true
                    }
                    Err(TryRecvError::Empty) => false,
                    Err(TryRecvError::Disconnected) => return self.shut_all(),
                };
                if !heard && !asked {
                    break;
                }
            }
            self.tell(Event::Idle);
            let timer = self
                .session
                .next_deadline()
                .map_or_else(crossbeam_channel::never, crossbeam_channel::at);
            let next = profile::wait(State::Work, || {
                crossbeam_channel::select! {
                    recv(heard) -> said => Next::Heard(said.expect("the driver holds a sender")),
                    recv(asked) -> ask => ask.map_or(Next::Gone, Next::Asked),
                    recv(timer) -> _ => Next::Tick,
                }
            });
            match next {
                Next::Heard(said) => self.hear(said),
                Next::Asked(ask) => self.ask(ask),
                Next::Tick => self.tell(Event::Tick),
                Next::Gone => return self.shut_all(),
            }
        }
    }

    fn ask(&mut self, ask: Ask) {
        match ask {
            Ask::Join(id, ends) => self.join(id, ends),
            Ask::Tell(event) => self.tell(event),
        }
    }

    fn hear(&mut self, said: Said) {
        match said {
            Said::Frame(id, frame) => self.tell(Event::Frame(id, frame)),
            Said::Wrote(id, bytes) => self.tell(Event::Wrote(id, bytes)),
            Said::Broken(id, wrong) => {
                if let Some(why) = wrong {
                    let _ = self
                        .notify
                        .send(Notice::Warn(format!("stream {id}: {why}")));
                }
                self.tell(Event::Broken(id));
            }
            Said::Finished(id) => {
                if let Some(stream) = self.streams.get_mut(&id) {
                    stream.finished = true;
                }
                self.sweep();
            }
        }
    }

    /// Starts a stream's reader and writer, and tells the session it
    /// joined; one numbered as another still held is refused, its ends
    /// closed.
    fn join(&mut self, id: StreamId, ends: Ends) {
        let Ends {
            input,
            output,
            held,
        } = ends;
        if self.streams.contains_key(&id) {
            let warned = format!("stream {id} was joined again, as another still there");
            let _ = self.notify.send(Notice::Warn(warned));
            return;
        }
        let shut = match (input.try_clone(), output.try_clone()) {
            (Ok(input), Ok(output)) => [input, output],
            (Err(error), _) | (_, Err(error)) => {
                let warned = format!("stream {id} could not be kept: {error}");
                let _ = self.notify.send(Notice::Warn(warned));
                return;
            }
        };
        let (frames, queue) = crossbeam_channel::unbounded();
        let said = self.said.clone();
        std::thread::spawn(move || read(id, input, &said));
        let said = self.said.clone();
        std::thread::spawn(move || write(id, output, &queue, &said));
        self.streams.insert(
            id,
            Executors {
                frames: Some(frames),
                shut,
                _held: held,
                finished: false,
            },
        );
        self.tell(Event::Joined(id));
    }

    /// Tells the session `event`, now, and does what it says.
    fn tell(&mut self, event: Event) {
        for command in self.session.on(event, Instant::now()) {
            self.carry_out(command);
        }
        self.sweep();
    }

    fn carry_out(&mut self, command: Command) {
        match command {
            Command::Write(id, frame) => {
                if let Some(frames) = self
                    .streams
                    .get(&id)
                    .and_then(|stream| stream.frames.as_ref())
                {
                    let _ = frames.send(frame);
                }
            }
            // Its writer writes what it was handed, then closes its end.
            Command::Close(id) => {
                if let Some(stream) = self.streams.get_mut(&id) {
                    stream.frames = None;
                }
            }
            Command::Broke(id) => {
                if let Some(stream) = self.streams.get_mut(&id) {
                    stream.frames = None;
                    shut(&stream.shut);
                }
                let _ = self.notify.send(Notice::Broke(id));
            }
            Command::Warn(warned) => {
                let _ = self.notify.send(Notice::Warn(warned));
            }
            Command::Deliver(message) => {
                let _ = self.deliver.send(Delivery::Control(message));
            }
            Command::DeliverBlock(bytes) => {
                let _ = self.deliver.send(Delivery::Block(bytes));
            }
            Command::Ended => {
                let _ = self.deliver.send(Delivery::Ended);
            }
            Command::Lost => {
                let _ = self.deliver.send(Delivery::Lost);
            }
            Command::Closed => {
                let _ = self.deliver.send(Delivery::Closed);
            }
        }
    }

    /// Lets go of each stream closed whose writer is done: its ends, and
    /// what was held for it.
    fn sweep(&mut self) {
        self.streams
            .retain(|_, stream| stream.frames.is_some() || !stream.finished);
    }

    /// Every handle is gone: each stream is shut down, and the driver ends.
    /// Once the session is closed, what each stream was handed is already
    /// written.
    fn shut_all(&mut self) {
        for stream in self.streams.values() {
            shut(&stream.shut);
        }
        self.streams.clear();
    }
}

/// Shuts a stream down both ways, whatever its writer still has: its
/// threads end, and its other end sees it so. A pipe cannot be: it ends as
/// its other end does.
fn shut(ends: &[File; 2]) {
    for end in ends {
        let _ = rustix::net::shutdown(end, Shutdown::Both);
    }
}

/// A stream's reader: hands the driver each frame as it comes, until the
/// stream ends or fails, or what comes is not a frame.
fn read(id: StreamId, input: File, said: &Sender<Said>) {
    let _thread = profile::thread("stream reader");
    let mut reader = BufReader::with_capacity(BUFFER, input);
    let wrong = loop {
        match profile::wait(State::ReadConnection, || frame::read(&mut reader)) {
            Ok(Some(frame)) => {
                if said.send(Said::Frame(id, frame)).is_err() {
                    return;
                }
            }
            Ok(None) | Err(Unread::Failed(_)) => break None,
            Err(Unread::Wrong(why)) => break Some(why),
        }
    };
    let _ = said.send(Said::Broken(id, wrong));
}

/// A stream's writer: writes the frames it is handed, and says how many
/// bytes once they are written out, not only buffered. Once let go, it
/// shuts its end for writing, so that the other end reads to its end; if
/// writing fails, it says the stream broke.
fn write(id: StreamId, output: File, frames: &Receiver<Frame>, said: &Sender<Said>) {
    let _thread = profile::thread("stream writer");
    let mut writer = BufWriter::with_capacity(BUFFER, &output);
    let mut unsaid = 0;
    let written = loop {
        let frame = match frames.try_recv() {
            Ok(frame) => frame,
            Err(waiting) => {
                // Nothing more waits: what is buffered goes out before the
                // stream would idle.
                if profile::wait(State::WriteConnection, || io::Write::flush(&mut writer)).is_err()
                {
                    break false;
                }
                if unsaid > 0 {
                    let _ = said.send(Said::Wrote(id, unsaid));
                    unsaid = 0;
                }
                if waiting == TryRecvError::Disconnected {
                    break true;
                }
                match profile::wait(State::Work, || frames.recv()) {
                    Ok(frame) => frame,
                    Err(_) => break true,
                }
            }
        };
        unsaid += frame.size();
        if profile::wait(State::WriteConnection, || frame::write(&mut writer, &frame)).is_err() {
            break false;
        }
    };
    // Nothing is left buffered, unless writing failed: none of it then is
    // tried again.
    let _ = writer.into_parts();
    if written {
        let _ = rustix::net::shutdown(&output, Shutdown::Write);
    } else {
        let _ = said.send(Said::Broken(id, None));
    }
    drop(output);
    let _ = said.send(Said::Finished(id));
}

#[cfg(test)]
mod tests;
