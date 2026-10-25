//! Starting the two servers: each host's first session becomes Ferry's
//! launcher there; with `--remote-sudo`, a host's sudo is made ready; each
//! server is started, by the launcher on a host, and shakes hands, then
//! waits, having changed nothing, until it is told to go on; and once it is,
//! the stream between the two can be made.

use std::sync::Arc;

use anyhow::{Context, Result};

use super::install::RemoteOptions;
use super::place::Place;
use crate::follow;
use crate::protocol::{Control, Guard, Hello, Role, RootIdentity, SudoAsks, Welcome};
use crate::remote::{self, Location, Sudo};
use crate::transport::launcher::Launcher;
use crate::transport::streams::Server;
use crate::transport::{ControlConnection, Transport};

/// Has a host's first session become Ferry's launcher there, running its
/// program, once it is ready; nothing on this machine, where the client
/// starts its servers itself. With `--remote-join`, what reaches the
/// launcher joins it through the shell on the host.
pub fn launcher(place: &mut Place, options: &RemoteOptions) -> Result<()> {
    let Some(session) = place.session.get_mut().expect("not poisoned").take() else {
        return Ok(());
    };
    let program = place
        .program
        .clone()
        .context("the program is ready before the launcher")?;
    let join = |socket: &str| {
        options
            .remote_join
            .then(|| remote::script(&program, &["server", "--join", socket]))
    };
    let _shown = crate::status::show(format!(
        "starting Ferry's launcher on {}",
        place.transport.place()
    ));
    place.launcher = Some(Launcher::start(
        Arc::clone(&place.transport),
        session,
        &program,
        join,
    )?);
    Ok(())
}

/// With `--remote-sudo`, makes ready the sudo a host's servers run through:
/// once for each host. None on this machine.
pub fn sudo(asked: bool, place: &mut Place) -> Result<()> {
    if !asked || matches!(*place.transport, Transport::Local) {
        return Ok(());
    }
    let program = place
        .program
        .as_deref()
        .context("the program is ready before sudo")?;
    place.sudo = Some(Sudo::prepare(&place.transport, program)?);
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

/// Starts the server for one side at its place, whose program is ready, and
/// shakes hands with it over its control connection. With the place's sudo,
/// the server runs through it. The server then waits, having changed
/// nothing, until it is told to go on.
pub fn start(
    at: &Place,
    location: &Location,
    role: Role,
    hello: &impl Fn(Role, Vec<u8>) -> Hello,
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
    let program = at
        .program
        .clone()
        .expect("installed before any server starts");
    // Only the server runs through sudo; what joins it does so as the login
    // user.
    let mut control = match &at.launcher {
        Some(launcher) => {
            let asks = sudo.map(|sudo| {
                if sudo.asks_password() {
                    SudoAsks::Password
                } else {
                    SudoAsks::Nothing
                }
            });
            let first = sudo.map(Sudo::first).unwrap_or_default();
            launcher
                .server(asks, &first)
                .with_context(|| place.clone())?
        }
        None => transport.control(&remote::script(&program, &["server"]))?,
    };
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

/// A server told to go on: the stream to it is yet to be made.
pub struct Going {
    pub side: follow::Side,
    welcome: Welcome,
    /// What a channel runs to join it, where they join through the shell.
    join: Option<String>,
}

impl Going {
    /// How the stream reaches it.
    pub fn server(&self) -> Server {
        Server {
            transport: Arc::clone(&self.side.transport),
            welcome: self.welcome.clone(),
            join: self.join.clone(),
        }
    }
}

/// Tells a started server to go on, kept by `guard`.
pub fn proceed(started: Started, guard: Guard, options: &RemoteOptions) -> Result<Going> {
    let Started {
        name,
        transport,
        control,
        welcome,
        program,
        sudo,
        ..
    } = started;
    control
        .sender()
        .send(&Control::Proceed(guard))
        .with_context(|| format!("tell {name} to go on"))?;
    let socket = std::str::from_utf8(&welcome.socket)
        .with_context(|| format!("the socket of {name} is not UTF-8"))?;
    let mut join = vec!["server", "--join", socket];
    if sudo {
        join.push("--sudo");
    }
    let join = remote::script(&program, &join);
    let join = (options.remote_join && matches!(*transport, Transport::Ssh(_))).then_some(join);
    Ok(Going {
        side: follow::Side {
            name,
            control,
            monitor: transport.monitor(),
            transport,
        },
        welcome,
        join,
    })
}
