//! Owners on the Destination: the Source's user and group IDs mapped to local
//! ones by name, and which changes of owner this process may make.
//!
//! An ID whose name the Source did not send, or that does not exist here,
//! stays as it is, as in rsync. A process that is not root can give a file
//! only its own user, and only a group it belongs to; any other owner is left
//! as it is, and the run warns.

use std::collections::{HashMap, HashSet};

use crate::names::{self, Database};

#[derive(Clone)]
pub struct Ownership {
    users: HashMap<u32, u32>,
    groups: HashMap<u32, u32>,
    root: bool,
    member_of: HashSet<u32>,
}

impl Ownership {
    /// The mapping before any names arrive, for this process.
    pub fn here() -> Self {
        let mut member_of: HashSet<u32> = rustix::process::getgroups()
            .unwrap_or_default()
            .into_iter()
            .map(|gid| gid.as_raw())
            .collect();
        member_of.insert(rustix::process::getgid().as_raw());
        Self {
            users: HashMap::new(),
            groups: HashMap::new(),
            root: rustix::process::geteuid().is_root(),
            member_of,
        }
    }

    /// Learns the Source's names for some of its IDs.
    pub fn learn(&mut self, users: &[(u32, String)], groups: &[(u32, String)]) {
        learn(&mut self.users, Database::Users, users);
        learn(&mut self.groups, Database::Groups, groups);
    }

    /// The local user for the Source's `uid`.
    pub fn uid(&self, uid: u32) -> u32 {
        self.users.get(&uid).copied().unwrap_or(uid)
    }

    /// The local group for the Source's `gid`.
    pub fn gid(&self, gid: u32) -> u32 {
        self.groups.get(&gid).copied().unwrap_or(gid)
    }

    /// Whether an entry owned by `current` can be given to `wanted`.
    pub fn can_set_user(&self, wanted: u32, current: u32) -> bool {
        self.root || wanted == current
    }

    /// Whether an entry can be given the group `wanted`.
    pub fn can_set_group(&self, wanted: u32) -> bool {
        self.root || self.member_of.contains(&wanted)
    }
}

fn learn(map: &mut HashMap<u32, u32>, database: Database, named: &[(u32, String)]) {
    let names: Vec<String> = named.iter().map(|(_, name)| name.clone()).collect();
    let local = names::ids(database, &names);
    for (remote, name) in named {
        if let Some(&id) = local.get(name) {
            map.insert(*remote, id);
        }
    }
}
