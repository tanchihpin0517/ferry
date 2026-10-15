//! The `ferry` command.

use std::ffi::OsString;
use std::io::{self, BufWriter};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::build_info;
use crate::config;
use crate::interrupt;
use crate::peer;
use crate::progress;
use crate::protocol::{Hello, Role};
use crate::remote::{self, Location, RemoteOptions};
use crate::report::Reporter;
use crate::session::Connection;
use crate::source;
use crate::testkit::{
    self, TestOptions,
    fixture::{self, Checks, Variant},
};

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

    /// Compress at this zstd level, from 1 to 19, instead of adapting it to
    /// whichever is slower, the network or compressing.
    #[arg(long, value_name = "LEVEL", value_parser = clap::value_parser!(i32).range(1..=19))]
    compress_level: Option<i32>,

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

        /// Run this Ferry binary on hosts, instead of the build Ferry installs.
        #[arg(long, value_name = "PATH")]
        remote_ferry: Option<String>,

        /// Install Ferry on a host without asking.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Serve one connection on standard input and output. The client runs
    /// this over SSH.
    #[command(hide = true)]
    Server,

    /// Create or remove a test tree. `ferry test` runs this on hosts.
    #[command(hide = true)]
    Fixture {
        #[command(subcommand)]
        action: FixtureAction,
    },
}

#[derive(Debug, Subcommand)]
enum FixtureAction {
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
        Commands::Server => Ok(serve()),
        Commands::Test {
            source,
            destination,
            remote_ferry,
            yes,
        } => {
            let config = config::load()?;
            let options = TestOptions {
                remote: RemoteOptions {
                    remote_ferry: remote_ferry.as_deref(),
                    yes: *yes,
                },
                config: &config,
            };
            testkit::run(
                Location::parse(source)?,
                Location::parse(destination)?,
                &options,
            )
        }
        Commands::Fixture { action } => {
            match action {
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
/// to the client in a packet, so it prints nothing itself.
fn serve() -> ExitCode {
    let mut reader = io::stdin().lock();
    // Not a lock: the Destination writes from more than one thread.
    let mut writer = BufWriter::new(io::stdout());
    match peer::serve(&mut reader, &mut writer, &build_info::identity()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Copies, or with `delete`, syncs: a push when SOURCE is local, a pull when
/// it is on a host.
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
    let local = |role, server_root, client_root| (None, role, server_root, client_root);
    // The server's host and command, if there is a server; which peer it is;
    // its root; and the client's root.
    let (server, role, server_root, client_root) = match (source, destination) {
        (Location::Local(source), Location::Local(destination)) => local(
            Role::Destination,
            destination.into_os_string().into_encoded_bytes(),
            source.into_os_string(),
        ),
        (Location::Local(source), Location::Remote { host, path }) => (
            Some(server(host, &remote_options)?),
            Role::Destination,
            remote_path(path),
            source.into_os_string(),
        ),
        (Location::Remote { host, path }, Location::Local(destination)) => (
            Some(server(host, &remote_options)?),
            Role::Source,
            remote_path(path),
            destination.into_os_string(),
        ),
        (Location::Remote { .. }, Location::Remote { .. }) => {
            bail!("copying from a host to a host is not implemented yet")
        }
    };

    let hello = Hello {
        build: build_info::identity(),
        role,
        root: server_root,
        dry_run: cli.dry_run,
        modify_window: cli.modify_window,
        numeric_ids: cli.numeric_ids,
        fsync: !cli.no_fsync,
        delete,
        compress: !cli.no_compress,
        compress_level: cli.compress_level,
    };
    let started = Instant::now();
    let show = progress::wanted(cli.progress);
    // A dry run's listing goes to standard output; a line redrawn on standard
    // error would tangle with it on a terminal.
    let progress_line = show && !cli.dry_run;
    let (out, reports) = mpsc::channel();
    let reporter = Reporter::new(out);
    let (transferred, counts) = std::thread::scope(|scope| {
        let display = scope.spawn(move || progress::show(reports, progress_line, started));
        let transferred = transfer_with(server, role, &client_root, &hello, &reporter);
        // The display ends once every reporter is gone.
        drop(reporter);
        let counts = display.join().expect("the display panicked");
        (transferred, counts)
    });
    let counts = counts.context("show the progress")?;
    if show {
        eprintln!(
            "ferry: {}",
            progress::summary(&counts, started.elapsed(), cli.dry_run)
        );
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

/// Runs the transfer: both peers here, or one here and one on a host. The
/// peer here reports to `reporter`.
fn transfer_with(
    server: Option<(String, std::process::Command)>,
    role: Role,
    client_root: &std::ffi::OsStr,
    hello: &Hello,
    reporter: &Reporter,
) -> Result<()> {
    match server {
        None => {
            let destination = std::ffi::OsStr::from_bytes(&hello.root).to_owned();
            peer::local(client_root, &destination, hello, reporter)
        }
        Some((host, command)) => {
            let mut connection = Connection::spawn(command)?;
            connection
                .handshake(hello.clone())
                .with_context(|| host.clone())?;
            match role {
                Role::Destination => peer::push(connection, client_root, hello, reporter),
                Role::Source => peer::pull(connection, client_root, hello, reporter),
            }
        }
    }
}

/// The host, and the command that starts a server there, installing this
/// build first if it is missing.
fn server(host: String, options: &RemoteOptions) -> Result<(String, std::process::Command)> {
    let command = remote::server_command(&host, options, &config::load()?.ssh)?;
    Ok((host, command))
}

/// A path on a host; an empty one is the home directory.
fn remote_path(path: Vec<u8>) -> Vec<u8> {
    if path.is_empty() { b".".to_vec() } else { path }
}
