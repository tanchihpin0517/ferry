//! Ferry, a fast replacement for rsync.
//!
//! The design is in `docs/design/`, and the order it is built in is
//! `TODO.md`.

pub mod adaptive;
pub mod build_info;
pub mod cli;
pub mod compare;
pub mod config;
pub mod destination;
pub mod frame;
pub mod fsys;
pub mod home;
pub mod interrupt;
pub mod manifest;
pub mod mux;
pub mod names;
pub mod owners;
pub mod peer;
pub mod pool;
pub mod progress;
pub mod protocol;
pub mod record;
pub mod remote;
pub mod session;
pub mod source;
pub mod testkit;
pub mod walk;
