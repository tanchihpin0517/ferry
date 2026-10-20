//! How many of the machine's cores Ferry runs on: [`DEFAULT`] unless told,
//! so that a transfer on a host many share, as a cluster's login node,
//! leaves the rest to the others there. Measured with 200,000 files of
//! 4 KiB on 96 cores, kept to 8 a copy was as fast, and spent less than half
//! the CPU: the rest went on threads spinning on the kernel's locks.
//!
//! A stage still has as many threads as the machine has cores, as
//! [`crate::pool::threads`] counts by [`machine`]: they mostly wait on the
//! filesystem, and on NFS, with fewer, the same copy took a tenth longer.
//!
//! On Linux, the process is kept to them by its CPU affinity, which every
//! thread it starts after inherits, and every process it runs, as a server
//! on this machine. The cores are the allowed ones from a place that each
//! process's ID sets, so that several Ferrys on one host are not all on the
//! same ones. Elsewhere, as on macOS, which has no such call, every core is
//! used.

use std::sync::OnceLock;

/// How many cores Ferry runs on, unless told otherwise.
pub const DEFAULT: usize = 8;

/// How many cores this process could run on before any limit.
static MACHINE: OnceLock<usize> = OnceLock::new();

/// How many cores this process could run on before [`limit`] kept it to
/// fewer, as a stage's threads are counted by.
pub fn machine() -> usize {
    *MACHINE.get_or_init(|| std::thread::available_parallelism().map_or(4, |count| count.get()))
}

/// Keeps this process, and every thread it starts from now on, to at most
/// `cpus` of the cores it may run on; all of them for `None`. Returns how
/// many it may run on now.
pub fn limit(cpus: Option<usize>) -> usize {
    let machine = machine();
    let Some(cpus) = cpus.filter(|&cpus| cpus < machine) else {
        return machine;
    };
    keep_to(cpus.max(1)).unwrap_or(machine)
}

#[cfg(target_os = "linux")]
fn keep_to(cpus: usize) -> Option<usize> {
    // SAFETY: a `cpu_set_t` is plain data, all of whose bits may be zero,
    // and each call is given its size.
    unsafe {
        let mut allowed: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut allowed) != 0 {
            return None;
        }
        let cores: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
            .filter(|&core| libc::CPU_ISSET(core, &allowed))
            .collect();
        if cores.len() <= cpus {
            return Some(cores.len());
        }
        let start = std::process::id() as usize % cores.len();
        let mut kept: libc::cpu_set_t = std::mem::zeroed();
        for at in 0..cpus {
            libc::CPU_SET(cores[(start + at) % cores.len()], &mut kept);
        }
        (libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &kept) == 0).then_some(cpus)
    }
}

#[cfg(not(target_os = "linux"))]
fn keep_to(_cpus: usize) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kept to one core, a thread started after runs on that one; the test
    /// runs on a thread of its own, so that no other test is kept so.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_thread_started_after_the_limit_keeps_to_it() {
        if machine() < 2 {
            return;
        }
        let kept = std::thread::spawn(|| {
            let now = limit(Some(1));
            let after = std::thread::spawn(|| std::thread::available_parallelism().unwrap().get())
                .join()
                .unwrap();
            (now, after)
        })
        .join()
        .unwrap();
        assert_eq!(kept, (1, 1));
        // No limit, or one of more than there are, keeps to none.
        assert_eq!(limit(None), machine());
        assert_eq!(limit(Some(machine() + 1)), machine());
    }
}
