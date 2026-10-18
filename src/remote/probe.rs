//! What a host has: its machine ID, platform, and Ferry builds.
//!
//! The probe is a POSIX shell script, because it runs before the host may have
//! any Ferry at all. It prints one `key=value` line per fact, between marks
//! that set them apart from what the host's shell prints. When the build
//! asked about is installed, the probe also marks it used and removes old
//! builds; see [`super::install::keep_recent`].

use anyhow::{Context, Result, bail};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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
