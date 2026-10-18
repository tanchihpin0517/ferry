//! User and group names, to carry owners between machines by name.
//!
//! On Linux, Ferry asks the system's `getent`: a release is a static musl
//! build, and musl reads only `/etc/passwd` and `/etc/group`, while `getent`
//! sees every account NSS sees, LDAP and SSSD included. One `getent` call
//! looks up many keys. Without `getent`, or on macOS, the C library is asked.
//!
//! ID 0 is never mapped by name: root is root everywhere, as in rsync.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy)]
pub enum Database {
    Users,
    Groups,
}

impl Database {
    fn getent_name(self) -> &'static str {
        match self {
            Database::Users => "passwd",
            Database::Groups => "group",
        }
    }
}

/// The names of `ids`, for those that have one. ID 0 is left out.
pub fn names(database: Database, ids: &[u32]) -> HashMap<u32, String> {
    let ids: Vec<u32> = ids.iter().copied().filter(|&id| id != 0).collect();
    if ids.is_empty() {
        return HashMap::new();
    }
    let keys: Vec<String> = ids.iter().map(u32::to_string).collect();
    if let Some(records) = getent(database, &keys) {
        return records
            .into_iter()
            .filter(|(_, id)| ids.contains(id))
            .map(|(name, id)| (id, name))
            .collect();
    }
    ids.into_iter()
        .filter_map(|id| libc_name(database, id).map(|name| (id, name)))
        .collect()
}

/// The IDs of `names`, for those that exist here. A name whose ID is 0 is left
/// out.
pub fn ids(database: Database, names: &[String]) -> HashMap<String, u32> {
    if names.is_empty() {
        return HashMap::new();
    }
    let found: HashMap<String, u32> = match getent(database, names) {
        Some(records) => records
            .into_iter()
            .filter(|(name, _)| names.contains(name))
            .collect(),
        None => names
            .iter()
            .filter_map(|name| libc_id(database, name).map(|id| (name.clone(), id)))
            .collect(),
    };
    found.into_iter().filter(|&(_, id)| id != 0).collect()
}

/// Runs `getent` on Linux, as `(name, id)` records. `None` where `getent`
/// cannot run, or off Linux.
fn getent(database: Database, keys: &[String]) -> Option<Vec<(String, u32)>> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // `getent` exits with 2 when some key is not found, and still prints the
    // others, so its output is read whatever its status.
    let output = crate::limits::restore_in_child(&mut Command::new("getent"))
        .arg(database.getent_name())
        .args(keys)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.lines().filter_map(parse_record).collect())
}

/// `name:password:id:…`, the shape of both `passwd` and `group` lines.
fn parse_record(line: &str) -> Option<(String, u32)> {
    let mut fields = line.split(':');
    let name = fields.next()?;
    let _password = fields.next()?;
    let id = fields.next()?.parse().ok()?;
    Some((name.to_owned(), id))
}

const BUFFER: usize = 16 * 1024;

fn libc_name(database: Database, id: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; BUFFER];
    // SAFETY: each call gets a zeroed record, a buffer of the stated length,
    // and a result pointer, and the name is read only when the call found one,
    // while the buffer it points into is alive.
    unsafe {
        match database {
            Database::Users => {
                let mut record: libc::passwd = std::mem::zeroed();
                let mut result = std::ptr::null_mut();
                libc::getpwuid_r(
                    id,
                    &mut record,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                );
                (!result.is_null()).then(|| {
                    CStr::from_ptr(record.pw_name)
                        .to_string_lossy()
                        .into_owned()
                })
            }
            Database::Groups => {
                let mut record: libc::group = std::mem::zeroed();
                let mut result = std::ptr::null_mut();
                libc::getgrgid_r(
                    id,
                    &mut record,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                );
                (!result.is_null()).then(|| {
                    CStr::from_ptr(record.gr_name)
                        .to_string_lossy()
                        .into_owned()
                })
            }
        }
    }
}

fn libc_id(database: Database, name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut buffer = vec![0 as libc::c_char; BUFFER];
    // SAFETY: as in `libc_name`.
    unsafe {
        match database {
            Database::Users => {
                let mut record: libc::passwd = std::mem::zeroed();
                let mut result = std::ptr::null_mut();
                libc::getpwnam_r(
                    name.as_ptr(),
                    &mut record,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                );
                (!result.is_null()).then_some(record.pw_uid)
            }
            Database::Groups => {
                let mut record: libc::group = std::mem::zeroed();
                let mut result = std::ptr::null_mut();
                libc::getgrnam_r(
                    name.as_ptr(),
                    &mut record,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut result,
                );
                (!result.is_null()).then_some(record.gr_gid)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_parse() {
        assert_eq!(
            parse_record("me:x:1000:1000:Me:/home/me:/bin/sh"),
            Some(("me".to_owned(), 1000))
        );
        assert_eq!(
            parse_record("staff:*:20:root,me"),
            Some(("staff".to_owned(), 20))
        );
        assert_eq!(parse_record("broken"), None);
    }

    #[test]
    fn this_user_round_trips() {
        let uid = rustix::process::getuid().as_raw();
        if uid == 0 {
            return;
        }
        let name = names(Database::Users, &[uid])
            .remove(&uid)
            .expect("this user has a name");
        assert_eq!(
            ids(Database::Users, &[name])
                .values()
                .copied()
                .collect::<Vec<_>>(),
            [uid]
        );
    }

    #[test]
    fn root_is_never_mapped() {
        assert!(names(Database::Users, &[0]).is_empty());
        assert!(
            ids(Database::Groups, &["root".to_owned(), "wheel".to_owned()])
                .values()
                .all(|&id| id != 0)
        );
    }
}
