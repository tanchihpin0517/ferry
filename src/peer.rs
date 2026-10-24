//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and relays between them. Both peers report to the
//! client, in report packets.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::destination;
use crate::fsys;
use crate::interrupt;
use crate::profile::{self, State};
use crate::protocol::{self, Control, Guard, Hello, PacketType, Role, Welcome};
use crate::report::Report;
use crate::session;
use crate::source;
use crate::stop::Reported;
use crate::transport::striped::{self, Kind};
use crate::transport::{self, heartbeat};

/// The server: over its standard input and output, it shakes hands and opens
/// a socket for its paths to reach it, then runs the peer the client asked
/// for over the paths. The control connection then goes on a channel of its
/// own to the socket, since a shell on the host, or what it left running,
/// may write to the server's output as well. It prints nothing more; a
/// failure goes to the client. It lives as long as its input: should that
/// close first, the client is gone, and the server stops.
/// With `sudo`, it was started through `sudo`, and its path joins as the user
/// who ran it.
pub fn serve(build: &str, sudo: bool) -> Result<()> {
    let mut joining = None;
    // Standard output as it is, unbuffered, so that the answer goes in one
    // write, which a pipe keeps whole; Rust's own would write up to each
    // newline in it apart.
    let mut output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let hello = session::server_handshake(&mut io::stdin().lock(), &mut output, build, |hello| {
        let listening = transport::listen(sudo)?;
        let welcome = Welcome {
            socket: listening.socket().as_os_str().as_bytes().to_vec(),
            root: fsys::root_identity(OsStr::from_bytes(&hello.root)),
        };
        joining = Some(listening);
        Ok(welcome)
    })?;
    let joining = joining.expect("the handshake got ready for the path");
    // Before any stage starts, so that each runs on them.
    crate::cpus::limit((hello.cpus > 0).then_some(hello.cpus as usize), hello.place);
    // The client checks the two roots against each other, then says to go
    // on. One that refuses closes the connection instead: the server ends,
    // having changed nothing.
    let guard =
        match protocol::receive(&mut io::stdin().lock()).context("wait for the client to go on")? {
            None => return Ok(()),
            Some(packet) => match packet.decode::<Control>()? {
                Control::Proceed(guard) => guard,
                other => bail!("expected the client to go on, got {other:?}"),
            },
        };
    if !hello.profile {
        profile::disable();
    }
    std::thread::spawn(|| {
        // Nothing more comes on it; it only stays open.
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
        // Once the peer is done, the client closes this as the server ends,
        // and the server ends as it would have.
        if !DONE.load(Ordering::SeqCst) {
            interrupt::stop(1);
        }
    });
    // Every path comes, and a heartbeat channel beside each; each says how
    // many paths there are. So does the control connection's channel. Each
    // joining process stays until its socket, kept in `held`, closes as the
    // server ends.
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut channels = Vec::new();
    let mut control = None;
    let mut held = Vec::new();
    loop {
        let transport::Joined {
            mut input,
            output,
            held: socket,
        } = joining.accept()?;
        let (paths, kind) = striped::read_header(&mut input)?;
        match kind {
            Kind::Path => {
                inputs.push(input);
                outputs.push(output);
            }
            Kind::Heartbeat => channels.push(heartbeat::Channel {
                name: format!("heartbeat channel {}", channels.len() + 1),
                reader: input,
                writer: output,
            }),
            Kind::Control => control = Some((input, output)),
        }
        held.extend(socket);
        if inputs.len() >= paths && channels.len() >= paths && control.is_some() {
            break;
        }
    }
    let (mut control_input, control_output) = control.expect("the loop waits for it");
    *CONTROL.lock().expect("not poisoned") = Some(control_output);
    std::thread::spawn(move || listen(&mut control_input));
    if profile::enabled() {
        std::thread::spawn(report_metrics);
    }
    drop(joining);
    // A heartbeat channel falling silent, or closing before its end, says the
    // client is gone, as its control connection closing does.
    let heartbeats = heartbeat::start(
        channels,
        heartbeat::server_times(),
        Box::new(|_| {
            if !DONE.load(Ordering::SeqCst) {
                interrupt::stop(1);
            }
        }),
    );
    // What comes on a path is from the client too, as its heartbeats may
    // wait behind it on a slow link. Which path is which SSH connection's is
    // not known here, so each stands for all: a server's silence is only
    // for when the client is gone.
    let hearing = heartbeats.hearing();
    let inputs: Vec<_> = inputs
        .into_iter()
        .map(|input| heartbeat::Heard::new(input, hearing.clone(), None))
        .collect();
    // The peer reads and writes one stream, whatever the paths: pipes join it
    // to the transport, which numbers its messages over the paths and puts
    // them back in order.
    let mut receiving = striped::receiving(inputs);
    let mut sending = striped::sending(outputs);
    let (peer_input, mut into_peer) = io::pipe()?;
    let (from_peer, peer_output) = io::pipe()?;
    // A connection that fails is why the peer then fails: it is told before
    // the peer's input closes.
    let (failed, cause) = crossbeam_channel::unbounded();
    let inward = std::thread::spawn(move || {
        let _thread = profile::thread("inward");
        loop {
            match receiving.recv() {
                Ok(Some(packet)) => {
                    let forwarded = profile::wait(State::WritePeer, || {
                        protocol::forward(&mut into_peer, &packet)
                    });
                    if forwarded.is_err() {
                        // The peer stopped reading, and says why itself.
                        receiving.drain();
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = failed.send(error);
                    break;
                }
            }
        }
    });
    let outward = std::thread::spawn(move || -> Result<()> {
        let _thread = profile::thread("outward");
        let mut from_peer = BufReader::new(from_peer);
        // Once the connection has failed, the peer's errors are its doing:
        // each says the connection's failure instead.
        let mut failure = None;
        while let Some(mut packet) =
            profile::wait(State::ReadPeer, || protocol::receive(&mut from_peer))?
        {
            if packet.kind == PacketType::Control
                && let Ok(Control::Error { .. }) = packet.decode::<Control>()
            {
                if let Ok(error) = cause.try_recv() {
                    failure = Some(format!("{error:#}"));
                }
                if let Some(message) = &failure {
                    let encoded = protocol::encode(&Control::Error {
                        message: message.clone(),
                    })?;
                    packet = protocol::receive(&mut encoded.as_slice())?.expect("a whole packet");
                }
            }
            sending.send(packet)?;
        }
        // A connection that failed under a peer that did not see it, as one
        // done but for its input, still fails the server, and says so.
        if failure.is_none()
            && let Ok(error) = cause.try_recv()
        {
            let encoded = protocol::encode(&Control::Error {
                message: format!("{error:#}"),
            })?;
            if let Some(packet) = protocol::receive(&mut encoded.as_slice())? {
                let _ = sending.send(packet);
            }
            let _ = sending.close();
            return Err(error);
        }
        sending.close()
    });
    let mut writer = BufWriter::new(peer_output);
    let result = run(hello, guard, &mut BufReader::new(peer_input), &mut writer);
    // A failure the peer already told, or that came from the other side, is
    // not told again.
    if let Err(error) = &result
        && !error.is::<Reported>()
    {
        // The client may already be gone; the error is returned either way.
        let _ = protocol::send(
            &mut writer,
            &Control::Error {
                message: format!("{error:#}"),
            },
        );
        let _ = writer.flush();
    }
    // Where the time went, once the peer is done: the client shows it, if
    // asked to.
    if profile::enabled() {
        let _ = protocol::send(&mut writer, &Report::Profile(profile::collect()));
        let _ = writer.flush();
    }
    // The peer's output closes, so the last of it goes out and the paths
    // close; what came in after the peer stopped no longer matters.
    drop(writer);
    let sent = outward
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error in outward")));
    drop(inward);
    // Letting go of the paths ends them for the client, which then closes the
    // control connection: from here, that is no longer the client going.
    DONE.store(true, Ordering::SeqCst);
    heartbeats.end();
    drop(held);
    result.and(sent)
}

/// Answers what the client asks on the control connection, a snapshot of
/// where the threads are, until the connection closes.
fn listen(reader: &mut impl Read) {
    loop {
        match protocol::receive(reader) {
            Ok(Some(packet)) => {
                if let Ok(Control::Snapshot) = packet.decode::<Control>() {
                    let _ = tell(&Report::Snapshot(profile::snapshot()));
                }
            }
            Ok(None) => return,
            // What cannot be read as packets is read to its end.
            Err(_) => {
                let _ = io::copy(reader, &mut io::sink());
                return;
            }
        }
    }
}

/// The control connection's channel, once it has come.
static CONTROL: Mutex<Option<Box<dyn Write + Send>>> = Mutex::new(None);

/// Sends the client a report on the control connection, whole, whichever
/// thread sends it.
fn tell(report: &Report) -> Result<()> {
    let mut control = CONTROL.lock().expect("not poisoned");
    let out = control
        .as_mut()
        .context("the control connection has no channel yet")?;
    protocol::send(out, report)?;
    out.flush()?;
    Ok(())
}

/// Tells the client `message`, to show it, as a server's own internal error,
/// without waiting for the control connection should another thread hold
/// it; false if the client could not be told, as on the client itself.
pub fn tell_internal(message: &str) -> bool {
    let Ok(mut control) = CONTROL.try_lock() else {
        return false;
    };
    let Some(out) = control.as_mut() else {
        return false;
    };
    protocol::send(out, &Report::Said(message.to_owned())).is_ok() && out.flush().is_ok()
}

/// How often the client is sent where the threads' time went.
const METRICS_EVERY: Duration = Duration::from_secs(1);

/// Sends the client where the threads' time went, every [`METRICS_EVERY`],
/// on the control connection, until the peer is done or the client gone.
fn report_metrics() {
    let mut meter = profile::Meter::new();
    loop {
        std::thread::sleep(METRICS_EVERY);
        if DONE.load(Ordering::SeqCst) {
            return;
        }
        if tell(&Report::Metrics(meter.take())).is_err() {
            return;
        }
    }
}

/// The peer is done, and the server is ending.
static DONE: AtomicBool = AtomicBool::new(false);

/// Runs the peer `hello` asks for over a path, kept by `guard` from what it
/// must never go into or delete.
fn run(
    hello: Hello,
    guard: Guard,
    reader: &mut impl Read,
    writer: &mut (impl Write + Send),
) -> Result<()> {
    match hello.role {
        Role::Destination => {
            let options = destination::Options {
                root: hello.root.clone(),
                dry_run: hello.dry_run,
                modify_window: hello.modify_window,
                numeric_ids: hello.numeric_ids,
                delete: hello.delete,
                report: true,
                protect: guard.protect,
                in_flight_entries: hello.in_flight_entries,
                data_credit: hello.data_credit,
                one_file_system: hello.one_file_system,
            };
            destination::run(reader, writer, &options).map(|_| ())
        }
        Role::Source => {
            let options = source::Options {
                root: OsStr::from_bytes(&hello.root),
                dry_run: hello.dry_run,
                numeric_ids: hello.numeric_ids,
                compress: hello.compress,
                compress_level: hello.compress_level,
                report: true,
                part_entries: hello.part_entries as usize,
                backlog_files: hello.backlog_files as usize,
                paced_bytes: hello.paced_bytes as usize,
                in_flight_entries: hello.in_flight_entries,
                avoid: guard.avoid,
                one_file_system: hello.one_file_system,
            };
            source::run(reader, writer, &options).map(|_| ())
        }
    }
}
