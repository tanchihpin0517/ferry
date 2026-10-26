//! What the client checks of the two roots before anything changes: that
//! neither holds the other, and that `sync` is not about to empty a
//! directory it is easy to name by mistake.
//!
//! Each server says what its root is, as its kernel knows it; see
//! [`RootIdentity`]. Two roots are compared by device and inode, never by
//! the text of their paths, and only when both servers run on the same
//! kernel, where those mean the same thing.

use anyhow::{Result, bail};

use super::ask::confirm_with;
use crate::protocol::{FileKey, Guard, RootIdentity};
use crate::remote::Location;
use crate::transport::scope::Scope;

/// Both servers have said what their roots are, and neither has changed
/// anything: one root holding the other is refused, and a Destination
/// `sync` would empty by mistake asked about, before either goes on.
/// Returns what keeps each side off the other's root.
pub fn agree(
    source: (&RootIdentity, &Named),
    destination: (&RootIdentity, &Named),
    delete: bool,
    dry_run: bool,
    allow_dangerous_destination: bool,
    stopping: &Scope,
) -> Result<(Guard, Guard)> {
    match check(source.0, destination.0, delete, source.1, destination.1) {
        Verdict::Ok => {}
        Verdict::Refuse(reason) => bail!(reason),
        Verdict::Ask(what) if dry_run => {
            eprintln!("ferry: warning: {what} A real run would ask first.");
        }
        Verdict::Ask(_) if allow_dangerous_destination => {}
        Verdict::Ask(what) => confirm_with(
            &format!("{what} Sync into it anyway?"),
            "not synced",
            "pass --allow-dangerous-destination to sync into it",
            stopping,
        )?,
    }
    Ok(guards(source.0, destination.0))
}

/// What the client makes of the two roots.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say.
    Ok,
    /// Refused: one root holds the other. It says why.
    Refuse(String),
    /// `sync` would empty a directory easily named by mistake: asked about,
    /// or refused without a terminal. It says what the directory is.
    Ask(String),
}

/// How a root is named in a message: as the user wrote it, and where.
pub struct Named<'a> {
    pub shown: &'a str,
    pub location: &'a Location,
}

/// Checks the Source's root and the Destination's against each other. With
/// `delete`, as `sync`, also whether the Destination is `/`, a home
/// directory, or a directory that holds one.
pub fn check(
    source: &RootIdentity,
    destination: &RootIdentity,
    delete: bool,
    from: &Named,
    to: &Named,
) -> Verdict {
    let (doing, copy_or_sync) = if delete {
        ("syncing", "sync")
    } else {
        ("copying", "copy")
    };
    if let Some(overlap) = overlap(source, destination) {
        let (from, to) = (from.shown, to.shown);
        return Verdict::Refuse(match overlap {
            Overlap::Same => format!(
                "{to} is {from}, the Source: there is nothing to {copy_or_sync}. Choose a DESTINATION apart from it."
            ),
            Overlap::DestinationInside => format!(
                "{to} is inside {from}, the Source: {doing} would {}. Choose a DESTINATION outside it.",
                if delete {
                    "delete what the Source has there"
                } else {
                    "copy the Source into itself"
                }
            ),
            Overlap::SourceInside => format!(
                "{from}, the Source, is inside {to}: {doing} would {}. Choose a DESTINATION that does not hold it.",
                if delete {
                    "delete the Source"
                } else {
                    "write into the Source as it is read"
                }
            ),
        });
    }
    if !delete {
        return Verdict::Ok;
    }
    match dangerous(destination, to) {
        Some(what) => Verdict::Ask(format!(
            "{}{what}; sync would delete everything in it that {} does not have.",
            to.shown, from.shown
        )),
        None => Verdict::Ok,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Overlap {
    Same,
    DestinationInside,
    SourceInside,
}

/// How the roots overlap, if they do, as their chains say.
fn overlap(source: &RootIdentity, destination: &RootIdentity) -> Option<Overlap> {
    if !same_kernel(source, destination) {
        return None;
    }
    let source_root = source.chain.first()?;
    if destination.exists {
        let destination_root = destination.chain.first()?;
        if source_root == destination_root {
            return Some(Overlap::Same);
        }
        if destination.chain[1..].contains(source_root) {
            return Some(Overlap::DestinationInside);
        }
        if source.chain[1..].contains(destination_root) {
            return Some(Overlap::SourceInside);
        }
        None
    } else {
        // Not there yet: it is to be made inside its nearest ancestor that
        // is, the first of its chain.
        destination
            .chain
            .contains(source_root)
            .then_some(Overlap::DestinationInside)
    }
}

/// Whether two roots are on the same running kernel, where their devices and
/// inodes can be compared.
fn same_kernel(source: &RootIdentity, destination: &RootIdentity) -> bool {
    !source.kernel.is_empty() && source.kernel == destination.kernel
}

/// What makes a Destination one `sync` should ask about, said after its
/// name: `/`, a home directory, or a directory that holds one.
fn dangerous(destination: &RootIdentity, to: &Named) -> Option<String> {
    if !destination.exists {
        return None;
    }
    let location = to.location;
    let place = match location {
        Location::Local(_) => "this machine".to_owned(),
        Location::Remote { host, .. } => host.clone(),
    };
    let canonical = destination.canonical.as_slice();
    if canonical == b"/" {
        return Some(format!(" is / on {place}"));
    }
    for home in &destination.homes {
        if canonical == home.as_slice() {
            // `host:` alone is the home directory: said so, as it is easy to
            // write without meaning to.
            // Named as it is, the home directory needs no naming again; `host:`
            // alone is said to be one, as it is easy to write without meaning
            // to.
            let named = match location {
                Location::Remote { path, .. } => path.is_empty(),
                Location::Local(_) => to.shown.as_bytes() == home.as_slice(),
            };
            return Some(if named {
                format!(" is the home directory on {place}")
            } else {
                format!(
                    " is the home directory {} on {place}",
                    String::from_utf8_lossy(home)
                )
            });
        }
        if home.starts_with(canonical) && home.get(canonical.len()) == Some(&b'/') {
            return Some(format!(
                " holds the home directory {} on {place}",
                String::from_utf8_lossy(home)
            ));
        }
    }
    None
}

/// What each peer is kept from during the transfer, behind the check before
/// it: the Source's scan from the Destination's root, and the Destination
/// from the Source's root and each directory above it. Returns the
/// Source's guard, then the Destination's. Roots on different kernels
/// cannot be compared, and guard nothing.
pub fn guards(source: &RootIdentity, destination: &RootIdentity) -> (Guard, Guard) {
    if !same_kernel(source, destination) {
        return (Guard::default(), Guard::default());
    }
    let avoid: Vec<FileKey> = if destination.exists {
        destination.chain.first().copied().into_iter().collect()
    } else {
        Vec::new()
    };
    (
        Guard {
            avoid,
            protect: Vec::new(),
        },
        Guard {
            avoid: Vec::new(),
            protect: source.chain.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ino: u64) -> FileKey {
        FileKey { dev: 1, ino }
    }

    /// A root at `/` + `names`, its chain made of inodes 1 for `/`, then
    /// `inodes` downwards.
    fn root(path: &str, inodes: &[u64], exists: bool) -> RootIdentity {
        let mut chain: Vec<FileKey> = inodes.iter().rev().map(|&ino| key(ino)).collect();
        chain.push(key(1));
        RootIdentity {
            kernel: "boot".to_owned(),
            canonical: path.as_bytes().to_vec(),
            exists,
            chain,
            homes: vec![b"/home/me".to_vec()],
        }
    }

    fn local(path: &str) -> Location {
        Location::Local(path.into())
    }

    fn verdict(
        source: &RootIdentity,
        destination: &RootIdentity,
        delete: bool,
        to: &Location,
    ) -> Verdict {
        let from = local("src");
        check(
            source,
            destination,
            delete,
            &Named {
                shown: "src",
                location: &from,
            },
            &Named {
                shown: "dst",
                location: to,
            },
        )
    }

    fn refused(verdict: Verdict, says: &str) {
        match verdict {
            Verdict::Refuse(reason) => assert!(reason.contains(says), "{reason}"),
            other => panic!("expected a refusal saying {says:?}, got {other:?}"),
        }
    }

    fn asked(verdict: Verdict, says: &str) {
        match verdict {
            Verdict::Ask(question) => assert!(question.contains(says), "{question}"),
            other => panic!("expected a question saying {says:?}, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_roots_are_refused_for_copy_and_sync() {
        let a = root("/a", &[10], true);
        let a_sub = root("/a/sub", &[10, 11], true);
        let a_new = root("/a/new", &[10], false);
        let to = local("dst");
        for delete in [false, true] {
            refused(verdict(&a, &a, delete, &to), "is src, the Source");
            refused(verdict(&a, &a_sub, delete, &to), "is inside src");
            refused(verdict(&a_sub, &a, delete, &to), "is inside dst");
            refused(verdict(&a, &a_new, delete, &to), "is inside src");
        }
    }

    #[test]
    fn roots_apart_are_not_refused() {
        let a = root("/a", &[10], true);
        let b = root("/b", &[20], true);
        let b_new = root("/b/new", &[20], false);
        let to = local("dst");
        assert_eq!(verdict(&a, &b, true, &to), Verdict::Ok);
        assert_eq!(verdict(&a, &b_new, true, &to), Verdict::Ok);
        // A Destination missing beside the Source is not inside it.
        let a_sibling = root("/a2", &[], false);
        assert_eq!(verdict(&a, &a_sibling, true, &to), Verdict::Ok);
    }

    #[test]
    fn roots_on_different_kernels_are_never_compared() {
        let a = root("/a", &[10], true);
        let mut elsewhere = root("/a", &[10], true);
        elsewhere.kernel = "another".to_owned();
        assert_eq!(verdict(&a, &elsewhere, true, &local("dst")), Verdict::Ok);
        let mut unknown = a.clone();
        unknown.kernel = String::new();
        assert_eq!(
            verdict(&unknown, &unknown, true, &local("dst")),
            Verdict::Ok
        );
    }

    #[test]
    fn sync_asks_before_emptying_root_or_a_home() {
        let a = root("/a", &[10], true);
        let to = local("dst");
        // On the same kernel, `/` holds the Source, and is refused as that;
        // a Source elsewhere is asked about.
        let slash = root("/", &[], true);
        refused(verdict(&a, &slash, true, &to), "is inside dst");
        let mut elsewhere = a.clone();
        elsewhere.kernel = "another".to_owned();
        asked(
            verdict(&elsewhere, &slash, true, &to),
            "is / on this machine",
        );
        let home = root("/home/me", &[30, 31], true);
        asked(
            verdict(&a, &home, true, &to),
            "is the home directory /home/me on",
        );
        let homes = root("/home", &[30], true);
        asked(
            verdict(&a, &homes, true, &to),
            "holds the home directory /home/me",
        );
        let remote_home = Location::Remote {
            host: "web1".to_owned(),
            path: Vec::new(),
        };
        asked(
            verdict(&a, &home, true, &remote_home),
            "is the home directory on web1",
        );
    }

    #[test]
    fn copy_never_asks() {
        let a = root("/a", &[10], true);
        let home = root("/home/me", &[30, 31], true);
        assert_eq!(verdict(&a, &home, false, &local("dst")), Verdict::Ok);
    }

    #[test]
    fn a_directory_beside_a_home_is_not_one() {
        let a = root("/a", &[10], true);
        let beside = root("/home/meme", &[30, 32], true);
        assert_eq!(verdict(&a, &beside, true, &local("dst")), Verdict::Ok);
    }

    #[test]
    fn guards_keep_each_peer_from_the_other_root() {
        let a = root("/a", &[10], true);
        let b = root("/b", &[20], true);
        let (source, destination) = guards(&a, &b);
        assert_eq!(source.avoid, vec![key(20)]);
        assert_eq!(destination.protect, vec![key(10), key(1)]);
        let mut elsewhere = b.clone();
        elsewhere.kernel = "another".to_owned();
        assert_eq!(guards(&a, &elsewhere), (Guard::default(), Guard::default()));
    }
}
