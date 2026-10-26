//! Where the transfer goes, as the roots' kinds allow, and what the client
//! asks you before any server starts: the question before a first install.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use rustix::event::PollFlags;

use super::probe::RootKind;
use super::shown;
use crate::profile::State;
use crate::remote::Location;
use crate::status;
use crate::transport::scope::Scope;

/// Where the transfer goes, as the roots' kinds allow: `sync` only from a
/// directory into a directory, or where there is none yet; `copy` never a
/// directory onto something else, and a file onto a directory, into it, as
/// rsync does. A missing source is left for its server to report.
pub fn place_destination(
    delete: bool,
    source: &Location,
    source_kind: RootKind,
    destination: Location,
    destination_kind: RootKind,
) -> Result<Location> {
    let (from, to) = (shown(source), shown(&destination));
    match (delete, source_kind, destination_kind) {
        (_, RootKind::Missing, _) => Ok(destination),
        (true, RootKind::Other, _) => anyhow::bail!(
            "sync makes a directory the same as another, and {from} is not a directory; \
             to copy a file, use `ferry copy`"
        ),
        (true, RootKind::Directory, RootKind::Other) => {
            anyhow::bail!("sync makes a directory the same as another, and {to} is not a directory")
        }
        (false, RootKind::Directory, RootKind::Other) => {
            anyhow::bail!("{to} is not a directory, and {from} is one")
        }
        (false, RootKind::Other, RootKind::Directory) => Ok(into(destination, &file_name(source)?)),
        _ => Ok(destination),
    }
}

/// The last component of a location's path: the name a file keeps when
/// copied into a directory.
fn file_name(location: &Location) -> Result<Vec<u8>> {
    let path: &[u8] = match location {
        Location::Local(path) => path.as_os_str().as_encoded_bytes(),
        Location::Remote { path, .. } => path,
    };
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
}
