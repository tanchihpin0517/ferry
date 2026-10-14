//! Where Ferry keeps its files, on the client and on hosts.
//!
//! A release build and a dev build never share any of them: a release uses
//! `~/.ferry/`, a dev build `~/.ferry-dev/`.

use crate::build_info::{PROFILE, Profile};

/// The name of Ferry's directory in a home directory.
pub fn dir_name() -> &'static str {
    match PROFILE {
        Profile::Release => ".ferry",
        Profile::Dev => ".ferry-dev",
    }
}
