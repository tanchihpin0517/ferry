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
//! thread it starts after inherits. A server on the client's machine is
//! started before, and keeps to its own, as its hello says, from all the
//! machine's. The cores are the allowed ones from a place that the client's
//! process ID sets, for the client and each server alike: those of one
//! transfer on one machine are the same, and several Ferrys on one host are
//! not all on the same ones. They are all on one NUMA node, where one has
//! enough allowed: on two nodes of 48, the same copy, spread over both, took
//! half as long again, its writers writing two and a half times as long.
//! Elsewhere, as on macOS, which has no such call, every core is used.

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
/// `cpus` of the cores it may run on, those `place` picks; all of them for
/// `None`. Returns how many it may run on now.
pub fn limit(cpus: Option<usize>, place: u32) -> usize {
    let machine = machine();
    let Some(cpus) = cpus.filter(|&cpus| cpus < machine) else {
        return machine;
    };
    keep_to(cpus.max(1), place as usize).unwrap_or(machine)
}

#[cfg(target_os = "linux")]
fn keep_to(cpus: usize, place: usize) -> Option<usize> {
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
        let mut kept: libc::cpu_set_t = std::mem::zeroed();
        for core in choose(&cores, &nodes(), cpus, place) {
            libc::CPU_SET(core, &mut kept);
        }
        (libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &kept) == 0).then_some(cpus)
    }
}

/// The cores of each NUMA node, as Linux lists them; none where it does not.
#[cfg(target_os = "linux")]
fn nodes() -> Vec<Vec<usize>> {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/node") else {
        return Vec::new();
    };
    // By number, so that each process on the machine takes them in the same
    // order, and the same place picks the same node.
    let mut nodes: Vec<(usize, Vec<usize>)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let number = entry
                .file_name()
                .to_str()?
                .strip_prefix("node")?
                .parse()
                .ok()?;
            let list = std::fs::read_to_string(entry.path().join("cpulist")).ok()?;
            Some((number, cpu_list(&list)?))
        })
        .collect();
    nodes.sort_unstable();
    nodes.into_iter().map(|(_, cores)| cores).collect()
}

/// The cores of a list as Linux writes one, such as `0-23,48-71`; `None`
/// if it is not one.
#[cfg(target_os = "linux")]
fn cpu_list(list: &str) -> Option<Vec<usize>> {
    let mut cores = Vec::new();
    for range in list.trim().split(',').filter(|range| !range.is_empty()) {
        match range.split_once('-') {
            Some((first, last)) => cores.extend(first.parse::<usize>().ok()?..=last.parse().ok()?),
            None => cores.push(range.parse().ok()?),
        }
    }
    Some(cores)
}

/// Which `cpus` of the `allowed` cores, in order, to keep to, as `place`
/// picks: a run of those of one of `nodes` that has as many allowed, or, if
/// none has, of all those allowed.
#[cfg(target_os = "linux")]
fn choose(allowed: &[usize], nodes: &[Vec<usize>], cpus: usize, place: usize) -> Vec<usize> {
    let enough: Vec<Vec<usize>> = nodes
        .iter()
        .map(|node| {
            node.iter()
                .copied()
                .filter(|core| allowed.binary_search(core).is_ok())
                .collect::<Vec<_>>()
        })
        .filter(|node| !node.is_empty() && node.len() >= cpus)
        .collect();
    let (from, place) = match enough.len() {
        0 => (allowed, place),
        count => (enough[place % count].as_slice(), place / count),
    };
    if from.is_empty() {
        return Vec::new();
    }
    let start = place % from.len();
    (0..cpus.min(from.len()))
        .map(|at| from[(start + at) % from.len()])
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn keep_to(_cpus: usize, _place: usize) -> Option<usize> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Kept to one core, a thread started after runs on that one; the test
    /// runs on a thread of its own, so that no other test is kept so.
    #[test]
    fn a_thread_started_after_the_limit_keeps_to_it() {
        if machine() < 2 {
            return;
        }
        let kept = std::thread::spawn(|| {
            let now = limit(Some(1), std::process::id());
            let after = std::thread::spawn(|| std::thread::available_parallelism().unwrap().get())
                .join()
                .unwrap();
            (now, after)
        })
        .join()
        .unwrap();
        assert_eq!(kept, (1, 1));
        // No limit, or one of more than there are, keeps to none.
        assert_eq!(limit(None, 0), machine());
        assert_eq!(limit(Some(machine() + 1), 0), machine());
    }

    /// Two nodes of 24 cores, each core's sibling 48 on: the machine this
    /// was measured on.
    fn two_nodes() -> Vec<Vec<usize>> {
        vec![
            (0..24).chain(48..72).collect(),
            (24..48).chain(72..96).collect(),
        ]
    }

    fn sorted(mut cores: Vec<usize>) -> Vec<usize> {
        cores.sort_unstable();
        cores
    }

    #[test]
    fn a_list_of_cores_is_read_as_linux_writes_it() {
        assert_eq!(cpu_list("0-3,8,10-11\n"), Some(vec![0, 1, 2, 3, 8, 10, 11]));
        assert_eq!(cpu_list("\n"), Some(vec![]));
        assert_eq!(cpu_list("0-x"), None);
    }

    /// Whatever the place, the cores kept to are on one node, as many as
    /// asked, each once; and as places differ, so do the cores.
    #[test]
    fn the_cores_kept_to_are_on_one_node() {
        let nodes = two_nodes();
        let allowed: Vec<usize> = (0..96).collect();
        let mut firsts = std::collections::HashSet::new();
        for place in 0..1000 {
            let chosen = choose(&allowed, &nodes, 8, place);
            let mut distinct = sorted(chosen.clone());
            distinct.dedup();
            assert_eq!(distinct.len(), 8, "{chosen:?}");
            assert!(
                nodes
                    .iter()
                    .any(|node| chosen.iter().all(|core| node.contains(core))),
                "{chosen:?} spans both nodes"
            );
            firsts.insert(chosen[0]);
        }
        assert_eq!(firsts.len(), 96);
    }

    /// Where only some cores are allowed, as under `taskset`, those of one
    /// node are kept to, if it has enough allowed.
    #[test]
    fn of_the_cores_allowed_those_of_one_node_are_kept_to() {
        let allowed: Vec<usize> = (20..28).collect();
        for place in 0..10 {
            let chosen = sorted(choose(&allowed, &two_nodes(), 4, place));
            assert!(
                chosen == [20, 21, 22, 23] || chosen == [24, 25, 26, 27],
                "{chosen:?}"
            );
        }
    }

    /// With no node of enough cores allowed, or none known, a run of all
    /// those allowed is kept to; a node of memory alone has no cores.
    #[test]
    fn without_a_node_of_enough_a_run_of_those_allowed_is_kept_to() {
        let allowed: Vec<usize> = (0..96).collect();
        assert_eq!(choose(&allowed, &[], 8, 100), (4..12).collect::<Vec<_>>());
        assert_eq!(
            choose(&allowed, &[vec![]], 8, 100),
            (4..12).collect::<Vec<_>>()
        );
        assert_eq!(
            choose(&allowed, &two_nodes(), 60, 100),
            (4..64).collect::<Vec<_>>()
        );
    }
}
