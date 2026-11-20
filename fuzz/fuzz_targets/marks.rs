//! What a host prints amid the lines of Ferry's that a tag marks, cut
//! anywhere, as a pipe hands it on: whatever comes, the scanner never
//! panics, and takes it apart alike however it is cut; and of lines framed
//! among what holds no mark, it finds each, as it was, and the rest is the
//! rest, as it was.
#![no_main]

use arbitrary::{Result, Unstructured};
use ferry::transport::start::{Came, Line, Scanner, frame};
use libfuzzer_sys::fuzz_target;

const TAG: &str = "0123456789abcdef";

/// The most a line holds, as a lifeline's does: past it, no line.
const MOST: usize = 64;

/// What the scanner makes of `bytes`, cut into parts of the sizes `cuts`
/// gives, in turn, the last as long as is left: what is not a line, each
/// stretch of it as one.
fn scanned(bytes: &[u8], cuts: &[usize]) -> Vec<Came> {
    let mut scanner = Scanner::new(TAG, MOST);
    let mut came = Vec::new();
    let mut rest = bytes;
    for &cut in cuts.iter().chain(std::iter::repeat(&usize::MAX)) {
        if rest.is_empty() {
            break;
        }
        let (part, after) = rest.split_at(cut.clamp(1, rest.len()));
        scanner.take(part, &mut |one| came.push(one));
        rest = after;
    }
    scanner.end(&mut |one| came.push(one));
    let mut merged: Vec<Came> = Vec::new();
    for one in came {
        if let (Some(Came::Noise(before)), Came::Noise(noise)) = (merged.last_mut(), &one) {
            before.extend_from_slice(noise);
            continue;
        }
        merged.push(one);
    }
    merged
}

fn cuts(u: &mut Unstructured) -> Result<Vec<usize>> {
    let count = u.int_in_range(0..=16)?;
    (0..count).map(|_| u.int_in_range(1..=40)).collect()
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(raw) = u.arbitrary::<Vec<u8>>() else {
        return;
    };
    let Ok(cut) = cuts(&mut u) else {
        return;
    };
    assert_eq!(scanned(&raw, &[]), scanned(&raw, &cut), "cut so: {cut:?}");

    // Lines among what holds no NUL, and so no mark.
    let (mut stream, mut lines, mut rest) = (Vec::new(), Vec::new(), Vec::new());
    let count = u.int_in_range(0..=6).unwrap_or(0);
    for _ in 0..=count {
        let noise: Vec<u8> = u
            .arbitrary::<Vec<u8>>()
            .unwrap_or_default()
            .into_iter()
            .filter(|&byte| byte != 0)
            .collect();
        stream.extend_from_slice(&noise);
        rest.extend_from_slice(&noise);
        if lines.len() == count {
            break;
        }
        let kind = *u.choose(&["probe", "done", "alive", "heard", "x"]).unwrap_or(&"x");
        let length = u.int_in_range(0..=MOST).unwrap_or(0);
        let content = u.bytes(length).unwrap_or_default().to_vec();
        stream.extend(frame(TAG, kind, &content));
        lines.push(Line {
            kind: kind.to_owned(),
            content,
        });
    }
    let cut = cuts(&mut u).unwrap_or_default();
    let (mut found, mut noise) = (Vec::new(), Vec::new());
    for one in scanned(&stream, &cut) {
        match one {
            Came::Line(line) => found.push(line),
            Came::Noise(bytes) => noise.extend_from_slice(&bytes),
            Came::Cut(bytes) => panic!("a line framed whole taken for cut: {bytes:?}"),
        }
    }
    assert_eq!(found, lines);
    assert_eq!(noise, rest);
});
