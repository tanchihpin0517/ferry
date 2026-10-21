//! A connection's paths as a far side gone wrong may write them: pieces of
//! any numbers and contents, on up to four paths, are put back in order, or
//! fail, never panicking, nor waiting for ever, as every path ends.
#![no_main]

use ferry::transport::striped::{self, END};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (Vec<Vec<(u64, Vec<u8>)>>, bool)| {
    let (paths, end) = input;
    let paths: Vec<_> = paths.into_iter().take(4).collect();
    if paths.is_empty() {
        return;
    }
    let readers: Vec<std::io::Cursor<Vec<u8>>> = paths
        .into_iter()
        .map(|pieces| {
            let mut bytes = Vec::new();
            for (number, payload) in pieces.into_iter().take(300) {
                bytes.extend_from_slice(&number.to_le_bytes());
                let _ = ferry::protocol::send_data(&mut bytes, &payload);
            }
            if end {
                bytes.extend_from_slice(&END.to_le_bytes());
            }
            std::io::Cursor::new(bytes)
        })
        .collect();
    let mut receiving = striped::receiving(readers);
    while let Ok(Some(_)) = receiving.recv() {}
});
