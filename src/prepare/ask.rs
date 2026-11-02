//! Where the transfer goes, as the roots' kinds allow, and what the client
//! asks you before any server starts: the question before a first install.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use rustix::event::PollFlags;

use super::probe::RootKind;
use super::shown;
use crate::fsys;
use crate::profile::State;
use crate::remote::Location;
use crate::status;
use crate::transport::scope::Scope;

/// Where the transfer goes, in rsync's notation, as the roots' kinds allow,
/// and whether its root is an entry placed in DESTINATION.
///
/// A SOURCE that names an entry, without a trailing slash, is copied as
/// itself, a symlink as one: into DESTINATION, as `DESTINATION/NAME`, where
/// DESTINATION is a directory, ends with `/`, or is to hold a directory; a
/// file, or a symlink, onto any other DESTINATION takes its name. A SOURCE
/// ending with `/`, `.` or `..` is a directory's contents, which
/// DESTINATION itself is made to hold. `sync` only from a directory, into a
/// directory or where there is none yet; `copy` never a directory onto
/// something else. A missing source is left for its server to report.
pub fn place_destination(
    delete: bool,
    source: &Location,
    source_kind: RootKind,
    destination: Location,
    destination_kind: RootKind,
) -> Result<(Location, bool)> {
    let (from, to) = (shown(source), shown(&destination));
    let names_entry = fsys::names_entry(path_of(source));
    // A symlink to a directory is the directory, but as a SOURCE that names
    // an entry, which is copied as the symlink it is.
    let source_is_directory = match source_kind {
        // A file named as a directory, as `f/`, is no more there than one
        // missing: its server says why it cannot be read.
        RootKind::Missing => return Ok((destination, false)),
        RootKind::Other if !names_entry => return Ok((destination, false)),
        RootKind::Directory => true,
        RootKind::LinkToDirectory => !names_entry,
        RootKind::Other => false,
    };
    let onto_other = destination_kind == RootKind::Other;
    if delete && !source_is_directory {
        if source_kind == RootKind::LinkToDirectory {
            bail!(
                "sync makes a directory the same as another, and {from} is a symlink, \
                 which a SOURCE without a trailing slash is copied as; \
                 to sync the directory it points to, write {from}/"
            );
        }
        bail!(
            "sync makes a directory the same as another, and {from} is not a directory; \
             to copy a file, use `ferry copy`"
        );
    }
    if source_is_directory {
        if onto_other && delete {
            bail!("sync makes a directory the same as another, and {to} is not a directory");
        }
        if onto_other {
            bail!("{to} is not a directory, and {from} is one");
        }
        return Ok(if names_entry {
            (into(destination, &file_name(source)?), true)
        } else {
            (destination, false)
        });
    }
    // A file, or a symlink, copied as itself.
    let into_directory = matches!(
        destination_kind,
        RootKind::Directory | RootKind::LinkToDirectory
    );
    if into_directory {
        return Ok((into(destination, &file_name(source)?), true));
    }
    if path_of(&destination).ends_with(b"/") {
        if onto_other {
            bail!("{to} is not a directory, to copy {from} into");
        }
        return Ok((into(destination, &file_name(source)?), true));
    }
    Ok((destination, false))
}

/// A location's path, as its bytes are.
fn path_of(location: &Location) -> &[u8] {
    match location {
        Location::Local(path) => path.as_os_str().as_encoded_bytes(),
        Location::Remote { path, .. } => path,
    }
}

/// The last component of a location's path: the name a file keeps when
/// copied into a directory.
fn file_name(location: &Location) -> Result<Vec<u8>> {
    let path = path_of(location);
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let name = trimmed
        .rsplit(|&byte| byte == b'/')
        .next()
        .unwrap_or_default();
    if name.is_empty() || name == b"." || name == b".." {
        anyhow::bail!("{} names no file to copy into a directory", shown(location));
    }
    Ok(name.to_vec())
}

/// `name` inside the directory at `location`.
fn into(location: Location, name: &[u8]) -> Location {
    match location {
        Location::Local(path) => {
            use std::os::unix::ffi::OsStrExt;
            Location::Local(path.join(std::ffi::OsStr::from_bytes(name)))
        }
        Location::Remote { host, mut path } => {
            if !path.is_empty() && !path.ends_with(b"/") {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            Location::Remote { host, path }
        }
    }
}

/// Asks on the terminal; without one, refuses and says how to agree. A no
/// fails with `refused`. It waits for the answer as long as it takes, or
/// until `stopping` ends, as the user asks to stop.
pub fn confirm(question: &str, refused: &str, stopping: &Scope) -> Result<()> {
    confirm_with(question, refused, "pass --yes to agree", stopping)
}

/// As [`confirm`], saying `how_to_agree` without a terminal, for a question
/// `--yes` does not answer.
pub fn confirm_with(
    question: &str,
    refused: &str,
    how_to_agree: &str,
    stopping: &Scope,
) -> Result<()> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        bail!("{question} Not without a terminal to ask on; {how_to_agree}");
    }
    let _aside = status::aside();
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    stopping
        .wait(io::stdin(), PollFlags::IN, None, State::Work)
        .context("wait for an answer")?;
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("{refused}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_is_the_last_component() {
        let remote = |path: &[u8]| Location::Remote {
            host: "h".into(),
            path: path.to_vec(),
        };
        assert_eq!(file_name(&remote(b"a/b.txt")).unwrap(), b"b.txt");
        assert_eq!(file_name(&remote(b"b.txt")).unwrap(), b"b.txt");
        assert_eq!(file_name(&Location::Local("/x/y".into())).unwrap(), b"y");
        for none in [&b""[..], b"/", b"a/..", b"."] {
            assert!(file_name(&remote(none)).is_err(), "{none:?}");
        }
        assert_eq!(shown(&into(remote(b""), b"f")), "h:f");
        assert_eq!(shown(&into(remote(b"/"), b"f")), "h:/f");
        assert_eq!(shown(&into(remote(b"d"), b"f")), "h:d/f");
        assert_eq!(shown(&into(remote(b"d/"), b"f")), "h:d/f");
    }

    /// Where each SOURCE goes, as rsync's notation says: one that names an
    /// entry, as itself, into DESTINATION where that is a directory, ends
    /// with `/`, or is to hold a directory; one ending with `/`, as its
    /// contents, into DESTINATION itself.
    #[test]
    fn a_source_goes_where_rsyncs_notation_says() {
        use RootKind::{Directory, LinkToDirectory, Missing, Other};
        let place = |delete, from: &str, from_kind, to: &str, to_kind| {
            place_destination(
                delete,
                &Location::Local(from.into()),
                from_kind,
                Location::Local(to.into()),
                to_kind,
            )
            .map(|(location, placed)| (shown(&location), placed))
            .map_err(|error| error.to_string())
        };
        let placed = |to: &str| Ok((to.to_owned(), true));
        let itself = |to: &str| Ok((to.to_owned(), false));
        for delete in [false, true] {
            // A directory: as itself, or its contents.
            assert_eq!(place(delete, "d", Directory, "b", Directory), placed("b/d"));
            assert_eq!(place(delete, "a/d", Directory, "b", Missing), placed("b/d"));
            assert_eq!(place(delete, "d", Directory, "b/", Missing), placed("b/d"));
            assert_eq!(
                place(delete, "d", Directory, "l", LinkToDirectory),
                placed("l/d")
            );
            assert_eq!(place(delete, "d/", Directory, "b", Directory), itself("b"));
            assert_eq!(place(delete, "d/.", Directory, "b", Missing), itself("b"));
            assert_eq!(place(delete, ".", Directory, "b", Directory), itself("b"));
            assert_eq!(place(delete, "l/", Directory, "b", Directory), itself("b"));
            // A missing source, for its server to report, as a file named
            // as a directory is.
            assert_eq!(place(delete, "m", Missing, "b", Directory), itself("b"));
            assert_eq!(place(delete, "f/", Other, "b", Directory), itself("b"));
            // Never a directory onto something else.
            for from in ["d", "d/"] {
                let refused = place(delete, from, Directory, "f", Other).unwrap_err();
                assert!(refused.contains("f is not a directory"), "{refused}");
            }
        }
        // `copy`: a file, or a symlink, as itself.
        assert_eq!(place(false, "a/f", Other, "b", Directory), placed("b/f"));
        assert_eq!(
            place(false, "f", Other, "l", LinkToDirectory),
            placed("l/f")
        );
        assert_eq!(place(false, "f", Other, "new", Missing), itself("new"));
        assert_eq!(place(false, "f", Other, "g", Other), itself("g"));
        assert_eq!(place(false, "f", Other, "new/", Missing), placed("new/f"));
        assert_eq!(
            place(false, "l", LinkToDirectory, "b", Directory),
            placed("b/l")
        );
        assert_eq!(
            place(false, "l", LinkToDirectory, "new", Missing),
            itself("new")
        );
        let refused = place(false, "f", Other, "g/", Other).unwrap_err();
        assert!(refused.contains("g/ is not a directory"), "{refused}");
        // `sync` only from a directory.
        let refused = place(true, "f", Other, "b", Directory).unwrap_err();
        assert!(refused.contains("use `ferry copy`"), "{refused}");
        let refused = place(true, "l", LinkToDirectory, "b", Directory).unwrap_err();
        assert!(refused.contains("write l/"), "{refused}");
    }
}
