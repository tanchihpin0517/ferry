//! Data packets, cut anywhere, as a peer may send them: put together into
//! frames and decoded, never panicking, nor any frame decoding to more than
//! a frame holds.
#![no_main]

use ferry::frame::{self, Assembler, FRAME};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (Vec<u8>, Vec<u8>)| {
    let (cuts, data) = input;
    let mut assembler = Assembler::default();
    let mut rest = data.as_slice();
    let mut cuts = cuts.iter().cycle();
    while !rest.is_empty() {
        let size = (usize::from(*cuts.next().unwrap_or(&255)) + 1).min(rest.len());
        let (piece, after) = rest.split_at(size);
        rest = after;
        let Ok(frames) = assembler.push(piece) else {
            return;
        };
        for done in frames {
            if let Ok(raw) = frame::decode(&done) {
                assert!(raw.len() <= FRAME);
            }
        }
    }
});
