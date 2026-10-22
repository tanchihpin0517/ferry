//! Preparing a transfer: everything between reading the arguments and the
//! first file, in the order of the
//! [pipeline](../../docs/design/pipeline.md#phase-1-prepare)'s first phase.
//! Each step has a module: reaching each place, once ([`place`]); what each
//! root is ([`probe`]); what to ask ([`ask`]); the build to run on a host
//! ([`install`]); starting the servers and joining their paths ([`start`]);
//! and what the two roots allow ([`plan`]). See
//! [the draft](../../docs/drafts/preparation.md) this follows.

mod ask;
mod install;
mod place;
mod plan;
mod probe;
mod start;

use anyhow::Result;

pub use install::{RemoteOptions, program};
pub use place::Place;

use crate::protocol::{Hello, Role};
use crate::relay;
use crate::remote::Location;
use place::Places;

/// What a transfer asks of preparing, from the command line.
pub struct Request<'a> {
    pub source: Location,
    pub destination: Location,
    pub remote: RemoteOptions<'a>,
    pub ssh_connections: usize,
    pub remote_sudo: bool,
    /// `sync`, rather than `copy`.
    pub delete: bool,
    pub dry_run: bool,
    pub allow_dangerous_destination: bool,
}

/// A transfer, prepared: both servers started, and their paths joined.
pub struct Prepared {
    pub source: relay::Side,
    pub destination: relay::Side,
    /// Where the transfer goes, as the user would write it: into a
    /// directory, for a file copied onto one.
    pub shown_destination: String,
}

/// Prepares a transfer, saying hello to each server with `hello`.
pub fn prepare(request: Request, hello: &impl Fn(Role, Vec<u8>) -> Hello) -> Result<Prepared> {
    let Request {
        source,
        destination,
        remote,
        ssh_connections,
        remote_sudo,
        delete,
        dry_run,
        allow_dangerous_destination,
    } = request;
    // Each side's place is opened first: for a host, that is logging in,
    // and the first thing that reaches it.
    let mut places = Places::open(&[&source, &destination], ssh_connections)?;
    // Each host is probed, once, which says what each root there is: `sync`
    // is between directories, and `copy` puts a file into a directory.
    let kinds = probe::roots(&mut places, &[&source, &destination])?;
    let destination = ask::place_destination(delete, &source, kinds[0], destination, kinds[1])?;
    let shown_destination = shown(&destination);
    // Every host that needs this build is asked about, then installed, before
    // any server starts: a server that has started waits for the others.
    install::ready(&mut places, &remote)?;
    // Each host's sudo, made ready once a host.
    for place in places.iter_mut() {
        start::sudo(remote_sudo, place)?;
    }
    let source_started = start::start(places.of(&source), &source, Role::Source, hello)?;
    let destination_started = start::start(
        places.of(&destination),
        &destination,
        Role::Destination,
        hello,
    )?;
    let shown_source = shown(&source);
    let (source_guard, destination_guard) = plan::agree(
        (
            &source_started.root,
            &plan::Named {
                shown: &shown_source,
                location: &source,
            },
        ),
        (
            &destination_started.root,
            &plan::Named {
                shown: &shown_destination,
                location: &destination,
            },
        ),
        delete,
        dry_run,
        allow_dangerous_destination,
    )?;
    let source = start::proceed(source_started, source_guard, &remote)?;
    let destination = start::proceed(destination_started, destination_guard, &remote)?;
    Ok(Prepared {
        source,
        destination,
        shown_destination,
    })
}

/// A location as the user would write it.
pub fn shown(location: &Location) -> String {
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
