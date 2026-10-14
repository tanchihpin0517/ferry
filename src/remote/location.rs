//! Where a transfer's side is: a local path, or `[user@]host:path`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    /// A path on a host, as bytes. An empty path is the remote home directory.
    Remote {
        host: String,
        path: Vec<u8>,
    },
}

impl Location {
    /// As scp and rsync read it: a colon before the first slash makes the
    /// part before it a host. A local path with such a colon can be written
    /// with a leading `./`.
    pub fn parse(argument: &OsStr) -> Result<Self> {
        let bytes = argument.as_bytes();
        let colon = bytes.iter().position(|&byte| byte == b':');
        let slash = bytes.iter().position(|&byte| byte == b'/');
        let Some(colon) =
            colon.filter(|&colon| colon > 0 && slash.is_none_or(|slash| colon < slash))
        else {
            return Ok(Self::Local(PathBuf::from(OsString::from(argument))));
        };
        let Ok(host) = std::str::from_utf8(&bytes[..colon]) else {
            bail!("the host in {} is not UTF-8", argument.display());
        };
        if host.starts_with('-') {
            bail!("a host cannot start with '-': {host}");
        }
        Ok(Self::Remote {
            host: host.to_owned(),
            path: bytes[colon + 1..].to_vec(),
        })
    }
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

    #[test]
    fn a_host_cannot_look_like_an_option() {
        assert!(Location::parse(OsStr::new("-oProxyCommand=x:path")).is_err());
    }
}
