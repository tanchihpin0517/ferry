//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and makes the streams between them, whose session
//! carries what one peer sends the other. Both peers report to the client
//! on the server's control connection, not among their messages to each
//! other.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::block::{self, Assembled};
use crate::destination;
use crate::fsys;
use crate::handshake;
use crate::interrupt;
use crate::mux;
use crate::profile::{self, State};
use crate::protocol::{self, BlocksEnd, Control, Guard, Hello, PacketType, Role, Welcome};
use crate::report::{Carried, Report, Reporter};
use crate::source;
use crate::stop::Reported;
use crate::transport::driver::{self, Delivery, Ends, Notice};
use crate::transport::session::{self, Bytes};
use crate::transport::{self, join};

/// The server: over its standard input and output, it shakes hands and opens
/// a socket for its streams to reach it, then runs the peer the client asked
/// for over its session with the other server, which the streams carry. The
/// control connection then goes on a channel of its own to the socket, since
/// a shell on the host, or what it left running, may write to the server's
/// output as well. It prints nothing more; a failure goes to the client. It
/// lives as long as its input: should that close first, the client is gone,
/// and the server stops. With `sudo`, it was started through `sudo`, and its
/// streams join as the user who ran it.
pub fn serve(build: &str, sudo: bool) -> Result<()> {
    let mut joining = None;
    // Standard output as it is, unbuffered, so that the answer goes in one
    // write, which a pipe keeps whole; Rust's own would write up to each
    // newline in it apart.
    let mut output = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let hello =
        handshake::server_handshake(&mut io::stdin().lock(), &mut output, build, |hello| {
            let listening = transport::listen(sudo)?;
            let welcome = Welcome {
                socket: listening.socket().as_os_str().as_bytes().to_vec(),
                root: fsys::root_identity(OsStr::from_bytes(&hello.root)),
            };
            joining = Some(listening);
            Ok(welcome)
        })?;
    let joining = joining.expect("the handshake got ready for the streams");
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
    // Closed once the client closes the control connection.
    let (closing, closed) = crossbeam_channel::bounded::<()>(0);
    std::thread::spawn(move || {
        // Nothing more comes on it; it only stays open.
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
        // Once the peer and the session are done, the client closes this,
        // and the server ends as it would have; before, the client is gone,
        // and the server stops.
        if !DONE.load(Ordering::SeqCst) {
            interrupt::stop(1);
        }
        drop(closing);
    });
    // The session with the other server, which the streams join as the
    // client makes them, before the control connection's channel comes or
    // after.
    let (session, deliveries, notices, driving) = driver::start(hello.session);
    let (mut control_input, control_output, held) = loop {
        if let Some(control) = accepted(joining.accept()?, &session)? {
            break control;
        }
    };
    *CONTROL.lock().expect("not poisoned") = Some(Box::new(control_output));
    std::thread::spawn(move || listen(&mut control_input));
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
    // Why a stream was taken for broken, when it broke the session's rules,
    // is for the client to say; and what each stream carried, for its
    // dashboard.
    std::thread::spawn(move || {
        for notice in notices {
            let report = match notice {
                Notice::Warn(why) => Report::Said(format!("warning: {why}")),
                Notice::Seen(seen) => Report::Streams(
                    seen.into_iter()
                        .map(|seen| Carried {
                            stream: seen.stream,
                            bytes: seen.bytes,
                            heard: seen.heard.map(|heard| heard.as_millis() as u64),
                        })
                        .collect(),
                ),
                Notice::Broke(stream) => Report::Broke(stream),
            };
            let _ = tell(&report);
        }
    });
    // Streams come, and come again, for as long as the server runs.
    let socket = joining.socket().to_owned();
    let stopping = Arc::new(AtomicBool::new(false));
    let accepting = {
        let (session, stopping) = (session.clone(), Arc::clone(&stopping));
        std::thread::spawn(move || accept(&joining, &session, &stopping))
    };
    // The peer reads and writes packets, as it always has: pipes join it to
    // the session, which carries its blocks as blocks, and every other
    // packet as a control message.
    let (peer_input, into_peer) = io::pipe()?;
    let (from_peer, peer_output) = io::pipe()?;
    for pipe in [into_peer.as_fd(), peer_output.as_fd()] {
        widen(pipe);
    }
    // A session that gives up is why the peer then fails: it is told before
    // the peer's input closes.
    let (failed, cause) = crossbeam_channel::unbounded();
    let inward = std::thread::spawn(move || {
        let _thread = profile::thread("inward");
        inward(&deliveries, into_peer, &failed);
    });
    let outward = {
        let session = session.clone();
        std::thread::spawn(move || -> Result<()> {
            let _thread = profile::thread("outward");
            outward(from_peer, &session, &cause)
        })
    };
    let mut writer = BufWriter::new(peer_output);
    let result = run(
        hello,
        guard,
        &mut BufReader::new(peer_input),
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
    // The peer's last reports, as its last counts, are written before
    // anything ends: the client reads them to the control connection's end.
    let _ = reporting.join();
    // Where the time went, once the peer is done: the client shows it, if
    // asked to.
    if profile::enabled() {
        let _ = tell(&Report::Profile(profile::collect()));
    }
    // The peer's output closes, so the last of it goes, and the session is
    // told this side is done; it ends once the other is too, or gives up.
    drop(writer);
    let sent = outward
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error in outward")));
    let _ = inward.join();
    // From here, the control connection closing is no longer the client
    // going. The client is told the server is done, and the server stays,
    // its session answering a stream joined after, should the other side
    // have missed its end, until the client closes the control connection.
    DONE.store(true, Ordering::SeqCst);
    let _ = tell(&Report::Done);
    let _ = closed.recv();
    stopping.store(true, Ordering::SeqCst);
    // Wakes the thread that takes the streams, which then lets go of the
    // socket, removing it.
    drop(UnixStream::connect(&socket));
    let _ = accepting.join();
    drop(session);
    let _ = driving.join();
    drop(held);
    result.and(sent)
}

/// Gives a pipe room for a block, where the system lets it: blocks go
/// through whole, 2 MiB each, which a pipe of the usual 64 KiB passes a
/// sixteenth at a time, its two ends waking each other for each.
fn widen(pipe: std::os::fd::BorrowedFd) {
    #[cfg(target_os = "linux")]
    // SAFETY: F_SETPIPE_SZ takes a size, and only fails on a pipe it may
    // not make as large, which is left as it was.
    unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(&pipe),
            libc::F_SETPIPE_SZ,
            1 << 20,
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pipe;
}

/// Takes a stream or a channel that came: a stream is handed to the
/// session, numbered as its header says; the control connection's channel
/// is returned, with what was held for it.
fn accepted(
    joined: transport::Joined,
    session: &driver::Handle,
) -> Result<Option<(File, File, Option<UnixStream>)>> {
    let transport::Joined {
        input,
        output,
        held,
    } = joined;
    let mut input = File::from(input);
    let output = File::from(output);
    match join::read_header(&mut input)? {
        join::Kind::Stream(number) => {
            session.join(
                number,
                Ends {
                    input,
                    output,
                    held,
                    marked: true,
                },
            );
            Ok(None)
        }
        join::Kind::Control => Ok(Some((input, output, held))),
    }
}

/// Takes the streams that come, and hands each to the session, until
/// `stopping`; a second control connection's channel is dropped.
fn accept(joining: &transport::Joining, session: &driver::Handle, stopping: &AtomicBool) {
    let _thread = profile::thread("join");
    loop {
        let joined = profile::wait(State::Work, || joining.accept());
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        // One that cannot be taken is no stream; the others go on.
        if joined.and_then(|joined| accepted(joined, session)).is_err() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Hands the session what the peer writes, until the peer is done: each
/// packet as a control message, but for the blocks of files, each whole a
/// block of the session's, and their end, which says how many there were.
/// Once the session has given up, as `cause` says, the peer's error is that
/// instead; and the client is told the peer failed before anyone else.
fn outward(
    from_peer: io::PipeReader,
    session: &driver::Handle,
    cause: &Receiver<String>,
) -> Result<()> {
    let mut from_peer = BufReader::new(from_peer);
    let mut assembler = block::Assembler::default();
    let mut blocks = 0;
    let mut failure = None;
    while let Some(packet) = profile::wait(State::ReadPeer, || protocol::receive(&mut from_peer))? {
        let packet = match packet.kind {
            PacketType::Data => {
                for assembled in assembler.push(&packet.into_payload())? {
                    match assembled {
                        Assembled::Block(block) => {
                            session.block(Bytes::new(block));
                            blocks += 1;
                        }
                        Assembled::End => session.control(protocol::encode(&BlocksEnd { blocks })?),
                    }
                }
                continue;
            }
            PacketType::Control => match packet.decode::<Control>() {
                Ok(Control::Error { message }) => {
                    if let Ok(error) = cause.try_recv() {
                        failure = Some(error);
                    }
                    let message = failure.clone().unwrap_or(message);
                    let _ = tell(&Report::Failed(message.clone()));
                    protocol::encode(&Control::Error { message })?
                }
                _ => forwarded(&packet)?,
            },
            _ => forwarded(&packet)?,
        };
        session.control(packet);
    }
    // A session that gave up under a peer that did not see it, as one done
    // but for its input, still fails the server, and says so.
    if failure.is_none()
        && let Ok(error) = cause.try_recv()
    {
        let _ = tell(&Report::Failed(error.clone()));
        session.control(protocol::encode(&Control::Error {
            message: error.clone(),
        })?);
        session.close();
        bail!(error);
    }
    session.close();
    Ok(())
}

/// A packet as read, whole again.
fn forwarded(packet: &protocol::Packet) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(5 + packet.size());
    protocol::forward(&mut bytes, packet)?;
    Ok(bytes)
}

/// Hands the peer what the session delivers, as packets, until the session
/// ends: each control message as it is, and each block as a data packet. The
/// end of the blocks goes once as many have come as the other server said;
/// the peer's input ends once the other peer is done. A session that gives
/// up is why the peer then fails: told on `failed` before its input ends.
fn inward(deliveries: &Receiver<Delivery>, into_peer: io::PipeWriter, failed: &Sender<String>) {
    let mut writer = Some(BufWriter::new(into_peer));
    let write = |writer: &mut Option<BufWriter<io::PipeWriter>>, parts: &[&[u8]]| {
        // A peer that stopped reading says why itself; what comes after is
        // dropped.
        if let Some(open) = writer
            && profile::wait(State::WritePeer, || {
                parts.iter().try_for_each(|part| open.write_all(part))
            })
            .is_err()
        {
            *writer = None;
        }
    };
    let mut blocks = 0;
    // How many blocks the other server said it sent, until their end is
    // handed on.
    let mut ending = None;
    while let Ok(delivery) = profile::wait(State::Work, || deliveries.recv()) {
        match delivery {
            Delivery::Control(message) => {
                if message.first() == Some(&(PacketType::BlocksEnd as u8)) {
                    match protocol::expect::<BlocksEnd>(&mut &message[..]) {
                        Ok(BlocksEnd { blocks }) => ending = Some(blocks),
                        Err(_) => writer = None,
                    }
                } else {
                    write(&mut writer, &[&message]);
                }
            }
            Delivery::Block(bytes) => {
                let length = u32::try_from(bytes.len()).expect("a block is far smaller");
                write(
                    &mut writer,
                    &[&[PacketType::Data as u8], &length.to_le_bytes(), &bytes],
                );
                blocks += 1;
            }
            // All the other peer sent is delivered: the peer reads to its
            // input's end.
            Delivery::Ended => writer = None,
            Delivery::Lost => {
                let _ = failed.send(format!(
                    "no stream reached the other side for {} s",
                    session::GRACE.as_secs()
                ));
                writer = None;
            }
            Delivery::Closed => break,
        }
        if ending == Some(blocks) {
            let length = block::END.len() as u32;
            write(
                &mut writer,
                &[
                    &[PacketType::Data as u8],
                    &length.to_le_bytes(),
                    &block::END,
                ],
            );
            ending = None;
        }
        if deliveries.is_empty()
            && let Some(open) = &mut writer
            && open.flush().is_err()
        {
            writer = None;
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

/// Runs the peer `hello` asks for over a path, kept by `guard` from what it
/// must never go into or delete, reporting to `reporter`.
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
