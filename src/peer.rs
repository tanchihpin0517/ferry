//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and relays between them. Both peers report to the
//! client, in report packets.

use std::ffi::OsStr;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use crate::destination;
use crate::interrupt;
use crate::profile::{self, State};
use crate::protocol::{self, Control, Hello, PacketType, Role};
use crate::report::Report;
use crate::session;
use crate::source;
use crate::transport::striped::{self, Kind};
use crate::transport::{self, heartbeat};

/// The server: over its control connection, its standard input and output,
/// it shakes hands and opens a socket for its path to join, then runs the
/// peer the client asked for over the path. It prints nothing; a failure goes
/// to the client. It lives as long as its control connection: should that
/// close first, the client is gone, and the server stops.
/// With `sudo`, it was started through `sudo`, and its path joins as the user
/// who ran it.
pub fn serve(build: &str, sudo: bool) -> Result<()> {
    let mut joining = None;
    let hello = session::server_handshake(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        build,
        || {
            let listening = transport::listen(sudo)?;
            let token = listening.token().to_owned();
            joining = Some(listening);
            Ok(token)
        },
    )?;
    let joining = joining.expect("the handshake got ready for the path");
    if !hello.profile {
        profile::disable();
    }
    std::thread::spawn(|| {
        listen(&mut io::stdin().lock());
        // Once the peer is done, the client closes this as the server ends,
        // and the server ends as it would have.
        if !DONE.load(Ordering::SeqCst) {
            interrupt::stop(1);
        }
    });
    if profile::enabled() {
        std::thread::spawn(report_metrics);
    }
    // Every path joins, and a heartbeat channel beside each; each says how
    // many paths there are. Each one's process stays until its socket, kept
    // in `held`, closes as the server ends.
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut channels = Vec::new();
    let mut held = Vec::new();
    loop {
        let (mut input, output, socket) = joining.accept()?;
        let (paths, kind) = striped::read_header(&mut input)?;
        match kind {
            Kind::Path => {
                inputs.push(input);
                outputs.push(output);
            }
            Kind::Heartbeat => channels.push(heartbeat::Channel {
                name: format!("heartbeat channel {}", channels.len() + 1),
                reader: Box::new(input),
                writer: Box::new(output),
            }),
        }
        held.push(socket);
        if inputs.len() >= paths && channels.len() >= paths {
            break;
        }
    }
    drop(joining);
    // A heartbeat channel falling silent, or closing before its end, says the
    // client is gone, as its control connection closing does.
    let heartbeats = heartbeat::start(
        channels,
        heartbeat::SERVER_TIMES,
        Box::new(|_| {
            if !DONE.load(Ordering::SeqCst) {
                interrupt::stop(1);
            }
        }),
    );
    // The peer reads and writes one stream, whatever the paths: pipes join it
    // to the transport, which numbers its messages over the paths and puts
    // them back in order.
    let mut receiving = striped::receiving(inputs);
    let mut sending = striped::sending(outputs);
    let (peer_input, mut into_peer) = io::pipe()?;
    let (from_peer, peer_output) = io::pipe()?;
    // A connection that fails is why the peer then fails: it is told before
    // the peer's input closes.
    let (failed, cause) = mpsc::channel();
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
        sending.close()
    });
    let mut writer = BufWriter::new(peer_output);
    let result = run(hello, &mut BufReader::new(peer_input), &mut writer);
    if let Err(error) = &result {
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
    let sent = outward.join().expect("the sending thread panicked");
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

/// Sends the client a report on the control connection, whole, whichever
/// thread sends it.
fn tell(report: &Report) -> Result<()> {
    let mut out = io::stdout().lock();
    protocol::send(&mut out, report)?;
    out.flush()?;
    Ok(())
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

/// Runs the peer `hello` asks for over a path.
fn run(hello: Hello, reader: &mut impl Read, writer: &mut (impl Write + Send)) -> Result<()> {
    match hello.role {
        Role::Destination => {
            let options = destination::Options {
                root: hello.root.clone(),
                dry_run: hello.dry_run,
                modify_window: hello.modify_window,
                numeric_ids: hello.numeric_ids,
                fsync: hello.fsync,
                delete: hello.delete,
                report: true,
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
            };
            source::run(reader, writer, &options).map(|_| ())
        }
    }
}
