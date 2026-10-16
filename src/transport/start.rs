//! The start mark: the first bytes a server, or a process that joins one,
//! writes on its standard output.
//!
//! On a host, Ferry runs through the user's login shell, whose startup files
//! may print to standard output before Ferry starts, as a tool that warns
//! there does. What comes before the mark is that: the client skips it, and
//! says what it was, rather than reading it as packets.

use std::io::{self, BufRead, Write};

/// The start mark.
pub const MARK: &[u8] = b"\0ferry: start\0";

/// The most the client skips before the mark: a shell prints a few lines.
const MOST: usize = 1 << 20;

/// Writes the start mark, before anything else.
pub fn write(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(MARK)?;
    writer.flush()
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
