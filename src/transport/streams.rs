//! The stream between a transfer's two servers, as the client makes it: a
//! socket pair, whose ends it hands over, never touching what goes on it. A
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
//! A transfer has one stream, and one only: should it break, the transfer
//! fails, and nothing makes it again.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::join::{self, Kind, STRAIGHT};
use super::{Half, Opens, Transport, start};
use crate::protocol::Welcome;

/// A server, as the stream reaches it.
pub struct Server {
    pub transport: Arc<Transport>,
    pub welcome: Welcome,
    /// What a channel runs to join it, for a host whose `sshd` will not
    /// forward to a socket.
    pub join: Option<String>,
}

impl Server {
    /// How the stream reaches it.
    fn half(&self) -> Half {
        self.transport.half(&self.welcome, self.join.as_deref())
    }
}

/// The stream, as the client keeps it: the processes of the channels that
/// carry it to hosts.
pub struct Streams {
    carriers: Vec<Child>,
}

/// How long the channels' processes are given to end on their own, once
/// the transfer is over, as their servers close their ends.
const ENDING: Duration = Duration::from_secs(5);

/// Makes the stream between `source` and `destination`. With `cut`, it is
/// cut short before anything goes on it, as a dropped connection would cut
/// it, and the transfer fails: for `ferry test`.
pub fn open(source: &Server, destination: &Server, cut: bool) -> Result<Streams> {
    let mut streams = Streams {
        carriers: Vec::new(),
    };
    streams.make(source.half(), destination.half(), cut)?;
    Ok(streams)
}

impl Streams {
    /// Makes the stream, whose ends `source_half` and `destination_half`
    /// take, cut short with `cut`.
    fn make(&mut self, source_half: Half, destination_half: Half, cut: bool) -> Result<()> {
        // The Source's end, and the Destination's.
        let (one, other) = UnixStream::pair().context("make the stream")?;
        (&other)
            .write_all(&first(&source_half, &destination_half))
            .and_then(|()| (&one).write_all(&first(&destination_half, &source_half)))
            .context("start the stream")?;
        let cutting = cut
            .then(|| one.try_clone())
            .transpose()
            .context("make the stream to cut")?;
        self.hand(one, source_half)?;
        self.hand(other, destination_half)?;
        if let Some(cutting) = cutting {
            let _ = cutting.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }

    /// Hands `end` over as `half` says: to a server on this machine through
    /// its socket, or as the input and output of a channel's process.
    fn hand(&mut self, end: UnixStream, half: Half) -> Result<()> {
        match half {
            Half::Here(socket) => join::hand(&socket, &end),
            Half::There { mut command, .. } => {
                let input = end.try_clone().context("hand the stream over")?;
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

    /// Lets go of the stream, once the transfer is over: each channel's
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

/// What the server that `half` reaches reads first on the stream, whose
/// other end `other` reaches.
fn first(half: &Half, other: &Half) -> Vec<u8> {
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
    join::write_header(&mut first, Kind::Stream).expect("a vector takes it");
    if let Half::Here(_) = other {
        start::write(&mut first).expect("a vector takes it");
    }
    first
}

impl Drop for Streams {
    /// Ends every channel's process still there, as on a failure.
    fn drop(&mut self) {
        for child in &mut self.carriers {
            if child.try_wait().is_ok_and(|status| status.is_none()) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{BufReader, Read};
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    use super::*;
    use crate::transport::Joining;

    /// Makes the stream, its halves as given; with `cut`, cut short.
    fn open_halves(source_half: Half, destination_half: Half, cut: bool) -> Result<Streams> {
        let mut streams = Streams {
            carriers: Vec::new(),
        };
        streams.make(source_half, destination_half, cut)?;
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

    /// The stream a server took, its header read: its ends.
    fn taken(joining: &Joining) -> (BufReader<File>, File) {
        let joined = joining.accept().unwrap();
        let mut input = BufReader::new(File::from(joined.input));
        let kind = join::read_header(&mut input).unwrap();
        assert_eq!(kind, Kind::Stream, "not the stream");
        (input, File::from(joined.output))
    }

    /// Skips to the start mark, which must come.
    fn started(input: &mut BufReader<File>) -> Vec<u8> {
        let skipped = start::skip(input).unwrap();
        assert!(skipped.found, "no start mark");
        skipped.before
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
            transport: Arc::new(Transport::Local),
            welcome: welcome(joining),
            join: None,
        };
        let (to_source, to_destination) = (server(&source), server(&destination));
        let opening = std::thread::spawn(move || open(&to_source, &to_destination, false));
        let (mut source_input, mut source_output) = taken(&source);
        let (mut destination_input, _) = taken(&destination);
        let streams = opening.join().unwrap().unwrap();
        assert_eq!(started(&mut source_input), b"");
        assert_eq!(started(&mut destination_input), b"");
        source_output.write_all(b"frames").unwrap();
        let mut frames = [0u8; 6];
        destination_input.read_exact(&mut frames).unwrap();
        assert_eq!(&frames, b"frames");
        streams.end();
    }

    /// The stream cut short is there, and ends at once, both ways.
    #[test]
    fn a_stream_cut_short_ends_both_ways() {
        let (source, source_half) = here();
        let (destination, destination_half) = here();
        let opening = opening(source_half, destination_half, true);
        let ends = [taken(&source), taken(&destination)];
        let streams = opening.join().unwrap().unwrap();
        for (mut input, _) in ends {
            started(&mut input);
            let mut rest = Vec::new();
            input.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"");
        }
        streams.end();
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
        let script = "dd bs=1 count=2 of=\"$FIRST\" 2>/dev/null; \
                      printf '\\000ferry: start\\000'; exec cat";
        let opening = opening(source_half, there(script, &first, true), false);
        let (mut input, mut output) = taken(&source);
        let streams = opening.join().unwrap().unwrap();
        // Its answer is the start mark the server here skips to.
        assert_eq!(started(&mut input), b"");
        output.write_all(b"frames").unwrap();
        // Back from it: the start mark the client wrote it, then the frames.
        let mut back = vec![0u8; start::MARK.len() + 6];
        input.read_exact(&mut back).unwrap();
        assert_eq!(back, [start::MARK, b"frames"].concat());
        assert_eq!(std::fs::read(&first).unwrap(), [STRAIGHT, 0]);
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
                "dd bs=1 count=2 of=\"$FIRST\".{name} 2>/dev/null; \
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
        streams.end();
        for (name, other) in [("A", "B"), ("B", "A")] {
            let read = |path: &std::path::Path| {
                std::fs::read(format!("{}.{name}", path.display())).unwrap()
            };
            assert_eq!(read(&first), [STRAIGHT, 0], "{name}");
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
                      dd bs=1 count=1 of=\"$FIRST\" 2>/dev/null; exec cat";
        let opening = opening(source_half, there(script, &first, false), false);
        let (mut input, output) = taken(&source);
        let streams = opening.join().unwrap().unwrap();
        assert_eq!(started(&mut input), b"noise\n");
        // The client's start mark comes back once the header is read.
        let mut back = vec![0u8; start::MARK.len()];
        input.read_exact(&mut back).unwrap();
        assert_eq!(back, start::MARK);
        assert_eq!(std::fs::read(&first).unwrap(), [0]);
        drop(output);
        drop(input);
        streams.end();
    }
}
