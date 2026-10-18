//! Ferry, a fast replacement for rsync.
//!
//! The design is in `docs/design/`, and the order it is built in is
//! `TODO.md`.

pub mod build_info;
pub mod cli;
pub mod compare;
pub mod control;
pub mod dashboard;
pub mod destination;
pub mod frame;
pub mod fsys;
pub mod home;
pub mod interrupt;
pub mod limits;
pub mod manifest;
pub mod mux;
pub mod names;
pub mod owners;
pub mod peer;
pub mod pool;
pub mod prepare;
pub mod profile;
pub mod progress;
pub mod protocol;
pub mod record;
pub mod relay;
pub mod remote;
pub mod report;
pub mod scan;
pub mod session;
pub mod source;
pub mod stage;
pub mod status;
pub mod stop;
#[cfg(test)]
mod test_support;
pub mod testkit;
pub mod transport;
pub mod update;
