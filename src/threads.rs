//! Every thread Ferry starts, named by its role, as the architecture says
//! each is: a panic in one says its role, profiled or not, and so does a
//! debugger or `top -H`. A test may have any of them panic as it starts, by
//! its role, as `FERRY_TEST_PANIC` says. A lint holds the rest of Ferry to
//! these; a stage of a peer starts its threads through
//! [`crate::stage::Stages`], which starts them here too.

use std::io;
use std::thread::{Builder, JoinHandle, Scope, ScopedJoinHandle};

/// Starts `work` on a thread named `name`; as `std::thread::spawn`, it
/// panics where the system starts no thread.
pub fn spawn<T: Send + 'static>(
    name: impl Into<String>,
    work: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    try_spawn(name, work).expect("the system starts a thread")
}

/// Starts `work` on a thread named `name`, or says why the system started
/// none.
#[allow(clippy::disallowed_methods, reason = "the one place threads start")]
pub fn try_spawn<T: Send + 'static>(
    name: impl Into<String>,
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<JoinHandle<T>> {
    let name = name.into();
    let role = name.clone();
    Builder::new().name(name).spawn(move || {
        crate::inject::panic_if(&role);
        work()
    })
}

/// Starts `work` on a thread of `scope` named `name`; as `Scope::spawn`, it
/// panics where the system starts no thread.
#[allow(clippy::disallowed_methods, reason = "the one place threads start")]
pub fn spawn_scoped<'scope, 'env, T: Send + 'scope>(
    scope: &'scope Scope<'scope, 'env>,
    name: impl Into<String>,
    work: impl FnOnce() -> T + Send + 'scope,
) -> ScopedJoinHandle<'scope, T> {
    let name = name.into();
    let role = name.clone();
    Builder::new()
        .name(name)
        .spawn_scoped(scope, move || {
            crate::inject::panic_if(&role);
            work()
        })
        .expect("the system starts a thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread is named by its role, scoped or not.
    #[test]
    fn a_thread_is_named_by_its_role() {
        let name = || std::thread::current().name().map(str::to_owned);
        assert_eq!(spawn("make", name).join().unwrap().as_deref(), Some("make"));
        std::thread::scope(|scope| {
            let named = spawn_scoped(scope, "writer", name).join().unwrap();
            assert_eq!(named.as_deref(), Some("writer"));
        });
    }
}
