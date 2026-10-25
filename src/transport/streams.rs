//! The streams between a transfer's two servers, as the client makes them:
//! for each, a socket pair, whose ends it hands over, never touching what
//! goes on it. A server on this machine is handed its end through its
//! socket; one on a host reaches its end as the input and output of a
//! channel over an SSH connection, which reaches the server's socket
//! straight, or runs a joining process. Between two hosts, the two channels
//! are joined back to back: the kernel carries the bytes from one `ssh` to
//! the other. See the draft, `docs/drafts/architecture-redesign.md`,
//! section 5.
//!
//! What each server reads first, the client writes into the other end
//! before it hands either over: what a server's socket takes first, as the
//! byte that says a channel came straight, and the header that says which
//! stream it is; then the start mark, where the other server is on this
//! machine and writes nothing before its session does. A server on a host
//! writes the start mark as a stream comes straight, or its joining process
//! does, after what the host's shell printed: each server skips to it, so
//! that one start mark goes each way.
//!
//! A stream that breaks is made again, as either server says it did: under
//! a number never used before, over the same SSH connections, whose master
//! is logged in again first, without asking, should it be gone.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::join::{self, Kind, STRAIGHT};
use super::{Half, Transport, start};
use crate::interrupt;
use crate::protocol::Welcome;

/// A server, as the streams reach it.
pub struct Server {
    pub transport: Arc<Transport>,
    pub welcome: Welcome,
    /// What a channel runs to join it, for a host whose `sshd` will not
    /// forward to a socket.
    pub join: Option<String>,
}

impl Server {
    /// How the stream `number` reaches it, its SSH connection first logged
    /// in again, without asking, should its master be gone: the new master's
    /// holder, if one was started, is kept in `holders`.
    fn half(&self, number: u32, holders: &mut Vec<(Child, interrupt::EndOnExit)>) -> Result<Half> {
        holders.extend(self.transport.ready(number)?);
        Ok(self
            .transport
            .half(&self.welcome, self.join.as_deref(), number))
    }
}

/// The streams, as the client keeps them, with the two servers they join, to
/// make again any that breaks.
pub struct Streams {
    source: Server,
    destination: Server,
    /// The next number never used.
    next: u32,
    /// The streams made, both ends handed over: one of which breaks is
    /// made again, and not one only one server was handed.
    made: HashSet<u32>,
    /// How far apart two numbers are that go over the same SSH connections.
    step: u32,
    /// The processes of the channels that carry those to hosts.
    carriers: Vec<Child>,
    /// The holders of masters logged in again.
    holders: Vec<(Child, interrupt::EndOnExit)>,
    /// Each stream's Source end again, while `FERRY_TEST_BREAK` is to break
    /// them.
    kept: Vec<(u32, UnixStream)>,
}

/// How long the channels' processes are given to end on their own, once
/// the transfer is over, as their servers close their ends.
const ENDING: Duration = Duration::from_secs(5);

/// A session's ID, made up for a transfer: its two servers' streams join
/// it by it.
pub fn session() -> Result<[u8; 16]> {
    let mut id = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut id))
        .context("make up a session's ID")?;
    Ok(id)
}

/// Makes the streams between `source` and `destination`: one on each SSH
/// connection, the more of either side's, or one on this machine. With
/// `cut`, the last is cut short before anything goes on it, as a dropped
/// connection would cut it, and made again: for `ferry test`.
pub fn open(source: Server, destination: Server, cut: bool) -> Result<Streams> {
    let (here, there) = (source.transport.streams(), destination.transport.streams());
    let count = here.max(there) as u32;
    let mut streams = Streams {
        source,
        destination,
        next: count,
        made: HashSet::new(),
        step: (here * there / gcd(here, there)) as u32,
        carriers: Vec::new(),
        holders: Vec::new(),
        kept: Vec::new(),
    };
    for number in 0..count {
        let (source_half, destination_half) = streams.halves(number)?;
        streams.make(
            number,
            source_half,
            destination_half,
            cut && number + 1 == count,
        )?;
    }
    Ok(streams)
}

/// The greatest number that divides both.
fn gcd(one: usize, other: usize) -> usize {
    if other == 0 {
        one
    } else {
        gcd(other, one % other)
    }
}

impl Streams {
    /// Makes another stream in place of `broken`, over the same SSH
    /// connections, once: returns its number, or none if `broken` was never
    /// made whole, as where one server could not be handed its end.
    pub fn remake(&mut self, broken: u32) -> Result<Option<u32>> {
        if !self.made.remove(&broken) {
            return Ok(None);
        }
        let mut number = broken;
        while number < self.next {
            number += self.step;
        }
        self.next = number + 1;
        let made = self
            .halves(number)
            .and_then(|(source_half, destination_half)| {
                self.make(number, source_half, destination_half, false)
            });
        if made.is_err() {
            // Tried again in its place, should it be.
            self.made.insert(broken);
        }
        made.map(|()| Some(number))
    }

    /// How the stream `number` reaches each server.
    fn halves(&mut self, number: u32) -> Result<(Half, Half)> {
        Ok((
            self.source.half(number, &mut self.holders)?,
            self.destination.half(number, &mut self.holders)?,
        ))
    }

    /// Makes the stream `number`, whose ends `source_half` and
    /// `destination_half` take, cut short with `cut`.
    fn make(
        &mut self,
        number: u32,
        source_half: Half,
        destination_half: Half,
        cut: bool,
    ) -> Result<()> {
        // The Source's end, and the Destination's.
        let (one, other) = UnixStream::pair().context("make a stream")?;
        (&other)
            .write_all(&first(&source_half, &destination_half, number))
            .and_then(|()| (&one).write_all(&first(&destination_half, &source_half, number)))
            .context("start a stream")?;
        let cutting = cut
            .then(|| one.try_clone())
            .transpose()
            .context("make a stream to cut")?;
        if crate::inject::breaking().is_some() {
            self.kept
                .push((number, one.try_clone().context("keep a stream")?));
        }
        self.hand(one, source_half)?;
        self.hand(other, destination_half)?;
        self.made.insert(number);
        if let Some(cutting) = cutting {
            let _ = cutting.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }

    /// Breaks a stream, as `FERRY_TEST_BREAK` asks: the one `pick` picks of
    /// those made; says which.
    pub fn break_one(&mut self, pick: u64) -> Option<u32> {
        if self.kept.is_empty() {
            return None;
        }
        let (number, end) = self.kept.remove(pick as usize % self.kept.len());
        let _ = end.shutdown(std::net::Shutdown::Both);
        Some(number)
    }

    /// Hands `end` over as `half` says: to a server on this machine through
    /// its socket, or as the input and output of a channel's process.
    fn hand(&mut self, end: UnixStream, half: Half) -> Result<()> {
        match half {
            Half::Here(socket) => join::hand(&socket, &end),
            Half::There { mut command, .. } => {
                let input = end.try_clone().context("hand a stream over")?;
                let child = command
                    .stdin(Stdio::from(OwnedFd::from(input)))
                    .stdout(Stdio::from(OwnedFd::from(end)))
                    .stderr(Stdio::inherit())
                    .spawn()
                    .with_context(|| format!("start {:?}", command.get_program()))?;
                self.carriers.push(child);
                Ok(())
            }
        }
    }

    /// Lets go of the streams, once the transfer is over: each channel's
    /// process is given [`ENDING`] to end, as its server closes its end, and
    /// then ended.
    pub fn end(mut self) {
        let deadline = Instant::now() + ENDING;
        for child in &mut self.carriers {
            while child.try_wait().is_ok_and(|status| status.is_none()) && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// What the server that `half` reaches reads first on the stream `number`,
/// whose other end `other` reaches.
fn first(half: &Half, other: &Half, number: u32) -> Vec<u8> {
    let mut first = Vec::new();
    if let Half::There { straight: true, .. } = half {
        first.push(STRAIGHT);
    }
    join::write_header(&mut first, Kind::Stream(number)).expect("a vector takes it");
    if let Half::Here(_) = other {
        start::write(&mut first).expect("a vector takes it");
    }
    first
}

impl Drop for Streams {
    /// Ends every channel's process still there, as on a failure, and every
    /// master's holder started again, as the transport ends its own.
    fn drop(&mut self) {
        for child in self
            .carriers
            .iter_mut()
            .chain(self.holders.iter_mut().map(|(holder, _)| holder))
        {
            if child.try_wait().is_ok_and(|status| status.is_none()) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    use super::*;
    use crate::transport::Joining;

    /// Makes `count` streams, the halves of each as `halves` gives them; with
    /// `cut`, the last cut short.
    fn open_halves(count: u32, halves: impl Fn(u32) -> (Half, Half), cut: bool) -> Result<Streams> {
        let server = || Server {
            transport: Arc::new(Transport::Local),
            welcome: Welcome {
                socket: Vec::new(),
                root: crate::protocol::RootIdentity::default(),
            },
            join: None,
        };
        let mut streams = Streams {
            source: server(),
            destination: server(),
            next: count,
            made: HashSet::new(),
            step: 1,
            carriers: Vec::new(),
            holders: Vec::new(),
            kept: Vec::new(),
        };
        for number in 0..count {
            let (source_half, destination_half) = halves(number);
            streams.make(
                number,
                source_half,
                destination_half,
                cut && number + 1 == count,
            )?;
        }
        Ok(streams)
    }

    /// A server's socket on this machine, and how a stream reaches it.
    fn here() -> (Joining, Half) {
        let joining = join::listen(false).unwrap();
        let half = Half::Here(joining.socket().to_owned());
        (joining, half)
    }

    /// A channel's process, as `sh` runs `script`, its first bytes, as a
    /// server would read them, written to `first`.
    fn there(script: &str, first: &std::path::Path, straight: bool) -> Half {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script).env("FIRST", first);
        Half::There { command, straight }
    }

    /// The stream a server took, its header read: which it is, and its ends.
    fn taken(joining: &Joining) -> (u32, BufReader<File>, File) {
        let joined = joining.accept().unwrap();
        let mut input = BufReader::new(File::from(joined.input));
        let Kind::Stream(number) = join::read_header(&mut input).unwrap() else {
            panic!("not a stream");
        };
        (number, input, File::from(joined.output))
    }

    /// Skips to the start mark, which must come.
    fn started(input: &mut BufReader<File>) -> Vec<u8> {
        let skipped = start::skip(input).unwrap();
        assert!(skipped.found, "no start mark");
        skipped.before
    }

    /// Two servers on this machine are each handed an end of every stream,
    /// numbered, with the start mark the other writes no session before;
    /// what one writes, the other reads.
    #[test]
    fn two_servers_here_are_each_handed_an_end() {
        let (source, source_half) = here();
        let (destination, destination_half) = here();
        let welcome = |joining: &Joining| Welcome {
            socket: joining.socket().as_os_str().as_bytes().to_vec(),
            root: crate::protocol::RootIdentity::default(),
        };
        let _ = (source_half, destination_half);
        let server = |joining: &Joining| Server {
            transport: Arc::new(Transport::Local),
            welcome: welcome(joining),
            join: None,
        };
        let streams = open(server(&source), server(&destination), false).unwrap();
        let (number, mut source_input, mut source_output) = taken(&source);
        assert_eq!(number, 0);
        let (number, mut destination_input, _) = taken(&destination);
        assert_eq!(number, 0);
        assert_eq!(started(&mut source_input), b"");
        assert_eq!(started(&mut destination_input), b"");
        source_output.write_all(b"frames").unwrap();
        let mut frames = [0u8; 6];
        destination_input.read_exact(&mut frames).unwrap();
        assert_eq!(&frames, b"frames");
        streams.end();
    }

    /// A stream that broke is made again once, under the next number that
    /// goes over the same SSH connections, both ends handed over; one never
    /// made whole is not.
    #[test]
    fn a_stream_that_broke_is_made_again_once() {
        let (source, _) = here();
        let (destination, _) = here();
        let welcome = |joining: &Joining| Welcome {
            socket: joining.socket().as_os_str().as_bytes().to_vec(),
            root: crate::protocol::RootIdentity::default(),
        };
        let server = |joining: &Joining| Server {
            transport: Arc::new(Transport::Local),
            welcome: welcome(joining),
            join: None,
        };
        let mut streams = open(server(&source), server(&destination), false).unwrap();
        for joining in [&source, &destination] {
            assert_eq!(taken(joining).0, 0);
        }
        assert_eq!(streams.remake(0).unwrap(), Some(1));
        for joining in [&source, &destination] {
            let (number, mut input, _) = taken(joining);
            assert_eq!(number, 1);
            started(&mut input);
        }
        // Both servers say it broke: it is made again once.
        assert_eq!(streams.remake(0).unwrap(), None);
        assert_eq!(streams.remake(7).unwrap(), None);
        streams.end();
    }

    /// Two numbers go over the same SSH connections of both sides as far
    /// apart as the two counts' least common multiple.
    #[test]
    fn numbers_on_the_same_connections_are_a_common_multiple_apart() {
        for (here, there, step) in [(1, 1, 1), (1, 3, 3), (2, 3, 6), (4, 6, 12)] {
            assert_eq!(here * there / gcd(here, there), step);
        }
    }

    /// The stream cut short is there, numbered, and ends at once, both ways.
    #[test]
    fn a_stream_cut_short_ends_both_ways() {
        let (source, source_half) = here();
        let (destination, destination_half) = here();
        let halves = std::cell::RefCell::new(Some((source_half, destination_half)));
        let streams = open_halves(1, |_| halves.borrow_mut().take().unwrap(), true).unwrap();
        for joining in [&source, &destination] {
            let (number, mut input, _) = taken(joining);
            assert_eq!(number, 0);
            started(&mut input);
            let mut rest = Vec::new();
            input.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"");
        }
        streams.end();
    }

    /// A server on a host, reached straight, reads the byte that says so
    /// and which stream it is, then the start mark from the client, as the
    /// other server, here, writes none; it answers with its own.
    #[test]
    fn a_server_reached_straight_reads_what_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let (source, source_half) = here();
        // A server on a host, as SSH would carry it: it answers with the
        // start mark, then sends back all that comes after its first bytes.
        let script = "dd bs=1 count=6 of=\"$FIRST\" 2>/dev/null; \
                      printf '\\000ferry: start\\000'; exec cat";
        let halves = std::cell::RefCell::new(Some((source_half, there(script, &first, true))));
        let streams = open_halves(1, |_| halves.borrow_mut().take().unwrap(), false).unwrap();
        let (number, mut input, mut output) = taken(&source);
        assert_eq!(number, 0);
        // Its answer is the start mark the server here skips to.
        assert_eq!(started(&mut input), b"");
        output.write_all(b"frames").unwrap();
        // Back from it: the start mark the client wrote it, then the frames.
        let mut back = vec![0u8; start::MARK.len() + 6];
        input.read_exact(&mut back).unwrap();
        assert_eq!(back, [start::MARK, b"frames"].concat());
        assert_eq!(std::fs::read(&first).unwrap(), [STRAIGHT, 0, 0, 0, 0, 0]);
        drop(output);
        drop(input);
        streams.end();
    }

    /// Two servers on hosts are joined back to back: each reads its own
    /// first bytes, then the other's start mark and what the other sends.
    #[test]
    fn two_servers_there_are_joined_back_to_back() {
        let temp = tempfile::tempdir().unwrap();
        let (first, got) = (temp.path().join("first"), temp.path().join("got"));
        let script = |name: &str| {
            format!(
                "dd bs=1 count=6 of=\"$FIRST\".{name} 2>/dev/null; \
                 printf '\\000ferry: start\\000from {name}'; \
                 dd bs=1 count=20 of=\"$GOT\".{name} 2>/dev/null"
            )
        };
        let half = |name: &str| {
            let Half::There { mut command, .. } = there(&script(name), &first, true) else {
                unreachable!()
            };
            command.env("GOT", &got);
            Half::There {
                command,
                straight: true,
            }
        };
        let halves = std::cell::RefCell::new(Some((half("A"), half("B"))));
        let streams = open_halves(1, |_| halves.borrow_mut().take().unwrap(), false).unwrap();
        streams.end();
        for (name, other) in [("A", "B"), ("B", "A")] {
            let read = |path: &std::path::Path| {
                std::fs::read(format!("{}.{name}", path.display())).unwrap()
            };
            assert_eq!(read(&first), [STRAIGHT, 0, 0, 0, 0, 0], "{name}");
            assert_eq!(
                read(&got),
                [start::MARK, format!("from {other}").as_bytes()].concat(),
                "{name}"
            );
        }
    }

    /// A joining process on a host, after what its shell printed, writes the
    /// start mark, which the server here skips to; it reads the header, with
    /// no byte before it.
    #[test]
    fn a_joining_process_writes_the_start_mark() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let (source, source_half) = here();
        let script = "echo noise; printf '\\000ferry: start\\000'; \
                      dd bs=1 count=5 of=\"$FIRST\" 2>/dev/null; exec cat";
        let halves = std::cell::RefCell::new(Some((source_half, there(script, &first, false))));
        let streams = open_halves(1, |_| halves.borrow_mut().take().unwrap(), false).unwrap();
        let (_, mut input, output) = taken(&source);
        assert_eq!(started(&mut input), b"noise\n");
        // The client's start mark comes back once the header is read.
        let mut back = vec![0u8; start::MARK.len()];
        input.read_exact(&mut back).unwrap();
        assert_eq!(back, start::MARK);
        assert_eq!(std::fs::read(&first).unwrap(), [0, 0, 0, 0, 0]);
        drop(output);
        drop(input);
        streams.end();
    }
}
