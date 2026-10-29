//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and makes the stream between them, which carries
//! what one peer sends the other. Both peers report to the client on the
//! server's control connection, not among their messages to each other.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::destination;
use crate::fsys;
use crate::handshake;
use crate::interrupt;
use crate::mux::{self, Mux};
use crate::profile;
use crate::protocol::{self, Control, Guard, Hello, Port, Proceed, Role, Welcome};
use crate::report::{self, Report, Reporter};
use crate::source;
use crate::stop::Reported;
use crate::transport::pipes::{self, PipeWriter};
use crate::transport::wire::{self, Wire};
use crate::transport::{self, join};

/// The server: over its standard input and output, its control connection,
/// it shakes hands and opens a socket for its stream to reach it, or, given
/// `port`, a port on loopback, then runs
/// the peer the client asked for over the stream to the other server. On a
/// host, the monitor started it, on a connection no shell can write to; on
/// this machine, the client did, through pipes. It prints nothing but its
/// packets; a failure goes to the client. Once it has answered the hello,
/// its control connection goes over the stream layer, its packets and its
/// end: it lives until the client ends it, once the peer is done. It is a
/// worker of its monitor, the host's or the client's: it ends as it is told
/// to stop, as its control connection ends early, or on SIGTERM; as its
/// monitor is gone, its lifeline breaking; or as it is done. Whether the far
/// end is there is its monitor's to judge, not its own. With
/// `sudo`, it was started through `sudo`, and its stream joins as the user
/// who ran it.
pub fn serve(build: &str, sudo: bool, port: bool, lifeline: bool) -> Result<()> {
    LIFELINE.store(lifeline, Ordering::Relaxed);
    // Its monitor gone, however it went, the server stops, as on SIGTERM.
    let own = lifeline
        .then(|| transport::lifeline::watch(|| interrupt::stop(1)))
        .transpose()
        .context("watch the lifeline")?;
    let mut joining = None;
    // Standard input and output, a wire, written as they are, so that the
    // answer goes in one write, which a pipe keeps whole; Rust's own
    // standard output would write up to each newline in it apart.
    let mut control = Wire::standard(&transport::scope::Scope::new()?)?;
    let hello = handshake::server_handshake(&mut control, build, |hello| {
        let listening = join::Listening::open(sudo, !port, port)?;
        let welcome = Welcome {
            socket: listening
                .socket()
                .map(|socket| socket.as_os_str().as_bytes().to_vec())
                .unwrap_or_default(),
            port: listening.port().map(|(number, secret)| Port {
                number,
                secret: secret.to_owned(),
            }),
            root: fsys::root_identity(OsStr::from_bytes(&hello.root)),
        };
        joining = Some(listening);
        Ok(welcome)
    })?;
    let joining = joining.expect("the handshake got ready for the stream");
    // The client goes on the control connection's pipes once it has the
    // answer, and so does the server: control both ways, and its reports.
    let mut to_client = pipes::Connection::open(
        vec![control],
        &pipes::control_connection(
            true,
            pipes::REPORTS_WINDOW,
            hello.compress.then_some(hello.compress_level),
        ),
    )
    .context("go on the control connection")?;
    let mut control = to_client.reader(pipes::CONTROL);
    let reports = to_client.writer(pipes::REPORTS);
    let to_client_ref = &to_client;
    let served = (move || -> Result<()> {
        // Before any stage starts, so that each runs on them.
        crate::cpus::limit((hello.cpus > 0).then_some(hello.cpus as usize), hello.place);
        // The client checks the two roots against each other, then says to go
        // on. One that refuses ends the control connection instead: the server
        // ends, having changed nothing.
        let (guard, streams) =
            match protocol::receive(&mut control).context("wait for the client to go on")? {
                None => return Ok(()),
                Some(packet) => match packet.decode::<Control>()? {
                    Control::Proceed(Proceed { guard, streams }) => (guard, streams),
                    other => bail!("expected the client to go on, got {other:?}"),
                },
            };
        if !hello.profile {
            profile::disable();
        }
        // What the server tells the client goes on control from now on.
        let _ = CONTROL.set(to_client_ref.writer(pipes::CONTROL));
        if own == Some(transport::lifeline::Own::Unreliable) {
            let _ = tell(&Report::Said(UNRELIABLE.to_owned()));
        }
        // Ended once the client ends the control connection.
        let (closing, closed) = crossbeam_channel::bounded::<()>(0);
        std::thread::spawn(move || {
            listen(&mut control);
            // Once the peer is done, the client ends it, and the server ends as
            // it would have; before, the client is gone, and the server stops.
            if !DONE.load(Ordering::SeqCst) {
                interrupt::stop(1);
            }
            drop(closing);
        });
        // What the stream carried goes on the control connection every second
        // from now on, for the dashboard.
        std::thread::spawn(report_carried);
        // What the peer reports goes on the reports' pipe, as the client shows
        // it, waiting there for room, as a dry run's listing does.
        if profile::enabled() {
            std::thread::spawn(report_metrics);
        }
        // Only the streams come on the socket, each once, by its number: what
        // else does is let go, as is one on which nothing comes. One that ends
        // before it says which it is, a stream cut short, fails the server, as
        // one that ends before its start mark does: the other side then ends,
        // whose stream breaks with it, rather than wait for the stream never to
        // come.
        let scope = transport::scope::Scope::new()?;
        let count = usize::from(streams.clamp(1, join::STREAMS));
        let mut taken_wires: Vec<Option<Wire>> = (0..count).map(|_| None).collect();
        // How the stream is compressed, as its first says: all are alike.
        let mut compress = None;
        while taken_wires.iter().any(Option::is_none) {
            let wire = match joining.accept(&scope) {
                Err(error) if error.is::<join::Silent>() => continue,
                accepted => accepted?.1,
            };
            let taken = match taken(wire) {
                Ok(taken) => taken,
                Err(error) => {
                    let _ = tell(&Report::Failed(format!("{error:#}")));
                    return Err(error);
                }
            };
            if let Taken::Stream(index, level, wire) = taken
                && let Some(slot) = taken_wires.get_mut(usize::from(index))
                && slot.is_none()
            {
                if index == 0 {
                    compress = level;
                }
                *slot = Some(wire);
            }
        }
        // The streams are one connection, whose pipes the peers read and write:
        // the other side's start mark found on each, past what a host's shell
        // printed. One that never starts, as one cut short before that mark
        // came, fails the server, which tells the client why.
        let mut wires = Vec::new();
        for (index, mut wire) in taken_wires.into_iter().flatten().enumerate() {
            if let Err(error) = wire.started() {
                let error = anyhow::Error::from(error).context(if count == 1 {
                    "start the stream".to_owned()
                } else {
                    format!("start stream {index}")
                });
                let _ = tell(&Report::Failed(format!("{error:#}")));
                return Err(error);
            }
            wires.push(wire);
        }
        let windows = pipes::Windows {
            manifest: match hello.paced_bytes {
                0 => pipes::Windows::default().manifest,
                bytes => bytes,
            },
            data: protocol::data_credit(hello.data_credit).min(u64::from(u32::MAX)) as u32,
            ..pipes::Windows::default()
        };
        let source = hello.role == Role::Source;
        let mut connection =
            pipes::Connection::open(wires, &pipes::stream(source, windows, compress))
                .context("go on the stream")?;
        let _ = CARRIED.set(connection.carried());
        let control_out = connection.writer(pipes::CONTROL);
        let peer = if source {
            let (answers, control) = (
                connection.reader(pipes::ANSWERS),
                connection.reader(pipes::CONTROL),
            );
            let stoppers = vec![answers.stopper(), control.stopper()];
            let data = connection.writer(pipes::DATA);
            let gauge = data.gauge();
            Peer::Source(
                source::Reading {
                    answers: Box::new(answers),
                    control: Box::new(control),
                },
                Mux::for_source(
                    Watching(control_out.clone()),
                    connection.writer(pipes::MANIFEST),
                    data,
                    Some(gauge),
                    stoppers,
                ),
            )
        } else {
            let (manifest, data, control) = (
                connection.reader(pipes::MANIFEST),
                connection.reader(pipes::DATA),
                connection.reader(pipes::CONTROL),
            );
            let stoppers = vec![manifest.stopper(), data.stopper(), control.stopper()];
            Peer::Destination(
                destination::Reading {
                    manifest: Box::new(manifest),
                    data: Box::new(data),
                    control: Box::new(control),
                },
                Mux::for_destination(
                    Watching(control_out.clone()),
                    connection.writer(pipes::ANSWERS),
                    stoppers,
                ),
            )
        };
        let result = run(
            hello,
            guard,
            peer,
            Reporter::to_pipe(reports.clone()),
            compress,
        );
        // A failure the peer already told, or that came from the other side, is
        // not told again.
        if let Err(error) = &result
            && !error.is::<Reported>()
        {
            // The client may already be gone; the error is returned either way.
            let message = format!("{error:#}");
            if let Ok(packet) = protocol::encode(&Control::Error { message }) {
                let _ = mux::Out::write_all(&Watching(control_out), &packet);
            }
        }
        // Each pipe's end, once all written before it went: the other side reads
        // to them. Should the connection be broken already, nothing more goes.
        let _ = connection.finish();
        // Where the time went, once the peer is done, the last of its reports:
        // the client shows it, if asked to, and reads the reports to their end.
        if profile::enabled()
            && let Ok(packet) = protocol::encode(&Report::Profile(profile::collect()))
        {
            let _ = reports.write_all(&packet);
        }
        reports.close();
        // From here, the control connection ending is no longer the client
        // going. The client is told the server is done, and the server ends as
        // the client ends the control connection; then its own end goes, so
        // that the client reads to it.
        DONE.store(true, Ordering::SeqCst);
        let _ = tell(&Report::Done);
        let _ = closed.recv();
        drop(connection);
        drop(joining);
        result
    })();
    // However the server ends, what it said goes first, then each pipe's
    // end, which the client reads to.
    let _ = to_client.finish();
    drop(to_client);
    served
}

/// What a server whose lifeline cannot be watched says, as where `sudo`
/// logs what it says, between it and its monitor.
const UNRELIABLE: &str = "warning: something stands between this server's standard error and \
     Ferry's monitor, as sudo does where it logs what a command says: should the monitor end \
     while the network drops too, this server would not end on its own";

/// What came on the server's socket.
enum Taken {
    /// One of the streams, by its number, and how it is compressed.
    Stream(u8, Option<i32>, Wire),
    /// What cannot be read as the stream: let go.
    Other,
}

/// Reads what came on the server's socket, by its header: a stream that
/// ends, or breaks, before it says which it is, fails.
fn taken(mut wire: Wire) -> Result<Taken> {
    match join::read_header(&mut wire) {
        Ok((join::Kind::Stream(index), compress)) => Ok(Taken::Stream(index, compress, wire)),
        Err(error) => match error.downcast_ref::<io::Error>() {
            Some(cut) if cut.kind() == io::ErrorKind::UnexpectedEof => {
                bail!("a stream ended before it said which one it is")
            }
            Some(_) => Err(error.context("a stream broke before it said which one it is")),
            None => Ok(Taken::Other),
        },
    }
}

/// The peer's control pipe, on which it says it failed: that tells the
/// client first, as the transfer's cause, should it be the first. Each write
/// is a packet, whole.
struct Watching(PipeWriter);

impl mux::Out for Watching {
    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        if let Ok(Control::Error { message }) = protocol::expect::<Control>(&mut &bytes[..]) {
            let _ = tell(&Report::Failed(message));
        }
        self.0.write_all(bytes)
    }
}

/// How often the client is told what the stream carried, for its dashboard.
const CARRIED_EVERY: Duration = Duration::from_secs(1);

/// What the streams carried, once they have started.
static CARRIED: OnceLock<Vec<wire::Carried>> = OnceLock::new();

/// The server was given a lifeline, its standard error, which its monitor
/// reads.
static LIFELINE: AtomicBool = AtomicBool::new(false);

/// Tells the client what the stream carried, every [`CARRIED_EVERY`], for
/// its dashboard, nothing before it starts, until the peer is done or the
/// client gone; and, on its lifeline, its monitor how long ago anything
/// came on the stream: each counts that as word from the far end, which a
/// heartbeat waiting behind the data on a slow connection may be slow to
/// bring.
fn report_carried() {
    loop {
        std::thread::sleep(CARRIED_EVERY);
        if DONE.load(Ordering::SeqCst) {
            return;
        }
        // All the streams' bytes, and the latest anything came on any.
        let (bytes, heard) = CARRIED.get().map_or((0, None), |carried| {
            carried.iter().map(wire::Carried::now).fold(
                (0, None),
                |(bytes, heard), (more, came)| {
                    let heard = match (heard, came) {
                        (Some(heard), Some(came)) => Some(std::cmp::min(heard, came)),
                        (heard, came) => heard.or(came),
                    };
                    (bytes + more, heard)
                },
            )
        });
        if let Some(heard) = heard
            && LIFELINE.load(Ordering::Relaxed)
        {
            transport::lifeline::tell_heard(heard);
        }
        let report = Report::Carried(report::Carried {
            bytes,
            heard: heard.map(|heard| heard.as_millis() as u64),
        });
        if tell(&report).is_err() {
            return;
        }
    }
}

/// Answers what the client asks on the control connection, a snapshot of
/// where the threads are, until the connection ends, or is taken for broken.
fn listen(reader: &mut impl Read) {
    while let Ok(Some(packet)) = protocol::receive(reader) {
        if let Ok(Control::Snapshot) = packet.decode::<Control>() {
            let _ = tell(&Report::Snapshot(profile::snapshot()));
        }
    }
}

/// The control pipe of the control connection, once the client said to go
/// on.
static CONTROL: OnceLock<PipeWriter> = OnceLock::new();

/// Sends the client a report on control, whole, whichever thread sends it:
/// it never waits.
fn tell(report: &Report) -> Result<()> {
    let control = CONTROL
        .get()
        .context("the control connection has no pipe yet")?;
    control.write_all(&protocol::encode(report)?)?;
    Ok(())
}

/// How long a server's internal error may take to go on the wire, as the
/// server ends at once.
const TELL_INTERNAL: Duration = Duration::from_secs(1);

/// Tells the client `message`, to show it, as a server's own internal error,
/// without waiting for control should another thread be writing it, and
/// waits a while for it to go; false if the client could not be told, as on
/// the client itself.
pub fn tell_internal(message: &str) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    let Ok(packet) = protocol::encode(&Report::Said(message.to_owned())) else {
        return false;
    };
    control.try_write_all(&packet) && control.wait_sent(TELL_INTERNAL)
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

/// The peer the client asked for, with the pipes it reads and where it
/// writes its own.
enum Peer {
    Source(source::Reading, Mux),
    Destination(destination::Reading, Mux),
}

/// Runs `peer`, as `hello` asks, kept by `guard` from what it must never go
/// into or delete, reporting to `reporter`, the stream compressed at
/// `compress`, if it is.
fn run(
    hello: Hello,
    guard: Guard,
    peer: Peer,
    reporter: Reporter,
    compress: Option<i32>,
) -> Result<()> {
    match peer {
        Peer::Destination(reading, mux) => {
            let options = destination::Options {
                root: hello.root.clone(),
                dry_run: hello.dry_run,
                modify_window: hello.modify_window,
                numeric_ids: hello.numeric_ids,
                delete: hello.delete,
                report: reporter,
                protect: guard.protect,
                batch_entries: hello.batch_entries as usize,
                data_credit: hello.data_credit,
                one_file_system: hello.one_file_system,
            };
            destination::run_on_pipes(reading, mux, &options).map(|_| ())
        }
        Peer::Source(reading, mux) => {
            let options = source::Options {
                root: OsStr::from_bytes(&hello.root),
                dry_run: hello.dry_run,
                numeric_ids: hello.numeric_ids,
                compress: compress.is_some(),
                compress_level: compress.unwrap_or(1),
                report: reporter,
                batch_entries: hello.batch_entries as usize,
                backlog_files: hello.backlog_files as usize,
                avoid: guard.avoid,
                one_file_system: hello.one_file_system,
            };
            source::run_on_pipes(reading, mux, &options).map(|_| ())
        }
    }
}
