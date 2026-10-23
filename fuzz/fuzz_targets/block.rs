//! Data packets as a peer may send them, cut anywhere: put together into
//! blocks, decoded, and what each holds checked, never panicking, nor any
//! block decoding to more than a block holds.
#![no_main]

use ferry::block::{self, Assembled, Assembler, MOST};
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
        let Ok(assembled) = assembler.push(piece) else {
            return;
        };
        for item in assembled {
            if let Assembled::Block(encoded) = item
                && let Ok(block) = block::decode(encoded)
            {
                assert!(block.payload().len() <= MOST);
                drop(block.into_parts());
            }
        }
    }
});
