//! Interruption: on Ctrl-C, or a request to terminate, remove the temporary
//! files of files not finished, then exit.
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
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Temporary files being written, by registration: the directory each is in,
/// and its name.
type Temporaries = HashMap<u64, (OwnedFd, Vec<u8>)>;

static TEMPORARY: Mutex<Option<Temporaries>> = Mutex::new(None);
/// Run before the process exits on a signal, as the dashboard puts the
/// terminal back.
static BEFORE_EXIT: Mutex<Option<fn()>> = Mutex::new(None);
static GATE: RwLock<()> = RwLock::new(());
static NEXT: AtomicU64 = AtomicU64::new(0);

fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises the set, which `sigaddset` then fills.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Takes over the signals. Must run before any other thread starts, so that
/// every thread inherits the blocked signals.
pub fn install() {
    let set = signal_set();
    // SAFETY: `set` is a valid signal set, and a null old set is allowed.
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    std::thread::spawn(move || {
        let mut signal = 0;
        // SAFETY: as above; `signal` receives the signal's number.
        unsafe {
            libc::sigwait(&set, &mut signal);
        }
        // Held until the process exits: nothing is created from now on.
        let _gate = GATE.write();
        remove_all();
        before_exit();
        std::process::exit(128 + signal);
    });
}

/// Removes the temporary files of files not finished, and exits with `code`,
/// as an interruption does: for a server whose client is gone.
pub fn stop(code: i32) -> ! {
    // Held until the process exits: nothing is created from now on.
    let _gate = GATE.write();
    remove_all();
    before_exit();
    std::process::exit(code);
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

/// Gives a child the signals back, so that Ctrl-C reaches `ssh` as usual.
pub fn unblock_in_child(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: only async-signal-safe calls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            let set = signal_set();
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            Ok(())
        })
    }
}

/// A registered temporary file. Dropping it unregisters the file, once it is
/// renamed into place or removed.
pub struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        if let Some(files) = TEMPORARY.lock().expect("not poisoned").as_mut() {
            files.remove(&self.0);
        }
    }
}

/// Creates a temporary file in `directory` with `create`, which returns what
/// it made and the name it made it under, and registers it, to be removed if
/// the process is interrupted before it is finished.
pub fn create<T>(
    directory: impl AsFd,
    create: impl FnOnce() -> io::Result<(T, Vec<u8>)>,
) -> io::Result<(T, Vec<u8>, Registered)> {
    let _gate = GATE.read().expect("not poisoned");
    let (made, name) = create()?;
    let registered = register(directory, &name)?;
    Ok((made, name, registered))
}

fn register(directory: impl AsFd, name: &[u8]) -> io::Result<Registered> {
    let directory = directory.as_fd().try_clone_to_owned()?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    TEMPORARY
        .lock()
        .expect("not poisoned")
        .get_or_insert_with(HashMap::new)
        .insert(id, (directory, name.to_vec()));
    Ok(Registered(id))
}

fn remove_all() {
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
