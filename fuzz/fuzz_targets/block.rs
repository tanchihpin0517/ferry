//! Blocks as a peer may send them: decoded, and what each holds checked,
//! never panicking, nor any block decoding to more than a block holds.
#![no_main]

use ferry::block::{self, MOST};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(block) = block::decode(data) {
        assert!(block.payload.len() <= MOST);
        drop(block.parts());
    }
});
