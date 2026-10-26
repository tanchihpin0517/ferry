//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and makes the stream between them, which carries
//! what one peer sends the other. Both peers report to the client on the
//! server's control connection, not among their messages to each other.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::destination;
use crate::fsys;
use crate::handshake;
use crate::interrupt;
use crate::mux;
use crate::profile;
use crate::protocol::{self, Control, Guard, Hello, PacketType, Port, Role, Welcome};
use crate::report::{self, Report, Reporter};
use crate::source;
use crate::stop::Reported;
use crate::transport::stream::{self, Ends};
use crate::transport::{self, join};

/// The server: over its standard input and output, its control connection,
/// it shakes hands and opens a socket for its stream to reach it, or, given
/// `port`, a port on loopback, then runs
/// the peer the client asked for over the stream to the other server. On a
/// host, the launcher started it, on a connection no shell can write to; on
/// this machine, the client did, through pipes. It prints nothing but its
/// packets; a failure goes to the client. It lives as long as its input:
/// should that close first, the client is gone, and the server stops. With
/// `sudo`, it was started through `sudo`, and its stream joins as the user
/// who ran it.
pub fn serve(build: &str, sudo: bool, port: bool) -> Result<()> {
    let mut joining = None;
    // Standard output as it is, unbuffered, so that the answer goes in one
    // write, which a pipe keeps whole; Rust's own would write up to each
    // newline in it apart.
    let mut output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let hello =
        handshake::server_handshake(&mut io::stdin().lock(), &mut output, build, |hello| {
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
    // Reports go on the control connection from now on.
    *CONTROL.lock().expect("not poisoned") = Some(Box::new(output));
    // Closed once the client closes the control connection.
    let (closing, closed) = crossbeam_channel::bounded::<()>(0);
    std::thread::spawn(move || {
        listen(&mut io::stdin().lock());
        // Once the peer is done, the client closes it, and the server ends
        // as it would have; before, the client is gone, and the server stops.
        if !DONE.load(Ordering::SeqCst) {
            interrupt::stop(1);
        }
        drop(closing);
    });
    // Something goes on the control connection every second from now on,
    // for the client to know the server is there.
    std::thread::spawn(report_carried);
    // What the peer reports goes on the control connection, as the client
    // shows it; a dry run's listing waits there for room, as it would among
    // the peer's messages.
    let (reports, outlet) = mux::channel_for_reports();
    let reporting = std::thread::spawn(move || {
        let _thread = profile::thread("reports");
        mux::pump(&mut Shared, outlet)
    });
    if profile::enabled() {
        std::thread::spawn(report_metrics);
    }
    // Only the stream comes on the socket: what else does is let go.
    let ends = loop {
        if let Taken::Stream(ends) = taken(joining.accept()?.1) {
            break ends;
        }
    };
    // The peer reads and writes its packets on the stream, as they are; the
    // stream's own keepalives and end it never sees. One that never starts,
    // as one cut short before the other side's start mark came, fails the
    // server, which tells the client why.
    let (mut reading, writing, carried) = match stream::start(ends) {
        Ok(started) => started,
        Err(error) => {
            let error = anyhow::Error::from(error).context("start the stream");
            let _ = tell(&Report::Failed(format!("{error:#}")));
            return Err(error);
        }
    };
    let _ = CARRIED.set(carried);
    let mut writer = Watching::new(BufWriter::new(writing));
    let result = run(
        hello,
        guard,
        &mut reading,
        &mut writer,
        Reporter::to_mux(reports),
    );
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
    // The peer's last packets, then the stream's end, which the other side
    // reads to: should the stream be broken already, nothing more goes.
    let _ = writer.finish();
    // The peer's last reports, as its last counts, are written before
    // anything ends: the client reads them to the control connection's end.
    let _ = reporting.join();
    // Where the time went, once the peer is done: the client shows it, if
    // asked to.
    if profile::enabled() {
        let _ = tell(&Report::Profile(profile::collect()));
    }
    // From here, the control connection closing is no longer the client
    // going. The client is told the server is done, and the server ends as
    // the client closes the control connection, or once it has waited
    // [`CLOSING`] for it.
    DONE.store(true, Ordering::SeqCst);
    let _ = tell(&Report::Done);
    let _ = closed.recv_timeout(CLOSING);
    drop(reading);
    drop(joining);
    result
}

/// What came on the server's socket.
enum Taken {
    /// The stream.
    Stream(Ends),
    /// What cannot be read as the stream: let go.
    Other,
}

/// Reads what came on the server's socket, by its header.
fn taken(joined: transport::Joined) -> Taken {
    let transport::Joined {
        input,
        output,
        held,
    } = joined;
    let mut input = File::from(input);
    let output = File::from(output);
    match join::read_header(&mut input) {
        Ok(join::Kind::Stream) => Taken::Stream(Ends {
            input,
            output,
            held,
            marked: true,
            before: Vec::new(),
        }),
        Err(_) => Taken::Other,
    }
}

/// The peer's packets, on their way to the stream: one that says the peer
/// failed tells the client first, as the transfer's cause, should it be the
/// first. Only control messages are looked into; of the others, only their
/// headers.
struct Watching<W> {
    inner: W,
    /// The header of the packet being written, as much of it as was, and
    /// what is left of its payload.
    header: [u8; 5],
    filled: usize,
    left: usize,
    /// A control message, whole, as it is written.
    control: Option<Vec<u8>>,
}

impl<W> Watching<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            header: [0; 5],
            filled: 0,
            left: 0,
            control: None,
        }
    }

    /// Follows the packets through `bytes`, just written.
    fn follow(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.left > 0 {
                let taken = self.left.min(bytes.len());
                if let Some(control) = &mut self.control {
                    control.extend_from_slice(&bytes[..taken]);
                }
                self.left -= taken;
                bytes = &bytes[taken..];
                if self.left == 0 {
                    self.whole();
                }
                continue;
            }
            let taken = (self.header.len() - self.filled).min(bytes.len());
            self.header[self.filled..self.filled + taken].copy_from_slice(&bytes[..taken]);
            self.filled += taken;
            bytes = &bytes[taken..];
            if self.filled == self.header.len() {
                self.filled = 0;
                self.left =
                    u32::from_le_bytes(self.header[1..].try_into().expect("four bytes")) as usize;
                self.control =
                    (self.header[0] == PacketType::Control as u8).then(|| self.header.to_vec());
                if self.left == 0 {
                    self.whole();
                }
            }
        }
    }

    /// A packet went whole: a control message that says the peer failed is
    /// told the client.
    fn whole(&mut self) {
        if let Some(control) = self.control.take()
            && let Ok(Control::Error { message }) = protocol::expect::<Control>(&mut &control[..])
        {
            let _ = tell(&Report::Failed(message));
        }
    }
}

impl<W: Write> Write for Watching<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.follow(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Watching<BufWriter<stream::Writing>> {
    /// Writes what is left, then the stream's end.
    fn finish(self) -> io::Result<()> {
        self.inner
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?
            .finish()
    }
}

/// How long a server done waits for the client to close its control
/// connection: the client does once both servers are done, or soon after;
/// one gone without a word is not waited for longer than this.
const CLOSING: Duration = stream::SILENCE;

/// How often the client is told what the stream carried: also how it knows
/// the server is there, as something goes on the control connection.
const CARRIED_EVERY: Duration = Duration::from_secs(1);

/// What the stream carried, once it has started.
static CARRIED: OnceLock<stream::Carried> = OnceLock::new();

/// Tells the client what the stream carried, every [`CARRIED_EVERY`], for
/// its dashboard, nothing before it starts, until the peer is done or the
/// client gone.
fn report_carried() {
    loop {
        std::thread::sleep(CARRIED_EVERY);
        if DONE.load(Ordering::SeqCst) {
            return;
        }
        let (bytes, heard) = CARRIED.get().map_or((0, None), stream::Carried::now);
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

/// The control connection's channel, as the reports' pump writes it: each
/// packet whole under its lock, so that what another thread tells goes
/// between two, never inside one.
struct Shared;

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut control = CONTROL.lock().expect("not poisoned");
        let out = control
            .as_mut()
            .ok_or_else(|| io::Error::other("the control connection has no channel"))?;
        out.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut control = CONTROL.lock().expect("not poisoned");
        control.as_mut().map_or(Ok(()), |out| out.flush())
    }
}

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

/// Runs the peer `hello` asks for over `reader` and `writer`, the stream's
/// two ends, kept by `guard` from what it must never go into or delete,
/// reporting to `reporter`.
fn run(
    hello: Hello,
    guard: Guard,
    reader: &mut impl Read,
    writer: &mut (impl Write + Send),
    reporter: Reporter,
) -> Result<()> {
    match hello.role {
        Role::Destination => {
            let options = destination::Options {
                root: hello.root.clone(),
                dry_run: hello.dry_run,
                modify_window: hello.modify_window,
                numeric_ids: hello.numeric_ids,
                delete: hello.delete,
                report: reporter,
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
                report: reporter,
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
