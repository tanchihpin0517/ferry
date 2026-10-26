//! `ferry test --scale`: one transfer as a user would run it, on a tree of a
//! given size, with the dashboard on a terminal.
//!
//! A tree of that size is generated at the source and copied; the source is
//! then changed, as a user would between two syncs, and synced; and a dry run
//! must then find nothing more to do. After each transfer, the destination
//! must have the same digest as the source: every entry's kind, size,
//! permissions, mtime, and content. The steps stop at the first that fails,
//! and the trees are then kept, to look into.
//!
//! The first Ctrl-C stops the step running, and removes the trees; a second
//! exits at once, keeping them.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitCode, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::generate::Kind;
use super::{Outcome, Side, TestOptions, ready_hosts, report, run_name, value_name};
use crate::interrupt;
use crate::progress::size;
use crate::remote::Location;

/// The exit status of a test stopped by Ctrl-C, as for a process it ended.
const STOPPED: u8 = 128 + libc::SIGINT as u8;

pub fn run(
    source: Location,
    destination: Location,
    kind: Kind,
    bytes: u64,
    options: &TestOptions,
) -> Result<ExitCode> {
    interrupt::graceful(
        "ferry test: stopping, then removing the test trees; Ctrl-C again to stop at once",
    );
    let run = run_name();
    let source = Side::new(source, &format!("{run}-source"), options)?;
    let destination = Side::new(destination, &format!("{run}-destination"), options)?;
    println!(
        "ferry test: {} of {}, from {} to {}",
        size(bytes),
        value_name(kind),
        source.argument().to_string_lossy(),
        destination.argument().to_string_lossy()
    );

    let mut outcomes = Vec::new();
    ready_hosts(&[&source, &destination], &mut outcomes);
    if interrupt::stop_asked() {
        println!("stopped");
        return Ok(ExitCode::from(STOPPED));
    }
    let failed = |outcomes: &[Outcome]| {
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Failed(_)))
    };
    if failed(&outcomes) {
        return Ok(finish(&outcomes));
    }

    let steps = Steps {
        source: &source,
        destination: &destination,
        options,
        kind,
        bytes,
    };
    let mut stopped = false;
    for step in STEPS {
        if interrupt::stop_asked() {
            stopped = true;
            break;
        }
        let started = Instant::now();
        let result = (step.run)(&steps);
        let elapsed = started.elapsed().as_secs_f64();
        if interrupt::stop_asked() {
            println!("  stop  {} ({elapsed:.2}s)", step.name);
            stopped = true;
            break;
        }
        let (name, outcome) = match result {
            Ok(said) if said.is_empty() => (step.name.to_owned(), Outcome::Passed),
            Ok(said) => (format!("{}, {said}", step.name), Outcome::Passed),
            Err(error) => (step.name.to_owned(), Outcome::Failed(format!("{error:#}"))),
        };
        report(&name, &outcome, Some(elapsed));
        outcomes.push(outcome);
        if failed(&outcomes) {
            break;
        }
    }

    if stopped {
        println!("removing the test trees");
        remove(&destination)?;
        remove(&source)?;
        println!("{} passed, stopped", outcomes.len());
        return Ok(ExitCode::from(STOPPED));
    }
    if failed(&outcomes) {
        println!(
            "kept {} and {}, to look into",
            source.argument().to_string_lossy(),
            destination.argument().to_string_lossy()
        );
    } else {
        destination.remove()?;
        source.remove()?;
    }
    Ok(finish(&outcomes))
}

/// Removes a side's tree, after a stop. A server on a host may still be
/// finishing a file as it ends, so a removal that fails is tried once more.
fn remove(side: &Side) -> Result<()> {
    side.remove().or_else(|_| {
        std::thread::sleep(Duration::from_secs(1));
        side.remove()
    })
}

/// Says how many passed and failed, and gives the exit status for it.
fn finish(outcomes: &[Outcome]) -> ExitCode {
    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Outcome::Failed(_)))
        .count();
    println!("{} passed, {failed} failed", outcomes.len() - failed);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What every step works on.
struct Steps<'a> {
    source: &'a Side<'a>,
    destination: &'a Side<'a>,
    options: &'a TestOptions<'a>,
    kind: Kind,
    bytes: u64,
}

/// A step, and what it runs: what it says on success goes after its name.
struct Step {
    name: &'static str,
    run: fn(&Steps) -> Result<String>,
}

const STEPS: &[Step] = &[
    Step {
        name: "generate: the source tree",
        run: |steps| {
            // The destination is a fixture from the start, so that it can be
            // removed whatever a stopped transfer leaves in it.
            steps.destination.mark()?;
            steps.source.generate(steps.kind, steps.bytes)
        },
    },
    Step {
        name: "copy: the tree is received",
        run: |steps| {
            let started = Instant::now();
            steps.transfer("copy")?;
            let seconds = started.elapsed().as_secs_f64();
            Ok(format!(
                "at {}/s",
                size((steps.bytes as f64 / seconds.max(0.001)) as u64)
            ))
        },
    },
    Step {
        name: "copy: the destination matches the source",
        run: |steps| steps.same(),
    },
    Step {
        name: "change: the source",
        run: |steps| steps.source.change(),
    },
    Step {
        name: "sync: the changes are received",
        run: |steps| steps.transfer("sync").map(|()| String::new()),
    },
    Step {
        name: "sync: the destination matches the source",
        run: |steps| steps.same(),
    },
    Step {
        name: "sync: nothing more is needed",
        run: |steps| {
            let mut command = steps.ferry("sync")?;
            command
                .arg("--dry-run")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let listed = run_child(command, Child::wait_with_output)?;
            anyhow::ensure!(
                listed.status.success(),
                "ferry exited with {}:\n{}",
                listed.status,
                String::from_utf8_lossy(&listed.stderr)
            );
            anyhow::ensure!(
                listed.stdout.is_empty(),
                "a dry run still lists:\n{}",
                String::from_utf8_lossy(&listed.stdout)
            );
            Ok(String::new())
        },
    },
];

impl Steps<'_> {
    /// `ferry <command>` from the source to the destination, as a user would
    /// run it, with whatever `ferry test` was given for hosts.
    fn ferry(&self, transfer: &str) -> Result<Command> {
        let mut command = Command::new(std::env::current_exe().context("find the ferry binary")?);
        crate::interrupt::unblock_in_child(&mut command);
        command.arg(transfer);
        if self.options.remote.yes {
            command.arg("--yes");
        }
        if let Some(program) = self.options.remote.remote_ferry {
            command.arg("--remote-ferry").arg(program);
        }
        if self.options.remote.remote_join {
            command.arg("--remote-join");
        }
        let arguments: [OsString; 2] = [self.source.argument(), self.destination.argument()];
        command.args(arguments);
        Ok(command)
    }

    /// Runs `ferry <command>` on this terminal, where its dashboard shows.
    fn transfer(&self, transfer: &str) -> Result<()> {
        let status = run_child(self.ferry(transfer)?, |mut child| child.wait())?;
        anyhow::ensure!(status.success(), "ferry {transfer} exited with {status}");
        Ok(())
    }

    /// Checks that the destination has the source's digest.
    fn same(&self) -> Result<String> {
        let place = |side: &Side| match &side.location {
            Location::Local(_) => "this machine".to_owned(),
            Location::Remote { host, .. } => host.clone(),
        };
        let (from, to) = (place(self.source), place(self.destination));
        let _shown = crate::status::show(if from == to {
            format!("hashing both trees on {from}")
        } else {
            format!("hashing the trees on {from} and {to}")
        });
        // Both at once: each reads its whole tree.
        let (source, destination) = std::thread::scope(|scope| {
            let source = scope.spawn(|| self.source.digest());
            let destination = self.destination.digest();
            let source = source
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error in hashing")));
            (source, destination)
        });
        let (source, destination) = (source?, destination?);
        anyhow::ensure!(
            source == destination,
            "the trees differ:\n  source       {source}\n  destination  {destination}"
        );
        Ok(String::new())
    }
}

/// Runs a child to its end with `wait`, passing a stop on to it. A child that
/// Ctrl-C ended asks this process to stop too: the dashboard takes Ctrl-C as
/// a key, so only its own process sees it.
fn run_child<T: Exited>(
    mut command: Command,
    wait: impl FnOnce(Child) -> std::io::Result<T>,
) -> Result<T> {
    let child = command.spawn().context("run ferry")?;
    let ended = {
        let _passing = interrupt::pass_stop_to(child.id());
        wait(child).context("run ferry")?
    };
    let status = ended.status();
    if status.code() == Some(i32::from(STOPPED)) || status.signal() == Some(libc::SIGINT) {
        interrupt::ask_to_stop();
    }
    Ok(ended)
}

/// What a wait for a child gives: its exit status, and maybe its output.
trait Exited {
    fn status(&self) -> ExitStatus;
}

impl Exited for ExitStatus {
    fn status(&self) -> ExitStatus {
        *self
    }
}

impl Exited for Output {
    fn status(&self) -> ExitStatus {
        self.status
    }
}
