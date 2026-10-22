use std::process::ExitCode;

/// Every stage allocates and frees buffers from many threads at once. musl's
/// own allocator, in Linux releases, makes those threads wait on one another;
/// mimalloc keeps free memory per thread. It is used on every platform, so
/// that each behaves the same.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let result = ferry::cli::run();
    ferry::interrupt::settle();
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ferry: {error:#}");
            // A server's internal error, which it said, is the client's.
            if error.downcast_ref::<ferry::stop::Internal>().is_some() {
                ExitCode::from(ferry::stop::INTERNAL)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
