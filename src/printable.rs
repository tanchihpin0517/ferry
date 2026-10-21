//! Paths and lines made safe to print: a name may hold any byte but `/` and
//! NUL, and a byte a terminal acts on, such as ESC, or a newline that starts
//! a line of its own, would let a name forge output or drive the terminal.
//!
//! Each such byte is written as `\#` and three octal digits, as rsync writes
//! it: the C0 controls and DEL; the C1 controls, U+0080 to U+009F; the bidi
//! controls, which reorder what is shown; and any byte that is not part of
//! UTF-8. A backslash that would read as the start of one is written so too,
//! so that every byte of the output stands for itself, or for one escape.
//! Every other character, ASCII or not, is as it is.

/// `bytes`, a path or anything a name may be in, made safe to print.
pub fn path(bytes: &[u8]) -> String {
    let mut shown = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        for (at, character) in valid.char_indices() {
            if is_acted_on(character) || reads_as_escape(&valid.as_bytes()[at..]) {
                let mut encoded = [0; 4];
                for byte in character.encode_utf8(&mut encoded).bytes() {
                    escape(&mut shown, byte);
                }
            } else {
                shown.push(character);
            }
        }
        for &byte in chunk.invalid() {
            escape(&mut shown, byte);
        }
    }
    shown
}

/// A line to print, as [`path`] makes it safe: what a server said, which
/// may hold a name, and a newline of its own, which would forge another.
pub fn line(text: &str) -> String {
    path(text.as_bytes())
}

/// A path a dry run lists as one to transfer: as [`path`], with its first
/// byte escaped where it would read as a deletion's line.
pub fn listed(bytes: &[u8]) -> String {
    let shown = path(bytes);
    match shown.strip_prefix("deleting ") {
        Some(rest) => format!("\\#{:03o}eleting {rest}", b'd'),
        None => shown,
    }
}

/// Whether a terminal acts on `character`, or shows other text in its
/// place: a control, or a bidi control.
fn is_acted_on(character: char) -> bool {
    character.is_control() || matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Whether `bytes`, starting at a backslash, read as an escape.
fn reads_as_escape(bytes: &[u8]) -> bool {
    bytes.len() >= 5
        && bytes[0] == b'\\'
        && bytes[1] == b'#'
        && bytes[2..5].iter().all(|byte| (b'0'..=b'7').contains(byte))
}

fn escape(shown: &mut String, byte: u8) {
    shown.push_str(&format!("\\#{byte:03o}"));
}

/// The bytes `shown` stands for, each escape back as its byte.
#[cfg(test)]
pub fn decode(shown: &str) -> Vec<u8> {
    let bytes = shown.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if reads_as_escape(&bytes[at..]) {
            let octal = std::str::from_utf8(&bytes[at + 2..at + 5]).expect("digits");
            decoded.push(u8::from_str_radix(octal, 8).expect("octal"));
            at += 5;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain names, ASCII or not, are as they are.
    #[test]
    fn a_plain_name_is_as_it_is() {
        assert_eq!(path(b"dir/file.txt"), "dir/file.txt");
        assert_eq!(path("café/日本".as_bytes()), "café/日本");
        assert_eq!(path(b"a b\\c"), "a b\\c");
    }

    /// A control, a C1 control, a bidi control, or a byte not part of UTF-8
    /// is an escape; so is a backslash that would read as one.
    #[test]
    fn what_a_terminal_acts_on_is_escaped() {
        assert_eq!(path(b"a\nb"), "a\\#012b");
        assert_eq!(path(b"x\x1b[31mred"), "x\\#033[31mred");
        assert_eq!(path(b"del\x7f"), "del\\#177");
        assert_eq!(path("c1\u{9b}".as_bytes()), "c1\\#302\\#233");
        assert_eq!(path("bidi\u{202e}".as_bytes()), "bidi\\#342\\#200\\#256");
        assert_eq!(path(b"caf\xe9"), "caf\\#351");
        assert_eq!(path(b"\\#012"), "\\#134#012");
    }

    /// A name to transfer that starts as a deletion's line does is told apart.
    #[test]
    fn a_name_that_starts_as_a_deletion_is_told_apart() {
        assert_eq!(listed(b"deleting important"), "\\#144eleting important");
        assert_eq!(listed(b"deleted"), "deleted");
    }

    /// Any bytes, made safe to print, come back as they were, and what is
    /// printed holds no control.
    #[test]
    fn any_bytes_come_back() {
        let mut random = crate::test_support::Random::new(13);
        for _ in 0..2_000 {
            let length = random.below(64) as usize;
            let mut bytes = random.bytes(length);
            // Now and then what reads as an escape, or a part of one.
            if random.below(4) == 0 {
                let at = random.below(bytes.len() as u64 + 1) as usize;
                bytes.splice(at..at, b"\\#0".iter().copied());
            }
            let shown = path(&bytes);
            assert_eq!(decode(&shown), bytes, "{shown}");
            assert!(!shown.chars().any(is_acted_on), "{shown}");
        }
    }

    /// Every byte comes back as it was, and what is printed holds no control.
    #[test]
    fn every_byte_comes_back() {
        let mut every: Vec<u8> = (0..=255).collect();
        every.extend_from_slice("\\#000 \u{9b} \u{2066} é".as_bytes());
        for start in 0..every.len() {
            let bytes = &every[start..];
            let shown = path(bytes);
            assert_eq!(decode(&shown), bytes, "{shown}");
            assert!(!shown.chars().any(is_acted_on), "{shown}");
        }
    }
}
