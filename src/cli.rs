//! The `ferry` command.

use std::ffi::OsString;
use std::io::{self, BufWriter};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::build_info;
use crate::peer;
use crate::protocol::Hello;
use crate::remote::{self, Location, RemoteOptions};
use crate::session::{self, Connection};

/// A fast replacement for rsync.
///
/// `ferry SOURCE DESTINATION` makes DESTINATION a copy of SOURCE. A trailing
/// slash changes nothing. A location is a local path or `[user@]host:path`.
#[derive(Debug, Parser)]
#[command(
    name = "ferry",
    version,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// What to copy. Only a local path, for now.
    #[arg(required = true, value_name = "SOURCE")]
    source: Option<OsString>,

    /// Where to copy it.
    #[arg(required = true, value_name = "DESTINATION")]
    destination: Option<OsString>,

    /// List what would be transferred, without copying anything.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Run this Ferry binary on the host. Needed until Ferry installs itself.
    #[arg(long, value_name = "PATH")]
    remote_ferry: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Serve one connection on standard input and output. The client runs
    /// this, over SSH or as a child process.
    #[command(hide = true)]
    Server,
}

pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Server) => Ok(serve()),
        None => transfer(&cli),
    }
}

/// The server writes nothing but packets to its output, and reports a failure
/// to the client in a packet, so it prints nothing itself.
fn serve() -> ExitCode {
    let mut reader = io::stdin().lock();
    let mut writer = BufWriter::new(io::stdout().lock());
    match peer::serve(&mut reader, &mut writer, &build_info::identity()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn transfer(cli: &Cli) -> Result<ExitCode> {
    let source = Location::parse(cli.source.as_deref().expect("required"))?;
    let destination = Location::parse(cli.destination.as_deref().expect("required"))?;
    let Location::Local(_source) = source else {
        bail!("copying from a host is not implemented yet");
    };
    if !cli.dry_run {
        bail!("copying files is not implemented yet; only --dry-run works");
    }

    let (command, destination_path) = match destination {
        Location::Local(path) => {
            let mut command =
                Command::new(std::env::current_exe().context("find the ferry binary")?);
            command.arg("server");
            (command, path.into_os_string().into_encoded_bytes())
        }
        Location::Remote { host, path } => {
            let options = RemoteOptions {
                remote_ferry: cli.remote_ferry.as_deref(),
            };
            let path = if path.is_empty() { b".".to_vec() } else { path };
            (remote::server_command(&host, &options)?, path)
        }
    };

    let mut connection = Connection::spawn(command)?;
    let hello = Hello {
        build: build_info::identity(),
        destination: destination_path,
        dry_run: cli.dry_run,
    };
    session::client_handshake(&mut connection.reader, &mut connection.writer, hello)?;

    // Nothing more to do yet: closing the connection ends the server.
    let (_reader, writer, server) = connection.split();
    drop(writer);
    server.wait()?;
    Ok(ExitCode::SUCCESS)
}
