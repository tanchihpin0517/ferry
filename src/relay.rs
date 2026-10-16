//! The client between a transfer's two servers: it passes on what one sends
//! to the other, and shows what they report.
//!
//! One thread for each direction reads packets as they come. A report is for
//! the client, and goes to the display; anything else is written to the other
//! server as it is, without being decoded. Each thread reads until its server
//! closes, even once the other server is gone, so that no server ever waits
//! to write. A server closing passes on as the other's input closing, which is
//! how each learns that the other is done.
//!
//! On `Ctrl-\`, or when nothing has passed between the servers for
//! [`STILL`], the client says where every thread is: its own, and, asked on
//! their control connections, the servers'.

use std::io::BufReader;
use std::process::ChildStdout;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::interrupt;
use crate::profile::{self, Profile};
use crate::protocol::{self, Control, PacketType};
use crate::report::Report;
use crate::transport::{
    Connection, ControlConnection, ControlSender, Receiving, Sending, Transport,
};

/// How long nothing may pass between the servers before the client says
/// where every thread is.
const STILL: Duration = Duration::from_secs(60);

/// One side of a transfer: its server's control connection and connection,
/// the transport they go through, and how to name it.
pub struct Side {
    pub name: String,
    pub control: ControlConnection,
    pub connection: Connection,
    /// Kept open until the transfer is over.
    pub transport: Transport,
}

/// Relays between the Source's server and the Destination's until both are
/// done, sending their reports to `reports`, but for their profiles, which go
/// to `profiles` with the name of their side. Fails with the first error
/// either server sent, or how a server ended if it ended badly.
pub fn relay(
    mut source: Side,
    mut destination: Side,
    reports: &Sender<Report>,
    profiles: &Mutex<Vec<(String, Profile)>>,
) -> Result<()> {
    for side in [&mut source, &mut destination] {
        if let Some(reader) = side.control.take_reader() {
            listen(reader, side.name.clone(), reports.clone());
        }
    }
    let senders = [source.control.sender(), destination.control.sender()];
    let (source_receiving, source_sending, source_paths) = source.connection.split();
    let (destination_receiving, destination_sending, destination_paths) =
        destination.connection.split();
    let failure = Mutex::new(None);
    // Packets passed in each direction, for the watcher to see them move.
    let passed = [AtomicU64::new(0), AtomicU64::new(0)];
    std::thread::scope(|scope| {
        let (relaying, done) = mpsc::channel::<()>();
        scope.spawn(|| watch(done, &passed, &senders, reports));
        let from_source = scope.spawn(|| {
            let _thread = profile::thread("relay from the Source");
            pass(
                source_receiving,
                destination_sending,
                reports,
                profiles,
                &source.name,
                &failure,
                &passed[0],
            )
        });
        {
            let _thread = profile::thread("relay from the Destination");
            pass(
                destination_receiving,
                source_sending,
                reports,
                profiles,
                &destination.name,
                &failure,
                &passed[1],
            );
        }
        from_source.join().expect("the relay panicked");
        drop(relaying);
    });
    // After a failure, a server whose SSH connection is gone never says it
    // ended: it is given a moment, then let go.
    let failed = failure.lock().expect("not poisoned").is_some();
    let close = |control: ControlConnection| {
        if failed {
            control.close_within(Duration::from_secs(5))
        } else {
            control.close()
        }
    };
    let ended = [
        source_paths
            .wait()
            .and_then(|()| close(source.control))
            .map_err(|error| anyhow!("{}: {error:#}", source.name)),
        destination_paths
            .wait()
            .and_then(|()| close(destination.control))
            .map_err(|error| anyhow!("{}: {error:#}", destination.name)),
    ];
    if let Some(failure) = failure.into_inner().expect("not poisoned") {
        return Err(anyhow!(failure));
    }
    let [source_ended, destination_ended] = ended;
    source_ended?;
    destination_ended?;
    Ok(())
}

/// Passes on messages from one server to the other until the first closes,
/// then closes the other's input. Records the first error a server sends.
fn pass(
    mut from: Receiving,
    to: Sending,
    reports: &Sender<Report>,
    profiles: &Mutex<Vec<(String, Profile)>>,
    name: &str,
    failure: &Mutex<Option<String>>,
    passed: &AtomicU64,
) {
    let fail = |message: String| {
        failure.lock().expect("not poisoned").get_or_insert(message);
    };
    let mut to = Some(to);
    loop {
        let packet = match from.recv() {
            Ok(Some(packet)) => {
                // Only this thread adds: no read-modify-write.
                passed.store(passed.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                packet
            }
            Ok(None) => break,
            Err(error) => {
                fail(format!("{name}: {error:#}"));
                // Whatever else comes from it cannot be read as messages.
                from.drain();
                break;
            }
        };
        match packet.kind {
            PacketType::Report => {
                match packet.decode::<Report>() {
                    Ok(Report::Profile(profile)) => {
                        let mut profiles = profiles.lock().expect("not poisoned");
                        profiles.push((name.to_owned(), profile));
                    }
                    // No one showing reports any more is no reason to stop.
                    Ok(report) => drop(reports.send(report)),
                    Err(error) => fail(format!("{name}: {error:#}")),
                }
                continue;
            }
            PacketType::Control => {
                if let Ok(Control::Error { message }) = packet.decode::<Control>() {
                    fail(format!("{name} failed: {message}"));
                }
            }
            _ => {}
        }
        if let Some(sending) = &mut to
            && sending.send(packet).is_err()
        {
            // The other server is gone; its own end says why.
            to = None;
        }
    }
    if let Some(sending) = to {
        let _ = sending.close();
    }
}

/// Until the relay is done, says where every thread is when `Ctrl-\` asks,
/// or once nothing has passed for [`STILL`], once for each time it stands
/// still.
fn watch(
    done: Receiver<()>,
    passed: &[AtomicU64; 2],
    senders: &[ControlSender],
    reports: &Sender<Report>,
) {
    let count = || {
        passed
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .sum::<u64>()
    };
    let mut last = count();
    let mut moved = Instant::now();
    let mut told = false;
    loop {
        match done.recv_timeout(Duration::from_millis(250)) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        let now = count();
        if now != last {
            last = now;
            moved = Instant::now();
            told = false;
        }
        let why = if interrupt::quit_asked() {
            "where every thread is, as asked:".to_owned()
        } else if !told && moved.elapsed() >= STILL {
            told = true;
            format!(
                "nothing has passed between the servers for {} s; where every thread is:",
                moved.elapsed().as_secs()
            )
        } else {
            continue;
        };
        let _ = reports.send(Report::Said(why));
        for line in profile::describe("the client", &profile::snapshot()) {
            let _ = reports.send(Report::Said(line));
        }
        // Each answers on its control connection; see [`listen`].
        for sender in senders {
            let _ = sender.send(&Control::Snapshot);
        }
    }
}

/// Reads what a server says on its control connection, its snapshots, in a
/// thread of its own until the server ends, and reports them as lines.
fn listen(mut reader: BufReader<ChildStdout>, name: String, reports: Sender<Report>) {
    std::thread::spawn(move || {
        while let Ok(Some(packet)) = protocol::receive(&mut reader) {
            if let Ok(Report::Snapshot(threads)) = packet.decode::<Report>() {
                for line in profile::describe(&name, &threads) {
                    let _ = reports.send(Report::Said(line));
                }
            }
        }
    });
}
