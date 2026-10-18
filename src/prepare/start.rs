//! Starting the two servers, and joining their paths: with `--remote-sudo`,
//! a host's sudo is made ready first; each server is started, and shakes
//! hands, then waits, having changed nothing, until it is told to go on; and
//! once it is, the connection whose paths join it is opened.

use std::sync::Arc;

use anyhow::{Context, Result};

use super::install::{self, RemoteOptions};
use super::place::Place;
use crate::protocol::{Control, Guard, Hello, Role, RootIdentity, Welcome};
use crate::relay;
use crate::remote::{self, Location, Sudo};
use crate::transport::{ControlConnection, Transport};

/// With `--remote-sudo`, makes ready the sudo a host's servers run through:
/// once for each host. None on this machine.
pub fn sudo(asked: bool, place: &mut Place) -> Result<()> {
    if !asked || matches!(*place.transport, Transport::Local) {
        return Ok(());
    }
    place.sudo = Some(Sudo::prepare(&place.transport)?);
    Ok(())
}

/// A side's server, started, that has said what its root is and waits to
/// be told to go on.
pub struct Started {
    name: String,
    transport: Arc<Transport>,
    control: ControlConnection,
    welcome: Welcome,
    /// What its root is, as it found it.
    pub root: RootIdentity,
    program: String,
    sudo: bool,
}

/// Starts the server for one side through its transport, installing this
/// build on a host first if it is missing, and shakes hands with it over its
/// control connection. With `sudo`, the server runs through it. The server
/// then waits, having changed nothing, until it is told to go on.
pub fn start(
    at: &Place,
    location: &Location,
    role: Role,
    hello: &impl Fn(Role, Vec<u8>) -> Hello,
    options: &RemoteOptions,
) -> Result<Started> {
    let transport = Arc::clone(&at.transport);
    let sudo = at.sudo.as_ref();
    let place = transport.place().to_owned();
    let peer = match role {
        Role::Source => "the Source",
        Role::Destination => "the Destination",
    };
    let _shown = crate::status::show(format!("starting {peer} on {place}"));
    let root = match location {
        Location::Local(path) => path.clone().into_os_string().into_encoded_bytes(),
        Location::Remote { path, .. } => super::remote_path(path.clone()),
    };
    let program = install::program(&transport, options)?;
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
pub fn proceed(started: Started, guard: Guard, options: &RemoteOptions) -> Result<relay::Side> {
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
    let join = (options.remote_join && matches!(*transport, Transport::Ssh(_))).then_some(&*join);
    let mut connection = transport.connect(&welcome, join)?;
    control.attach(&mut connection);
    Ok(relay::Side {
        name,
        control,
        connection,
        transport,
    })
}
