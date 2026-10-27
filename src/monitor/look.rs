//! A process as its monitor sees it from outside, from the system, whatever
//! the process itself can say: each thread, running, asleep, stopped, or in
//! a wait in the kernel that nothing interrupts, as on a filesystem that
//! does not answer; on Linux, also where in the kernel it waits, in which
//! call, and on which file. What the process says of itself, each thread's
//! role and state, comes on its control connection; this is what is seen
//! when it cannot say, as when all its threads are stuck. See the
//! [draft](../../../docs/drafts/silent-drops.md).
//!
//! A worker run through `sudo` runs as root, whose threads the monitor may
//! not read: then only whether it is there is seen.

use serde::{Deserialize, Serialize};

/// A process, seen from outside.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outside {
    /// It is there.
    pub alive: bool,
    /// It is stopped, as by SIGSTOP.
    pub stopped: bool,
    /// Its threads, where they could be read.
    pub threads: Vec<Thread>,
}

/// A thread, seen from outside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Its name, as the system has it.
    pub name: String,
    pub state: State,
    /// Where in the kernel it waits, as Linux names it.
    pub wchan: Option<String>,
    /// The call it is in, and the file that call is on, where known.
    pub call: Option<String>,
    pub file: Option<String>,
}

/// What a thread is doing, as the system says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Running,
    /// Asleep, as waiting for something that may interrupt it.
    Waiting,
    /// In a wait nothing interrupts, as on a filesystem that does not
    /// answer; not even SIGKILL ends it.
    Uninterruptible,
    Stopped,
    /// Ended, its process not yet.
    Ending,
    /// Something else the system says.
    Other,
}

impl State {
    fn shown(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Uninterruptible => "in a wait nothing interrupts",
            Self::Stopped => "stopped",
            Self::Ending => "ending",
            Self::Other => "in another state",
        }
    }
}

/// The process `pid`, seen from outside.
pub fn outside(pid: u32) -> Outside {
    // SAFETY: `kill` with no signal only asks whether the process is there.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return Outside::default();
    }
    let mut seen = threads(pid);
    seen.alive = true;
    seen
}

#[cfg(target_os = "linux")]
fn threads(pid: u32) -> Outside {
    let mut outside = Outside::default();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return outside;
    };
    for task in tasks.filter_map(Result::ok) {
        let path = task.path();
        let Ok(stat) = std::fs::read_to_string(path.join("stat")) else {
            continue;
        };
        // `pid (name) state ...`: the name may hold anything, a `)` too.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let name = stat[open + 1..close].to_owned();
        let state = match stat[close + 1..].trim_start().chars().next() {
            Some('R') => State::Running,
            Some('S' | 'I') => State::Waiting,
            Some('D') => State::Uninterruptible,
            Some('T' | 't') => State::Stopped,
            Some('Z' | 'X') => State::Ending,
            _ => State::Other,
        };
        let wchan = std::fs::read_to_string(path.join("wchan"))
            .ok()
            .filter(|wchan| !wchan.is_empty() && wchan != "0");
        let (call, file) = std::fs::read_to_string(path.join("syscall"))
            .ok()
            .map_or((None, None), |syscall| call(pid, &syscall));
        outside.threads.push(Thread {
            name,
            state,
            wchan,
            call,
            file,
        });
    }
    outside.stopped = !outside.threads.is_empty()
        && outside
            .threads
            .iter()
            .all(|thread| thread.state == State::Stopped);
    outside
}

/// The call a thread is in, as `/proc/<pid>/task/<tid>/syscall` says, and
/// the file it is on, for a call on a descriptor.
#[cfg(target_os = "linux")]
fn call(pid: u32, syscall: &str) -> (Option<String>, Option<String>) {
    let mut fields = syscall.split_whitespace();
    let Some(Ok(number)) = fields.next().map(str::parse::<i64>) else {
        return (None, None);
    };
    let Some((name, on_fd)) = name_of(number) else {
        return (Some(format!("system call {number}")), None);
    };
    let file = on_fd
        .then(|| fields.next())
        .flatten()
        .and_then(|fd| i64::from_str_radix(fd.trim_start_matches("0x"), 16).ok())
        .and_then(|fd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok())
        .map(|target| target.display().to_string());
    (Some(format!("{name}()")), file)
}

/// The names of the calls a thread is most often found waiting in, and
/// whether their first argument is a descriptor.
#[cfg(target_os = "linux")]
fn name_of(number: i64) -> Option<(&'static str, bool)> {
    #[cfg(target_arch = "x86_64")]
    let known: &[(i64, &str, bool)] = &[
        (0, "read", true),
        (1, "write", true),
        (3, "close", true),
        (5, "fstat", true),
        (7, "poll", false),
        (17, "pread64", true),
        (18, "pwrite64", true),
        (19, "readv", true),
        (20, "writev", true),
        (23, "select", false),
        (35, "nanosleep", false),
        (40, "sendfile", true),
        (42, "connect", true),
        (43, "accept", true),
        (46, "sendmsg", true),
        (47, "recvmsg", true),
        (61, "wait4", false),
        (74, "fsync", true),
        (75, "fdatasync", true),
        (77, "ftruncate", true),
        (82, "rename", false),
        (87, "unlink", false),
        (202, "futex", false),
        (217, "getdents64", true),
        (230, "clock_nanosleep", false),
        (232, "epoll_wait", false),
        (257, "openat", false),
        (262, "newfstatat", false),
        (263, "unlinkat", false),
        (264, "renameat", false),
        (270, "pselect6", false),
        (271, "ppoll", false),
        (275, "splice", true),
        (276, "tee", true),
        (281, "epoll_pwait", false),
        (285, "fallocate", true),
        (288, "accept4", true),
        (295, "preadv", true),
        (296, "pwritev", true),
        (316, "renameat2", false),
        (326, "copy_file_range", true),
        (332, "statx", false),
    ];
    #[cfg(target_arch = "aarch64")]
    let known: &[(i64, &str, bool)] = &[
        (22, "epoll_pwait", false),
        (35, "unlinkat", false),
        (38, "renameat", false),
        (46, "ftruncate", true),
        (47, "fallocate", true),
        (56, "openat", false),
        (57, "close", true),
        (61, "getdents64", true),
        (63, "read", true),
        (64, "write", true),
        (65, "readv", true),
        (66, "writev", true),
        (67, "pread64", true),
        (68, "pwrite64", true),
        (69, "preadv", true),
        (70, "pwritev", true),
        (71, "sendfile", true),
        (72, "pselect6", false),
        (73, "ppoll", false),
        (76, "splice", true),
        (77, "tee", true),
        (79, "newfstatat", false),
        (80, "fstat", true),
        (82, "fsync", true),
        (83, "fdatasync", true),
        (98, "futex", false),
        (101, "nanosleep", false),
        (115, "clock_nanosleep", false),
        (202, "accept", true),
        (203, "connect", true),
        (211, "sendmsg", true),
        (212, "recvmsg", true),
        (242, "accept4", true),
        (260, "wait4", false),
        (276, "renameat2", false),
        (285, "copy_file_range", true),
        (291, "statx", false),
    ];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let known: &[(i64, &str, bool)] = &[];
    known
        .iter()
        .find(|(known, ..)| *known == number)
        .map(|(_, name, on_fd)| (*name, *on_fd))
}

#[cfg(target_os = "macos")]
fn threads(pid: u32) -> Outside {
    macos::threads(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn threads(_pid: u32) -> Outside {
    Outside::default()
}

#[cfg(target_os = "macos")]
mod macos {
    //! `proc_pidinfo`, which reads another process of the same user's, as
    //! tried on macOS 15 and 26: its threads, each running or waiting, and
    //! whether the process is stopped, which a thread does not say.

    use super::{Outside, State, Thread};
    use std::mem::{MaybeUninit, size_of};

    /// The flavor that lists a process's threads, which `libc` does not
    /// name.
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    /// `pbi_status` of a stopped process.
    const SSTOP: u32 = 4;
    /// `pth_run_state`s.
    const TH_STATE_RUNNING: i32 = 1;
    const TH_STATE_STOPPED: i32 = 2;
    const TH_STATE_WAITING: i32 = 3;
    const TH_STATE_UNINTERRUPTIBLE: i32 = 4;

    pub(super) fn threads(pid: u32) -> Outside {
        let pid = pid as libc::c_int;
        let mut outside = Outside::default();
        let mut bsd = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `proc_pidinfo` writes at most `size` bytes to `bsd`.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                bsd.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            // Not ours to read, as root's.
            return outside;
        }
        // SAFETY: filled, as `proc_pidinfo` said.
        outside.stopped = unsafe { bsd.assume_init() }.pbi_status == SSTOP;
        let mut ids = vec![0u64; 512];
        let bytes = (ids.len() * size_of::<u64>()) as libc::c_int;
        // SAFETY: `proc_pidinfo` writes at most `bytes` bytes to `ids`.
        let listed = unsafe {
            libc::proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, ids.as_mut_ptr().cast(), bytes)
        };
        if listed <= 0 {
            return outside;
        }
        ids.truncate(listed as usize / size_of::<u64>());
        for id in ids {
            let mut info = MaybeUninit::<libc::proc_threadinfo>::zeroed();
            let size = size_of::<libc::proc_threadinfo>() as libc::c_int;
            // SAFETY: as above, for one thread's.
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTHREADINFO,
                    id,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if read != size {
                continue;
            }
            // SAFETY: filled, as `proc_pidinfo` said.
            let info = unsafe { info.assume_init() };
            let state = match info.pth_run_state {
                TH_STATE_RUNNING => State::Running,
                TH_STATE_WAITING => State::Waiting,
                TH_STATE_UNINTERRUPTIBLE => State::Uninterruptible,
                TH_STATE_STOPPED => State::Stopped,
                _ => State::Other,
            };
            let name: Vec<u8> = info
                .pth_name
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| *byte as u8)
                .collect();
            outside.threads.push(Thread {
                name: String::from_utf8_lossy(&name).into_owned(),
                // A stopped process's threads say they wait.
                state: if outside.stopped {
                    State::Stopped
                } else {
                    state
                },
                wchan: None,
                call: None,
                file: None,
            });
        }
        outside
    }
}

/// What the user is told of `outside`, `name` the process as they know it:
/// a line for each state its threads are in, how many; and one for each in
/// a wait nothing interrupts, where, and on what.
pub fn describe(name: &str, outside: &Outside) -> Vec<String> {
    if !outside.alive {
        return vec![format!("{name}, from outside: gone")];
    }
    if outside.threads.is_empty() {
        let what = if outside.stopped { "stopped" } else { "there" };
        return vec![format!(
            "{name}, from outside: {what}; its threads cannot be read here"
        )];
    }
    let mut lines = Vec::new();
    let mut counted: Vec<(State, usize)> = Vec::new();
    for thread in &outside.threads {
        match counted.iter_mut().find(|(state, _)| *state == thread.state) {
            Some((_, count)) => *count += 1,
            None => counted.push((thread.state, 1)),
        }
    }
    let counts: Vec<String> = counted
        .iter()
        .map(|(state, count)| format!("{count} {}", state.shown()))
        .collect();
    // Where only the process says it is stopped, as on macOS, whose threads
    // then say they wait.
    let stopped = outside.stopped
        && !outside
            .threads
            .iter()
            .any(|thread| thread.state == State::Stopped);
    lines.push(format!(
        "{name}, from outside: {}{} thread{}, {}",
        if stopped { "stopped, " } else { "" },
        outside.threads.len(),
        if outside.threads.len() == 1 { "" } else { "s" },
        counts.join(", ")
    ));
    for thread in &outside.threads {
        if thread.state != State::Uninterruptible {
            continue;
        }
        let mut said = format!("  {} is in a wait nothing interrupts", thread.name);
        if let Some(wchan) = &thread.wchan {
            said.push_str(&format!(", in the kernel at {wchan}"));
            if let Some(what) = waits_on(wchan) {
                said.push_str(&format!(", which waits for {what}"));
            }
        }
        if let Some(call) = &thread.call {
            said.push_str(&format!(", in {call}"));
        }
        if let Some(file) = &thread.file {
            said.push_str(&format!(" on {file}"));
        }
        lines.push(said);
    }
    lines
}

/// What a wait in the kernel at `wchan` waits for, where its name says:
/// a filesystem that answers over the network, or a program.
fn waits_on(wchan: &str) -> Option<&'static str> {
    const KNOWN: [(&str, &str); 9] = [
        ("nfs", "NFS"),
        ("rpc_", "NFS"),
        ("fuse", "a FUSE filesystem"),
        ("request_wait_answer", "a FUSE filesystem"),
        ("cifs", "SMB"),
        ("smb", "SMB"),
        ("ptlrpc", "Lustre"),
        ("ll_", "Lustre"),
        ("ceph", "Ceph"),
    ];
    KNOWN
        .iter()
        .find(|(start, _)| wchan.starts_with(start))
        .map(|(_, what)| *what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::transport::child::Child;

    #[test]
    fn a_process_is_seen_waiting_then_stopped_from_outside() {
        let child = Child::spawn(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null()),
            None,
        )
        .unwrap();
        let pid = child.id();
        // Just started, as it runs its program, it may show no thread yet,
        // on macOS under load: looked at until it does.
        let started = Instant::now();
        let seen = loop {
            let seen = outside(pid);
            if !seen.threads.is_empty()
                || !cfg!(any(target_os = "linux", target_os = "macos"))
                || started.elapsed() > Duration::from_secs(10)
            {
                break seen;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(seen.alive);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(!seen.threads.is_empty(), "{seen:?}");
            assert!(!seen.stopped);
        }
        // SAFETY: a signal to the test's own child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) };
        let started = Instant::now();
        while !child.paused() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = outside(pid);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(seen.stopped, "{seen:?}");
            let said = describe("the server", &seen);
            assert!(said[0].contains("stopped"), "{said:?}");
        }
        child.kill();
        child.wait();
        assert!(!outside(pid).alive);
        assert_eq!(
            describe("the server", &outside(pid)),
            ["the server, from outside: gone"]
        );
    }

    #[test]
    fn a_wait_nothing_interrupts_is_said_where_and_on_what() {
        let seen = Outside {
            alive: true,
            stopped: false,
            threads: vec![
                Thread {
                    name: "writer".to_owned(),
                    state: State::Uninterruptible,
                    wchan: Some("nfs_wait_bit_killable".to_owned()),
                    call: Some("write()".to_owned()),
                    file: Some("/work/x/y".to_owned()),
                },
                Thread {
                    name: "pump".to_owned(),
                    state: State::Waiting,
                    wchan: None,
                    call: None,
                    file: None,
                },
            ],
        };
        assert_eq!(
            describe("the Destination on nano4", &seen),
            [
                "the Destination on nano4, from outside: 2 threads, 1 in a wait nothing interrupts, 1 waiting",
                "  writer is in a wait nothing interrupts, in the kernel at nfs_wait_bit_killable, which waits for NFS, in write() on /work/x/y",
            ]
        );
    }

    /// A thread blocked reading a pipe is seen in `read()`, on the pipe: the
    /// shell's own `read`, as a `cat` may splice instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_thread_in_a_call_is_seen_in_it_on_its_file() {
        let child = Child::spawn(
            Command::new("sh")
                .args(["-c", "read -r line"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null()),
            None,
        )
        .unwrap();
        let pid = child.id();
        let started = Instant::now();
        let thread = loop {
            let seen = outside(pid);
            if let Some(thread) = seen
                .threads
                .iter()
                .find(|thread| thread.call.as_deref() == Some("read()"))
            {
                break thread.clone();
            }
            assert!(started.elapsed() < Duration::from_secs(10), "{seen:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            thread
                .file
                .as_deref()
                .is_some_and(|file| file.starts_with("pipe:")),
            "{thread:?}"
        );
        child.kill();
        child.wait();
    }
}
