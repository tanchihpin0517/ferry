//! The `ferry` command.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::build_info;
use crate::dashboard;
use crate::interrupt;
use crate::peer;
use crate::profile;
use crate::progress;
use crate::protocol::{Hello, Role};
use crate::relay;
use crate::remote::{self, Location, RemoteOptions};
use crate::source;
use crate::testkit::{
    self, TestOptions,
    fixture::{self, Checks, Variant},
    generate,
};
use crate::transport::{self, Transport};
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

    /// Install Ferry on a host without asking.
    #[arg(short = 'y', long)]
    yes: bool,

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
    /// As `copy`, and then whatever DESTINATION has that SOURCE does not is
    /// deleted, once everything else has been copied. If anything failed, or
    /// part of SOURCE could not be read, nothing is deleted.
    Sync(TransferArgs),

    /// Run the whole flow from SOURCE to DESTINATION on a tree it creates, then
    /// remove the tree.
    ///
    /// SOURCE and DESTINATION are directories, local or `[user@]host:path`.
    /// The test creates `ferry-test-*-source` and `ferry-test-*-destination`
    /// inside them, and removes only those.
    ///
    /// With --scale, it instead generates a tree of SIZE, copies it, changes
    /// it, and syncs it, as a user would, with the dashboard on a terminal;
    /// after each transfer the destination must match the source.
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

        /// Install Ferry on a host without asking.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Transfer a generated tree of SIZE, such as 500M or 20G, instead of
        /// running the scenarios.
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        scale: Option<u64>,
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
    },

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
    /// Generate a tree for benchmarks at ROOT, which must not exist: of a
    /// KIND, or of SIZE bytes, a fixture that `change` changes.
    Generate {
        #[arg(
            long,
            value_enum,
            required_unless_present = "size",
            conflicts_with = "size"
        )]
        kind: Option<generate::Kind>,
        #[arg(long, value_parser = parse_size)]
        size: Option<u64>,
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
    let cli = Cli::parse();
    match &cli.command {
        Commands::Copy(arguments) => transfer(arguments, false),
        Commands::Sync(arguments) => transfer(arguments, true),
        Commands::Server { join } => Ok(serve(join.as_deref())),
        Commands::Update { check, version } => update::run(*check, version.as_deref()),
        Commands::Test {
            source,
            destination,
            ssh_connections,
            remote_ferry,
            yes,
            scale,
        } => {
            let options = TestOptions {
                remote: RemoteOptions {
                    remote_ferry: remote_ferry.as_deref(),
                    yes: *yes,
                },
                ssh_connections: usize::from(*ssh_connections),
            };
            let (source, destination) = (Location::parse(source)?, Location::parse(destination)?);
            match scale {
                Some(bytes) => testkit::run_at_scale(source, destination, *bytes, &options),
                None => testkit::run(source, destination, &options),
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
                FixtureAction::Generate {
                    kind: Some(kind),
                    root,
                    ..
                } => generate::generate(root, *kind)?,
                FixtureAction::Generate {
                    size: Some(bytes),
                    root,
                    ..
                } => println!("{}", generate::scaled(root, *bytes)?),
                FixtureAction::Generate { .. } => unreachable!("clap requires a kind or a size"),
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
fn serve(join: Option<&str>) -> ExitCode {
    let served = match join {
        Some(token) => transport::join(token).inspect_err(|error| eprintln!("ferry: {error:#}")),
        None => peer::serve(&build_info::identity()),
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Copies, or with `delete`, syncs. The client starts a server for each side,
/// on this machine or on a host, and relays between the two.
fn transfer(cli: &TransferArgs, delete: bool) -> Result<ExitCode> {
    let source = Location::parse(&cli.source)?;
    let destination = Location::parse(&cli.destination)?;
    let remote_options = RemoteOptions {
        remote_ferry: cli.remote_ferry.as_deref(),
        yes: cli.yes,
    };
    // A local source is checked first, before any host is reached.
    if let Location::Local(source) = &source {
        source::check_root(source.as_os_str())?;
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
    };
    if cli.no_profile {
        profile::disable();
    }
    // Each side's transport is opened first: for a host, that is logging in,
    // and the first thing that reaches it.
    let ssh_connections = usize::from(cli.ssh_connections);
    let source_transport = Transport::open(&source, ssh_connections)?;
    let destination_transport = Transport::open(&destination, ssh_connections)?;
    let source = start(
        source_transport,
        source,
        Role::Source,
        &hello,
        &remote_options,
    )?;
    let mut destination = start(
        destination_transport,
        destination,
        Role::Destination,
        &hello,
        &remote_options,
    )?;
    if cli.cut_a_path {
        destination.connection.cut_a_path();
    }

    let started = Instant::now();
    let show = progress::wanted(cli.progress);
    // A dry run's listing goes to standard output; a line redrawn on standard
    // error would tangle with it on a terminal.
    let progress_line = show && !cli.dry_run;
    // On a terminal, a transfer has the dashboard instead; a dry run, whose
    // listing is what it is for, does not.
    let dashboard = !cli.dry_run && !cli.no_ui && std::io::stderr().is_terminal();
    let view = dashboard::View {
        from: cli.source.to_string_lossy().into_owned(),
        to: cli.destination.to_string_lossy().into_owned(),
        monitors: [&source, &destination]
            .iter()
            .filter_map(|side| side.connection.monitor())
            .collect(),
    };
    let (out, reports) = mpsc::channel();
    let profiles = Mutex::new(Vec::new());
    let (transferred, shown) = std::thread::scope(|scope| {
        let display = scope.spawn(|| {
            if dashboard {
                dashboard::show(reports, &view, started)
            } else {
                progress::show(reports, progress_line, started).map(|counts| (counts, Vec::new()))
            }
        });
        let transferred = relay::relay(source, destination, &out, &profiles);
        // The display ends once nothing can send it a report.
        drop(out);
        let shown = display.join().expect("the display panicked");
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
        std::fs::write(file, profile::json(started.elapsed(), &sides))
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

/// Starts the server for one side through its transport, installing this
/// build on a host first if it is missing; shakes hands with it over its
/// control connection, and opens the connection whose paths join it.
fn start(
    transport: Transport,
    location: Location,
    role: Role,
    hello: &impl Fn(Role, Vec<u8>) -> Hello,
    options: &RemoteOptions,
) -> Result<relay::Side> {
    let place = transport.place().to_owned();
    let root = match location {
        Location::Local(path) => path.into_os_string().into_encoded_bytes(),
        Location::Remote { path, .. } => remote_path(path),
    };
    let program = remote::program(&transport, options)?;
    let mut control = transport.control(&remote::script(&program, &["server"]))?;
    let token = control
        .handshake(hello(role, root))
        .with_context(|| place.clone())?;
    let connection = transport.connect(&remote::script(&program, &["server", "--join", &token]))?;
    let peer = match role {
        Role::Source => "the Source",
        Role::Destination => "the Destination",
    };
    Ok(relay::Side {
        name: format!("{peer} on {place}"),
        control,
        connection,
        transport,
    })
}

/// A path on a host; an empty one is the home directory.
fn remote_path(path: Vec<u8>) -> Vec<u8> {
    if path.is_empty() { b".".to_vec() } else { path }
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
    fn scale_needs_a_size() {
        let scale = |arguments: &[&str]| {
            let mut all = vec!["ferry", "test", "a", "b"];
            all.extend_from_slice(arguments);
            match Cli::try_parse_from(all)?.command {
                Commands::Test { scale, .. } => Ok::<_, clap::Error>(scale),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(scale(&[]).unwrap(), None);
        assert_eq!(scale(&["--scale", "1G"]).unwrap(), Some(1 << 30));
        assert!(scale(&["--scale"]).is_err());
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
