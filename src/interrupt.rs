//! Interruption: on Ctrl-C, or a request to terminate, remove the temporary
//! files of files not finished, and the sockets left open, end the children
//! that would outlive the process, such as the `ssh` holding a connection,
//! then exit. `Ctrl-\`, SIGQUIT, instead asks
//! for a snapshot of where every thread is; see [`quit_asked`]. In
//! [`graceful`] mode, the first Ctrl-C only asks the process to stop, and the
//! second exits. While the client's monitor runs, each Ctrl-C, SIGTERM and
//! SIGHUP goes to it instead, which says what to do; see [`monitor`].
//!
//! The signals are blocked in every thread, and one thread waits for them,
//! because a signal handler may do almost nothing safely, while a thread may
//! remove files. A temporary file is created and registered in one step, and
//! unregistered once it is renamed into place or removed.
//!
//! A gate keeps the two apart. Creating takes its shared side, so files are
//! created in parallel; the cleanup takes its exclusive side, which waits for
//! creations in progress, and never lets go, since the process exits. So no
//! file is ever created unregistered, or after the cleanup.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::transport::child::{Child, Handle, Signal};

const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// The signal that asks the thread that takes signals whether one that ends
/// the process has come: blocked, as the others, and taken by it alone, so
/// that it alone says. One no one else sends, whose default is to end the
/// process, as the others': macOS drops at once a signal whose default is to
/// be ignored, as SIGURG's, blocked or not, so that it never comes.
const ASK: libc::c_int = libc::SIGUSR2;

/// How long the end of `main` waits for that thread to answer: should a
/// system drop the asking, the process ends late, rather than never.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A SIGQUIT came, and no one has taken it yet.
static QUIT: AtomicBool = AtomicBool::new(false);

/// What a signal that would end the process asks of the client's monitor,
/// while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// Ctrl-C, SIGINT.
    Interrupt,
    /// SIGTERM, or SIGHUP.
    Terminate,
}

/// A transfer stopped, as a Ctrl-C, SIGTERM or SIGHUP asked: the process
/// exits as the signal would have it, having said what it waited for.
#[derive(Debug)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("stopped, as asked")
    }
}

impl std::error::Error for Stopped {}

/// Where such a signal goes, while the client's monitor runs.
type Monitored = Box<dyn Fn(Asked) + Send>;

static MONITOR: Mutex<Option<Monitored>> = Mutex::new(None);

/// The first such signal that came while the client's monitor ran, or 0:
/// the process exits as that signal would have it, whatever else it came
/// to.
static STOPPED: AtomicI32 = AtomicI32::new(0);

/// In graceful mode, what the first Ctrl-C says, instead of exiting.
static GRACEFUL: OnceLock<&'static str> = OnceLock::new();
/// The process has been asked to stop, and will exit at the next Ctrl-C.
static STOP: AtomicBool = AtomicBool::new(false);
/// The child that a stop is passed on to while it runs, or 0.
static CHILD: AtomicI32 = AtomicI32::new(0);
/// A signal that ends the process has been taken, and it is exiting.
static EXITING: AtomicBool = AtomicBool::new(false);
/// The thread that takes signals was started.
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// How many times it was asked whether a signal that ends the process has
/// come, and, of those, up to which it answered none had.
static ASKED: AtomicU64 = AtomicU64::new(0);
static ANSWERED: AtomicU64 = AtomicU64::new(0);

/// Temporary files being written, by registration: the directory each is in,
/// shared with its writer rather than duplicated, so that registering one
/// takes no descriptor; and its name.
type Temporaries = HashMap<u64, (Arc<OwnedFd>, Vec<u8>)>;

static TEMPORARY: Mutex<Option<Temporaries>> = Mutex::new(None);
/// Run before the process exits on a signal, as the dashboard puts the
/// terminal back.
static BEFORE_EXIT: Mutex<Option<fn()>> = Mutex::new(None);
static GATE: RwLock<()> = RwLock::new(());
static NEXT: AtomicU64 = AtomicU64::new(0);
/// Children to end if the process exits on a signal, by registration: each
/// signalled through its watcher, which sends nothing to one that ended.
static CHILDREN: Mutex<Vec<(u64, Handle)>> = Mutex::new(Vec::new());
/// Paths to remove if the process exits so, such as a socket listened on.
static PATHS: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

fn signal_set(signals: &[libc::c_int]) -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises the set, which `sigaddset` then fills.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &signal in signals {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Takes over the signals. Must run before any other thread starts, so that
/// every thread inherits the blocked signals.
pub fn install() {
    let mut signals = SIGNALS.to_vec();
    signals.extend([libc::SIGQUIT, ASK]);
    let set = signal_set(&signals);
    // SAFETY: `set` is a valid signal set, and a null old set is allowed.
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    std::thread::spawn(move || {
        crate::inject::stall_if("signals");
        let mut signal = 0;
        loop {
            // SAFETY: as above; `signal` receives the signal's number.
            unsafe {
                libc::sigwait(&set, &mut signal);
            }
            crate::inject::stall_if("signal-taken");
            if signal == ASK {
                // Taken by no one else, one that came before the asking is
                // still waiting here.
                if !pending() {
                    ANSWERED.store(ASKED.load(Ordering::SeqCst), Ordering::SeqCst);
                }
                continue;
            }
            if signal != libc::SIGQUIT
                && let Some(monitor) = MONITOR.lock().expect("not poisoned").as_ref()
            {
                let _ = STOPPED.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
                monitor(if signal == libc::SIGINT {
                    Asked::Interrupt
                } else {
                    Asked::Terminate
                });
                continue;
            }
            if signal == libc::SIGINT
                && let Some(saying) = GRACEFUL.get()
                && !STOP.swap(true, Ordering::Relaxed)
            {
                eprintln!("\n{saying}");
                pass_on_stop();
                continue;
            }
            if signal != libc::SIGQUIT {
                break;
            }
            QUIT.store(true, Ordering::Relaxed);
        }
        EXITING.store(true, Ordering::SeqCst);
        // Held until the process exits: nothing is created from now on.
        let _gate = GATE.write();
        remove_all();
        end_children();
        before_exit();
        std::process::exit(128 + signal);
    });
    INSTALLED.store(true, Ordering::SeqCst);
}

/// Waits, if a signal that ends the process has come, for it to end it:
/// so that a process interrupted exits as interrupted, with 128 and the
/// signal, though what the signal ended first, as its server, a Ctrl-C
/// reaching the whole group, made it fail meanwhile. For the end of `main`.
///
/// The thread that takes signals is asked, and says once no such signal is
/// waiting: one waiting may still only ask a graceful process to stop, and
/// one just taken is not yet seen to be exiting, which once made a process
/// interrupted exit with 1, no signal waiting, and none seen taken. Without
/// an answer in [`ANSWER_WAIT`], only a signal still waiting is waited for.
///
/// One that came while the client's monitor ran, which stopped the
/// transfer rather than ending the process, is returned: the process exits
/// as it would have it.
pub fn settle() -> Option<i32> {
    if !INSTALLED.load(Ordering::SeqCst) {
        return None;
    }
    let asked = ASKED.fetch_add(1, Ordering::SeqCst) + 1;
    // SAFETY: `kill` has no memory effects.
    unsafe {
        libc::kill(libc::getpid(), ASK);
    }
    let since = std::time::Instant::now();
    loop {
        if EXITING.load(Ordering::SeqCst) {
            loop {
                std::thread::park();
            }
        }
        let answered = ANSWERED.load(Ordering::SeqCst) >= asked;
        if answered || (since.elapsed() >= ANSWER_WAIT && !pending()) {
            return Some(STOPPED.load(Ordering::SeqCst)).filter(|signal| *signal != 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Whether a signal that ends the process has come, and is not yet taken.
fn pending() -> bool {
    // SAFETY: `sigpending` fills the set, which `sigismember` then reads.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigpending(&mut set);
        SIGNALS
            .iter()
            .any(|&signal| libc::sigismember(&set, signal) == 1)
    }
}

/// Has each Ctrl-C, SIGTERM and SIGHUP go to `monitored`, the client's
/// monitor, rather than end the process; `None` takes them back.
pub fn monitor(monitored: Option<Monitored>) {
    *MONITOR.lock().expect("not poisoned") = monitored;
}

/// Makes the first Ctrl-C ask the process to stop, saying `saying`, rather
/// than exit: the process checks [`stop_asked`], and stops as it sees fit. A
/// second Ctrl-C exits as usual.
pub fn graceful(saying: &'static str) {
    let _ = GRACEFUL.set(saying);
}

/// Whether the process has been asked to stop.
pub fn stop_asked() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// Asks the process to stop, as a first Ctrl-C does: for a child that was
/// stopped by one the process did not see, such as the dashboard's.
pub fn ask_to_stop() {
    STOP.store(true, Ordering::Relaxed);
}

/// Passes a stop on to the child `child`, as a Ctrl-C, while the returned
/// guard lives, which must be dropped once it is waited for.
/// A stop asked before is not: what runs after one, such as removing what was
/// made, is to run to its end.
pub fn pass_stop_to(child: u32) -> PassingStop {
    CHILD.store(child as i32, Ordering::Relaxed);
    PassingStop
}

/// While it lives, a stop is passed on to a child; see [`pass_stop_to`].
pub struct PassingStop;

impl Drop for PassingStop {
    fn drop(&mut self) {
        CHILD.store(0, Ordering::Relaxed);
    }
}

fn pass_on_stop() {
    let child = CHILD.load(Ordering::Relaxed);
    if child > 0 {
        // SAFETY: `kill` has no memory effects. A child that has exited keeps
        // its process ID until it is waited on, and the guard is dropped as
        // soon as the wait is over.
        unsafe {
            libc::kill(child, libc::SIGINT);
        }
    }
}

/// Asks for where every thread is, as a SIGQUIT does: for the dashboard,
/// whose terminal sends no signal for `Ctrl-\`.
pub fn ask_for_snapshot() {
    QUIT.store(true, Ordering::Relaxed);
}

/// Whether a SIGQUIT came since this was last asked.
pub fn quit_asked() -> bool {
    QUIT.swap(false, Ordering::Relaxed)
}

/// Removes the temporary files of files not finished, and exits with `code`,
/// as an interruption does: for a server whose client is gone.
pub fn stop(code: i32) -> ! {
    // Held until the process exits: nothing is created from now on.
    let _gate = GATE.write();
    remove_all();
    end_children();
    before_exit();
    std::process::exit(code);
}

/// As [`stop`], for a panic, which may come while this thread holds the
/// gate, the registry, or what runs before exiting: each is tried for a
/// moment, and gone without, rather than waited for.
pub fn exit_now(code: i32) -> ! {
    // Held until the process exits, if it can be had at all.
    let _gate = (0..100).find_map(|_| {
        GATE.try_write().ok().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(1));
            None
        })
    });
    let files = (0..100).find_map(|_| {
        TEMPORARY.try_lock().ok().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(1));
            None
        })
    });
    if let Some(mut files) = files
        && let Some(files) = files.take()
    {
        for (directory, name) in files.into_values() {
            let _ = rustix::fs::unlinkat(
                &directory,
                OsStr::from_bytes(&name),
                rustix::fs::AtFlags::empty(),
            );
        }
    }
    if let Ok(paths) = PATHS.try_lock() {
        for path in paths.iter() {
            let _ = std::fs::remove_file(path);
        }
    }
    if let Ok(children) = CHILDREN.try_lock() {
        end(&children);
    }
    if let Ok(run) = BEFORE_EXIT.try_lock()
        && let Some(run) = *run
    {
        run();
    }
    std::process::exit(code);
}

/// Ends `child` with SIGTERM, should the process exit on a signal, or be
/// stopped, before the returned guard is dropped. `ssh` ends so as it does
/// on its own, letting go of its master.
pub fn end_on_exit(child: &Child) -> EndOnExit {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    CHILDREN
        .lock()
        .expect("not poisoned")
        .push((id, child.handle()));
    EndOnExit(id)
}

/// While it lives, its child is ended if the process exits on a signal; see
/// [`end_on_exit`].
pub struct EndOnExit(u64);

impl Drop for EndOnExit {
    fn drop(&mut self) {
        let mut children = CHILDREN.lock().expect("not poisoned");
        if let Some(at) = children.iter().position(|(id, _)| *id == self.0) {
            children.swap_remove(at);
        }
    }
}

/// Whether the child `pid` is ended should the process exit on a signal.
#[cfg(test)]
pub fn ends_on_exit(pid: u32) -> bool {
    CHILDREN
        .lock()
        .expect("not poisoned")
        .iter()
        .any(|(_, handle)| handle.id() == pid)
}

fn end_children() {
    if let Ok(children) = CHILDREN.lock() {
        end(&children);
    }
}

fn end(children: &[(u64, Handle)]) {
    for (_, handle) in children {
        handle.signal(Signal::Term);
    }
}

/// Removes `path`, should the process exit on a signal, or be stopped,
/// before the returned guard is dropped, which removes it too.
pub fn remove_on_exit(path: &Path) -> RemoveOnExit {
    PATHS.lock().expect("not poisoned").push(path.to_owned());
    RemoveOnExit(path.to_owned())
}

/// Removes its path when dropped, and on a signal before; see
/// [`remove_on_exit`].
pub struct RemoveOnExit(std::path::PathBuf);

impl Drop for RemoveOnExit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut paths = PATHS.lock().expect("not poisoned");
        if let Some(at) = paths.iter().position(|path| *path == self.0) {
            paths.swap_remove(at);
        }
    }
}

/// Has `run` run before the process exits on a signal, or `None` nothing.
pub fn on_exit(run: Option<fn()>) {
    *BEFORE_EXIT.lock().expect("not poisoned") = run;
}

fn before_exit() {
    if let Some(run) = *BEFORE_EXIT.lock().expect("not poisoned") {
        run();
    }
}

/// Gives a child the signals back, so that Ctrl-C reaches `ssh` as usual. A
/// SIGQUIT stays blocked, so that `Ctrl-\` asks this process for a snapshot
/// without ending `ssh`.
pub fn unblock_in_child(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: only async-signal-safe calls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            let set = signal_set(&SIGNALS);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            Ok(())
        })
    }
}

/// A registered temporary file, which it owns: dropped, it removes the
/// file, unless told it took its final name. So a temporary file is removed
/// however its writing ends, by an error as much as by a signal.
pub struct Registered {
    id: u64,
    /// Whether the file is still to be removed: until it takes its name.
    armed: bool,
}

impl Registered {
    /// The file took its final name: it is no longer to be removed.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let entry = TEMPORARY
            .lock()
            .ok()
            .and_then(|mut files| files.as_mut()?.remove(&self.id));
        // Gone from the registry, it was removed on a signal already.
        if self.armed
            && let Some((directory, name)) = entry
        {
            let _ = rustix::fs::unlinkat(
                &directory,
                OsStr::from_bytes(&name),
                rustix::fs::AtFlags::empty(),
            );
        }
    }
}

/// Creates a temporary file in `directory` with `create`, which returns what
/// it made and the name it made it under, and registers it, to be removed if
/// the process is interrupted before it is finished. Registering takes no
/// descriptor, so it cannot fail and leave a file made but not registered.
pub fn create<T>(
    directory: &Arc<OwnedFd>,
    create: impl FnOnce() -> io::Result<(T, Vec<u8>)>,
) -> io::Result<(T, Vec<u8>, Registered)> {
    let _gate = GATE.read().expect("not poisoned");
    let (made, name) = create()?;
    Ok((made, name.clone(), register(directory, name)))
}

fn register(directory: &Arc<OwnedFd>, name: Vec<u8>) -> Registered {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    TEMPORARY
        .lock()
        .expect("not poisoned")
        .get_or_insert_with(HashMap::new)
        .insert(id, (Arc::clone(directory), name));
    Registered { id, armed: true }
}

fn remove_all() {
    if let Ok(paths) = PATHS.lock() {
        for path in paths.iter() {
            let _ = std::fs::remove_file(path);
        }
    }
    let Some(files) = TEMPORARY.lock().ok().and_then(|mut files| files.take()) else {
        return;
    };
    for (directory, name) in files.into_values() {
        let _ = rustix::fs::unlinkat(
            &directory,
            OsStr::from_bytes(&name),
            rustix::fs::AtFlags::empty(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes `name` in `directory`, as a temporary file is made, registered.
    fn made(directory: &std::path::Path, name: &str) -> Registered {
        let opened = Arc::new(crate::fsys::open_root(directory.as_os_str()).unwrap());
        let ((), _, registered) = create(&opened, || {
            std::fs::write(directory.join(name), "x")?;
            Ok(((), name.as_bytes().to_vec()))
        })
        .unwrap();
        registered
    }

    #[test]
    fn a_registered_file_not_renamed_is_removed_when_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let registered = made(temp.path(), ".f.ferry-dropped");
        assert!(temp.path().join(".f.ferry-dropped").exists());
        drop(registered);
        assert!(!temp.path().join(".f.ferry-dropped").exists());
    }

    #[test]
    fn a_disarmed_file_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let mut registered = made(temp.path(), ".f.ferry-kept");
        // Renamed into place, as its writer would, then let go.
        std::fs::rename(temp.path().join(".f.ferry-kept"), temp.path().join("f")).unwrap();
        registered.disarm();
        drop(registered);
        assert!(temp.path().join("f").exists());
    }
}
