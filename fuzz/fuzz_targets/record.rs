//! A record stream as a peer may send it: headers and trailers read in turn
//! never panic.
#![no_main]

use ferry::record;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = data;
    for turn in 0.. {
        let read = if turn % 2 == 0 {
            record::read_header(&mut reader).map(drop)
        } else {
            record::read_trailer(&mut reader).map(drop)
        };
        if read.is_err() || reader.is_empty() {
            break;
        }
    }
});
