//! What a host has: its machine ID, platform, and Ferry builds; and what the
//! roots of the sides there are.
//!
//! The probe is a POSIX shell script, because it runs before the host may have
//! any Ferry at all. It prints one `key=value` line per fact, between marks
//! that set them apart from what the host's shell prints. When the build
//! asked about is installed, the probe also marks it used and removes old
//! builds; see [`super::install::keep_recent`]. Each host is probed once,
//! however many sides are there; on this machine, the roots are looked at
//! directly.

use anyhow::{Context, Result, bail};

use super::place::{Place, Places};
use crate::remote::Location;
use crate::status;
use crate::transport::Transport;

#[derive(Debug, PartialEq, Eq)]
pub struct Probe {
    /// Tells apart the nodes of a cluster that share one home directory.
    pub machine_id: String,
    pub os: String,
    pub arch: String,
    /// Ferry's directory for this machine on the host.
    pub host_dir: String,
    /// Whether the build that was asked about is installed.
    pub installed: bool,
    /// Whether any build is installed.
    pub any_build: bool,
    pub cargo: bool,
    /// What each root asked about is, in the order asked.
    pub roots: Vec<RootKind>,
}

impl Probe {
    /// Refuses a host Ferry's directory there would not be this machine's
    /// alone on: checked before anything is installed there. A host that
    /// runs the binary `--remote-ferry` names needs none of it.
    pub fn checked(&self) -> Result<()> {
        if self.machine_id.is_empty() {
            bail!(
                "the host has no machine ID (/etc/machine-id), so Ferry cannot tell it apart from other machines sharing its home directory"
            );
        }
        if !self.machine_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!(
                "the host's machine ID is not hexadecimal: {}",
                self.machine_id
            );
        }
        if !self.host_dir.starts_with('/') {
            bail!(
                "Ferry's directory on the host is not absolute: {}",
                self.host_dir
            );
        }
        Ok(())
    }
}

/// What a location's root is, as its server will find it: a symlink to a
/// directory, as the user named it, is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Directory,
    Other,
    Missing,
}

/// The probe script, which also says what the root at each of `roots` is.
/// `dir_name` and `build` must be plain words, which Ferry's directory names
/// and build identities are.
pub fn script(dir_name: &str, build: &str, roots: &[Vec<u8>]) -> String {
    for word in [dir_name, build] {
        assert!(
            word.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte)),
            "not a plain word: {word}"
        );
    }
    let mut script = format!(
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
    );
    for root in roots {
        script.push_str(&format!(
            "{}; if [ -d \"$p\" ]; then echo root=directory; \
             elif [ -e \"$p\" ] || [ -h \"$p\" ]; then echo root=other; else echo root=missing; fi\n",
            crate::remote::shell::assign("p", root)
        ));
    }
    script
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
    let roots = output
        .lines()
        .filter_map(|line| line.strip_prefix("root="))
        .map(|kind| match kind {
            "directory" => Ok(RootKind::Directory),
            "other" => Ok(RootKind::Other),
            "missing" => Ok(RootKind::Missing),
            other => bail!("the probe reported root={other}"),
        })
        .collect::<Result<_>>()?;
    Ok(Probe {
        machine_id: value("machine_id")?,
        os: value("os")?,
        arch: value("arch")?,
        host_dir: value("host_dir")?,
        installed: flag("installed")?,
        any_build: flag("any_build")?,
        cargo: flag("cargo")?,
        roots,
    })
}

/// Probes the host `transport` reaches, asking what each of `roots` is. Only
/// what the probe printed is read, between its marks, never what the host's
/// shell printed around it, as a startup file might print `arch=…`.
pub fn run(transport: &Transport, dir_name: &str, build: &str, roots: &[Vec<u8>]) -> Result<Probe> {
    let place = transport.place();
    let (status, output) = transport
        .output(&script(dir_name, build, roots))
        .with_context(|| format!("probe {place}"))?;
    if !status.success() {
        bail!("could not probe {place}: it exited with {status}");
    }
    let output = output.with_context(|| format!("could not probe {place}: it did not finish"))?;
    let output = String::from_utf8(output).context("the probe's output is not UTF-8")?;
    let probe = parse(&output).with_context(|| format!("probe {place}"))?;
    if probe.roots.len() != roots.len() {
        bail!("probe {place}: it did not say what every root is");
    }
    Ok(probe)
}

/// Probes `place`, if it is a host, keeping what it found, and says what
/// each of `locations`' roots there is.
pub fn probe(place: &mut Place, locations: &[&Location]) -> Result<Vec<RootKind>> {
    if matches!(*place.transport, Transport::Local) {
        return Ok(locations
            .iter()
            .map(|location| local_kind(location))
            .collect());
    }
    let roots: Vec<Vec<u8>> = locations
        .iter()
        .map(|location| match location {
            Location::Remote { path, .. } => super::remote_path(path.clone()),
            Location::Local(_) => unreachable!("a local root is not on a host"),
        })
        .collect();
    let probe = {
        let _shown = status::show(format!("checking Ferry on {}", place.transport.place()));
        run(
            &place.transport,
            crate::home::dir_name(),
            &crate::build_info::identity(),
            &roots,
        )?
    };
    let kinds = probe.roots.clone();
    place.probe = Some(probe);
    Ok(kinds)
}

/// Probes every place once, and says what the root of each of `locations`,
/// a side each, is.
pub fn roots(places: &mut Places, locations: &[&Location]) -> Result<Vec<RootKind>> {
    let mut kinds = vec![RootKind::Missing; locations.len()];
    for place in places.iter_mut() {
        let here: Vec<usize> = (0..locations.len())
            .filter(|&index| super::place::PlaceKey::of(locations[index]) == place.key)
            .collect();
        let found = probe(
            place,
            &here
                .iter()
                .map(|&index| locations[index])
                .collect::<Vec<_>>(),
        )?;
        for (index, kind) in here.into_iter().zip(found) {
            kinds[index] = kind;
        }
    }
    Ok(kinds)
}

/// What a root on this machine is.
fn local_kind(location: &Location) -> RootKind {
    let Location::Local(path) = location else {
        unreachable!("a root on this machine is local");
    };
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => RootKind::Directory,
        Ok(_) => RootKind::Other,
        // A symlink to nothing is there, as something else.
        Err(_) if std::fs::symlink_metadata(path).is_ok() => RootKind::Other,
        Err(_) => RootKind::Missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// The probe says what a root is, run here, through the login shell's
    /// `sh -c`, on every kind, and on names any bytes make.
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
                .arg(crate::remote::shell::sh_command(&script(
                    ".ferry-test",
                    "dev-0",
                    &[path],
                )))
                .env("HOME", root)
                .output()
                .unwrap();
            let output = String::from_utf8(output.stdout).unwrap();
            parse(&output).unwrap().roots
        };
        assert_eq!(kind(b"directory"), [RootKind::Directory]);
        assert_eq!(kind(b"to-directory"), [RootKind::Directory]);
        assert_eq!(kind(b"file"), [RootKind::Other]);
        assert_eq!(kind(b"dangling"), [RootKind::Other]);
        assert_eq!(kind(b"missing"), [RootKind::Missing]);
        assert_eq!(kind(odd), [RootKind::Directory]);
        if latin_made {
            assert_eq!(kind(latin), [RootKind::Directory]);
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
        assert!(probe.roots.is_empty());
        probe.checked().unwrap();
    }

    #[test]
    fn a_host_without_a_machine_id_is_refused() {
        let probe = parse("machine_id=\nos=Linux\narch=x86_64\nhost_dir=/h/hosts/\ninstalled=no\nany_build=no\ncargo=no\n")
            .unwrap();
        let error = probe.checked().unwrap_err();
        assert!(error.to_string().contains("machine ID"));
    }

    /// The script runs on this machine as it would on a host.
    #[test]
    fn the_script_runs_here() {
        let home = tempfile::tempdir().unwrap();
        // As a host's login shell runs it.
        let output = Command::new("sh")
            .arg("-c")
            .arg(crate::remote::shell::sh_command(&script(
                ".ferry-test",
                "dev-0",
                &[],
            )))
            .env("HOME", home.path())
            .env_remove("FERRY_HOME")
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let probe = parse(&output).unwrap();
        match probe.checked() {
            Ok(()) => {
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
                crate::transport::start::marked(&script(".ferry-test", "dev-0", &[]))
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
