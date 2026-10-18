//! What the tests share, for tests only.

use std::time::Duration;

/// Runs `body` on a thread of its own, and fails the test, naming `what`, if
/// it takes longer than `limit`, rather than wait for ever: a test that hangs
/// says so, and where. The stuck thread is left behind.
pub fn within<T: Send + 'static>(
    what: &str,
    limit: Duration,
    body: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (done, result) = std::sync::mpsc::channel();
    let running = std::thread::spawn(move || {
        let _ = done.send(body());
    });
    match result.recv_timeout(limit) {
        Ok(value) => value,
        // The body panicked: its thread says why, and fails the test.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = running.join() {
                std::panic::resume_unwind(panic);
            }
            panic!("{what} ended without a result");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} took more than {limit:?}")
        }
    }
}

/// How long a test that waits on threads, pipes, or sockets may take before
/// it is taken to hang.
pub const PATIENCE: Duration = Duration::from_secs(60);
