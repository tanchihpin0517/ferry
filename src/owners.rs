//! Owners on the Destination: the Source's user and group IDs mapped to local
//! ones by name, and which changes of owner this process may make.
//!
//! An ID whose name the Source did not send, or that does not exist here,
//! stays as it is, as in rsync. A process that is not root can give a file
//! only its own user, and, on a file it owns, only a group it belongs to;
//! any other owner is left as it is, without a word, as in rsync. Nor can
//! it set the permissions or mtime of a file it does not own.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use crate::names::{self, Database};

/// Shared by every stage that sets owners, and learning as the Source's
/// names come: what is learned is added in place, never copied, however
/// many hold it.
pub struct Ownership {
    mapped: RwLock<Mapped>,
    root: bool,
    /// This process's user.
    euid: u32,
    member_of: HashSet<u32>,
}

/// The Source's IDs mapped to local ones, as far as their names are known.
#[derive(Default)]
struct Mapped {
    users: HashMap<u32, u32>,
    groups: HashMap<u32, u32>,
}

impl Ownership {
    /// The mapping before any names arrive, for this process.
    /// As root would: for a test that need not run as root.
    #[cfg(test)]
    pub fn as_root() -> Self {
        Self {
            root: true,
            euid: 0,
            ..Self::here()
        }
    }

    pub fn here() -> Self {
        let mut member_of: HashSet<u32> = rustix::process::getgroups()
            .unwrap_or_default()
            .into_iter()
            .map(|gid| gid.as_raw())
            .collect();
        member_of.insert(rustix::process::getgid().as_raw());
        let euid = rustix::process::geteuid();
        Self {
            mapped: RwLock::default(),
            root: euid.is_root(),
            euid: euid.as_raw(),
            member_of,
        }
    }

    /// Learns the Source's names for some of its IDs: looked up here first,
    /// as that may be slow, then added.
    pub fn learn(&self, users: &[(u32, String)], groups: &[(u32, String)]) {
        self.add(
            local(Database::Users, users),
            local(Database::Groups, groups),
        );
    }

    /// Adds the Source's IDs mapped to local ones, in place.
    fn add(&self, users: Vec<(u32, u32)>, groups: Vec<(u32, u32)>) {
        let mut mapped = self.mapped.write().expect("not poisoned");
        mapped.users.extend(users);
        mapped.groups.extend(groups);
    }

    /// The local user for the Source's `uid`.
    pub fn uid(&self, uid: u32) -> u32 {
        let mapped = self.mapped.read().expect("not poisoned");
        mapped.users.get(&uid).copied().unwrap_or(uid)
    }

    /// The local group for the Source's `gid`.
    pub fn gid(&self, gid: u32) -> u32 {
        let mapped = self.mapped.read().expect("not poisoned");
        mapped.groups.get(&gid).copied().unwrap_or(gid)
    }

    /// Whether an entry owned by `current` can be given to `wanted`.
    pub fn can_set_user(&self, wanted: u32, current: u32) -> bool {
        self.root || wanted == current
    }

    /// Whether an entry owned by `current` can be given the group `wanted`:
    /// only by its owner, and only a group the owner belongs to, or by root.
    pub fn can_set_group(&self, wanted: u32, current: u32) -> bool {
        self.root || (current == self.euid && self.member_of.contains(&wanted))
    }

    /// Whether the permissions and mtime of an entry owned by `current` can
    /// be set: only by its owner, or by root.
    pub fn can_set_mode(&self, current: u32) -> bool {
        self.root || current == self.euid
    }
}

/// The local IDs, of the Source's IDs whose names exist here.
fn local(database: Database, named: &[(u32, String)]) -> Vec<(u32, u32)> {
    let names: Vec<String> = named.iter().map(|(_, name)| name.clone()).collect();
    let local = names::ids(database, &names);
    named
        .iter()
        .filter_map(|(remote, name)| local.get(name).map(|&id| (*remote, id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// What is learned is added in place: a stage that holds the ownership,
    /// as one comparing a part does, sees it, and none of it is copied,
    /// however many names come one at a time.
    #[test]
    fn what_is_learned_is_added_in_place_however_many_hold_it() {
        let ownership = Arc::new(Ownership::here());
        let held = Arc::clone(&ownership);
        for remote in 0..10_000 {
            ownership.add(
                vec![(100_000 + remote, remote)],
                vec![(200_000 + remote, remote)],
            );
        }
        assert_eq!(held.uid(100_000 + 9_999), 9_999);
        assert_eq!(held.gid(200_000 + 42), 42);
        // One never named stays as it is.
        assert_eq!(held.uid(7), 7);
    }

    /// A process that is not root sets the group, the permissions, and the
    /// mtime only of what it owns, and the group only to one it is in.
    #[test]
    fn who_is_not_root_sets_only_what_it_owns() {
        let ownership = Ownership {
            mapped: RwLock::default(),
            root: false,
            euid: 1000,
            member_of: HashSet::from([1000, 27]),
        };
        assert!(ownership.can_set_group(27, 1000));
        assert!(!ownership.can_set_group(27, 1001));
        assert!(!ownership.can_set_group(28, 1000));
        assert!(ownership.can_set_mode(1000));
        assert!(!ownership.can_set_mode(0));
        assert!(ownership.can_set_user(1000, 1000));
        assert!(!ownership.can_set_user(1000, 1001));
        let root = Ownership {
            root: true,
            euid: 0,
            ..ownership
        };
        assert!(root.can_set_group(28, 1001));
        assert!(root.can_set_mode(1001));
    }
}
