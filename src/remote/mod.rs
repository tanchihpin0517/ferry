//! What reaching a host takes, beside the transport: where a transfer's
//! sides are, quoting for the shell there, its sudo, and the releases that
//! are installed there.

mod location;
pub(crate) mod release;
pub(crate) mod shell;
mod sudo;

pub use location::Location;
pub use sudo::{Sudo, take_password_from_environment};

/// The script that runs `program` with `arguments`.
pub fn script(program: &str, arguments: &[&str]) -> String {
    let mut line = format!("exec {}", shell::quote(program));
    for argument in arguments {
        line.push(' ');
        line.push_str(&shell::quote(argument));
    }
    line
}
