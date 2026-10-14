//! What this binary is: a release of a version, or a dev build of some source.
//!
//! A client and a server must be the same build, so the identity here is what
//! the handshake compares and what names a build's directory on a host.

mod source;

use std::path::Path;

pub use source::{source_files, source_hash};

/// How the binary was built. The build profile decides it at compile time:
/// `cargo build --release` is a release, `cargo build` a dev build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Release,
    Dev,
}

pub const PROFILE: Profile = if cfg!(debug_assertions) {
    Profile::Dev
} else {
    Profile::Release
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The hash of the source this binary was compiled from; see [`source_hash`].
pub const SOURCE_HASH: &str = env!("FERRY_SOURCE_HASH");

/// The directory this binary was compiled in, which a dev build uploads.
pub fn source_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// This build's identity: the version for a release, `dev-<source hash>` for a
/// dev build.
pub fn identity() -> String {
    match PROFILE {
        Profile::Release => VERSION.to_owned(),
        Profile::Dev => format!("dev-{SOURCE_HASH}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_hash_matches_the_source() {
        let files = source_files(source_dir()).unwrap();
        assert!(files.iter().any(|file| file == Path::new("src/main.rs")));
        assert_eq!(source_hash(source_dir(), &files).unwrap(), SOURCE_HASH);
    }
}
