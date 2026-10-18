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
use crate::guard;
use crate::interrupt;
use crate::peer;
use crate::profile;
use crate::progress;
use crate::protocol::{Control, Guard, Hello, Role, RootIdentity, Welcome};
use crate::relay;
use crate::remote::{self, Location, RemoteOptions};
use crate::source;
use crate::testkit::{
    self, TestOptions,
    fixture::{self, Checks, Variant},
    generate,
};
use crate::transport::{self, ControlConnection, Transport};
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
    let cli = Cli::parse();
    match &cli.command {
        Commands::Copy(arguments) => transfer(arguments, false),
        Commands::Sync(arguments) => transfer(arguments, true),
        Commands::Server { join, sudo } => Ok(serve(join.as_deref(), *sudo)),
        Commands::Update { check, version } => update::run(*check, version.as_deref()),
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
    // Each side's transport is opened first: for a host, that is logging in,
    // and the first thing that reaches it.
    let ssh_connections = usize::from(cli.ssh_connections);
    let source_transport = Transport::open(&source, ssh_connections)?;
    let destination_transport = Transport::open(&destination, ssh_connections)?;
    // What each root is, before any server starts: `sync` is between
    // directories, and `copy` puts a file into a directory, if agreed to.
    let source_kind = root_kind(&source_transport, &source)?;
    let destination_kind = root_kind(&destination_transport, &destination)?;
    let destination = place_destination(
        cli,
        delete,
        &source,
        source_kind,
        destination,
        destination_kind,
    )?;
    let shown_destination = shown(&destination);
    // Each host's sudo, made ready once a host, after logging in.
    let source_sudo = sudo_for(cli.remote_sudo, &source_transport, None)?;
    let destination_sudo = sudo_for(
        cli.remote_sudo,
        &destination_transport,
        source_sudo
            .as_ref()
            .map(|sudo| (source_transport.place(), sudo)),
    )?;
    let source_location = source;
    let destination_location = destination;
    let source = start(
        source_transport,
        &source_location,
        Role::Source,
        &hello,
        &remote_options,
        source_sudo.as_ref(),
    )?;
    let destination = start(
        destination_transport,
        &destination_location,
        Role::Destination,
        &hello,
        &remote_options,
        destination_sudo.as_ref(),
    )?;
    // Both servers have said what their roots are, and neither has changed
    // anything: one root holding the other is refused, and a Destination
    // `sync` would empty by mistake asked about, before either goes on.
    let from_shown = shown(&source_location);
    let verdict = guard::check(
        &source.root,
        &destination.root,
        delete,
        &guard::Named {
            shown: &from_shown,
            location: &source_location,
        },
        &guard::Named {
            shown: &shown_destination,
            location: &destination_location,
        },
    );
    match verdict {
        guard::Verdict::Ok => {}
        guard::Verdict::Refuse(reason) => anyhow::bail!(reason),
        guard::Verdict::Ask(what) if cli.dry_run => {
            eprintln!("ferry: warning: {what} A real run would ask first.");
        }
        guard::Verdict::Ask(_) if cli.allow_dangerous_destination => {}
        guard::Verdict::Ask(what) => remote::confirm_with(
            &format!("{what} Sync into it anyway?"),
            "not synced",
            "pass --allow-dangerous-destination to sync into it",
        )?,
    }
    let (source_guard, destination_guard) = guard::guards(&source.root, &destination.root);
    let source = proceed(source, source_guard, &remote_options)?;
    let mut destination = proceed(destination, destination_guard, &remote_options)?;
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
    let (out, reports) = mpsc::sync_channel(REPORTS);
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

/// With `--remote-sudo`, the sudo to run a host's server through: `known`'s,
/// if it is for the same host, or else one made ready. None on this machine.
fn sudo_for(
    asked: bool,
    transport: &Transport,
    known: Option<(&str, &remote::Sudo)>,
) -> Result<Option<remote::Sudo>> {
    if !asked || matches!(transport, Transport::Local) {
        return Ok(None);
    }
    if let Some((place, sudo)) = known
        && place == transport.place()
    {
        return Ok(Some(sudo.clone()));
    }
    remote::Sudo::prepare(transport).map(Some)
}

/// A side's server, started, that has said what its root is and waits to
/// be told to go on.
struct Started {
    name: String,
    transport: Transport,
    control: ControlConnection,
    welcome: Welcome,
    /// What its root is, as it found it.
    root: RootIdentity,
    program: String,
    sudo: bool,
}

/// Starts the server for one side through its transport, installing this
/// build on a host first if it is missing, and shakes hands with it over its
/// control connection. With `sudo`, the server runs through it. The server
/// then waits, having changed nothing, until it is told to go on.
fn start(
    transport: Transport,
    location: &Location,
    role: Role,
    hello: &impl Fn(Role, Vec<u8>) -> Hello,
    options: &RemoteOptions,
    sudo: Option<&remote::Sudo>,
) -> Result<Started> {
    let place = transport.place().to_owned();
    let peer = match role {
        Role::Source => "the Source",
        Role::Destination => "the Destination",
    };
    let _shown = crate::status::show(format!("starting {peer} on {place}"));
    let root = match location {
        Location::Local(path) => path.clone().into_os_string().into_encoded_bytes(),
        Location::Remote { path, .. } => remote_path(path.clone()),
    };
    let program = remote::program(&transport, options)?;
    // Only the server runs through sudo; its paths join as the login user.
    let (script, first) = match sudo {
        Some(sudo) => (sudo.script(&program, &["server", "--sudo"]), sudo.first()),
        None => (remote::script(&program, &["server"]), Vec::new()),
    };
    let mut control = transport.control(&script, &first)?;
    let mut welcome = control
        .handshake(hello(role, root))
        .with_context(|| place.clone())?;
    Ok(Started {
        name: format!("{peer} on {place}"),
        transport,
        control,
        root: std::mem::take(&mut welcome.root),
        welcome,
        program,
        sudo: sudo.is_some(),
    })
}

/// Tells a started server to go on, kept by `guard`, and opens the
/// connection whose paths join it.
fn proceed(started: Started, guard: Guard, options: &RemoteOptions) -> Result<relay::Side> {
    let Started {
        name,
        transport,
        mut control,
        welcome,
        program,
        sudo,
        ..
    } = started;
    control
        .sender()
        .send(&Control::Proceed(guard))
        .with_context(|| format!("tell {name} to go on"))?;
    let mut join = vec!["server", "--join", &welcome.token];
    if sudo {
        join.push("--sudo");
    }
    let join = remote::script(&program, &join);
    let join = (options.remote_join && matches!(transport, Transport::Ssh(_))).then_some(&*join);
    let mut connection = transport.connect(&welcome, join)?;
    control.attach(&mut connection);
    Ok(relay::Side {
        name,
        control,
        connection,
        transport,
    })
}

/// What a location's root is, as its server will find it: a symlink to a
/// directory, as the user named it, is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    Directory,
    Other,
    Missing,
}

/// Finds what a location's root is: on this machine, directly; on a host,
/// with a shell, before Ferry need be there.
fn root_kind(transport: &Transport, location: &Location) -> Result<RootKind> {
    let path = match location {
        Location::Local(path) => {
            return Ok(match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => RootKind::Directory,
                Ok(_) => RootKind::Other,
                // A symlink to nothing is there, as something else.
                Err(_) if std::fs::symlink_metadata(path).is_ok() => RootKind::Other,
                Err(_) => RootKind::Missing,
            });
        }
        Location::Remote { path, .. } => remote_path(path.clone()),
    };
    let output = transport
        .command(&kind_script(&path))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("look at {} on {}", shown(location), transport.place()))?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "directory" => Ok(RootKind::Directory),
        "other" => Ok(RootKind::Other),
        "missing" => Ok(RootKind::Missing),
        said => anyhow::bail!(
            "look at {} on {}: unexpected answer {said:?}",
            shown(location),
            transport.place()
        ),
    }
}

/// A script that says what the root at `path` is, as `root_kind` reads it.
fn kind_script(path: &[u8]) -> String {
    format!(
        "{}; if [ -d \"$p\" ]; then echo directory; \
         elif [ -e \"$p\" ] || [ -h \"$p\" ]; then echo other; else echo missing; fi",
        remote::shell::assign("p", path)
    )
}

/// Where the transfer goes, as the roots' kinds allow: `sync` only from a
/// directory into a directory, or where there is none yet; `copy` never a
/// directory onto something else, and a file onto a directory, only into it,
/// once agreed to. A missing source is left for its server to report.
fn place_destination(
    cli: &TransferArgs,
    delete: bool,
    source: &Location,
    source_kind: RootKind,
    destination: Location,
    destination_kind: RootKind,
) -> Result<Location> {
    let (from, to) = (shown(source), shown(&destination));
    match (delete, source_kind, destination_kind) {
        (_, RootKind::Missing, _) => Ok(destination),
        (true, RootKind::Other, _) => anyhow::bail!(
            "sync makes a directory the same as another, and {from} is not a directory; \
             to copy a file, use `ferry copy`"
        ),
        (true, RootKind::Directory, RootKind::Other) => {
            anyhow::bail!("sync makes a directory the same as another, and {to} is not a directory")
        }
        (false, RootKind::Directory, RootKind::Other) => {
            anyhow::bail!("{to} is not a directory, and {from} is one")
        }
        (false, RootKind::Other, RootKind::Directory) => {
            let inside = into(destination, &file_name(source)?);
            let question = format!(
                "{to} is a directory. Copy {from} into it, as {}?",
                shown(&inside)
            );
            if !cli.yes {
                remote::confirm(&question, "not copied")?;
            }
            Ok(inside)
        }
        _ => Ok(destination),
    }
}

/// The last component of a location's path: the name a file keeps when
/// copied into a directory.
fn file_name(location: &Location) -> Result<Vec<u8>> {
    let path: &[u8] = match location {
        Location::Local(path) => path.as_os_str().as_encoded_bytes(),
        Location::Remote { path, .. } => path,
    };
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let name = trimmed
        .rsplit(|&byte| byte == b'/')
        .next()
        .unwrap_or_default();
    if name.is_empty() || name == b"." || name == b".." {
        anyhow::bail!("{} names no file to copy into a directory", shown(location));
    }
    Ok(name.to_vec())
}

/// `name` inside the directory at `location`.
fn into(location: Location, name: &[u8]) -> Location {
    match location {
        Location::Local(path) => {
            use std::os::unix::ffi::OsStrExt;
            Location::Local(path.join(std::ffi::OsStr::from_bytes(name)))
        }
        Location::Remote { host, mut path } => {
            if !path.is_empty() && !path.ends_with(b"/") {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            Location::Remote { host, path }
        }
    }
}

/// A location as the user would write it.
fn shown(location: &Location) -> String {
    match location {
        Location::Local(path) => path.display().to_string(),
        Location::Remote { host, path } => {
            format!("{host}:{}", String::from_utf8_lossy(path))
        }
    }
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

    /// The script a host runs to say what a root is, run here, through the
    /// login shell's `sh -c`, on every kind, and on names any bytes make.
    #[test]
    fn a_host_says_what_a_root_is() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("directory")).unwrap();
        std::fs::write(root.join("file"), "x").unwrap();
        std::os::unix::fs::symlink("directory", root.join("to-directory")).unwrap();
        std::os::unix::fs::symlink("nowhere", root.join("dangling")).unwrap();
        let odd = "it's é\n".as_bytes();
        std::fs::create_dir(root.join(std::ffi::OsStr::from_bytes(odd))).unwrap();
        // Not UTF-8, where the filesystem takes it: a Mac's does not.
        let latin = b"caf\xe9";
        let latin_made = std::fs::create_dir(root.join(std::ffi::OsStr::from_bytes(latin))).is_ok();
        let kind = |name: &[u8]| {
            let mut path = root.as_os_str().as_bytes().to_vec();
            path.push(b'/');
            path.extend_from_slice(name);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(remote::shell::sh_command(&kind_script(&path)))
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        assert_eq!(kind(b"directory"), "directory");
        assert_eq!(kind(b"to-directory"), "directory");
        assert_eq!(kind(b"file"), "other");
        assert_eq!(kind(b"dangling"), "other");
        assert_eq!(kind(b"missing"), "missing");
        assert_eq!(kind(odd), "directory");
        if latin_made {
            assert_eq!(kind(latin), "directory");
        }
    }

    #[test]
    fn a_file_name_is_the_last_component() {
        let remote = |path: &[u8]| Location::Remote {
            host: "h".into(),
            path: path.to_vec(),
        };
        assert_eq!(file_name(&remote(b"a/b.txt")).unwrap(), b"b.txt");
        assert_eq!(file_name(&remote(b"b.txt")).unwrap(), b"b.txt");
        assert_eq!(file_name(&Location::Local("/x/y".into())).unwrap(), b"y");
        for none in [&b""[..], b"/", b"a/..", b"."] {
            assert!(file_name(&remote(none)).is_err(), "{none:?}");
        }
        assert_eq!(shown(&into(remote(b""), b"f")), "h:f");
        assert_eq!(shown(&into(remote(b"/"), b"f")), "h:/f");
        assert_eq!(shown(&into(remote(b"d"), b"f")), "h:d/f");
        assert_eq!(shown(&into(remote(b"d/"), b"f")), "h:d/f");
    }

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
