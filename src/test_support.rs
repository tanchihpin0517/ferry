//! What the tests share, for tests only.

use std::time::Duration;

/// Runs `body` on a thread of its own, and fails the test, naming `what`, if
/// it takes longer than `limit`, rather than wait for ever: a test that hangs
/// says so, and where, with every thread of the process, as
/// [`say_where_threads_are`] finds them. The stuck thread is left behind.
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
            say_where_threads_are(what);
            panic!("{what} took more than {limit:?}")
        }
    }
}

/// Says, on standard error, where every thread of this process is, for a
/// hang to leave its evidence where it happened, as on CI (TRN-11): on
/// macOS, each thread's stack, by `sample`, and the open descriptors, by
/// `lsof`; on Linux, each thread's name, state, and what it waits in.
pub fn say_where_threads_are(what: &str) {
    let pid = std::process::id().to_string();
    eprintln!("{what}: where every thread of process {pid} is:");
    if cfg!(target_os = "macos") {
        for command in [
            vec!["sample", pid.as_str(), "2"],
            vec!["lsof", "-p", pid.as_str()],
        ] {
            match std::process::Command::new(command[0])
                .args(&command[1..])
                .output()
            {
                Ok(output) => {
                    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
                    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
                }
                Err(error) => eprintln!("{}: {error}", command[0]),
            }
        }
        return;
    }
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return;
    };
    for task in tasks.flatten() {
        let read = |name: &str| {
            std::fs::read_to_string(task.path().join(name))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let stat = read("stat");
        // The state follows the name, which is in parentheses.
        let state = stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.split(' ').next())
            .unwrap_or("?")
            .to_owned();
        eprintln!(
            "  thread {} {:?}: state {state}, waiting in {}",
            task.file_name().to_string_lossy(),
            read("comm"),
            read("wchan")
        );
    }
}

/// Numbers that look random, the same every run from the same seed: for a
/// test of a property over many inputs, without a crate for it.
pub struct Random(u64);

impl Random {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// The next number, by xorshift.
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number below `bound`, which is more than 0.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// `length` bytes, of all values, or, now and then, of few, which
    /// compress.
    pub fn bytes(&mut self, length: usize) -> Vec<u8> {
        let few = self.below(4) == 0;
        (0..length)
            .map(|_| {
                let byte = self.next() as u8;
                if few { byte % 3 } else { byte }
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A body that takes too long fails the test, by name, having said
    /// where the threads are.
    #[test]
    #[should_panic(expected = "a sleep took more than")]
    fn a_body_that_takes_too_long_fails_by_name() {
        within("a sleep", Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(2));
        });
    }
}
