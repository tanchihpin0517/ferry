//! The process's limit on open files.
//!
//! A transfer holds many files open at once: those being written, those
//! waiting for their batch to be renamed, and the directories they are in.
//! The soft limit many systems start a process with is low (256 on macOS), so
//! it is raised to what the system allows, as Go does, and children are given
//! back the limit this process started with.

use std::process::Command;
use std::sync::OnceLock;

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// Below this many open files, the user is warned that transfers are slower.
pub const LOW: u64 = 1024;

/// The limit this process started with, once raised.
static STARTED_WITH: OnceLock<Rlimit> = OnceLock::new();

/// Raises the soft limit on open files to the most the system allows. Not
/// being able to is no error: the transfer works within what it has.
/// Returns the soft limit now.
pub fn raise() -> u64 {
    let started = getrlimit(Resource::Nofile);
    let _ = STARTED_WITH.set(started);
    let wanted = most_allowed(started.maximum);
    if started.current.is_some_and(|current| current < wanted) {
        let raised = Rlimit {
            current: Some(wanted),
            maximum: started.maximum,
        };
        let _ = setrlimit(Resource::Nofile, raised);
    }
    open_files()
}

/// How many files this process may have open at once.
pub fn open_files() -> u64 {
    getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX)
}

/// The most the soft limit may be raised to under the hard limit `maximum`,
/// `None` for none. On macOS, `setrlimit` refuses more than
/// `kern.maxfilesperproc`, which an unlimited hard limit does not say.
fn most_allowed(maximum: Option<u64>) -> u64 {
    let maximum = maximum.unwrap_or(u64::MAX);
    #[cfg(target_os = "macos")]
    {
        // `OPEN_MAX`, where the system does not say.
        maximum.min(max_files_per_process().unwrap_or(10240))
    }
    #[cfg(not(target_os = "macos"))]
    {
        maximum
    }
}

#[cfg(target_os = "macos")]
fn max_files_per_process() -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, and `value` and `length` describe
    // a buffer of the size the call is told.
    let found = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    (found && value > 0).then_some(value as u64)
}

/// Gives a child the limit this process started with: some programs close
/// every descriptor up to their soft limit, which takes long when it is
/// large.
pub fn restore_in_child(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    let Some(&started) = STARTED_WITH.get() else {
        return command;
    };
    // SAFETY: only `setrlimit`, a system call, runs between fork and exec.
    unsafe {
        command.pre_exec(move || {
            let _ = setrlimit(Resource::Nofile, started);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raised to what the system allows, and given back to a child as it was.
    /// It runs in a child of its own, so that no other test sees the limit
    /// change under it.
    #[test]
    fn the_limit_is_raised_and_a_child_gets_it_back() {
        let limit = getrlimit(Resource::Nofile);
        let maximum = limit.maximum.unwrap_or(u64::MAX);
        let soft = maximum.min(256);
        let script = format!(
            "ulimit -S -n {soft} && exec \"$0\" --exact limits::tests::raised_in_this_process --include-ignored --nocapture"
        );
        let output = Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg(std::env::current_exe().unwrap())
            .env("FERRY_LIMIT_STARTED_WITH", soft.to_string())
            .output()
            .unwrap();
        let shown = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{shown}");
        assert!(shown.contains("1 passed"), "{shown}");
    }

    /// The child's half of the test above.
    #[test]
    #[ignore = "run by the_limit_is_raised_and_a_child_gets_it_back"]
    fn raised_in_this_process() {
        // Run on its own, it has nothing to check.
        let Ok(started) = std::env::var("FERRY_LIMIT_STARTED_WITH") else {
            return;
        };
        let started: u64 = started.parse().unwrap();
        assert_eq!(open_files(), started);
        let maximum = getrlimit(Resource::Nofile).maximum;
        let raised = raise();
        assert_eq!(raised, most_allowed(maximum));
        assert_eq!(open_files(), raised);

        let mut command = Command::new("sh");
        command.args(["-c", "ulimit -S -n"]);
        let output = restore_in_child(&mut command).output().unwrap();
        let child = String::from_utf8_lossy(&output.stdout);
        assert_eq!(child.trim(), started.to_string());
    }
}
