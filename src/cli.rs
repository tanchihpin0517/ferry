//! The `ferry` command.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::build_info;
use crate::dashboard;
use crate::interrupt;
use crate::limits;
use crate::peer;
use crate::prepare::{self, RemoteOptions};
use crate::profile;
use crate::progress;
use crate::protocol::Hello;
use crate::relay;
use crate::remote::Location;
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

    /// Count mtimes up to SECONDS apart as the same. 0, the default, compares
    /// whole seconds; -1 compares nanoseconds too.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 0,
        allow_negative_numbers = true
    )]
    modify_window: i64,

    /// Keep owners' numeric user and group IDs, instead of mapping them by
    /// name.
    #[arg(long)]
    numeric_ids: bool,

    /// Do not fsync files and directories before finishing. Faster, but a power
    /// loss soon after may lose what was copied.
    #[arg(long)]
    no_fsync: bool,

    /// Do not compress what is sent. Saves CPU on a fast network.
    #[arg(long, conflicts_with = "compress_level")]
    no_compress: bool,

    /// Compress at this zstd level, from 1 to 19.
    #[arg(
        long,
        value_name = "LEVEL",
        default_value_t = 1,
        value_parser = clap::value_parser!(i32).range(1..=19)
    )]
    compress_level: i32,

    /// Open N SSH connections to each host, each its own TCP connection, and
    /// spread the transfer over them. Each may need its own login.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=64))]
    ssh_connections: u16,

    /// Run this Ferry binary on the host, instead of the build Ferry installs.
    #[arg(long, value_name = "PATH")]
    remote_ferry: Option<String>,

    /// Agree without asking: to install Ferry on a host, and, for `copy`, to
    /// copy a file into the directory DESTINATION is.
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

    /// Have the paths to a host join its server by running Ferry there,
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

    /// Cut the last path to the Destination short before its first piece,
    /// as a dropped SSH connection would: for `ferry test`.
    #[arg(long, hide = true)]
    cut_a_path: bool,

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

        /// Have the paths to hosts join their servers through the shell, in
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
        /// Instead, join the server waiting under this token, handing it this
        /// process's standard input and output as its path.
        #[arg(long)]
        join: Option<String>,

        /// The server is started through sudo, for --remote-sudo, and its
        /// paths join as the user who ran it; with --join, join such a server.
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
/// to the client in a packet, so it prints nothing itself. A path joining it
/// prints why it cannot.
fn serve(join: Option<&str>, sudo: bool) -> ExitCode {
    let served = match join {
        Some(token) => {
            transport::join(token, sudo).inspect_err(|error| eprintln!("ferry: {error:#}"))
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
/// on this machine or on a host, and relays between the two.
fn transfer(cli: &TransferArgs, delete: bool) -> Result<ExitCode> {
    let source = Location::parse(&cli.source)?;
    let destination = Location::parse(&cli.destination)?;
    let remote_options = RemoteOptions {
        remote_ferry: cli.remote_ferry.as_deref(),
        yes: cli.yes,
        remote_join: cli.remote_join,
    };
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
    let hello = |role, root| Hello {
        build: build_info::identity(),
        role,
        root,
        dry_run: cli.dry_run,
        modify_window: cli.modify_window,
        numeric_ids: cli.numeric_ids,
        fsync: !cli.no_fsync,
        delete,
        compress: !cli.no_compress,
        compress_level: cli.compress_level,
        profile: !cli.no_profile,
        part_entries: cli.part_entries,
        backlog_files: cli.backlog_files,
        paced_bytes: cli.paced_bytes,
    };
    if cli.no_profile {
        profile::disable();
    }
    let prepare::Prepared {
        source,
        mut destination,
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
        },
        &hello,
    )?;
    if cli.cut_a_path {
        destination.connection.cut_a_path();
    }

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
        from: cli.source.to_string_lossy().into_owned(),
        to: shown_destination,
        monitors: [&source, &destination]
            .iter()
            .filter_map(|side| side.connection.monitor())
            .collect(),
        dry_run: cli.dry_run,
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
                progress::show(reports, shown, started).map(|counts| (counts, Vec::new()))
            }
        });
        let transferred = relay::relay(source, destination, &out, &profiles, &queues);
        // The display ends once nothing can send it a report.
        drop(out);
        let shown = display
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("internal error in the display")));
        (transferred, shown)
    });
    let (counts, said) = shown.context("show the progress")?;
    // Where the time went, after the summary, wherever it is shown.
    let mut sides = profiles.into_inner().expect("not poisoned");
    if profile::enabled() {
        sides.insert(0, ("the client".to_owned(), profile::collect()));
    }
    if show {
        eprintln!(
            "ferry: {}",
            progress::summary(&counts, started.elapsed(), cli.dry_run)
        );
        if !sides.is_empty() {
            eprint!("ferry: where the time went\n{}", profile::render(&sides));
        }
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
    transferred?;

    if counts.problems > 0 {
        eprintln!(
            "ferry: {} path{} in {} could not be read",
            counts.problems,
            if counts.problems == 1 { "" } else { "s" },
            cli.source.to_string_lossy(),
        );
    }
    // Gone as they were listed: not to copy, nor a failure.
    if counts.vanished > 0 {
        eprintln!(
            "ferry: warning: {} path{} in {} vanished before {} could be read",
            counts.vanished,
            if counts.vanished == 1 { "" } else { "s" },
            cli.source.to_string_lossy(),
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
    if counts.problems > 0 || counts.failed > 0 {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
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
        assert!(parse(&["--profile"]).is_err());
    }
}
