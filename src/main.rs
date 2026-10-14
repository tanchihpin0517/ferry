use std::process::ExitCode;

/// Every stage allocates and frees buffers from many threads at once. musl's
/// own allocator, in Linux releases, makes those threads wait on one another;
/// mimalloc keeps free memory per thread. It is used on every platform, so
/// that each behaves the same.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    match ferry::cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ferry: {error:#}");
            ExitCode::FAILURE
        }
    }
}
