//! What a host has: its machine ID, platform, and Ferry builds.
//!
//! The probe is a POSIX shell script, because it runs before the host may have
//! any Ferry at all. It prints one `key=value` line per fact, between marks
//! that set them apart from what the host's shell prints. When the build
//! asked about is installed, the probe also marks it used and removes old
//! builds; see [`super::install::keep_recent`].
//!
//! And what a side's root is: on a host, a shell finds it, before Ferry need
//! be there.

use anyhow::{Context, Result, bail};

use crate::remote::Location;
use crate::transport::Transport;

#[derive(Debug, PartialEq, Eq)]
pub struct Probe {
    /// Tells apart the nodes of a cluster that share one home directory.
    pub machine_id: String,
    pub os: String,
    pub arch: String,
    /// Ferry's directory for this machine on the host, absolute.
    pub host_dir: String,
    /// Whether the build that was asked about is installed.
    pub installed: bool,
    /// Whether any build is installed.
    pub any_build: bool,
    pub cargo: bool,
}

/// The probe script. `dir_name` and `build` must be plain words, which
/// Ferry's directory names and build identities are.
pub fn script(dir_name: &str, build: &str) -> String {
    for word in [dir_name, build] {
        assert!(
            word.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte)),
            "not a plain word: {word}"
        );
    }
    format!(
        r#"id=""
for f in /etc/machine-id /var/lib/dbus/machine-id; do
    if [ -r "$f" ]; then id=$(tr -d '[:space:]' < "$f"); break; fi
done
if [ -z "$id" ] && command -v ioreg >/dev/null 2>&1; then
    id=$(ioreg -rd1 -c IOPlatformExpertDevice | awk -F'"' '/IOPlatformUUID/ {{ print $4 }}' | tr -d '-' | tr 'A-F' 'a-f')
fi
base=${{FERRY_HOME:-$HOME/{dir_name}}}
dir="$base/hosts/$id"
echo "machine_id=$id"
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
echo "host_dir=$dir"
build={build}
if [ -x "$dir/bin/$build/ferry" ]; then
    echo installed=yes
    {{
{keep_recent}    }} >/dev/null 2>&1
else
    echo installed=no
fi
if [ -n "$(ls -A "$dir/bin" 2>/dev/null)" ]; then echo any_build=yes; else echo any_build=no; fi
if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; then echo cargo=yes; else echo cargo=no; fi
"#,
        keep_recent = super::install::keep_recent(),
    )
}

pub fn parse(output: &str) -> Result<Probe> {
    let value = |key: &str| -> Result<String> {
        output
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
            .with_context(|| format!("the probe did not report {key}"))
    };
    let flag = |key: &str| -> Result<bool> {
        match value(key)?.as_str() {
            "yes" => Ok(true),
            "no" => Ok(false),
            other => bail!("the probe reported {key}={other}"),
        }
    };
    let machine_id = value("machine_id")?;
    if machine_id.is_empty() {
        bail!(
            "the host has no machine ID (/etc/machine-id), so Ferry cannot tell it apart from other machines sharing its home directory"
        );
    }
    if !machine_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("the host's machine ID is not hexadecimal: {machine_id}");
    }
    let host_dir = value("host_dir")?;
    if !host_dir.starts_with('/') {
        bail!("Ferry's directory on the host is not absolute: {host_dir}");
    }
    Ok(Probe {
        machine_id,
        os: value("os")?,
        arch: value("arch")?,
        host_dir,
        installed: flag("installed")?,
        any_build: flag("any_build")?,
        cargo: flag("cargo")?,
    })
}

/// Probes the host `transport` reaches. Only what the probe printed is read,
/// between its marks, never what the host's shell printed around it, as a
/// startup file might print `arch=…`.
pub fn run(transport: &Transport, dir_name: &str, build: &str) -> Result<Probe> {
    let place = transport.place();
    let (status, output) = transport
        .output(&script(dir_name, build))
        .with_context(|| format!("probe {place}"))?;
    if !status.success() {
        bail!("could not probe {place}: it exited with {status}");
    }
    let output = output.with_context(|| format!("could not probe {place}: it did not finish"))?;
    let output = String::from_utf8(output).context("the probe's output is not UTF-8")?;
    parse(&output).with_context(|| format!("probe {place}"))
}

/// What a location's root is, as its server will find it: a symlink to a
/// directory, as the user named it, is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Directory,
    Other,
    Missing,
}

/// Finds what a location's root is: on this machine, directly; on a host,
/// with a shell, before Ferry need be there.
pub fn root_kind(transport: &Transport, location: &Location) -> Result<RootKind> {
    let path = match location {
        Location::Local(path) => {
            return Ok(match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => RootKind::Directory,
                Ok(_) => RootKind::Other,
                // A symlink to nothing is there, as something else.
                Err(_) if std::fs::symlink_metadata(path).is_ok() => RootKind::Other,
                Err(_) => RootKind::Missing,
            });
        }
        Location::Remote { path, .. } => super::remote_path(path.clone()),
    };
    let (_, said) = transport.output(&kind_script(&path)).with_context(|| {
        format!(
            "look at {} on {}",
            super::shown(location),
            transport.place()
        )
    })?;
    match String::from_utf8_lossy(&said.unwrap_or_default()).trim() {
        "directory" => Ok(RootKind::Directory),
        "other" => Ok(RootKind::Other),
        "missing" => Ok(RootKind::Missing),
        said => anyhow::bail!(
            "look at {} on {}: unexpected answer {said:?}",
            super::shown(location),
            transport.place()
        ),
    }
}

/// A script that says what the root at `path` is, as `root_kind` reads it.
fn kind_script(path: &[u8]) -> String {
    format!(
        "{}; if [ -d \"$p\" ]; then echo directory; \
         elif [ -e \"$p\" ] || [ -h \"$p\" ]; then echo other; else echo missing; fi",
        crate::remote::shell::assign("p", path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// The script a host runs to say what a root is, run here, through the
    /// login shell's `sh -c`, on every kind, and on names any bytes make.
    #[test]
    fn a_host_says_what_a_root_is() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("directory")).unwrap();
        std::fs::write(root.join("file"), "x").unwrap();
        std::os::unix::fs::symlink("directory", root.join("to-directory")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("dangling")).unwrap();
        let odd = "it's é\n".as_bytes();
        std::fs::create_dir(root.join(std::ffi::OsStr::from_bytes(odd))).unwrap();
        // Not UTF-8, where the filesystem takes it: a Mac's does not.
        let latin = b"caf\xe9";
        let latin_made = std::fs::create_dir(root.join(std::ffi::OsStr::from_bytes(latin))).is_ok();
        let kind = |name: &[u8]| {
            let mut path = root.as_os_str().as_bytes().to_vec();
            path.push(b'/');
            path.extend_from_slice(name);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(crate::remote::shell::sh_command(&kind_script(&path)))
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        assert_eq!(kind(b"directory"), "directory");
        assert_eq!(kind(b"to-directory"), "directory");
        assert_eq!(kind(b"file"), "other");
        assert_eq!(kind(b"dangling"), "other");
        assert_eq!(kind(b"missing"), "missing");
        assert_eq!(kind(odd), "directory");
        if latin_made {
            assert_eq!(kind(latin), "directory");
        }
    }

    #[test]
    fn parses_a_probe() {
        let probe = parse(
            "machine_id=0123abcd\nos=Linux\narch=x86_64\nhost_dir=/home/me/.ferry/hosts/0123abcd\n\
             installed=no\nany_build=yes\ncargo=yes\n",
        )
        .unwrap();
        assert_eq!(probe.machine_id, "0123abcd");
        assert_eq!(probe.os, "Linux");
        assert_eq!(probe.host_dir, "/home/me/.ferry/hosts/0123abcd");
        assert!(!probe.installed && probe.any_build && probe.cargo);
    }

    #[test]
    fn a_host_without_a_machine_id_is_refused() {
        let error = parse("machine_id=\nos=Linux\narch=x86_64\nhost_dir=/h/hosts/\ninstalled=no\nany_build=no\ncargo=no\n")
            .unwrap_err();
        assert!(error.to_string().contains("machine ID"));
    }

    /// The script runs on this machine as it would on a host.
    #[test]
    fn the_script_runs_here() {
        let home = tempfile::tempdir().unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(script(".ferry-test", "dev-0"))
            .env("HOME", home.path())
            .env_remove("FERRY_HOME")
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        match parse(&output) {
            Ok(probe) => {
                assert!(
                    probe
                        .host_dir
                        .starts_with(&format!("{}/.ferry-test/hosts/", home.path().display()))
                );
                assert!(!probe.installed && !probe.any_build);
            }
            // A container without a machine ID: the script still reports it.
            Err(error) => assert!(error.to_string().contains("machine ID"), "{error}"),
        }
    }

    /// What a startup file prints before the probe, or after, is not read
    /// as what the probe found.
    #[test]
    fn what_the_shell_prints_is_not_the_probe() {
        let home = tempfile::tempdir().unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "echo arch=bogus; echo machine_id=0123\n{}\necho os=bogus",
                crate::transport::start::marked(&script(".ferry-test", "dev-0"))
            ))
            .env("HOME", home.path())
            .env_remove("FERRY_HOME")
            .output()
            .unwrap();
        assert!(output.status.success());
        let (before, inside) = crate::transport::start::unmarked(&output.stdout);
        assert_eq!(before, b"arch=bogus\nmachine_id=0123\n");
        let inside = String::from_utf8(inside.unwrap().to_vec()).unwrap();
        let uname = |flag| {
            let output = Command::new("uname").arg(flag).output().unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        assert!(
            inside.contains(&format!("arch={}\n", uname("-m"))),
            "{inside}"
        );
        assert!(
            inside.contains(&format!("os={}\n", uname("-s"))),
            "{inside}"
        );
        assert!(!inside.contains("bogus") && !inside.contains("machine_id=0123\n"));
    }
}
