//! The `ferry` command.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::build_info;
use crate::dashboard;
use crate::follow;
use crate::interrupt;
use crate::limits;
use crate::peer;
use crate::prepare::{self, RemoteOptions};
use crate::profile;
use crate::progress;
use crate::protocol::Hello;
use crate::remote::Location;
use crate::report::Counts;
use crate::source;
use crate::stop;
use crate::testkit::{
    self, TestOptions,
    fixture::{self, Checks, Variant},
    generate,
};
use crate::transport;
use crate::update;

/// A fast replacement for rsync.
#[derive(Debug, Parser)]
#[command(name = "ferry", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// What `copy` and `sync` take.
#[derive(Debug, Args)]
struct TransferArgs {
    /// What to copy.
    #[arg(value_name = "SOURCE")]
    source: OsString,

    /// Where to copy it.
    #[arg(value_name = "DESTINATION")]
    destination: OsString,

    /// List what would be transferred, without copying anything.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// With --dry-run, list each entry as `+ PATH` or, to delete, `- PATH`,
    /// its bytes as they are, ending in a NUL rather than a newline: for a
    /// script.
    #[arg(short = '0', long, requires = "dry_run")]
    null: bool,

    /// Count mtimes up to SECONDS apart as the same. 0, the default, compares
    /// whole seconds; -1 compares nanoseconds too.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 0,
        allow_negative_numbers = true,
        value_parser = parse_modify_window
    )]
    modify_window: i64,

    /// Keep owners' numeric user and group IDs, instead of mapping them by
    /// name.
    #[arg(long)]
    numeric_ids: bool,

    /// Stay on the filesystem each root is on: go into no directory on
    /// another, a mount point, to copy what it holds, nor, for `sync`, to
    /// delete in it. The mount point itself is copied, as a directory.
    #[arg(short = 'x', long)]
    one_file_system: bool,

    /// fsync what is written, so that a power loss soon after loses nothing
    /// copied. Not built yet: given, it is refused.
    #[arg(long)]
    fsync: bool,

    /// Does nothing, as nothing is fsynced: taken so that a command line
    /// written for Ferry 0.16 or before, which synced unless given this,
    /// still runs.
    #[arg(long, hide = true)]
    no_fsync: bool,

    /// Do not compress what is sent. Saves CPU on a fast network.
    #[arg(long, conflicts_with = "compress_level")]
    no_compress: bool,

    /// Compress at this zstd level, from 1 to 19; 1 unless given. Given, it
    /// compresses where both sides are on this machine too, which otherwise
    /// send as it is.
    #[arg(
        long,
        value_name = "LEVEL",
        value_parser = clap::value_parser!(i32).range(1..=19)
    )]
    compress_level: Option<i32>,

    /// Open N SSH connections to each host, each its own TCP connection, and
    /// spread the transfer over them. Each may need its own login.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=64))]
    ssh_connections: u16,

    /// Run on at most N of each machine's cores, the client's and each
    /// host's, or on all of them. On Linux only; elsewhere every core is
    /// used.
    #[arg(long, value_name = "N|all", default_value = "8", value_parser = parse_cpus)]
    cpus: Cpus,

    /// Run this Ferry binary on the host, instead of the build Ferry installs.
    #[arg(long, value_name = "PATH")]
    remote_ferry: Option<String>,

    /// Agree without asking to install Ferry on a host.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Sync into DESTINATION without asking, even when it is /, a home
    /// directory, or a directory that holds one, such as `host:` alone.
    /// --yes does not agree to this.
    #[arg(long)]
    allow_dangerous_destination: bool,

    /// Run Ferry on a host as root, through sudo, so that it can keep every
    /// owner. A password sudo needs is asked for once, or taken from
    /// FERRY_SUDO_PASSWORD.
    #[arg(long)]
    remote_sudo: bool,

    /// Have the streams to a host join its server by running Ferry there,
    /// through the shell on the host, rather than reach it through SSH: for
    /// a host whose sshd will not forward to a Unix socket.
    #[arg(long)]
    remote_join: bool,

    /// Show progress, and a summary at the end, even when standard error is
    /// not a terminal. On a terminal, they are shown anyway.
    #[arg(long)]
    progress: bool,

    /// Show the progress line instead of the dashboard, on a terminal.
    #[arg(long)]
    no_ui: bool,

    /// Also write where the threads of the client and of each server spent
    /// their time to FILE, as JSON, with each state's histogram.
    #[arg(long, value_name = "FILE")]
    profile_file: Option<PathBuf>,

    /// After the summary, say where each side's threads spent their time:
    /// which side, and which stage, held the transfer back.
    #[arg(long, conflicts_with = "no_profile")]
    profile: bool,

    /// Cut the last stream short before anything goes on it, as a dropped
    /// SSH connection would, for it to be made again: for `ferry test`.
    #[arg(long, hide = true)]
    cut_a_stream: bool,

    /// Send at most N entries of a directory in a manifest part, rather
    /// than 10,000: for tests of a directory in several parts, which then
    /// need few files.
    #[arg(long, hide = true, value_name = "N", default_value_t = 0)]
    part_entries: u32,

    /// Have the Source's scan wait while more than N needed files wait to
    /// be sent, rather than 100,000: for tests of that wait, which then
    /// need few files.
    #[arg(long, hide = true, value_name = "N", default_value_t = 0)]
    backlog_files: u32,

    /// Have the Source's manifest wait for room once N bytes of messages
    /// wait to be written, rather than 8 MiB: for tests in which the scan
    /// should not run far ahead, which then need few files.
    #[arg(long, hide = true, value_name = "N", default_value_t = 0)]
    paced_bytes: u32,

    /// Have the Source's scan wait, before a directory, while more than N
    /// entries it sent are not compared by the Destination yet, rather than
    /// 262,144: for tests of that wait, which then need few files.
    #[arg(long, hide = true, value_name = "N", default_value_t = 0)]
    in_flight_entries: u32,

    /// Have the Destination give the Source a data credit of N bytes,
    /// rather than 64 MiB: for measuring what the credit does.
    #[arg(long, hide = true, value_name = "N", default_value_t = 0)]
    data_credit: u64,

    /// Do not time where the threads' time goes, on the client or the
    /// servers: for measuring what that costs.
    #[arg(long, hide = true, conflicts_with = "profile_file")]
    no_profile: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Copy SOURCE into DESTINATION, keeping anything else DESTINATION has.
    ///
    /// SOURCE's contents go into DESTINATION, whether or not SOURCE ends with a
    /// slash: from `data` to `backup`, `data/x` becomes `backup/x`. A location
    /// is a local path or `[user@]host:path`.
    Copy(TransferArgs),

    /// Make DESTINATION the same as SOURCE, deleting anything else it has.
    ///
    /// As `copy`, and whatever DESTINATION has that SOURCE does not is deleted,
    /// a directory at a time, as the transfer goes. What SOURCE has but could
    /// not read is left as it is in DESTINATION, never deleted.
    Sync(TransferArgs),

    /// Run the whole flow from SOURCE to DESTINATION on a tree it creates, then
    /// remove the tree.
    ///
    /// SOURCE and DESTINATION are directories, local or `[user@]host:path`.
    /// The test creates `ferry-test-*-source` and `ferry-test-*-destination`
    /// inside them, and removes only those.
    ///
    /// With --scale and --kind, it instead generates a tree of SIZE, of KIND,
    /// copies it, changes it, and syncs it, as a user would, with the
    /// dashboard on a terminal; after each transfer the destination must
    /// match the source.
    Test {
        /// The directory to create the source tree in.
        #[arg(value_name = "SOURCE")]
        source: OsString,

        /// The directory to create destination trees in.
        #[arg(value_name = "DESTINATION")]
        destination: OsString,

        /// Open N SSH connections to each host, in every transfer.
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=64))]
        ssh_connections: u16,

        /// Run this Ferry binary on hosts, instead of the build Ferry installs.
        #[arg(long, value_name = "PATH")]
        remote_ferry: Option<String>,

        /// Have the streams to hosts join their servers through the shell, in
        /// every transfer.
        #[arg(long)]
        remote_join: bool,

        /// Install Ferry on a host without asking.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Transfer a generated tree of SIZE, such as 500M or 20G, instead of
        /// running the scenarios.
        #[arg(long, value_name = "SIZE", value_parser = parse_size, requires = "kind")]
        scale: Option<u64>,

        /// What the tree of --scale is shaped as: small files, large files,
        /// or both, as a project is.
        #[arg(long, value_enum, requires = "scale")]
        kind: Option<generate::Kind>,
    },

    /// Replace this binary with the latest release, or VERSION.
    ///
    /// The release for this machine is downloaded from GitHub, and checked
    /// against its SHA256SUMS. A dev build, built from source, cannot update
    /// itself.
    Update {
        /// Only say whether a newer release is out: exit 2 if one is, 0 if
        /// not.
        #[arg(long, conflicts_with = "version")]
        check: bool,

        /// Update to this version instead, older or newer, such as 0.1.0.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },

    /// Serve one side of a transfer: shake hands on standard input and
    /// output, the control connection, then run the peer over the path that
    /// joins. The client runs this through its transport.
    #[command(hide = true)]
    Server {
        /// Instead, join the server waiting at this socket, handing it this
        /// process's standard input and output as its path.
        #[arg(long)]
        join: Option<String>,

        /// The server is started through sudo, for --remote-sudo, and its
        /// streams join as the user who ran it; with --join, join such a
        /// server.
        #[arg(long)]
        sudo: bool,
    },

    /// Print this build's identity, which the handshake compares: a
    /// release's version, or a dev build's source hash. Installing on a host
    /// runs this, to check the binary runs there and is the build it is
    /// named as.
    #[command(hide = true)]
    BuildId,

    /// Create or remove a test tree. `ferry test` runs this on hosts.
    #[command(hide = true)]
    Fixture {
        /// Stop once standard input closes: for `ferry test` on a host, so
        /// that the action stops when the SSH connection running it does.
        #[arg(long)]
        until_stdin_closes: bool,

        #[command(subcommand)]
        action: FixtureAction,
    },
}

#[derive(Debug, Subcommand)]
enum FixtureAction {
    /// Generate a tree of KIND, of SIZE bytes in its files, at ROOT, which
    /// must not exist: a fixture that `change` changes.
    Generate {
        #[arg(long, value_enum)]
        kind: generate::Kind,
        #[arg(long, value_parser = parse_size)]
        size: u64,
        root: PathBuf,
    },
    /// Change a tree generated with --size, as a user would between syncs.
    Change { root: PathBuf },
    /// Print a digest of everything `verify` compares in the tree at ROOT.
    Digest { root: PathBuf },
    /// Create the tree at ROOT, which must not exist.
    Create {
        #[arg(long, value_enum, default_value_t = Variant::Base)]
        variant: Variant,
        root: PathBuf,
    },
    /// Create ROOT, which must not exist, with only the fixture's marker.
    Mark { root: PathBuf },
    /// Remove the tree at ROOT, which must be a fixture.
    Remove { root: PathBuf },
    /// Check that the tree at ROOT is VARIANT.
    Verify {
        #[arg(long, value_enum, default_value_t = Variant::Base)]
        variant: Variant,
        /// Compare only regular files and directories.
        #[arg(long)]
        files_and_directories_only: bool,
        /// Leave directories' mtimes out.
        #[arg(long)]
        ignore_directory_mtimes: bool,
        root: PathBuf,
    },
}

pub fn run() -> Result<ExitCode> {
    // While this is the only thread.
    crate::remote::take_password_from_environment();
    interrupt::install();
    // A panic is a bug in Ferry: said, by the role of the thread it came in,
    // the temporary files removed, and the process ended at once, whatever
    // other threads were doing. A server tells its client, which shows it;
    // one that cannot, and the client itself, say it on standard error.
    std::panic::set_hook(Box::new(|info| {
        // The first panic says so and ends the process; any other, at once
        // in another thread, waits for it.
        static PANICKED: AtomicBool = AtomicBool::new(false);
        if PANICKED.swap(true, Ordering::SeqCst) {
            loop {
                std::thread::park();
            }
        }
        let role = profile::current_role().unwrap_or("main");
        let payload = info.payload();
        let what = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("a panic");
        let message = match info.location() {
            Some(at) => format!("internal error in {role}: {what}, at {at}"),
            None => format!("internal error in {role}: {what}"),
        };
        if !peer::tell_internal(&message) {
            eprintln!("ferry: {message}");
        }
        interrupt::exit_now(stop::INTERNAL.into());
    }));
    limits::raise();
    let cli = Cli::parse();
    match &cli.command {
        Commands::Copy(arguments) => transfer(arguments, false),
        Commands::Sync(arguments) => transfer(arguments, true),
        Commands::Server { join, sudo } => Ok(serve(join.as_deref(), *sudo)),
        Commands::Update { check, version } => update::run(*check, version.as_deref()),
        Commands::BuildId => {
            println!("{}", build_info::identity());
            Ok(ExitCode::SUCCESS)
        }
        Commands::Test {
            source,
            destination,
            ssh_connections,
            remote_ferry,
            remote_join,
            yes,
            scale,
            kind,
        } => {
            let options = TestOptions {
                remote: RemoteOptions {
                    remote_ferry: remote_ferry.as_deref(),
                    yes: *yes,
                    remote_join: *remote_join,
                },
                ssh_connections: usize::from(*ssh_connections),
            };
            let (source, destination) = (Location::parse(source)?, Location::parse(destination)?);
            match (scale, kind) {
                (Some(bytes), Some(kind)) => {
                    testkit::run_at_scale(source, destination, *kind, *bytes, &options)
                }
                _ => testkit::run(source, destination, &options),
            }
        }
        Commands::Fixture {
            until_stdin_closes,
            action,
        } => {
            if *until_stdin_closes {
                std::thread::spawn(|| {
                    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
                    interrupt::stop(128 + libc::SIGHUP);
                });
            }
            match action {
                FixtureAction::Generate { kind, size, root } => {
                    println!("{}", generate::generate(root, *kind, *size)?)
                }
                FixtureAction::Mark { root } => fixture::mark(root)?,
                FixtureAction::Change { root } => println!("{}", generate::change(root)?),
                FixtureAction::Digest { root } => println!("{}", fixture::digest(root)?),
                FixtureAction::Create { variant, root } => fixture::create(root, *variant)?,
                FixtureAction::Remove { root } => fixture::remove(root)?,
                FixtureAction::Verify {
                    variant,
                    files_and_directories_only,
                    ignore_directory_mtimes,
                    root,
                } => {
                    let checks = Checks {
                        files_and_directories_only: *files_and_directories_only,
                        ignore_directory_mtimes: *ignore_directory_mtimes,
                    };
                    fixture::verify(root, *variant, checks)?
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The server writes nothing but packets to its output, and reports a failure
/// to the client in a packet, so it prints nothing itself. A stream joining
/// it prints why it cannot.
fn serve(join: Option<&str>, sudo: bool) -> ExitCode {
    let served = match join {
        Some(socket) => {
            transport::join(socket, sudo).inspect_err(|error| eprintln!("ferry: {error:#}"))
        }
        None => peer::serve(&build_info::identity(), sudo),
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// How many reports may wait for the display: a dry run's listing comes in
/// reports of up to 1 MiB, so at most 16 MiB of it.
const REPORTS: usize = 16;

/// Copies, or with `delete`, syncs. The client starts a server for each side,
/// on this machine or on a host, and makes the streams between the two.
fn transfer(cli: &TransferArgs, delete: bool) -> Result<ExitCode> {
    if cli.fsync {
        refuse(
            delete,
            "--fsync is not built yet: Ferry does not fsync what it writes",
        );
    }
    let source = Location::parse(&cli.source)?;
    let destination = Location::parse(&cli.destination)?;
    let remote_options = RemoteOptions {
        remote_ferry: cli.remote_ferry.as_deref(),
        yes: cli.yes,
        remote_join: cli.remote_join,
    };
    if matches!(
        (&source, &destination),
        (Location::Local(_), Location::Local(_))
    ) {
        warn_of_remote_options(cli);
    }
    // A local source is checked first, before any host is reached.
    if let Location::Local(source) = &source {
        source::check_root(source.as_os_str())?;
    }
    // A local server has this process's limit.
    let open_files = limits::open_files();
    if open_files < limits::LOW
        && (matches!(source, Location::Local(_)) || matches!(destination, Location::Local(_)))
    {
        eprintln!(
            "ferry: warning: at most {open_files} files may be open at once here, which `ulimit -Hn` sets; Ferry works within it, but more slowly"
        );
    }
    // Between two sides on this machine, nothing crosses a network: a copy
    // compresses only if a level is asked for.
    let both_here = matches!(
        (&source, &destination),
        (Location::Local(_), Location::Local(_))
    );
    let compress = !cli.no_compress && (cli.compress_level.is_some() || !both_here);
    // The two servers' streams join one session.
    let session = transport::streams::session()?;
    let hello = |role, root| Hello {
        build: build_info::identity(),
        role,
        root,
        dry_run: cli.dry_run,
        modify_window: cli.modify_window,
        numeric_ids: cli.numeric_ids,
        delete,
        one_file_system: cli.one_file_system,
        compress,
        compress_level: cli.compress_level.unwrap_or(1),
        profile: !cli.no_profile,
        part_entries: cli.part_entries,
        backlog_files: cli.backlog_files,
        paced_bytes: cli.paced_bytes,
        in_flight_entries: cli.in_flight_entries,
        data_credit: cli.data_credit,
        cpus: cli
            .cpus
            .0
            .map_or(0, |cpus| u32::try_from(cpus).unwrap_or(u32::MAX)),
        place: std::process::id(),
        session,
    };
    if cli.no_profile {
        profile::disable();
    }
    let prepare::Prepared {
        source,
        destination,
        streams,
        shown_destination,
    } = prepare::prepare(
        prepare::Request {
            source,
            destination,
            remote: remote_options,
            ssh_connections: usize::from(cli.ssh_connections),
            remote_sudo: cli.remote_sudo,
            delete,
            dry_run: cli.dry_run,
            allow_dangerous_destination: cli.allow_dangerous_destination,
            cut_a_stream: cli.cut_a_stream,
        },
        &hello,
    )?;
    // Each server keeps to its own, as its hello says, from all its host's.
    crate::cpus::limit(cli.cpus.0, std::process::id());

    let started = Instant::now();
    let show = progress::wanted(cli.progress);
    // A dry run's line says how far its comparison has got, which may list
    // nothing for a long time; its listing goes to standard output, the line
    // cleared before each part of it.
    let shown = match (show, cli.dry_run) {
        (false, _) => progress::Line::None,
        (true, false) => progress::Line::Transfer,
        (true, true) => progress::Line::DryRun,
    };
    // On a terminal, a run has the dashboard instead; a dry run's listing then
    // goes to standard output only where that is not the terminal.
    let dashboard = !cli.no_ui && std::io::stderr().is_terminal();
    let view = dashboard::View {
        from: crate::printable::path(cli.source.as_encoded_bytes()),
        to: shown_destination,
        monitors: [&source, &destination]
            .iter()
            .filter_map(|side| side.monitor.clone())
            .collect(),
        dry_run: cli.dry_run,
        null: cli.null,
    };
    // Bounded, so that a display that cannot keep up, as a dry run's listing
    // into a slow pipe, holds the transfer back rather than filling memory.
    let (out, reports) = crossbeam_channel::bounded(REPORTS);
    let profiles = Mutex::new(Vec::new());
    let queues = Mutex::new(Vec::new());
    let (transferred, shown) = std::thread::scope(|scope| {
        let display = scope.spawn(|| {
            if dashboard {
                dashboard::show(reports, &view, started)
            } else {
                progress::show(reports, shown, started, cli.null).map(|counts| (counts, Vec::new()))
            }
        });
        let transferred = follow::follow(source, destination, streams, &out, &profiles, &queues);
        // The display ends once nothing can send it a report.
        drop(out);
        let shown = display.join().unwrap_or_else(|_| {
            Err(progress::Failed {
                counts: Counts::default(),
                error: std::io::Error::other("internal error in the display"),
            })
        });
        (transferred, shown)
    });
    // A display that failed is said last: the transfer's result comes first.
    let (counts, said, unshown) = match shown {
        Ok((counts, said)) => (counts, said, None),
        Err(progress::Failed { counts, error }) => (counts, Vec::new(), Some(error)),
    };
    let mut sides = profiles.into_inner().expect("not poisoned");
    if profile::enabled() {
        sides.insert(0, ("the client".to_owned(), profile::collect()));
    }
    if show {
        eprintln!(
            "ferry: {}",
            progress::summary(&counts, started.elapsed(), cli.dry_run)
        );
    }
    // Where the time went, after the summary, when asked.
    if cli.profile && !sides.is_empty() {
        eprint!("ferry: where the time went\n{}", profile::render(&sides));
    }
    // What the dashboard kept, now that the terminal is back.
    for line in said {
        eprintln!("ferry: {line}");
    }
    if let Some(file) = &cli.profile_file {
        let queues = queues.into_inner().expect("not poisoned");
        std::fs::write(file, profile::json(started.elapsed(), &sides, &queues))
            .with_context(|| format!("write the profile to {}", file.display()))?;
    }
    if let Some(error) = &unshown {
        eprintln!("ferry: warning: could not show the progress: {error}");
    }
    transferred?;

    if counts.problems > 0 {
        eprintln!(
            "ferry: {} path{} in {} could not be read",
            counts.problems,
            if counts.problems == 1 { "" } else { "s" },
            crate::printable::path(cli.source.as_encoded_bytes()),
        );
    }
    // Gone as they were listed: not to copy, nor a failure.
    if counts.vanished > 0 {
        eprintln!(
            "ferry: warning: {} path{} in {} vanished before {} could be read",
            counts.vanished,
            if counts.vanished == 1 { "" } else { "s" },
            crate::printable::path(cli.source.as_encoded_bytes()),
            if counts.vanished == 1 { "it" } else { "they" },
        );
    }
    if counts.failed > 0 {
        eprintln!(
            "ferry: {} entr{} could not be copied",
            counts.failed,
            if counts.failed == 1 { "y" } else { "ies" },
        );
    }
    Ok(ExitCode::from(status(&counts)))
}

/// What a transfer that ran to its end exits with, by what it counted, as
/// rsync's partial transfers do: [`SOME_FAILED`] if some entry could not be
/// copied or deleted, or some path in SOURCE could not be read,
/// [`SOME_VANISHED`] if only some paths in SOURCE vanished as they were
/// read, and 0 otherwise.
fn status(counts: &Counts) -> u8 {
    if counts.failed > 0 || counts.problems > 0 {
        SOME_FAILED
    } else if counts.vanished > 0 {
        SOME_VANISHED
    } else {
        0
    }
}

/// Some entry could not be copied, or deleted, or some path in SOURCE could
/// not be read: rsync's partial transfer, which it exits with for a file it
/// could not open too.
pub const SOME_FAILED: u8 = 23;
/// Every entry was copied, but some paths in SOURCE vanished as they were
/// read: rsync's vanished source files.
pub const SOME_VANISHED: u8 = 24;

/// How many cores to run on: a number, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cpus(Option<usize>);

fn parse_cpus(text: &str) -> Result<Cpus, String> {
    if text == "all" {
        return Ok(Cpus(None));
    }
    match text.parse::<usize>() {
        Ok(cpus) if cpus > 0 => Ok(Cpus(Some(cpus))),
        _ => Err(format!("`{text}` is not a number of cores, nor `all`")),
    }
}

/// A modify window: 0 or more seconds, or -1, which compares nanoseconds;
/// no other negative number means anything.
fn parse_modify_window(text: &str) -> Result<i64, String> {
    let seconds: i64 = text
        .parse()
        .map_err(|_| format!("`{text}` is not a number of seconds"))?;
    if seconds < -1 {
        return Err(format!(
            "{seconds} is not a window: use -1 to compare nanoseconds"
        ));
    }
    Ok(seconds)
}

/// Warns of each option given for a host where both sides are on this
/// machine: it does nothing there, which a script may not expect.
/// Refuses an option that is taken but not built, as a wrong command line is,
/// rather than taking it and doing otherwise: says why, with the usage of
/// `copy`, or of `sync`, and exits with 2.
fn refuse(delete: bool, why: &str) -> ! {
    let mut command = Cli::command();
    command.build();
    let name = if delete { "sync" } else { "copy" };
    command
        .find_subcommand_mut(name)
        .expect("a transfer's subcommand")
        .error(ErrorKind::ArgumentConflict, why)
        .exit()
}

fn warn_of_remote_options(cli: &TransferArgs) {
    let given = [
        ("--remote-sudo", cli.remote_sudo),
        ("--remote-join", cli.remote_join),
        ("--remote-ferry", cli.remote_ferry.is_some()),
        ("--ssh-connections", cli.ssh_connections != 1),
    ];
    for (option, _) in given.iter().filter(|(_, given)| *given) {
        eprintln!("ferry: warning: {option} does nothing when both sides are on this machine");
    }
}

/// A size in bytes, such as 500M or 20G: a whole number, and a binary unit
/// of K, M, G, or T, with or without a trailing iB or B.
fn parse_size(text: &str) -> Result<u64, String> {
    let wrong = || format!("`{text}` is not a size, such as 500M or 20G");
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().map_err(|_| wrong())?;
    let unit = unit.to_ascii_uppercase();
    let unit = unit
        .strip_suffix("IB")
        .or_else(|| unit.strip_suffix('B'))
        .unwrap_or(&unit);
    let shift = match unit {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(wrong()),
    };
    let bytes = number
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("`{text}` is too large"))?;
    if bytes == 0 {
        return Err("a size must be more than 0".to_owned());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_in_binary_units() {
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size("500M"), Ok(500 << 20));
        assert_eq!(parse_size("20G"), Ok(20 << 30));
        assert_eq!(parse_size("20GiB"), Ok(20 << 30));
        assert_eq!(parse_size("2tb"), Ok(2 << 40));
        assert_eq!(parse_size("4k"), Ok(4 << 10));
        for wrong in ["", "G", "1.5G", "20X", "0", "0G", "-1G", "99999999999T"] {
            assert!(parse_size(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn scale_needs_a_size_and_a_kind() {
        let scale = |arguments: &[&str]| {
            let mut all = vec!["ferry", "test", "a", "b"];
            all.extend_from_slice(arguments);
            match Cli::try_parse_from(all)?.command {
                Commands::Test { scale, kind, .. } => Ok::<_, clap::Error>((scale, kind)),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(scale(&[]).unwrap(), (None, None));
        assert_eq!(
            scale(&["--scale", "1G", "--kind", "mixed"]).unwrap(),
            (Some(1 << 30), Some(generate::Kind::Mixed))
        );
        assert!(scale(&["--scale", "1G"]).is_err());
        assert!(scale(&["--kind", "small"]).is_err());
        assert!(scale(&["--scale", "--kind", "small"]).is_err());
    }

    fn parse(arguments: &[&str]) -> Result<TransferArgs, clap::Error> {
        let mut all = vec!["ferry", "copy"];
        all.extend_from_slice(arguments);
        all.extend(["a", "b"]);
        match Cli::try_parse_from(all)?.command {
            Commands::Copy(arguments) => Ok(arguments),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_profile_goes_to_a_file_if_asked_and_is_off_only_unasked_for() {
        assert_eq!(parse(&[]).unwrap().profile_file, None);
        assert_eq!(
            parse(&["--profile-file", "p.json"]).unwrap().profile_file,
            Some(PathBuf::from("p.json"))
        );
        assert!(parse(&["--no-profile"]).unwrap().no_profile);
        assert!(parse(&["--no-profile", "--profile-file", "p.json"]).is_err());
        // Where the time went is said only when asked, and never unprofiled.
        assert!(!parse(&[]).unwrap().profile);
        assert!(parse(&["--profile"]).unwrap().profile);
        assert!(parse(&["--no-profile", "--profile"]).is_err());
    }

    /// A modify window is 0 or more seconds, or -1; nothing below.
    #[test]
    fn a_modify_window_is_a_window_or_minus_one() {
        assert_eq!(parse(&["--modify-window", "2"]).unwrap().modify_window, 2);
        assert_eq!(parse(&["--modify-window", "-1"]).unwrap().modify_window, -1);
        let error = parse(&["--modify-window", "-5"]).unwrap_err().to_string();
        assert!(error.contains("use -1 to compare nanoseconds"), "{error}");
    }

    /// `update --check` exits with 3 for a newer release, never with 2,
    /// which is a command line that is wrong.
    #[test]
    fn a_wrong_command_line_and_a_newer_release_exit_apart() {
        let wrong = Cli::try_parse_from(["ferry", "copy", "--bogus", "a", "b"]).unwrap_err();
        assert_eq!(wrong.exit_code(), 2);
        assert_eq!(crate::update::NEWER, 3);
    }

    /// A transfer that ran to its end exits as rsync's partial transfers do:
    /// 23 for an entry that could not be copied, or a path in SOURCE that
    /// could not be read, whatever else; 24 for paths that vanished, alone.
    #[test]
    fn a_partial_transfer_exits_as_rsyncs_do() {
        let counted = |problems, vanished, failed| Counts {
            problems,
            vanished,
            failed,
            ..Counts::default()
        };
        assert_eq!(status(&counted(0, 0, 0)), 0);
        assert_eq!(status(&counted(0, 0, 1)), SOME_FAILED);
        assert_eq!(status(&counted(1, 1, 1)), SOME_FAILED);
        assert_eq!(status(&counted(1, 0, 0)), SOME_FAILED);
        assert_eq!(status(&counted(1, 1, 0)), SOME_FAILED);
        assert_eq!(status(&counted(0, 1, 0)), SOME_VANISHED);
    }
}
