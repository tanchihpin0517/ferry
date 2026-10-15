//! The `ferry` command.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::build_info;
use crate::dashboard;
use crate::interrupt;
use crate::peer;
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

    /// Cut the last path to the Destination short before its first piece,
    /// as a dropped SSH connection would: for `ferry test`.
    #[arg(long, hide = true)]
    cut_a_path: bool,
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
        #[command(subcommand)]
        action: FixtureAction,
    },
}

#[derive(Debug, Subcommand)]
enum FixtureAction {
    /// Generate a tree for benchmarks at ROOT, which must not exist.
    Generate {
        #[arg(long, value_enum)]
        kind: generate::Kind,
        root: PathBuf,
    },
    /// Create the tree at ROOT, which must not exist.
    Create {
        #[arg(long, value_enum, default_value_t = Variant::Base)]
        variant: Variant,
        root: PathBuf,
    },
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
        Commands::Test {
            source,
            destination,
            ssh_connections,
            remote_ferry,
            yes,
        } => {
            let options = TestOptions {
                remote: RemoteOptions {
                    remote_ferry: remote_ferry.as_deref(),
                    yes: *yes,
                },
                ssh_connections: usize::from(*ssh_connections),
            };
            testkit::run(
                Location::parse(source)?,
                Location::parse(destination)?,
                &options,
            )
        }
        Commands::Fixture { action } => {
            match action {
                FixtureAction::Generate { kind, root } => generate::generate(root, *kind)?,
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
    };
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
    let (transferred, shown) = std::thread::scope(|scope| {
        let display = scope.spawn(|| {
            if dashboard {
                dashboard::show(reports, &view, started)
            } else {
                progress::show(reports, progress_line, started).map(|counts| (counts, Vec::new()))
            }
        });
        let transferred = relay::relay(source, destination, &out);
        // The display ends once nothing can send it a report.
        drop(out);
        let shown = display.join().expect("the display panicked");
        (transferred, shown)
    });
    let (counts, said) = shown.context("show the progress")?;
    if show {
        eprintln!(
            "ferry: {}",
            progress::summary(&counts, started.elapsed(), cli.dry_run)
        );
    }
    // What the dashboard kept, now that the terminal is back.
    for line in said {
        eprintln!("ferry: {line}");
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
