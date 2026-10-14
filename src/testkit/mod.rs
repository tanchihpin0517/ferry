//! `ferry test`: the whole flow between two places, on trees it creates and
//! then removes.
//!
//! Each scenario puts a known tree at the destination, runs the real `ferry`
//! command between the two places, and checks what it prints and, after a
//! real transfer, what the destination holds. The scenarios grow as Ferry
//! does.

pub mod fixture;

use std::ffi::OsString;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::remote::{self, Location, RemoteOptions};
use fixture::{Checks, Variant};

/// The base tree, listed in full: what a missing destination needs.
const EVERYTHING: &str = ".
.ferry-fixture
dangling
dir/
empty-dir/
fifo
file.txt
link
dir/deeper/
dir/nested.txt
dir/deeper/deep.bin
";

/// What the changed tree lacks or has differently, in manifest order.
const CHANGED: &str = "empty-dir/
file.txt
link
dir/deeper/
dir/nested.txt
dir/deeper/deep.bin
";

/// Every transfer command. Each scenario runs with each of them.
const COMMANDS: &[&str] = &["copy", "sync"];

/// What the destination is compared on after a transfer: everything but
/// owners.
const CHECKS: Checks = Checks {
    files_and_directories_only: false,
    ignore_directory_mtimes: false,
};

/// A destination tree, a transfer to it, and what follows. `sync` lists the
/// same as `copy` until deleting is built; then the scenarios list deletions
/// too.
struct Scenario {
    name: &'static str,
    /// The tree at the destination, or none.
    destination: Option<Variant>,
    arguments: &'static [&'static str],
    dry_run: bool,
    /// What the transfer prints.
    expected: &'static str,
    /// The tree the destination must hold afterwards, if it is checked.
    verify: Option<Variant>,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "a missing destination needs everything",
        destination: None,
        arguments: &[],
        dry_run: true,
        expected: EVERYTHING,
        verify: None,
    },
    Scenario {
        name: "an identical destination needs nothing",
        destination: Some(Variant::Base),
        arguments: &[],
        dry_run: true,
        expected: "",
        verify: None,
    },
    Scenario {
        name: "a changed destination needs what changed",
        destination: Some(Variant::Changed),
        arguments: &[],
        dry_run: true,
        expected: CHANGED,
        verify: None,
    },
    Scenario {
        name: "mtimes in the same second are equal",
        destination: Some(Variant::Nanoseconds),
        arguments: &[],
        dry_run: true,
        expected: "",
        verify: None,
    },
    Scenario {
        name: "--modify-window=-1 compares nanoseconds",
        destination: Some(Variant::Nanoseconds),
        arguments: &["--modify-window=-1"],
        dry_run: true,
        expected: "file.txt\n",
        verify: None,
    },
    Scenario {
        name: "other permissions need their attributes",
        destination: Some(Variant::Modes),
        arguments: &[],
        dry_run: true,
        expected: "file.txt\ndir/deeper/\n",
        verify: None,
    },
    Scenario {
        name: "copying to a missing destination creates the tree",
        destination: None,
        arguments: &[],
        dry_run: false,
        expected: "",
        verify: Some(Variant::Base),
    },
    Scenario {
        name: "copying over a changed destination restores the tree",
        destination: Some(Variant::Changed),
        arguments: &[],
        dry_run: false,
        expected: "",
        verify: Some(Variant::Base),
    },
    Scenario {
        name: "copying restores permissions",
        destination: Some(Variant::Modes),
        arguments: &[],
        dry_run: false,
        expected: "",
        verify: Some(Variant::Base),
    },
];

pub struct TestOptions<'a> {
    pub remote: RemoteOptions<'a>,
    pub config: &'a Config,
}

/// One side of the test: a fixture's root in a local or remote directory.
struct Side<'a> {
    location: Location,
    options: &'a TestOptions<'a>,
}

impl Side<'_> {
    /// A side whose root is `name` inside the directory at `parent`.
    fn new<'a>(parent: Location, name: &str, options: &'a TestOptions<'a>) -> Result<Side<'a>> {
        let location = match parent {
            Location::Local(path) => Location::Local(path.join(name)),
            Location::Remote { host, path } => {
                let mut path = String::from_utf8(path)
                    .context("a remote path for `ferry test` must be UTF-8")?;
                if !path.is_empty() && !path.ends_with('/') {
                    path.push('/');
                }
                path.push_str(name);
                Location::Remote {
                    host,
                    path: path.into_bytes(),
                }
            }
        };
        Ok(Side { location, options })
    }

    /// The location as an argument to `ferry`.
    fn argument(&self) -> OsString {
        match &self.location {
            Location::Local(path) => path.clone().into_os_string(),
            Location::Remote { host, path } => {
                format!("{host}:{}", String::from_utf8_lossy(path)).into()
            }
        }
    }

    fn create(&self, variant: Variant) -> Result<()> {
        self.fixture(Action::Create(variant))
    }

    fn remove(&self) -> Result<()> {
        self.fixture(Action::Remove)
    }

    /// Checks that the tree here is `variant`.
    fn verify(&self, variant: Variant) -> Result<()> {
        self.fixture(Action::Verify(variant))
    }

    /// Runs a fixture action here, or with `ferry fixture` on the host.
    fn fixture(&self, action: Action) -> Result<()> {
        let (host, path) = match &self.location {
            Location::Local(path) => {
                return match action {
                    Action::Create(variant) => fixture::create(path, variant),
                    Action::Remove => fixture::remove(path),
                    Action::Verify(variant) => fixture::verify(path, variant, CHECKS),
                };
            }
            Location::Remote { host, path } => (host, String::from_utf8_lossy(path)),
        };
        let mut arguments = vec!["fixture".to_owned()];
        match action {
            Action::Create(variant) => {
                arguments.extend([
                    "create".to_owned(),
                    "--variant".to_owned(),
                    variant_name(variant),
                ]);
            }
            Action::Remove => arguments.push("remove".to_owned()),
            Action::Verify(variant) => {
                arguments.extend([
                    "verify".to_owned(),
                    "--variant".to_owned(),
                    variant_name(variant),
                ]);
                if CHECKS.files_and_directories_only {
                    arguments.push("--files-and-directories-only".to_owned());
                }
                if CHECKS.ignore_directory_mtimes {
                    arguments.push("--ignore-directory-mtimes".to_owned());
                }
            }
        }
        arguments.push(path.into_owned());
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = remote::command(
            host,
            &self.options.remote,
            &self.options.config.ssh,
            &arguments,
        )?
        .stdin(Stdio::null())
        .output()
        .context("start ssh")?;
        if !output.status.success() {
            bail!(
                "`ferry {}` on {host} failed:\n{}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

fn variant_name(variant: Variant) -> String {
    format!("{variant:?}").to_lowercase()
}

enum Action {
    Create(Variant),
    Remove,
    Verify(Variant),
}

enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
}

pub fn run(source: Location, destination: Location, options: &TestOptions) -> Result<ExitCode> {
    let run = run_name();
    let source = Side::new(source, &format!("{run}-source"), options)?;
    let destination = Side::new(destination, &format!("{run}-destination"), options)?;
    println!(
        "ferry test: from {} to {}",
        source.argument().to_string_lossy(),
        destination.argument().to_string_lossy()
    );

    // Installing on a host happens here, where its output shows, rather than
    // inside the first scenario, where it would be captured and timed.
    for side in [&source, &destination] {
        if let Location::Remote { host, .. } = &side.location {
            remote::prepare(host, &options.remote, &options.config.ssh)?;
        }
    }

    source.create(Variant::Base)?;
    let outcomes = scenarios(&source, &destination, options);
    let cleanup = source.remove();

    let outcomes = outcomes?;
    cleanup?;
    let count =
        |wanted: fn(&Outcome) -> bool| outcomes.iter().filter(|outcome| wanted(outcome)).count();
    let (passed, failed, skipped) = (
        count(|outcome| matches!(outcome, Outcome::Passed)),
        count(|outcome| matches!(outcome, Outcome::Failed(_))),
        count(|outcome| matches!(outcome, Outcome::Skipped(_))),
    );
    println!("{passed} passed, {failed} failed, {skipped} skipped");
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs every scenario with every command, leaving no destination tree
/// behind. Once a command is skipped as not built yet, its later scenarios are
/// skipped without running.
fn scenarios(source: &Side, destination: &Side, options: &TestOptions) -> Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for command in COMMANDS {
        let mut skip_reason: Option<String> = None;
        for scenario in SCENARIOS {
            let name = format!("{command}: {}", scenario.name);
            if let Some(reason) = &skip_reason {
                report(&name, &Outcome::Skipped(reason.clone()), None);
                outcomes.push(Outcome::Skipped(reason.clone()));
                continue;
            }
            if let Some(variant) = scenario.destination {
                destination.create(variant)?;
            }
            let started = Instant::now();
            let mut result = transfer(command, source, destination, scenario, options);
            let elapsed = started.elapsed().as_secs_f64();
            if let (Ok(Outcome::Passed), Some(variant)) = (&result, scenario.verify)
                && let Err(error) = destination.verify(variant)
            {
                result = Ok(Outcome::Failed(format!("{error:#}")));
            }
            destination.remove()?;
            let outcome = result?;
            if let Outcome::Skipped(reason) = &outcome {
                skip_reason = Some(reason.clone());
            }
            report(&name, &outcome, Some(elapsed));
            outcomes.push(outcome);
        }
    }
    Ok(outcomes)
}

/// Runs `ferry <command>` between the two sides, as a user would.
fn transfer(
    transfer_command: &str,
    source: &Side,
    destination: &Side,
    scenario: &Scenario,
    options: &TestOptions,
) -> Result<Outcome> {
    let mut command = Command::new(std::env::current_exe().context("find the ferry binary")?);
    command.arg(transfer_command).args(scenario.arguments);
    if scenario.dry_run {
        command.arg("--dry-run");
    }
    if options.remote.yes {
        command.arg("--yes");
    }
    if let Some(program) = options.remote.remote_ferry {
        command.arg("--remote-ferry").arg(program);
    }
    let output = command
        .arg(source.argument())
        .arg(destination.argument())
        .stdin(Stdio::null())
        .output()
        .context("run ferry")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not implemented yet") {
        return Ok(Outcome::Skipped(
            stderr.trim().trim_start_matches("ferry: ").to_owned(),
        ));
    }
    if !output.status.success() {
        return Ok(Outcome::Failed(format!(
            "ferry exited with {}:\n{stderr}",
            output.status
        )));
    }
    if stdout != scenario.expected {
        return Ok(Outcome::Failed(format!(
            "expected:\n{}got:\n{stdout}",
            scenario.expected
        )));
    }
    Ok(Outcome::Passed)
}

fn report(name: &str, outcome: &Outcome, elapsed: Option<f64>) {
    let time = elapsed
        .map(|seconds| format!(" ({seconds:.2}s)"))
        .unwrap_or_default();
    match outcome {
        Outcome::Passed => println!("  ok    {name}{time}"),
        Outcome::Skipped(reason) => println!("  skip  {name}: {reason}"),
        Outcome::Failed(details) => {
            println!("  FAIL  {name}{time}");
            for line in details.lines() {
                println!("          {line}");
            }
        }
    }
}

/// A name no other run uses at the same time.
fn run_name() -> String {
    let nanoseconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    format!("ferry-test-{:x}{:08x}", std::process::id(), nanoseconds)
}
