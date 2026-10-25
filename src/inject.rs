//! Faults put in on purpose, for tests to see what a bug or a failure does:
//! read from the environment in a debug build only; a release build reads
//! neither.
//!
//! - `FERRY_TEST_PANIC=<role>`: each stage of that role panics as it starts,
//!   such as `stat` or `reader`.
//! - `FERRY_TEST_FAIL=<point>`: Ferry fails where it passes that point:
//!   `block-encode`, where a block is compressed; `write`, where a writer
//!   writes a whole file; `unnamed`, where a file is made without a name, as
//!   a filesystem that makes none does, so that it is made under a
//!   temporary name; `close`, where a file written under a temporary name is
//!   closed, as on NFS, which tells there an error writing it back, each
//!   file then made under a temporary name, as with `unnamed`.
//! - `FERRY_TEST_MAX_PAYLOAD=<bytes>`: the largest payload a packet may
//!   have, instead of [`crate::protocol::MAX_PAYLOAD`], so that a test can
//!   pass it with a smaller tree. Every message must still fit: a dry run's
//!   listed lines go a mebibyte at a time, a manifest part may be 4 MiB, and
//!   a data packet 64 KiB, so it is for tests that send little but names,
//!   and is no less than 2 MiB.
//! - `FERRY_TEST_MASTER_WAIT=<seconds>`: how long a quiet SSH login waits
//!   for a ControlMaster that never answers, instead of 30 s.
//! - `FERRY_TEST_STILL=<seconds>`: how long nothing moves before the client
//!   says where every thread is, instead of 60 s.
//! - `FERRY_TEST_SILENCE=<seconds>`: how long nothing may come on the
//!   stream, or on a server's control connection, before it is taken for
//!   broken, instead of 20 s; the stream's keepalives go four times as
//!   often.
//! - `FERRY_TEST_STALL=<role>`: each stage of that role waits 5 s as it
//!   starts, as one held up would; `signals`, the thread that takes them,
//!   so that a signal waits meanwhile; `signal-taken`, the same thread once
//!   it has taken a signal, before it does anything about it.
//! - `FERRY_TEST_REMOVE_DEPTH=<directories>`: how deep a tree is deleted,
//!   instead of [`crate::fsys::REMOVE_DEPTH`], so that a test can go past it
//!   with a tree any filesystem holds, and quickly: thousands of levels are
//!   slow on a Mac, and too long a path for overlayfs.
//!
//! A server started on this machine inherits the client's environment, so a
//! test sets them for the command it runs.

use std::sync::OnceLock;

use anyhow::{Result, bail};

static PANIC: OnceLock<Option<String>> = OnceLock::new();
static FAIL: OnceLock<Option<String>> = OnceLock::new();
static MAX_PAYLOAD: OnceLock<Option<u32>> = OnceLock::new();
static MASTER_WAIT: OnceLock<Option<u64>> = OnceLock::new();
static STILL: OnceLock<Option<u64>> = OnceLock::new();
static SILENCE: OnceLock<Option<u64>> = OnceLock::new();
static STALL: OnceLock<Option<String>> = OnceLock::new();
static REMOVE_DEPTH: OnceLock<Option<usize>> = OnceLock::new();

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

/// How deep a tree is deleted, if `FERRY_TEST_REMOVE_DEPTH` says.
pub fn remove_depth() -> Option<usize> {
    if !cfg!(debug_assertions) {
        return None;
    }
    *REMOVE_DEPTH.get_or_init(|| {
        let depth = std::env::var("FERRY_TEST_REMOVE_DEPTH").ok()?;
        Some(
            depth
                .parse()
                .expect("FERRY_TEST_REMOVE_DEPTH is a number of directories"),
        )
    })
}

/// How long a quiet SSH login waits for a ControlMaster, if
/// `FERRY_TEST_MASTER_WAIT` says.
pub fn master_wait() -> Option<std::time::Duration> {
    if !cfg!(debug_assertions) {
        return None;
    }
    MASTER_WAIT
        .get_or_init(|| {
            let seconds = std::env::var("FERRY_TEST_MASTER_WAIT").ok()?;
            Some(
                seconds
                    .parse()
                    .expect("FERRY_TEST_MASTER_WAIT is a number of seconds"),
            )
        })
        .map(std::time::Duration::from_secs)
}

/// Seconds read once from `variable`, in a debug build.
fn seconds(cell: &'static OnceLock<Option<u64>>, variable: &str) -> Option<std::time::Duration> {
    if !cfg!(debug_assertions) {
        return None;
    }
    cell.get_or_init(|| {
        let seconds = std::env::var(variable).ok()?;
        Some(
            seconds
                .parse()
                .unwrap_or_else(|_| panic!("{variable} is a number of seconds")),
        )
    })
    .map(std::time::Duration::from_secs)
}

/// How long nothing moves before the client says where every thread is, if
/// `FERRY_TEST_STILL` says.
pub fn still() -> Option<std::time::Duration> {
    seconds(&STILL, "FERRY_TEST_STILL")
}

/// How long nothing may come on the stream, or on a server's control
/// connection, if `FERRY_TEST_SILENCE` says.
pub fn silence() -> Option<std::time::Duration> {
    seconds(&SILENCE, "FERRY_TEST_SILENCE")
}

/// Waits 5 s, if `FERRY_TEST_STALL` names `role`.
pub fn stall_if(role: &str) {
    if wanted(&STALL, "FERRY_TEST_STALL") == Some(role) {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}
