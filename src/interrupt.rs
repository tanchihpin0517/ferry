//! Interruption: on Ctrl-C, or a request to terminate, remove the temporary
//! files of files not finished, and the sockets left open, end the children
//! that would outlive the process, such as the `ssh` holding a connection,
//! then exit. `Ctrl-\`, SIGQUIT, instead asks
//! for a snapshot of where every thread is; see [`quit_asked`]. In
//! [`graceful`] mode, the first Ctrl-C only asks the process to stop, and the
//! second exits.
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
use std::process::Child;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// A SIGQUIT came, and no one has taken it yet.
static QUIT: AtomicBool = AtomicBool::new(false);

/// In graceful mode, what the first Ctrl-C says, instead of exiting.
static GRACEFUL: OnceLock<&'static str> = OnceLock::new();
/// The process has been asked to stop, and will exit at the next Ctrl-C.
static STOP: AtomicBool = AtomicBool::new(false);
/// The child that a stop is passed on to while it runs, or 0.
static CHILD: AtomicI32 = AtomicI32::new(0);

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
/// Children to end if the process exits on a signal, by process ID: each
/// is not yet waited for, so its ID is still its own.
static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
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
    signals.push(libc::SIGQUIT);
    let set = signal_set(&signals);
    // SAFETY: `set` is a valid signal set, and a null old set is allowed.
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    std::thread::spawn(move || {
        let mut signal = 0;
        loop {
            // SAFETY: as above; `signal` receives the signal's number.
            unsafe {
                libc::sigwait(&set, &mut signal);
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
        // Held until the process exits: nothing is created from now on.
        let _gate = GATE.write();
        remove_all();
        end_children();
        before_exit();
        std::process::exit(128 + signal);
    });
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

/// Passes a stop on to `child`, as a Ctrl-C, while the returned guard lives.
/// A stop asked before is not: what runs after one, such as removing what was
/// made, is to run to its end.
pub fn pass_stop_to(child: &Child) -> PassingStop {
    CHILD.store(child.id() as i32, Ordering::Relaxed);
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
/// stopped, before the returned guard is dropped, which must be after the
/// child is waited for: until then, its ID is its own. `ssh` ends so as it
/// does on its own, letting go of its master.
pub fn end_on_exit(child: &Child) -> EndOnExit {
    let id = child.id();
    CHILDREN.lock().expect("not poisoned").push(id);
    EndOnExit(id)
}

/// While it lives, its child is ended if the process exits on a signal; see
/// [`end_on_exit`].
pub struct EndOnExit(u32);

impl Drop for EndOnExit {
    fn drop(&mut self) {
        let mut children = CHILDREN.lock().expect("not poisoned");
        if let Some(at) = children.iter().position(|&id| id == self.0) {
            children.swap_remove(at);
        }
    }
}

fn end_children() {
    if let Ok(children) = CHILDREN.lock() {
        end(&children);
    }
}

fn end(children: &[u32]) {
    for &id in children {
        // SAFETY: `kill` has no memory effects, and each ID is a child's not
        // yet waited for, so no other process's.
        unsafe {
            libc::kill(id as libc::pid_t, libc::SIGTERM);
        }
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
