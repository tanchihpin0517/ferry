//! The marks that set Ferry's output apart from what else is written where
//! it writes.
//!
//! On a host, Ferry runs through the user's login shell, whose startup files
//! may print to standard output, as a tool that warns there does, and may
//! leave a program running that prints later. So the server's answer to the
//! handshake, the one message it sends there, goes between the start mark
//! and the end mark, written at once, as one write of a few hundred bytes,
//! which a pipe keeps whole: whatever else is printed comes before or after
//! it, never inside. The client skips what comes before the start mark, and
//! says what it was, rather than reading it as packets. A process that joins
//! the server writes the start mark as it starts, and so does the server, to
//! answer a stream or channel that reaches its socket straight.

use std::io::{self, BufRead, Write};

/// The start mark.
pub const MARK: &[u8] = b"\0ferry: start\0";

/// The end mark, after the server's answer.
pub const END: &[u8] = b"\0ferry: end\0";

/// The most the client skips before the mark: a shell prints a few lines.
const MOST: usize = 1 << 20;

/// Writes the start mark, before anything else.
pub fn write(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(MARK)?;
    writer.flush()
}

/// `packet` between the start mark and the end mark, to write at once.
pub fn framed(packet: &[u8]) -> Vec<u8> {
    [MARK, packet, END].concat()
}

/// Reads the end mark, after the server's answer: anything else there says
/// that something cut into the answer.
pub fn expect_end(reader: &mut impl io::Read) -> io::Result<()> {
    let mut end = [0u8; END.len()];
    reader.read_exact(&mut end)?;
    if end != END {
        return Err(io::Error::other(
            "something the host printed cut into the server's answer",
        ));
    }
    Ok(())
}

/// What came before the mark.
#[derive(Debug, PartialEq, Eq)]
pub struct Skipped {
    pub before: Vec<u8>,
    /// Whether the mark came, or the stream ended first.
    pub found: bool,
}

/// Reads up to the end of the start mark, keeping what came before it; or,
/// if the stream ends first, all of it.
pub fn skip(reader: &mut impl BufRead) -> io::Result<Skipped> {
    let mut before = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(Skipped {
                before,
                found: false,
            });
        }
        let mut used = 0;
        let mut found = false;
        for &byte in available {
            used += 1;
            before.push(byte);
            if before.ends_with(MARK) {
                found = true;
                break;
            }
        }
        reader.consume(used);
        if found {
            before.truncate(before.len() - MARK.len());
            return Ok(Skipped {
                before,
                found: true,
            });
        }
        if before.len() > MOST {
            return Err(io::Error::other(format!(
                "more than {} MiB came before Ferry started",
                MOST >> 20
            )));
        }
    }
}

/// Shell that runs `script` and exits as it did, with what it prints between
/// the start mark and the end mark, as a server's answer is: for a script
/// whose output the client reads, such as the probe, which the host's shell
/// may print before, or after.
pub fn marked(script: &str) -> String {
    format!(
        "printf '\\000ferry: start\\000'\n(\n{script}\n)\nstatus=$?\nprintf '\\000ferry: end\\000'\nexit $status\n"
    )
}

/// What came of a script that [`marked`] ran: what was printed before it,
/// and what it printed itself, or `None` if it did not run to its end.
pub fn unmarked(output: &[u8]) -> (Vec<u8>, Option<&[u8]>) {
    let mut reader = output;
    let Ok(Skipped { before, found }) = skip(&mut reader) else {
        return (Vec::new(), None);
    };
    if !found {
        return (before, None);
    }
    let inside = reader
        .windows(END.len())
        .position(|window| window == END)
        .map(|end| &reader[..end]);
    (before, inside)
}

/// What a shell printed, to say on a line: without escape sequences or
/// other control characters, its lines joined, and at most 300 characters.
/// Empty if it printed nothing to see.
pub fn shown(before: &[u8]) -> String {
    let text = String::from_utf8_lossy(before);
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' {
            // A CSI sequence ends at its final byte; any other escape is two
            // characters.
            if chars.next_if_eq(&'[').is_some() {
                while chars
                    .next()
                    .is_some_and(|next| !('\x40'..='\x7e').contains(&next))
                {}
            } else {
                chars.next();
            }
        } else if char == '\n' || char == '\r' {
            plain.push('\n');
        } else if !char.is_control() {
            plain.push(char);
        }
    }
    let joined = plain
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let mut shown: String = joined.chars().take(300).collect();
    if shown.len() < joined.len() {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_comes_before_the_mark_is_skipped_and_kept() {
        let mut stream = b"noise\nmore".to_vec();
        stream.extend_from_slice(MARK);
        stream.extend_from_slice(b"packets");
        let mut reader = io::BufReader::with_capacity(3, stream.as_slice());
        let skipped = skip(&mut reader).unwrap();
        assert_eq!(
            skipped,
            Skipped {
                before: b"noise\nmore".to_vec(),
                found: true
            }
        );
        let mut rest = Vec::new();
        io::Read::read_to_end(&mut reader, &mut rest).unwrap();
        assert_eq!(rest, b"packets");
    }

    #[test]
    fn a_stream_that_ends_first_has_no_mark() {
        let skipped = skip(&mut b"sh: ferry: not found\n".as_slice()).unwrap();
        assert!(!skipped.found);
        assert_eq!(skipped.before, b"sh: ferry: not found\n");
        let mut nothing = MARK.to_vec();
        assert_eq!(
            skip(&mut nothing.as_slice()).unwrap(),
            Skipped {
                before: Vec::new(),
                found: true
            }
        );
        nothing.pop();
        assert!(!skip(&mut nothing.as_slice()).unwrap().found);
    }

    /// What a marked script prints is told apart from what the shell
    /// printed around it, and it exits as the script did.
    #[test]
    fn a_marked_script_is_told_apart_from_the_shell() {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "echo arch=bogus\n{}",
                marked("echo arch=right; exit 3")
            ))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        let (before, inside) = unmarked(&output.stdout);
        assert_eq!(before, b"arch=bogus\n");
        assert_eq!(inside, Some(&b"arch=right\n"[..]));

        // One that never got to its end did not run whole.
        let mut cut = MARK.to_vec();
        cut.extend_from_slice(b"arch=");
        assert_eq!(unmarked(&cut).1, None);
        assert_eq!(unmarked(b"sh: not found").1, None);
    }

    #[test]
    fn what_a_shell_printed_is_shown_plain() {
        assert_eq!(
            shown(b"\x1b[33m WARN\x1b[0m \x1b[2msrc/commands.rs\x1b[0m: busy\n\nsecond\r\n"),
            "WARN src/commands.rs: busy / second"
        );
        assert_eq!(shown(b"\n \x1b[0m\n"), "");
        assert_eq!(shown(&[b'x'; 400]).chars().count(), 301);
    }
}
