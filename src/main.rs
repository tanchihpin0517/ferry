use std::io::Write;
use std::process::ExitCode;

/// Every stage allocates and frees buffers from many threads at once. musl's
/// own allocator, in Linux releases, makes those threads wait on one another;
/// mimalloc keeps free memory per thread. It is used on every platform, so
/// that each behaves the same.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let result = ferry::cli::run();
    // Stopped by a signal, as the client's monitor was asked to, the process
    // exits as that signal would have it, having said why already.
    if let Some(signal) = ferry::interrupt::settle() {
        return ExitCode::from(128 + signal as u8);
    }
    match result {
        Ok(code) => code,
        // Stopped, as asked, though by no signal seen: said so.
        Err(error) if error.is::<ferry::interrupt::Stopped>() => {
            let _ = writeln!(std::io::stderr(), "ferry: {error}");
            ExitCode::from(130)
        }
        Err(error) => {
            // Standard error may be gone, as a server's lifeline is once its
            // monitor is: saying so must not panic.
            let _ = writeln!(std::io::stderr(), "ferry: {error:#}");
            // A server's internal error, which it said, is the client's.
            if error.downcast_ref::<ferry::stop::Internal>().is_some() {
                ExitCode::from(ferry::stop::INTERNAL)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
