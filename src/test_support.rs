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
    let (done, result) = crossbeam_channel::unbounded();
    let running = std::thread::spawn(move || {
        let _ = done.send(body());
    });
    match result.recv_timeout(limit) {
        Ok(value) => value,
        // The body panicked: its thread says why, and fails the test.
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = running.join() {
                std::panic::resume_unwind(panic);
            }
            panic!("{what} ended without a result");
        }
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            panic!("{what} took more than {limit:?}")
        }
    }
}

/// How long a test that waits on threads, pipes, or sockets may take before
/// it is taken to hang.
pub const PATIENCE: Duration = Duration::from_secs(60);

/// A temporary directory where a file may be made without a name, as on
/// most Linux filesystems: the system's, or one beside the build, where the
/// system's makes none, as a container's overlay may not. `None` where
/// neither does, for a test of such files to say it could not run.
#[cfg(target_os = "linux")]
pub fn unnamed_tempdir() -> Option<tempfile::TempDir> {
    let beside = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let found = [tempfile::tempdir().ok(), tempfile::tempdir_in(beside).ok()]
        .into_iter()
        .flatten()
        .find(|temp| {
            crate::fsys::open_root(temp.path().as_os_str())
                .and_then(crate::fsys::create_unnamed)
                .is_ok_and(|file| file.is_some())
        });
    if found.is_none() {
        eprintln!("no temporary directory here makes a file without a name: not tested");
    }
    found
}
