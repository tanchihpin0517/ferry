//! `ferry test`: the whole flow between two places, on trees it creates and
//! then removes.
//!
//! Each scenario puts a known tree at the destination, runs the real `ferry`
//! command between the two places, and checks what it prints and what the
//! destination holds afterwards. The scenarios are in two phases, as a run
//! is: preparing, whose scenarios fail before the transfer and must leave
//! the destination as it was, and transferring. The scenarios grow as Ferry
//! does.

pub mod fixture;
pub mod generate;

use std::cell::OnceCell;
use std::ffi::OsString;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};

use crate::remote::{self, Location, RemoteOptions};
use crate::transport::Transport;
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

/// What `sync` lists, beyond `copy`, for the tree with extra entries.
const EXTRAS: &str = "deleting extra.txt
deleting dir/extra-dir/
";

/// What one command must do in a scenario.
#[derive(Debug, Clone, Copy)]
struct Expect {
    /// Whether the transfer must fail, rather than succeed.
    fails: bool,
    /// What the transfer prints.
    prints: &'static str,
    /// What its errors must include, if they are checked.
    says: Option<&'static str>,
    /// The tree the destination must hold afterwards, if it is checked.
    leaves: Option<Variant>,
}

/// Succeeds, printing `text`.
const fn prints(text: &'static str) -> Expect {
    Expect {
        fails: false,
        prints: text,
        says: None,
        leaves: None,
    }
}

/// Succeeds, printing nothing, and leaves `variant`.
const fn leaves(variant: Variant) -> Expect {
    Expect {
        fails: false,
        prints: "",
        says: None,
        leaves: Some(variant),
    }
}

/// Fails while preparing, saying `error`, and leaves the destination's
/// [`UNTOUCHED`] tree as it was.
const fn refused(error: &'static str) -> Expect {
    Expect {
        fails: true,
        prints: "",
        says: Some(error),
        leaves: Some(UNTOUCHED),
    }
}

/// The tree a scenario that fails while preparing puts at the destination.
/// It differs from the source's, so that a transfer that went ahead, by
/// either command, would change it.
const UNTOUCHED: Variant = Variant::Changed;

/// Which source a scenario transfers from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum From {
    /// The source tree.
    Tree,
    /// A path beside it that does not exist.
    Missing,
}

/// A destination tree, a transfer to it, and what each command must do.
struct Scenario {
    name: &'static str,
    from: From,
    /// The tree at the destination, or none.
    destination: Option<Variant>,
    arguments: &'static [&'static str],
    dry_run: bool,
    /// Runs only when a side is on a host, whose server `--remote-ferry` names.
    needs_host: bool,
    copy: Expect,
    sync: Expect,
}

impl Scenario {
    fn expect(&self, command: &str) -> Expect {
        if command == "sync" {
            self.sync
        } else {
            self.copy
        }
    }
}

/// Phase 1: scenarios that fail while preparing.
const PREPARING: &[Scenario] = &[
    Scenario {
        name: "a missing source fails before anything changes",
        from: From::Missing,
        destination: Some(UNTOUCHED),
        arguments: &[],
        dry_run: false,
        needs_host: false,
        copy: refused("does not exist"),
        sync: refused("does not exist"),
    },
    Scenario {
        name: "a server that does not start fails before anything changes",
        from: From::Tree,
        destination: Some(UNTOUCHED),
        arguments: &["--remote-ferry", "/nonexistent/ferry"],
        dry_run: false,
        needs_host: true,
        copy: refused("did not start"),
        sync: refused("did not start"),
    },
];

/// Phase 2: scenarios that transfer.
const TRANSFERRING: &[Scenario] = &[
    Scenario {
        name: "a missing destination needs everything",
        from: From::Tree,
        destination: None,
        arguments: &[],
        dry_run: true,
        needs_host: false,
        copy: prints(EVERYTHING),
        sync: prints(EVERYTHING),
    },
    Scenario {
        name: "an identical destination needs nothing",
        from: From::Tree,
        destination: Some(Variant::Base),
        arguments: &[],
        dry_run: true,
        needs_host: false,
        copy: prints(""),
        sync: prints(""),
    },
    Scenario {
        name: "a changed destination needs what changed",
        from: From::Tree,
        destination: Some(Variant::Changed),
        arguments: &[],
        dry_run: true,
        needs_host: false,
        copy: prints(CHANGED),
        sync: prints(CHANGED),
    },
    Scenario {
        name: "mtimes in the same second are equal",
        from: From::Tree,
        destination: Some(Variant::Nanoseconds),
        arguments: &[],
        dry_run: true,
        needs_host: false,
        copy: prints(""),
        sync: prints(""),
    },
    Scenario {
        name: "--modify-window=-1 compares nanoseconds",
        from: From::Tree,
        destination: Some(Variant::Nanoseconds),
        arguments: &["--modify-window=-1"],
        dry_run: true,
        needs_host: false,
        copy: prints("file.txt\n"),
        sync: prints("file.txt\n"),
    },
    Scenario {
        name: "other permissions need their attributes",
        from: From::Tree,
        destination: Some(Variant::Modes),
        arguments: &[],
        dry_run: true,
        needs_host: false,
        copy: prints("file.txt\ndir/deeper/\n"),
        sync: prints("file.txt\ndir/deeper/\n"),
    },
    Scenario {
        name: "extra entries are listed for deleting by sync only",
        from: From::Tree,
        destination: Some(Variant::Extra),
        arguments: &[],
        dry_run: true,
        needs_host: false,
        copy: prints(""),
        sync: prints(EXTRAS),
    },
    Scenario {
        name: "a missing destination receives the tree",
        from: From::Tree,
        destination: None,
        arguments: &[],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "a changed destination is restored",
        from: From::Tree,
        destination: Some(Variant::Changed),
        arguments: &[],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "without fsync, the tree is received",
        from: From::Tree,
        destination: None,
        arguments: &["--no-fsync"],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "without compression, the tree is received",
        from: From::Tree,
        destination: None,
        arguments: &["--no-compress"],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "at a fixed compression level, the tree is received",
        from: From::Tree,
        destination: None,
        arguments: &["--compress-level", "19"],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "with progress shown, the tree is received",
        from: From::Tree,
        destination: None,
        arguments: &["--progress"],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "with --no-ui, the tree is received",
        from: From::Tree,
        destination: None,
        arguments: &["--no-ui"],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "extra entries are kept by copy and deleted by sync",
        from: From::Tree,
        destination: Some(Variant::Extra),
        arguments: &[],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Extra),
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "a directory in the way fails one file, unless syncing",
        from: From::Tree,
        destination: Some(Variant::Blocked),
        arguments: &[],
        dry_run: false,
        needs_host: false,
        copy: Expect {
            fails: true,
            prints: "",
            says: None,
            leaves: Some(Variant::BlockedAfter),
        },
        sync: leaves(Variant::Base),
    },
    Scenario {
        name: "a path cut short fails the transfer, not hangs",
        from: From::Tree,
        // A fixture, so that what is left of it can be removed.
        destination: Some(Variant::Changed),
        arguments: &["--cut-a-path"],
        dry_run: false,
        needs_host: false,
        copy: Expect {
            fails: true,
            prints: "",
            says: Some("ended before its end"),
            leaves: None,
        },
        sync: Expect {
            fails: true,
            prints: "",
            says: Some("ended before its end"),
            leaves: None,
        },
    },
    Scenario {
        name: "other permissions are restored",
        from: From::Tree,
        destination: Some(Variant::Modes),
        arguments: &[],
        dry_run: false,
        needs_host: false,
        copy: leaves(Variant::Base),
        sync: leaves(Variant::Base),
    },
];

pub struct TestOptions<'a> {
    pub remote: RemoteOptions<'a>,
    /// How many SSH connections each transfer opens to a host.
    pub ssh_connections: usize,
}

/// One side of the test: a fixture's root in a local or remote directory.
struct Side<'a> {
    location: Location,
    options: &'a TestOptions<'a>,
    /// On a host, the transport to it and the Ferry program there, once
    /// ready.
    reach: OnceCell<(Transport, String)>,
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
        Ok(Side {
            location,
            options,
            reach: OnceCell::new(),
        })
    }

    /// Gets the host ready, if this side is on one: logs in, and installs
    /// this build if it is missing.
    fn ready(&self) -> Result<()> {
        if self.reach.get().is_some() || matches!(self.location, Location::Local(_)) {
            return Ok(());
        }
        let transport = Transport::open(&self.location, self.options.ssh_connections)?;
        let program = remote::program(&transport, &self.options.remote)?;
        let _ = self.reach.set((transport, program));
        Ok(())
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
        self.ready()?;
        let (transport, program) = self.reach.get().expect("ready");
        let output = transport
            .command(&remote::script(program, &arguments))
            .stdin(Stdio::null())
            .output()
            .context("reach the host")?;
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

/// The variant's name on the command line, as clap spells it.
fn variant_name(variant: Variant) -> String {
    use clap::ValueEnum;
    variant
        .to_possible_value()
        .expect("every variant has a name")
        .get_name()
        .to_owned()
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

/// The places a run uses: the source tree, a missing path beside it, and the
/// destination.
struct Places<'a> {
    source: Side<'a>,
    missing: Side<'a>,
    destination: Side<'a>,
}

pub fn run(source: Location, destination: Location, options: &TestOptions) -> Result<ExitCode> {
    let run = run_name();
    let places = Places {
        source: Side::new(source.clone(), &format!("{run}-source"), options)?,
        missing: Side::new(source, &format!("{run}-missing"), options)?,
        destination: Side::new(destination, &format!("{run}-destination"), options)?,
    };
    println!(
        "ferry test: from {} to {}",
        places.source.argument().to_string_lossy(),
        places.destination.argument().to_string_lossy()
    );

    let mut outcomes = Vec::new();
    // Once a command is skipped as not built yet, its later scenarios are
    // skipped without running, in either phase.
    let mut skipped: Vec<(&str, String)> = Vec::new();

    println!("phase 1: prepare");
    // Installing on a host happens here, where its output shows, rather than
    // inside the first scenario, where it would be captured and timed.
    let mut hosts: Vec<&str> = Vec::new();
    for side in [&places.source, &places.destination] {
        let Location::Remote { host, .. } = &side.location else {
            continue;
        };
        let started = Instant::now();
        let ready = side.ready();
        if hosts.contains(&host.as_str()) && ready.is_ok() {
            continue;
        }
        hosts.push(host);
        let outcome = match ready {
            Ok(()) => Outcome::Passed,
            Err(error) => Outcome::Failed(format!("{error:#}")),
        };
        report(
            &format!("{host}: ready"),
            &outcome,
            Some(started.elapsed().as_secs_f64()),
        );
        outcomes.push(outcome);
    }
    let ready = !outcomes
        .iter()
        .any(|outcome| matches!(outcome, Outcome::Failed(_)));
    if ready {
        places.source.create(Variant::Base)?;
        let phases =
            scenarios(PREPARING, &places, options, &mut skipped, &mut outcomes).and_then(|()| {
                println!("phase 2: transfer");
                if outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, Outcome::Failed(_)))
                {
                    skip_all(TRANSFERRING, "preparing failed", &mut outcomes);
                    return Ok(());
                }
                scenarios(TRANSFERRING, &places, options, &mut skipped, &mut outcomes)
            });
        let cleanup = places.source.remove();
        phases?;
        cleanup?;
    } else {
        skip_all(PREPARING, "a host is not ready", &mut outcomes);
        println!("phase 2: transfer");
        skip_all(TRANSFERRING, "a host is not ready", &mut outcomes);
    }

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

/// Skips a phase's scenarios, with every command, on one line.
fn skip_all(phase: &[Scenario], reason: &str, outcomes: &mut Vec<Outcome>) {
    println!("  skip  every scenario: {reason}");
    for _ in 0..phase.len() * COMMANDS.len() {
        outcomes.push(Outcome::Skipped(reason.to_owned()));
    }
}

/// Runs a phase's scenarios with every command, leaving no destination tree
/// behind.
fn scenarios(
    phase: &[Scenario],
    places: &Places,
    options: &TestOptions,
    skipped: &mut Vec<(&'static str, String)>,
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    let has_host = [&places.source, &places.destination]
        .iter()
        .any(|side| matches!(side.location, Location::Remote { .. }));
    for &command in COMMANDS {
        for scenario in phase {
            let name = format!("{command}: {}", scenario.name);
            let reason = skipped
                .iter()
                .find(|(skipped, _)| *skipped == command)
                .map(|(_, reason)| reason.clone())
                .or_else(|| {
                    (scenario.needs_host && !has_host).then(|| {
                        "both sides are on this machine, so no server runs on a host".to_owned()
                    })
                });
            if let Some(reason) = reason {
                report(&name, &Outcome::Skipped(reason.clone()), None);
                outcomes.push(Outcome::Skipped(reason));
                continue;
            }
            let destination = &places.destination;
            if let Some(variant) = scenario.destination {
                destination.create(variant)?;
            }
            let source = match scenario.from {
                From::Tree => &places.source,
                From::Missing => &places.missing,
            };
            let started = Instant::now();
            let mut result = transfer(command, source, destination, scenario, options);
            let elapsed = started.elapsed().as_secs_f64();
            if let (Ok(Outcome::Passed), Some(variant)) = (&result, scenario.expect(command).leaves)
                && let Err(error) = destination.verify(variant)
            {
                result = Ok(Outcome::Failed(format!("{error:#}")));
            }
            destination.remove()?;
            let outcome = result?;
            if let Outcome::Skipped(reason) = &outcome {
                skipped.push((command, reason.clone()));
            }
            report(&name, &outcome, Some(elapsed));
            outcomes.push(outcome);
        }
    }
    Ok(())
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
    crate::interrupt::unblock_in_child(&mut command);
    command.arg(transfer_command).args(scenario.arguments);
    if scenario.dry_run {
        command.arg("--dry-run");
    }
    if options.remote.yes {
        command.arg("--yes");
    }
    command
        .arg("--ssh-connections")
        .arg(options.ssh_connections.to_string());
    // A scenario's own `--remote-ferry` wins: it may be given only once.
    if let Some(program) = options.remote.remote_ferry
        && !scenario.arguments.contains(&"--remote-ferry")
    {
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
    let expect = scenario.expect(transfer_command);
    if output.status.success() == expect.fails {
        return Ok(Outcome::Failed(format!(
            "ferry exited with {}, which was not expected:\n{stderr}",
            output.status
        )));
    }
    if let Some(error) = expect.says
        && !stderr.contains(error)
    {
        return Ok(Outcome::Failed(format!(
            "expected an error saying \"{error}\", got:\n{stderr}"
        )));
    }
    if stdout != expect.prints {
        return Ok(Outcome::Failed(format!(
            "expected:\n{}got:\n{stdout}",
            expect.prints
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
