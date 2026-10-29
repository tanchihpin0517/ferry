//! The streams between a transfer's two servers, as the client makes them:
//! each a socket pair, whose ends it hands over, never touching what goes
//! on them. A
//! server on this machine is handed its end through its socket; one on a
//! host reaches its end as the input and output of a channel over its SSH
//! connection, which reaches the server's socket straight, or runs a
//! joining process. Between two hosts, the two channels are joined back to
//! back: the kernel carries the bytes from one `ssh` to the other. See the
//! draft, `docs/drafts/architecture-redesign.md`, section 5, and
//! "2026-10-25 重新決定".
//!
//! What each server reads first, the client writes into the other end
//! before it hands either over: what a server's socket takes first, as the
//! byte that says a channel came straight, and the header that says it is
//! the stream; then the start mark, where the other server is on this
//! machine and writes nothing before its peer does. A server on a host
//! writes the start mark as the stream comes straight, or its joining
//! process does, after what the host's shell printed: each server skips to
//! it, so that one start mark goes each way.
//!
//! A transfer has K streams, numbered from 0, as its header says: the first
//! carries all but blocks, which any carries, so that a long round trip,
//! over which one SSH channel's window holds the data back, is filled by
//! several, all over the host's one SSH connection. Should any break, the
//! transfer fails, and nothing makes it again.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};

use super::child::Child;
use super::join::{self, Kind, STRAIGHT};
use super::{Compression, Half, Opens, Transport, start};
use crate::monitor::Role;
use crate::monitor::client::Node;
use crate::protocol::Welcome;

/// How many streams a transfer needs, where the round trip to the farthest
/// host is `round_trip`: one SSH channel carries at most its window, 2 MiB,
/// each round trip, so filling about 1 Gbit/s takes one for each 15 ms of
/// it; at least one, and at most [`join::STREAMS`]. One on this machine.
pub fn count_for(round_trip: Option<std::time::Duration>) -> u8 {
    let Some(round_trip) = round_trip else {
        return 1;
    };
    let each = std::time::Duration::from_millis(15);
    let count = round_trip.as_nanos().div_ceil(each.as_nanos());
    count.clamp(1, u128::from(join::STREAMS)) as u8
}

/// A server, as the stream reaches it.
pub struct Server {
    pub transport: Arc<Transport>,
    pub welcome: Welcome,
    /// What a channel runs to join it, for a host whose `sshd` will not
    /// forward to a socket.
    pub join: Option<String>,
    /// The server, as the client's monitor has it: what carries the stream
    /// to it is a scope of it.
    pub worker: Node,
}

impl Server {
    /// How the stream reaches it, and the scope it is reached in.
    fn half(&self) -> (Half, Node) {
        (
            self.transport.half(&self.welcome, self.join.as_deref()),
            self.worker.clone(),
        )
    }
}

/// The stream, as the client keeps it: the processes of the channels that
/// carry it to hosts.
pub struct Streams {
    carriers: Vec<Child>,
}

/// Makes the `count` streams between `source` and `destination`. With
/// `cut`, the first is cut short as soon as it is handed over, both ways, as
/// a dropped connection would cut it, and the transfer fails: for `ferry
/// test`; and so is the one `FERRY_TEST_CUT_STREAM` names, for a test.
pub fn open(
    source: &Server,
    destination: &Server,
    cut: bool,
    count: u8,
    compression: Compression,
) -> Result<Streams> {
    let mut streams = Streams {
        carriers: Vec::new(),
    };
    let cut_one = crate::inject::cut_stream();
    for index in 0..count {
        let cut = (cut && index == 0) || cut_one == Some(index);
        streams.make(index, source.half(), destination.half(), cut, compression)?;
    }
    Ok(streams)
}

impl Streams {
    /// Makes the stream numbered `index`, whose ends `source_half` and
    /// `destination_half` take, each reached in its server's scope, cut
    /// short with `cut`: each end is shut for writing, so that each server
    /// reads what came before, then the end. Not shut for reading: macOS
    /// would discard what the server had not read yet, as its first bytes,
    /// which a dropped connection never takes back.
    fn make(
        &mut self,
        index: u8,
        (source_half, source): (Half, Node),
        (destination_half, destination): (Half, Node),
        cut: bool,
        compression: Compression,
    ) -> Result<()> {
        // Compressed if it reaches a host, at either end.
        let remote = [&source_half, &destination_half]
            .iter()
            .any(|half| matches!(half, Half::There { .. }));
        let compress = compression.level(remote);
        // The Source's end, and the Destination's.
        let (one, other) = UnixStream::pair().context("make the stream")?;
        (&other)
            .write_all(&first(index, &source_half, &destination_half, compress))
            .and_then(|()| {
                (&one).write_all(&first(index, &destination_half, &source_half, compress))
            })
            .context("start the stream")?;
        let cutting = cut
            .then(|| Ok::<_, std::io::Error>((one.try_clone()?, other.try_clone()?)))
            .transpose()
            .context("make the stream to cut")?;
        self.hand(one, source_half, &source)?;
        self.hand(other, destination_half, &destination)?;
        if let Some((one, other)) = cutting {
            let _ = one.shutdown(std::net::Shutdown::Write);
            let _ = other.shutdown(std::net::Shutdown::Write);
        }
        Ok(())
    }

    /// Hands `end` over as `half` says, to the server `worker` is: to one
    /// on this machine through its socket, or as the input and output of a
    /// channel's process, a scope of `worker`'s.
    fn hand(&mut self, end: UnixStream, half: Half, worker: &Node) -> Result<()> {
        match half {
            Half::Here(socket) => join::hand(&socket, &end, worker.scope()),
            Half::There { mut command, .. } => {
                let input = end.try_clone().context("hand the stream over")?;
                let child = Child::spawn(
                    command
                        .stdin(Stdio::from(OwnedFd::from(input)))
                        .stdout(Stdio::from(OwnedFd::from(end)))
                        .stderr(Stdio::inherit()),
                    Some(worker.told()),
                )
                .with_context(|| format!("start {:?}", command.get_program()))?;
                worker.process(&child, Role::Carrier, "the stream's channel")?;
                self.carriers.push(child);
                Ok(())
            }
        }
    }
}

/// What the server that `half` reaches reads first on the stream numbered
/// `index`, compressed at `compress`, whose other end `other` reaches.
fn first(index: u8, half: &Half, other: &Half, compress: Option<i32>) -> Vec<u8> {
    let mut first = Vec::new();
    match half {
        Half::There {
            opens: Opens::Straight,
            ..
        } => first.push(STRAIGHT),
        Half::There {
            opens: Opens::Secret(secret),
            ..
        } => first.extend_from_slice(secret.as_bytes()),
        _ => {}
    }
    join::write_header(&mut first, Kind::Stream(index), compress).expect("a vector takes it");
    if let Half::Here(_) = other {
        start::write(&mut first).expect("a vector takes it");
    }
    first
}

impl Drop for Streams {
    /// Ends every channel's process still there, as on a failure.
    fn drop(&mut self) {
        for child in &self.carriers {
            child.kill();
            child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    use super::*;
    use crate::transport::Joining;
    use crate::transport::wire::Wire;

    /// Makes the stream, its halves as given; with `cut`, cut short.
    fn open_halves(source_half: Half, destination_half: Half, cut: bool) -> Result<Streams> {
        let mut streams = Streams {
            carriers: Vec::new(),
        };
        let detached = || crate::monitor::client::Node::detached().unwrap();
        streams.make(
            0,
            (source_half, detached()),
            (destination_half, detached()),
            cut,
            crate::transport::Compression::default(),
        )?;
        Ok(streams)
    }

    /// [`open_halves`], in a thread of its own, as each end handed to a
    /// server here waits for the server, the test, to take it.
    fn opening(
        source_half: Half,
        destination_half: Half,
        cut: bool,
    ) -> std::thread::JoinHandle<Result<Streams>> {
        std::thread::spawn(move || open_halves(source_half, destination_half, cut))
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
        let opens = if straight {
            Opens::Straight
        } else {
            Opens::Joining
        };
        Half::There { command, opens }
    }

    /// The stream a server took, its header read.
    fn taken(joining: &Joining) -> Wire {
        let mut wire = joining
            .accept(&crate::transport::scope::Scope::new().unwrap())
            .unwrap();
        let (kind, _) = join::read_header(&mut wire).unwrap();
        assert_eq!(kind, Kind::Stream(0), "not the first stream");
        wire
    }

    /// Skips to the start mark, which must come.
    fn started(wire: &mut Wire) -> Vec<u8> {
        let skipped = start::skip(wire).unwrap();
        assert!(skipped.found, "no start mark");
        skipped.before
    }

    /// A stream for each 15 ms of the round trip, from one to eight.
    #[test]
    fn streams_go_with_the_round_trip() {
        let ms = std::time::Duration::from_millis;
        for (round_trip, count) in [
            (None, 1),
            (Some(ms(0)), 1),
            (Some(ms(2)), 1),
            (Some(ms(15)), 1),
            (Some(ms(16)), 2),
            (Some(ms(30)), 2),
            (Some(ms(31)), 3),
            (Some(ms(120)), 8),
            (Some(ms(250)), 8),
        ] {
            assert_eq!(count_for(round_trip), count, "{round_trip:?}");
        }
    }

    /// Two servers on this machine are each handed an end of the stream,
    /// with the start mark, as neither writes one; what one writes, the
    /// other reads.
    #[test]
    fn two_servers_here_are_each_handed_an_end() {
        let (source, source_half) = here();
        let (destination, destination_half) = here();
        let welcome = |joining: &Joining| Welcome {
            socket: joining.socket().as_os_str().as_bytes().to_vec(),
            port: None,
            root: crate::protocol::RootIdentity::default(),
        };
        let _ = (source_half, destination_half);
        let server = |joining: &Joining| Server {
            transport: Arc::new(Transport::Local(
                crate::monitor::client::Node::detached().unwrap(),
            )),
            welcome: welcome(joining),
            join: None,
            worker: crate::monitor::client::Node::detached().unwrap(),
        };
        let (to_source, to_destination) = (server(&source), server(&destination));
        let opening = std::thread::spawn(move || {
            open(
                &to_source,
                &to_destination,
                false,
                1,
                Compression::default(),
            )
        });
        let mut source_wire = taken(&source);
        let mut destination_wire = taken(&destination);
        let streams = opening.join().unwrap().unwrap();
        assert_eq!(started(&mut source_wire), b"");
        assert_eq!(started(&mut destination_wire), b"");
        source_wire.write_all(b"frames").unwrap();
        let mut frames = [0u8; 6];
        destination_wire.read_exact(&mut frames).unwrap();
        assert_eq!(&frames, b"frames");
        drop(streams);
    }

    /// The stream cut short is there, and ends at once, both ways, what
    /// came before it ends still read: its header, and the start mark.
    #[test]
    fn a_stream_cut_short_ends_both_ways() {
        let (source, source_half) = here();
        let (destination, destination_half) = here();
        let opening = opening(source_half, destination_half, true);
        let wires = [taken(&source), taken(&destination)];
        let streams = opening.join().unwrap().unwrap();
        for mut wire in wires {
            started(&mut wire);
            let mut rest = Vec::new();
            wire.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"");
        }
        drop(streams);
    }

    /// A server on a host, reached straight, reads the byte that says so
    /// and the header that says it is the stream, then the start mark from
    /// the client, as the other server, here, writes none; it answers with
    /// its own.
    #[test]
    fn a_server_reached_straight_reads_what_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let (source, source_half) = here();
        // A server on a host, as SSH would carry it: it answers with the
        // start mark, then sends back all that comes after its first bytes.
        let script = "dd bs=1 count=3 of=\"$FIRST\" 2>/dev/null; \
                      printf '\\000ferry: start\\000'; exec cat";
        let opening = opening(source_half, there(script, &first, true), false);
        let mut wire = taken(&source);
        let streams = opening.join().unwrap().unwrap();
        // Its answer is the start mark the server here skips to.
        assert_eq!(started(&mut wire), b"");
        wire.write_all(b"frames").unwrap();
        // Back from it: the start mark the client wrote it, then the frames.
        let mut back = vec![0u8; start::MARK.len() + 6];
        wire.read_exact(&mut back).unwrap();
        assert_eq!(back, [start::MARK, b"frames"].concat());
        // Reaching a host, it is compressed, at level 1.
        assert_eq!(std::fs::read(&first).unwrap(), [STRAIGHT, 0, 1]);
        drop(wire);
        drop(streams);
    }

    /// Two servers on hosts are joined back to back: each reads its own
    /// first bytes, then the other's start mark and what the other sends.
    #[test]
    fn two_servers_there_are_joined_back_to_back() {
        let temp = tempfile::tempdir().unwrap();
        let (first, got) = (temp.path().join("first"), temp.path().join("got"));
        let script = |name: &str| {
            format!(
                "dd bs=1 count=3 of=\"$FIRST\".{name} 2>/dev/null; \
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
                opens: Opens::Straight,
            }
        };
        let streams = open_halves(half("A"), half("B"), false).unwrap();
        // Each channel's process ends as it is done with its end.
        for carrier in &streams.carriers {
            carrier.wait();
        }
        drop(streams);
        for (name, other) in [("A", "B"), ("B", "A")] {
            let read = |path: &std::path::Path| {
                std::fs::read(format!("{}.{name}", path.display())).unwrap()
            };
            assert_eq!(read(&first), [STRAIGHT, 0, 1], "{name}");
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
                      dd bs=1 count=2 of=\"$FIRST\" 2>/dev/null; exec cat";
        let opening = opening(source_half, there(script, &first, false), false);
        let mut wire = taken(&source);
        let streams = opening.join().unwrap().unwrap();
        assert_eq!(started(&mut wire), b"noise\n");
        // The client's start mark comes back once the header is read.
        let mut back = vec![0u8; start::MARK.len()];
        wire.read_exact(&mut back).unwrap();
        assert_eq!(back, start::MARK);
        assert_eq!(std::fs::read(&first).unwrap(), [0, 1]);
        drop(wire);
        drop(streams);
    }
}
