//! The `ferry` command.

use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::build_info;
use crate::config;
use crate::interrupt;
use crate::peer;
use crate::protocol::{Hello, Role};
use crate::remote::{self, Location, RemoteOptions};
use crate::session::{self, Connection};
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

    /// Run this Ferry binary on the host, instead of the build Ferry installs.
    #[arg(long, value_name = "PATH")]
    remote_ferry: Option<String>,

    /// Install Ferry on a host without asking.
    #[arg(short = 'y', long)]
    yes: bool,
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
    let local = |role, server_root, client_root| (None, role, server_root, client_root);
    // The server's command, if there is a server; which peer it is; its root;
    // and the client's root.
    let (command, role, server_root, client_root) = match (source, destination) {
        (Location::Local(source), Location::Local(destination)) => local(
            Role::Destination,
            destination.into_os_string().into_encoded_bytes(),
            source.into_os_string(),
        ),
        (Location::Local(source), Location::Remote { host, path }) => (
            Some(remote::server_command(
                &host,
                &remote_options,
                &config::load()?.ssh,
            )?),
            Role::Destination,
            remote_path(path),
            source.into_os_string(),
        ),
        (Location::Remote { host, path }, Location::Local(destination)) => (
            Some(remote::server_command(
                &host,
                &remote_options,
                &config::load()?.ssh,
            )?),
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
    };
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let outcome = match command {
        None => {
            let destination = std::ffi::OsStr::from_bytes(&hello.root).to_owned();
            peer::local(&client_root, &destination, &hello, &mut out)?
        }
        Some(command) => {
            let mut connection = Connection::spawn(command)?;
            session::client_handshake(
                &mut connection.reader,
                &mut connection.writer,
                hello.clone(),
            )?;
            match role {
                Role::Destination => peer::push(connection, &client_root, &hello, &mut out)?,
                Role::Source => peer::pull(connection, &client_root, &hello, &mut out)?,
            }
        }
    };
    out.flush()?;
    drop(out);

    if outcome.problems > 0 {
        eprintln!(
            "ferry: {} path{} in {} could not be read",
            outcome.problems,
            if outcome.problems == 1 { "" } else { "s" },
            cli.source.to_string_lossy(),
        );
    }
    if outcome.failed > 0 {
        eprintln!(
            "ferry: {} entr{} could not be copied",
            outcome.failed,
            if outcome.failed == 1 { "y" } else { "ies" },
        );
    }
    if outcome.problems > 0 || outcome.failed > 0 {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// A path on a host; an empty one is the home directory.
fn remote_path(path: Vec<u8>) -> Vec<u8> {
    if path.is_empty() { b".".to_vec() } else { path }
}
