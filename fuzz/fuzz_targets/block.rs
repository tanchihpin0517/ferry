//! The data's pipe as a peer may write it: its records read, and what each
//! holds of a file checked, never panicking, whatever comes.
#![no_main]

use ferry::block;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = std::io::BufReader::new(data);
    while let Ok(Some((buffer, record))) = block::read(&mut reader) {
        assert!(buffer.len() <= block::MOST);
        drop(block::part(buffer, record, || {}));
    }
});
