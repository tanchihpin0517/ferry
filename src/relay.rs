//! The client between a transfer's two servers: it passes on what one sends
//! to the other, and shows what they report.
//!
//! One thread for each direction reads packets as they come. A report is for
//! the client, and goes to the display; anything else is written to the other
//! server as it is, without being decoded. Each thread reads until its server
//! closes, even once the other server is gone, so that no server ever waits
//! to write. A server closing passes on as the other's input closing, which is
//! how each learns that the other is done.

use std::sync::Mutex;
use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow};

use crate::protocol::{Control, PacketType};
use crate::report::Report;
use crate::transport::{Connection, ControlConnection, Receiving, Sending, Transport};

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
/// done, sending their reports to `reports`. Fails with the first error
/// either server sent, or how a server ended if it ended badly.
pub fn relay(source: Side, destination: Side, reports: &Sender<Report>) -> Result<()> {
    let (source_receiving, source_sending, source_paths) = source.connection.split();
    let (destination_receiving, destination_sending, destination_paths) =
        destination.connection.split();
    let failure = Mutex::new(None);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            pass(
                source_receiving,
                destination_sending,
                reports,
                &source.name,
                &failure,
            )
        });
        pass(
            destination_receiving,
            source_sending,
            reports,
            &destination.name,
            &failure,
        );
    });
    let ended = [
        source_paths
            .wait()
            .and_then(|()| source.control.close())
            .map_err(|error| anyhow!("{}: {error:#}", source.name)),
        destination_paths
            .wait()
            .and_then(|()| destination.control.close())
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
    name: &str,
    failure: &Mutex<Option<String>>,
) {
    let fail = |message: String| {
        failure.lock().expect("not poisoned").get_or_insert(message);
    };
    let mut to = Some(to);
    loop {
        let packet = match from.recv() {
            Ok(Some(packet)) => packet,
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
        if let Some(sending) = &mut to {
            // Messages go out together while more are already here, and
            // what waits is sent before receiving would wait.
            let sent = sending.send(&packet).and_then(|()| {
                if !from.ready() {
                    sending.flush()?;
                }
                Ok(())
            });
            if sent.is_err() {
                // The other server is gone; its own end says why.
                to = None;
            }
        }
    }
    if let Some(mut sending) = to {
        let _ = sending.flush();
    }
}
