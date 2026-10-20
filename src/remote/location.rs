//! Where a transfer's side is: a local path, or `[user@]host:path`, an IPv6
//! address in brackets, as `[user@][address]:path`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    /// A path on a host, as bytes, relative to the home directory there
    /// unless it starts with `/`. An empty path is the home directory.
    Remote {
        host: String,
        path: Vec<u8>,
    },
}

impl Location {
    /// As scp and rsync read it: a colon before the first slash makes the
    /// part before it a host. A local path with such a colon can be written
    /// with a leading `./`. On a host, `~` and `~/…` are the home directory,
    /// as they are to scp and rsync, whose shell there reads them so; a name
    /// that is `~` is written `./~`. An IPv6 address, which has colons of its
    /// own, is in brackets, as to scp and rsync: `[::1]:path`; a first word
    /// in brackets not followed by `:` is a local path.
    pub fn parse(argument: &OsStr) -> Result<Self> {
        let bytes = argument.as_bytes();
        if let Some(bracketed) = bracketed(bytes) {
            let Some((host, path)) = bracketed else {
                return Ok(Self::Local(PathBuf::from(OsString::from(argument))));
            };
            return remote(&host, path, argument);
        }
        let colon = bytes.iter().position(|&byte| byte == b':');
        let slash = bytes.iter().position(|&byte| byte == b'/');
        let Some(colon) =
            colon.filter(|&colon| colon > 0 && slash.is_none_or(|slash| colon < slash))
        else {
            return Ok(Self::Local(PathBuf::from(OsString::from(argument))));
        };
        remote(&bytes[..colon], &bytes[colon + 1..], argument)
    }
}

/// A location on `host`, at `path`, as written in `argument`.
fn remote(host: &[u8], path: &[u8], argument: &OsStr) -> Result<Location> {
    let Ok(host) = std::str::from_utf8(host) else {
        bail!("the host in {} is not UTF-8", argument.display());
    };
    if host.starts_with('-') {
        bail!("a host cannot start with '-': {host}");
    }
    Ok(Location::Remote {
        host: host.to_owned(),
        path: home_relative(path, argument)?,
    })
}

/// Where `bytes` start with an address in brackets, after a user's `@` if
/// any: the host, the user's `@` and the address without its brackets, and
/// the path after `]:`; `Some(None)` where the brackets are not followed by
/// a colon. `None` where they do not start so.
fn bracketed(bytes: &[u8]) -> Option<Option<(Vec<u8>, &[u8])>> {
    let open = bytes.iter().position(|&byte| byte == b'[')?;
    let user = &bytes[..open];
    if !(user.is_empty() || user.ends_with(b"@"))
        || user.iter().any(|&byte| byte == b'/' || byte == b':')
    {
        return None;
    }
    let close = open + bytes[open..].iter().position(|&byte| byte == b']')?;
    if bytes.get(close + 1) != Some(&b':') {
        return Some(None);
    }
    let mut host = user.to_vec();
    host.extend_from_slice(&bytes[open + 1..close]);
    Some(Some((host, &bytes[close + 2..])))
}

/// A path on a host without the `~` that names the home directory: the path
/// is relative to it anyway. Another user's, `~user`, is refused for now.
fn home_relative(path: &[u8], argument: &OsStr) -> Result<Vec<u8>> {
    if path == b"~" {
        return Ok(Vec::new());
    }
    if let Some(rest) = path.strip_prefix(b"~/") {
        let start = rest
            .iter()
            .position(|&byte| byte != b'/')
            .unwrap_or(rest.len());
        return Ok(rest[start..].to_vec());
    }
    if path.first() == Some(&b'~') {
        bail!(
            "{} names another user's home directory, which is not supported yet; \
             write the path in full, or ./~… for a name that starts with ~",
            argument.display()
        );
    }
    Ok(path.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argument: &str) -> Location {
        Location::parse(OsStr::new(argument)).unwrap()
    }

    fn remote(host: &str, path: &str) -> Location {
        Location::Remote {
            host: host.to_owned(),
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn local_paths() {
        assert_eq!(parse("dir"), Location::Local("dir".into()));
        assert_eq!(parse("/abs/path"), Location::Local("/abs/path".into()));
        assert_eq!(parse("./a:b"), Location::Local("./a:b".into()));
        assert_eq!(parse("dir/a:b"), Location::Local("dir/a:b".into()));
        assert_eq!(parse(":x"), Location::Local(":x".into()));
    }

    #[test]
    fn remote_paths() {
        assert_eq!(parse("host:dir"), remote("host", "dir"));
        assert_eq!(parse("me@host:/abs"), remote("me@host", "/abs"));
        assert_eq!(parse("host:"), remote("host", ""));
        assert_eq!(parse("host:a:b"), remote("host", "a:b"));
    }

    /// An IPv6 address is in brackets, with a user or without, as scp and
    /// rsync take it; without a colon after them, it is a local path.
    #[test]
    fn an_address_in_brackets_is_a_host() {
        assert_eq!(parse("[::1]:dir"), remote("::1", "dir"));
        assert_eq!(parse("me@[fe80::1]:/abs"), remote("me@fe80::1", "/abs"));
        assert_eq!(parse("[::1]:"), remote("::1", ""));
        assert_eq!(parse("[::1]:a:b"), remote("::1", "a:b"));
        assert_eq!(parse("./[::1]:x"), Location::Local("./[::1]:x".into()));
        assert_eq!(parse("[::1]"), Location::Local("[::1]".into()));
        let error = Location::parse(OsStr::new("[-x]:y")).unwrap_err();
        assert!(
            error.to_string().contains("cannot start with '-'"),
            "{error}"
        );
    }

    #[test]
    fn a_tilde_is_the_home_directory() {
        assert_eq!(parse("host:~"), remote("host", ""));
        assert_eq!(parse("host:~/"), remote("host", ""));
        assert_eq!(parse("host:~/a/b"), remote("host", "a/b"));
        assert_eq!(parse("host:~//a"), remote("host", "a"));
        // A name that is ~, written so.
        assert_eq!(parse("host:./~/a"), remote("host", "./~/a"));
        assert_eq!(parse("host:a/~"), remote("host", "a/~"));
        // Here, the shell reads ~ before Ferry does.
        assert_eq!(parse("~/a"), Location::Local("~/a".into()));
    }

    #[test]
    fn another_users_home_is_refused() {
        for argument in ["host:~root", "host:~root/a", "host:~~"] {
            let error = Location::parse(OsStr::new(argument)).unwrap_err();
            assert!(error.to_string().contains("another user's home"), "{error}");
        }
    }

    #[test]
    fn a_host_cannot_look_like_an_option() {
        assert!(Location::parse(OsStr::new("-oProxyCommand=x:path")).is_err());
    }
}
