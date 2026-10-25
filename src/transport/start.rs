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

/// A line of what a host's first session tells the client, amid what the
/// host's shell, and what it left running, may print there: `\0ferry:`, the
/// run's tag, the line's kind, the length of its content in bytes, each
/// followed by `:`, then the content and a newline. The tag is the client's
/// own, new each run, so that nothing printed by chance is taken for a line;
/// the length finds anything printed into one. Each line goes in one write,
/// small enough that a pipe keeps it whole among others' writes. A part of
/// them, such as the probe's, ends with a line of the kind [`DONE`], which
/// counts the lines before it, so that one lost to noise shows. The tag is
/// no secret: the script that prints it is on the host's command line.
pub fn frame(tag: &str, kind: &str, content: &[u8]) -> Vec<u8> {
    let mut frame = format!("\0ferry:{tag}:{kind}:{}:", content.len()).into_bytes();
    frame.extend_from_slice(content);
    frame.push(b'\n');
    frame
}

/// The kind of the line that ends a part, and says how many lines it had.
pub const DONE: &str = "done";

/// A new tag for a run's lines: 16 hexadecimal digits, from the system's
/// randomness.
pub fn new_tag() -> io::Result<String> {
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut urandom| io::Read::read_exact(&mut urandom, &mut random))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Shell that defines, for the rest of a first session's script, `say KIND
/// CONTENT`, which prints a line as [`frame`] lays it out, in one `printf`,
/// and `finish`, which ends a part. The length is of bytes, as `C` counts
/// them, whatever the locale: set in a subshell, so that nothing the script
/// runs has it.
pub fn framing(tag: &str) -> String {
    format!(
        r#"tag={tag}
n=0
say() {{
    length=$(LC_ALL=C; printf %s "${{#2}}")
    printf '\000ferry:%s:%s:%s:%s\n' "$tag" "$1" "$length" "$2"
    n=$((n + 1))
}}
finish() {{
    printf '\000ferry:%s:{DONE}:%s:%s\n' "$tag" "${{#n}}" "$n"
    n=0
}}
"#
    )
}

/// A line of what a first session tells the client, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: String,
    pub content: Vec<u8>,
}

/// The longest a line's kind, or its length, may be written.
const FIELD: usize = 16;

/// Reads the next line with `tag` of what a first session tells the client,
/// keeping in `skipped` what came before it, which is no line of Ferry's; or
/// `None`, once it ends first.
pub fn next_line(
    reader: &mut impl BufRead,
    tag: &str,
    skipped: &mut Vec<u8>,
) -> io::Result<Option<Line>> {
    let marker = format!("\0ferry:{tag}:").into_bytes();
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            skipped.extend_from_slice(&pending);
            return Ok(None);
        }
        let mut used = 0;
        let mut found = false;
        for &byte in available {
            used += 1;
            pending.push(byte);
            if pending.ends_with(&marker) {
                found = true;
                break;
            }
        }
        reader.consume(used);
        if found {
            pending.truncate(pending.len() - marker.len());
            skipped.extend_from_slice(&pending);
            break;
        }
        // All but what may yet begin the marker is noise.
        let keep = marker.len() - 1;
        if pending.len() > keep {
            let noise = pending.len() - keep;
            skipped.extend(pending.drain(..noise));
        }
        if skipped.len() > MOST {
            return Err(io::Error::other(format!(
                "more than {} MiB came that was not Ferry's",
                MOST >> 20
            )));
        }
    }
    let cut = || io::Error::other("something the host printed cut into a line of Ferry's");
    let field = |reader: &mut dyn BufRead| -> io::Result<String> {
        let mut field = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            reader.read_exact(&mut byte)?;
            if byte[0] == b':' {
                break;
            }
            if field.len() == FIELD || !byte[0].is_ascii_graphic() {
                return Err(cut());
            }
            field.push(byte[0]);
        }
        Ok(String::from_utf8(field).expect("graphic ASCII"))
    };
    let kind = field(reader)?;
    let length: usize = field(reader)?.parse().map_err(|_| cut())?;
    if length > MOST {
        return Err(cut());
    }
    let mut content = vec![0; length];
    reader.read_exact(&mut content)?;
    let mut end = [0u8; 1];
    reader.read_exact(&mut end)?;
    if end[0] != b'\n' {
        return Err(cut());
    }
    Ok(Some(Line { kind, content }))
}

/// Reads a part of what a first session tells the client: its lines, up to
/// the [`DONE`] line that ends it, which must count them all; or `None`,
/// once it ends first.
pub fn read_part(
    reader: &mut impl BufRead,
    tag: &str,
    skipped: &mut Vec<u8>,
) -> io::Result<Option<Vec<Line>>> {
    let mut lines = Vec::new();
    loop {
        let Some(line) = next_line(reader, tag, skipped)? else {
            return Ok(None);
        };
        if line.kind != DONE {
            lines.push(line);
            continue;
        }
        let counted = std::str::from_utf8(&line.content)
            .ok()
            .and_then(|count| count.parse::<usize>().ok());
        if counted != Some(lines.len()) {
            return Err(io::Error::other(
                "a line of Ferry's went missing among what the host printed",
            ));
        }
        return Ok(Some(lines));
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

    /// Lines a script prints with `say`, in sh and in bash, are read back as
    /// they were, their lengths in bytes whatever the locale, amid what was
    /// printed around them and between them; and a part counts its lines.
    #[test]
    fn lines_said_amid_noise_are_read_as_they_were() {
        let tag = "0123456789abcdef";
        let script = format!(
            "{}echo noise\nsay probe 'os=Linux'\nprintf 'between'\nsay probe 'path=/tmp/é'\nfinish\necho after\n",
            framing(tag)
        );
        for shell in ["sh", "bash"] {
            let Ok(output) = std::process::Command::new(shell)
                .arg("-c")
                .arg(&script)
                .env("LC_ALL", "C.UTF-8")
                .output()
            else {
                continue;
            };
            assert!(output.status.success(), "{shell}");
            let mut reader = output.stdout.as_slice();
            let mut skipped = Vec::new();
            let lines = read_part(&mut reader, tag, &mut skipped).unwrap().unwrap();
            assert_eq!(
                lines,
                [
                    Line {
                        kind: "probe".into(),
                        content: b"os=Linux".to_vec()
                    },
                    Line {
                        kind: "probe".into(),
                        content: "path=/tmp/é".as_bytes().to_vec()
                    },
                ],
                "{shell}"
            );
            assert_eq!(skipped, b"noise\nbetween", "{shell}");
            let mut rest = Vec::new();
            assert!(next_line(&mut reader, tag, &mut rest).unwrap().is_none());
            assert_eq!(rest, b"after\n");
        }
    }

    /// A line another run's tag marks, or one printed into, is not taken for
    /// one of this run's; a part missing a line says so.
    #[test]
    fn what_is_not_this_runs_lines_is_not_taken_for_them() {
        let tag = "0123456789abcdef";
        let mut stream = frame("fedcba9876543210", "probe", b"os=Other");
        stream.extend(frame(tag, "probe", b"os=Linux"));
        let mut skipped = Vec::new();
        let line = next_line(&mut stream.as_slice(), tag, &mut skipped)
            .unwrap()
            .unwrap();
        assert_eq!(line.content, b"os=Linux");
        assert!(skipped.starts_with(b"\0ferry:fedcba9876543210:probe"));

        let mut cut = frame(tag, "probe", b"os=Linux");
        cut.insert(cut.len() - 3, b'!');
        let error = next_line(&mut cut.as_slice(), tag, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("cut into"), "{error}");

        let mut short = frame(tag, "probe", b"os=Linux");
        short.extend(frame(tag, DONE, b"2"));
        let error = read_part(&mut short.as_slice(), tag, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("went missing"), "{error}");
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
