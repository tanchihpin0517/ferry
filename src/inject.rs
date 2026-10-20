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
//!
//! A server started on this machine inherits the client's environment, so a
//! test sets them for the command it runs.

use std::sync::OnceLock;

use anyhow::{Result, bail};

static PANIC: OnceLock<Option<String>> = OnceLock::new();
static FAIL: OnceLock<Option<String>> = OnceLock::new();

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
