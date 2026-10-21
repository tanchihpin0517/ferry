//! What `getent` prints, which another account's name may make anything:
//! read as records, never panicking.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    for line in text.lines() {
        let _ = ferry::names::parse_record_for_fuzzing(line);
    }
});
