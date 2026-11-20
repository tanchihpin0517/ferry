//! What an error means, by what it is, wherever it is met: the rules of the
//! pipeline's table, in `docs/design/pipeline.md`, which the Source and the
//! Destination both follow, kept in one place. Where it was met tells only
//! what an error can be there; what is said of it is said where it was met.

use std::io;

use rustix::io::Errno;

use crate::fsys;

/// What an error means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Means {
    /// What was to be deleted is to be protected: the transfer stops.
    Protected,
    /// The drive is full, or out of the user's quota, or read-only, or this
    /// side's temporary files fail: every entry after would fail the same
    /// way, so the transfer stops.
    Stops,
    /// Gone between its listing and its reading: it vanished, as rsync says,
    /// which is no failure.
    Vanished,
    /// What this process may not set, or the filesystem does not keep, is
    /// warned of, and the entry kept.
    Unsettable,
    /// What is not deleted, on another filesystem with `-x`, or left out by
    /// the rules, is warned of, as rsync keeps it.
    Kept,
    /// Its entry fails alone.
    Fails,
}

impl Means {
    /// Whether the transfer stops for it.
    pub fn stops(self) -> bool {
        matches!(self, Means::Protected | Means::Stops)
    }
}

/// Where an error was met, which some errors mean only there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Met {
    /// The Source reading or listing what it listed.
    Reading,
    /// Setting an owner, permissions, or a time; `invalid`, where an invalid
    /// value is one this filesystem cannot hold, as a time before 1980 on
    /// FAT, or an owner with no ID here.
    Setting { invalid: bool },
    /// Making a socket or a device, which needs privilege.
    MakingSpecial,
    /// Deleting what the Destination has besides.
    Deleting,
    /// Anything else.
    Elsewhere,
}

/// What `error` means, met so: any cause of it, as it is, decides.
pub fn means(error: &anyhow::Error, met: Met) -> Means {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .map(|cause| io_means(cause, met))
        .find(|means| *means != Means::Fails)
        .unwrap_or(Means::Fails)
}

/// What an I/O error means, met so.
pub fn io_means(error: &io::Error, met: Met) -> Means {
    if fsys::is_protected(error) {
        return Means::Protected;
    }
    if crate::sorted::is_temp_failure(error) {
        return Means::Stops;
    }
    let errno = Errno::from_io_error(error);
    if errno.is_some_and(|errno| [Errno::NOSPC, Errno::DQUOT, Errno::ROFS].contains(&errno)) {
        return Means::Stops;
    }
    match met {
        Met::Reading if error.kind() == io::ErrorKind::NotFound => Means::Vanished,
        Met::Setting { invalid } if unsettable(errno, invalid) => Means::Unsettable,
        Met::MakingSpecial if errno == Some(Errno::PERM) => Means::Unsettable,
        Met::Deleting if fsys::is_elsewhere(error) || fsys::is_excluded(error) => Means::Kept,
        _ => Means::Fails,
    }
}

/// Whether setting an attribute failed because this process may not set it,
/// or the filesystem does not keep it: as root's `chown` where root is
/// squashed, or `chmod` on FAT.
fn unsettable(errno: Option<Errno>, invalid: bool) -> bool {
    match errno {
        Some(Errno::PERM) => true,
        // The same on Linux, not on macOS.
        Some(errno) if errno == Errno::OPNOTSUPP || errno == Errno::NOTSUP => true,
        Some(Errno::INVAL) => invalid,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errno(errno: Errno) -> io::Error {
        io::Error::from(errno)
    }

    /// Every row of the pipeline's table, where it may be met.
    #[test]
    fn an_error_means_what_the_table_says() {
        // A drive full, out of quota, or read-only stops the transfer,
        // wherever it is met.
        for met in [
            Met::Reading,
            Met::Setting { invalid: true },
            Met::MakingSpecial,
            Met::Deleting,
            Met::Elsewhere,
        ] {
            for full in [Errno::NOSPC, Errno::DQUOT, Errno::ROFS] {
                assert_eq!(io_means(&errno(full), met), Means::Stops, "{met:?}");
            }
            assert_eq!(
                io_means(&io::Error::other(fsys::Protected), met),
                Means::Protected
            );
            assert_eq!(io_means(&errno(Errno::IO), met), Means::Fails);
        }
        // Gone as it is read vanished; gone as anything else is done fails.
        assert_eq!(
            io_means(&errno(Errno::NOENT), Met::Reading),
            Means::Vanished
        );
        assert_eq!(io_means(&errno(Errno::NOENT), Met::Elsewhere), Means::Fails);
        // What may not be set is warned of; an invalid value only where
        // said.
        let setting = Met::Setting { invalid: false };
        for unsupported in [Errno::PERM, Errno::OPNOTSUPP, Errno::NOTSUP] {
            assert_eq!(io_means(&errno(unsupported), setting), Means::Unsettable);
        }
        assert_eq!(io_means(&errno(Errno::ACCESS), setting), Means::Fails);
        assert_eq!(io_means(&errno(Errno::INVAL), setting), Means::Fails);
        assert_eq!(
            io_means(&errno(Errno::INVAL), Met::Setting { invalid: true }),
            Means::Unsettable
        );
        assert_eq!(
            io_means(&errno(Errno::PERM), Met::MakingSpecial),
            Means::Unsettable
        );
        assert_eq!(io_means(&errno(Errno::PERM), Met::Elsewhere), Means::Fails);
        // What deleting stops short of is kept, and warned of.
        let elsewhere = io::Error::other(fsys::Elsewhere { path: Vec::new() });
        let excluded = io::Error::other(fsys::Excluded {
            path: b"f.o".to_vec(),
        });
        assert_eq!(io_means(&elsewhere, Met::Deleting), Means::Kept);
        assert_eq!(io_means(&excluded, Met::Deleting), Means::Kept);
        // Any cause in the chain decides.
        let wrapped = anyhow::Error::from(errno(Errno::NOSPC)).context("write f");
        assert_eq!(means(&wrapped, Met::Elsewhere), Means::Stops);
        assert!(Means::Protected.stops() && Means::Stops.stops());
        assert!(!Means::Fails.stops());
    }
}
