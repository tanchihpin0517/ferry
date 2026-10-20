//! Faults put in on purpose, for tests to see what a bug or a failure does:
//! read from the environment in a debug build only; a release build reads
//! neither.
//!
//! - `FERRY_TEST_PANIC=<role>`: each stage of that role panics as it starts,
//!   such as `stat` or `reader`.
//! - `FERRY_TEST_FAIL=<point>`: Ferry fails where it passes that point:
//!   `frame-encode`, where a frame is compressed; `unnamed`, where a file is
//!   made without a name, as a filesystem that makes none does, so that it
//!   is made under a temporary name.
//! - `FERRY_TEST_MAX_PAYLOAD=<bytes>`: the largest payload a packet may
//!   have, instead of [`crate::protocol::MAX_PAYLOAD`], so that a test can
//!   pass it with a smaller tree. Every message must still fit: a dry run's
//!   listed lines go a mebibyte at a time, a manifest part may be 4 MiB, and
//!   a frame of data some 2 MiB, so it is for tests that send little but
//!   names, and is no less than 2 MiB.
//!
//! A server started on this machine inherits the client's environment, so a
//! test sets them for the command it runs.

use std::sync::OnceLock;

use anyhow::{Result, bail};

static PANIC: OnceLock<Option<String>> = OnceLock::new();
static FAIL: OnceLock<Option<String>> = OnceLock::new();
static MAX_PAYLOAD: OnceLock<Option<u32>> = OnceLock::new();

/// The value of `variable`, in a debug build, read once.
fn wanted(cell: &'static OnceLock<Option<String>>, variable: &str) -> Option<&'static str> {
    if !cfg!(debug_assertions) {
        return None;
    }
    cell.get_or_init(|| std::env::var(variable).ok()).as_deref()
}

/// Panics, if `FERRY_TEST_PANIC` names `role`.
pub fn panic_if(role: &str) {
    if wanted(&PANIC, "FERRY_TEST_PANIC") == Some(role) {
        panic!("a panic put in for a test");
    }
}

/// Fails, if `FERRY_TEST_FAIL` names `point`.
pub fn fail_if(point: &str) -> Result<()> {
    if failing(point) {
        bail!("a failure put in for a test, at {point}");
    }
    Ok(())
}

/// Whether `FERRY_TEST_FAIL` names `point`: for a failure that is not an
/// error, but a way taken instead of another.
pub fn failing(point: &str) -> bool {
    wanted(&FAIL, "FERRY_TEST_FAIL") == Some(point)
}

/// The largest payload a packet may have, if `FERRY_TEST_MAX_PAYLOAD` sets
/// one: read once, as every packet asks.
pub fn max_payload() -> Option<u32> {
    if !cfg!(debug_assertions) {
        return None;
    }
    *MAX_PAYLOAD.get_or_init(|| {
        let bytes = std::env::var("FERRY_TEST_MAX_PAYLOAD").ok()?;
        Some(
            bytes
                .parse()
                .expect("FERRY_TEST_MAX_PAYLOAD is a number of bytes"),
        )
    })
}
