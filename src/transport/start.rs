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

/// The most the client skips before the mark, a shell printing a few lines;
/// and the most a line of Ferry's may hold.
pub const MOST: usize = 1 << 20;

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

/// Where `mark` begins in `bytes`; or, if it is not there, how many of
/// their last bytes may yet begin it, as more comes. What comes before it
/// is no mark of Ferry's.
fn seek(bytes: &[u8], mark: &[u8]) -> Result<usize, usize> {
    if let Some(at) = bytes.windows(mark.len()).position(|window| window == mark) {
        return Ok(at);
    }
    Err((1..mark.len())
        .rev()
        .find(|&length| bytes.ends_with(&mark[..length]))
        .unwrap_or(0))
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
        let held = before.len();
        let read = available.len();
        before.extend_from_slice(available);
        // The mark may begin among what was held, which held less of it.
        let from = held.saturating_sub(MARK.len() - 1);
        if let Ok(at) = seek(&before[from..], MARK) {
            let at = from + at;
            reader.consume(at + MARK.len() - held);
            before.truncate(at);
            return Ok(Skipped {
                before,
                found: true,
            });
        }
        reader.consume(read);
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

/// A host's first session, as the monitor it became has it: its standard
/// output, where the monitor says where it is, in lines a run's tag marks,
/// and which the client holds for as long as it keeps the session. It is
/// written as it is, blocking, the one connection no
/// [`Wire`](super::wire::Wire) is: what the user's shell left running shares
/// it, whose own writes there would fail were it made not to block. The
/// monitor writes a few hundred bytes there, once, before it waits for
/// anything.
pub struct Session(std::fs::File);

impl Session {
    /// This process's standard output, as its first session's.
    pub fn standard() -> io::Result<Self> {
        use std::os::fd::AsFd;
        let output = io::stdout().as_fd().try_clone_to_owned()?;
        Ok(Self(std::fs::File::from(output)))
    }

    /// Says a line of `kind`, marked with `tag`, as [`frame`] lays it out,
    /// in one write, which a pipe keeps whole.
    pub fn say(&mut self, tag: &str, kind: &str, content: &[u8]) -> io::Result<()> {
        self.0.write_all(&frame(tag, kind, content))
    }

    /// Waits until the client lets go of the session: it is gone, or done.
    pub fn wait_let_go(&self) {
        use rustix::event::{PollFd, PollFlags, poll};
        let mut polled = [PollFd::new(&self.0, PollFlags::empty())];
        loop {
            match poll(&mut polled, None) {
                Ok(_)
                    if polled[0]
                        .revents()
                        .intersects(PollFlags::ERR | PollFlags::HUP) =>
                {
                    return;
                }
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => return,
            }
        }
    }
}

/// The kind of the line a first session prints every beat, its heartbeat
/// before the monitor is there: no part counts it.
pub const ALIVE: &str = "alive";

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
/// runs has it. It also defines what the session's heartbeat, once
/// [`heartbeat`] starts it, is told by the commands that read the client's
/// input to its end: `ferry_watch FILE`, which counts the file growing, as
/// an upload into it, as word from the client; `ferry_unwatch`; `ferry_quiet`,
/// once the client can no longer be heard, its input read to its end;
/// `ferry_end`, which ends the heartbeat, as the session's commands end; and
/// `ferry_become COMMAND`, which ends the heartbeat and runs the command in
/// the session's place. Without a heartbeat, each does only what is left.
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
ferry_activity=
ferry_alive=
ferry_watchdog=
ferry_watch() {{
    if [ -n "$ferry_activity" ]; then printf %s "$1" > "$ferry_activity.watch"; fi
}}
ferry_unwatch() {{
    if [ -n "$ferry_activity" ]; then rm -f "$ferry_activity.watch"; fi
}}
ferry_quiet() {{
    if [ -n "$ferry_watchdog" ]; then kill "$ferry_watchdog" 2>/dev/null || :; fi
    if [ -n "$ferry_activity" ]; then rm -f "$ferry_activity" "$ferry_activity.watch"; fi
    ferry_activity=
}}
ferry_end() {{
    if [ -n "$ferry_alive" ]; then kill "$ferry_alive" 2>/dev/null || :; fi
    ferry_alive=
    ferry_quiet
}}
ferry_become() {{
    ferry_end
    exec "$@"
}}
"#
    )
}

/// Shell that starts a first session's heartbeat, both ways, as [`framing`]
/// defines what tells it: an [`ALIVE`] line every `beat` seconds, and a
/// watchdog that ends the session, and all it started, once nothing came
/// from the client for `silence` seconds: no command line, which the
/// client sends every beat, `:` when it has nothing else, nor a watched file
/// growing. The command loop marks each line it reads in the activity file.
/// Both end as the session's shell does, or as [`framing`]'s `ferry_become`
/// or `ferry_quiet` says. Neither leaves anything holding the session's
/// output once ended, not even the `sleep` it was in, for which `sshd` would
/// keep the session open.
pub fn heartbeat(beat: u64, silence: u64) -> String {
    format!(
        r#"ferry_activity="${{TMPDIR:-/tmp}}/ferry-session.$$"
(umask 077; : > "$ferry_activity")
ferry_shell=$$
(
    while sleep {beat} >/dev/null 2>&1; do
        kill -0 "$ferry_shell" 2>/dev/null || exit 0
        printf '\000ferry:%s:{ALIVE}:0:\n' "$tag"
    done
) &
ferry_alive=$!
(
    last=
    quiet=0
    while sleep {beat}; do
        if [ ! -e "$ferry_activity" ] || ! kill -0 "$ferry_shell" 2>/dev/null; then
            rm -f "$ferry_activity" "$ferry_activity.watch"
            exit 0
        fi
        now=$(wc -c < "$ferry_activity" 2>/dev/null)
        watched=$(cat "$ferry_activity.watch" 2>/dev/null) || watched=
        if [ -n "$watched" ]; then now="$now $(wc -c < "$watched" 2>/dev/null)"; fi
        if [ "$now" = "$last" ]; then quiet=$((quiet + {beat})); else last=$now; quiet=0; fi
        if [ "$quiet" -ge {silence} ]; then
            rm -f "$ferry_activity" "$ferry_activity.watch"
            kill -TERM -"$ferry_shell" 2>/dev/null || kill -TERM "$ferry_shell" "$ferry_alive" 2>/dev/null
            exit 0
        fi
    done
) >/dev/null 2>&1 &
ferry_watchdog=$!
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

/// What came on a host's output, as [`Scanner`] takes it apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Came {
    /// What is no line of Ferry's: what the host's shell printed, or what
    /// it left running; or what is for people.
    Noise(Vec<u8>),
    /// A line of Ferry's.
    Line(Line),
    /// The mark of a line of Ferry's, with no line after it: something
    /// printed cut into it. What came after the mark is taken apart anew.
    Cut(Vec<u8>),
}

/// Lines of Ferry's that one tag marks, as [`frame`] lays them out, taken
/// apart from what else is printed among them, as it comes, cut anywhere:
/// bytes in, what they come to out, doing no I/O. Every reader of a host's
/// output that tells lines of Ferry's from the rest goes through it: a first
/// session's, and a worker's lifeline.
pub struct Scanner {
    mark: Vec<u8>,
    /// The most a line may hold: more is no line.
    most: usize,
    /// What came and is not handed on yet: what may begin a mark, or a line
    /// not ended yet.
    pending: Vec<u8>,
}

/// What follows a line's mark, read so far.
enum Parsed {
    /// A line, and how many bytes past the mark it took.
    Line(Line, usize),
    /// Not all of it came yet.
    Wait,
    /// No line.
    Cut,
}

impl Scanner {
    /// A scanner of the lines `tag` marks, each holding at most `most`
    /// bytes.
    pub fn new(tag: &str, most: usize) -> Self {
        Self {
            mark: format!("\0ferry:{tag}:").into_bytes(),
            most,
            pending: Vec::new(),
        }
    }

    /// Takes the next bytes that came, handing `came` what they complete,
    /// in order.
    pub fn take(&mut self, bytes: &[u8], came: &mut impl FnMut(Came)) {
        self.pending.extend_from_slice(bytes);
        loop {
            match seek(&self.pending, &self.mark) {
                Err(keep) => {
                    let noise = self.pending.len() - keep;
                    if noise > 0 {
                        came(Came::Noise(self.pending.drain(..noise).collect()));
                    }
                    return;
                }
                Ok(at) if at > 0 => came(Came::Noise(self.pending.drain(..at).collect())),
                Ok(_) => {}
            }
            let marked = self.mark.len();
            match parse(&self.pending[marked..], self.most) {
                Parsed::Wait => return,
                Parsed::Line(line, length) => {
                    self.pending.drain(..marked + length);
                    came(Came::Line(line));
                }
                Parsed::Cut => came(Came::Cut(self.pending.drain(..marked).collect())),
            }
        }
    }

    /// The output ended: what waits is noise, or a line cut short.
    pub fn end(&mut self, came: &mut impl FnMut(Came)) {
        let pending = std::mem::take(&mut self.pending);
        if pending.starts_with(&self.mark) {
            came(Came::Cut(pending));
        } else if !pending.is_empty() {
            came(Came::Noise(pending));
        }
    }
}

/// What follows a line's mark, as far as it came: its kind, its length,
/// each graphic ASCII ended by `:`, then its content, of at most `most`
/// bytes, and a newline.
fn parse(rest: &[u8], most: usize) -> Parsed {
    let mut at = 0;
    let mut fields = Vec::with_capacity(2);
    while fields.len() < 2 {
        let start = at;
        loop {
            let Some(&byte) = rest.get(at) else {
                return Parsed::Wait;
            };
            at += 1;
            if byte == b':' {
                break;
            }
            if at - start > FIELD || !byte.is_ascii_graphic() {
                return Parsed::Cut;
            }
        }
        fields.push(&rest[start..at - 1]);
    }
    let Some(length) = std::str::from_utf8(fields[1])
        .ok()
        .and_then(|length| length.parse::<usize>().ok())
        .filter(|&length| length <= most)
    else {
        return Parsed::Cut;
    };
    let Some(&end) = rest.get(at + length) else {
        return Parsed::Wait;
    };
    if end != b'\n' {
        return Parsed::Cut;
    }
    let line = Line {
        kind: String::from_utf8(fields[0].to_vec()).expect("graphic ASCII"),
        content: rest[at..at + length].to_vec(),
    };
    Parsed::Line(line, at + length + 1)
}

/// A part of what a first session tells the client, its lines as they
/// come, up to the [`DONE`] line that ends it, which must count them all.
#[derive(Default)]
pub struct Part(Vec<Line>);

impl Part {
    /// Takes the next line: the part's lines, once it ended, or that one
    /// went missing.
    pub fn take(&mut self, line: Line) -> Option<io::Result<Vec<Line>>> {
        if line.kind == ALIVE {
            return None;
        }
        if line.kind != DONE {
            self.0.push(line);
            return None;
        }
        let lines = std::mem::take(&mut self.0);
        let counted = std::str::from_utf8(&line.content)
            .ok()
            .and_then(|count| count.parse::<usize>().ok());
        Some(if counted == Some(lines.len()) {
            Ok(lines)
        } else {
            Err(io::Error::other(
                "a line of Ferry's went missing among what the host printed",
            ))
        })
    }
}

/// That something the host printed cut into a line of Ferry's.
pub fn cut() -> io::Error {
    io::Error::other("something the host printed cut into a line of Ferry's")
}

/// Reads the next line with `tag` of what a first session tells the client,
/// a byte at a time, keeping in `skipped` what came before it, which is no
/// line of Ferry's; or `None`, once it ends first: for the tests, which read
/// a session's output as it is.
pub fn next_line(
    reader: &mut impl BufRead,
    tag: &str,
    skipped: &mut Vec<u8>,
) -> io::Result<Option<Line>> {
    let mut scanner = Scanner::new(tag, MOST);
    loop {
        let mut came = Vec::new();
        let byte = reader.fill_buf()?.first().copied();
        match byte {
            None => scanner.end(&mut |one| came.push(one)),
            Some(byte) => {
                reader.consume(1);
                scanner.take(&[byte], &mut |one| came.push(one));
            }
        }
        for one in came {
            match one {
                Came::Noise(noise) => skipped.extend_from_slice(&noise),
                Came::Line(line) => return Ok(Some(line)),
                Came::Cut(_) => return Err(cut()),
            }
        }
        if byte.is_none() {
            return Ok(None);
        }
    }
}

/// Reads a part of what a first session tells the client, as [`Part`] has
/// it; or `None`, once it ends first: for the tests.
pub fn read_part(
    reader: &mut impl BufRead,
    tag: &str,
    skipped: &mut Vec<u8>,
) -> io::Result<Option<Vec<Line>>> {
    let mut part = Part::default();
    while let Some(line) = next_line(reader, tag, skipped)? {
        if let Some(lines) = part.take(line) {
            return lines.map(Some);
        }
    }
    Ok(None)
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
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts threads as it likes"
)]
mod tests {
    use super::*;

    /// A first session's command loop, with its heartbeat, run here by `sh`,
    /// beating every second, and silent after `silence` seconds.
    fn session(silence: u64) -> crate::transport::child::Child {
        use std::process::{Command, Stdio};
        let script = format!(
            "{}{}finish\nwhile IFS= read -r line; do \
             if [ -n \"$ferry_activity\" ]; then printf . >> \"$ferry_activity\"; fi; \
             eval \"$line\"; done\nferry_end\n",
            framing("t"),
            heartbeat(1, silence)
        );
        crate::transport::child::Child::spawn(
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped()),
            None,
        )
        .unwrap()
    }

    /// Once nothing comes from the client, a first session's watchdog ends
    /// it, as when the client went silently; meanwhile it said it was alive.
    #[test]
    fn a_first_session_ends_once_nothing_comes_from_the_client() {
        use std::time::{Duration, Instant};
        let mut session = session(2);
        let _input = session.stdin.take().unwrap();
        let mut output = io::BufReader::new(session.stdout.take().unwrap());
        let started = Instant::now();
        // The part that starts it, then its heartbeat.
        assert!(
            read_part(&mut output, "t", &mut Vec::new())
                .unwrap()
                .is_some()
        );
        let alive = next_line(&mut output, "t", &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(alive.kind, ALIVE);
        assert!(
            session
                .wait_until(Instant::now() + Duration::from_secs(10))
                .is_some(),
            "the session stayed though nothing came"
        );
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    /// A first session the client sends a line every beat stays; once its
    /// input ends, it does too.
    #[test]
    fn a_first_session_heard_from_stays() {
        use std::io::Write;
        use std::time::{Duration, Instant};
        let mut session = session(2);
        let mut input = session.stdin.take().unwrap();
        let mut output = session.stdout.take().unwrap();
        // Read, so that its alive lines never wait for room.
        std::thread::spawn(move || io::copy(&mut output, &mut io::sink()));
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            input.write_all(b":\n").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(
            session.try_wait().is_none(),
            "the session ended though heard from"
        );
        drop(input);
        assert!(
            session
                .wait_until(Instant::now() + Duration::from_secs(10))
                .is_some()
        );
    }

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
