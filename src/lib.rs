//! Ferry, a fast replacement for rsync.
//!
//! The design is in `docs/design/`, and the order it is built in is
//! `TODO.md`.

pub mod build_info;
pub mod cli;
pub mod peer;
pub mod protocol;
pub mod remote;
pub mod session;
