//! The client between a transfer's two servers: it passes on what one sends
//! to the other, and shows what they report.
//!
//! One thread for each direction reads packets as they come. A report is for
//! the client, and goes to the display; anything else is written to the other
//! server as it is, without being decoded. Each thread reads until its server
//! closes, even once the other server is gone, so that no server ever waits
//! to write. A server closing passes on as the other's input closing, which is
//! how each learns that the other is done.

use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow};

use crate::protocol::{self, Control, PacketType};
use crate::report::Report;
use crate::session::Connection;

/// One side of a transfer: its server's connection, and how to name it.
pub struct Side {
    pub name: String,
    pub connection: Connection,
}

/// Relays between the Source's server and the Destination's until both are
/// done, sending their reports to `reports`. Fails with the first error
/// either server sent, or how a server ended if it ended badly.
pub fn relay(source: Side, destination: Side, reports: &Sender<Report>) -> Result<()> {
    let (source_reader, source_writer, source_server) = source.connection.split();
    let (destination_reader, destination_writer, destination_server) =
        destination.connection.split();
    let failure = Mutex::new(None);
    let (passed, answered) = std::thread::scope(|scope| {
        let forth = scope.spawn(|| {
            pass(
                source_reader,
                destination_writer,
                reports,
                &source.name,
                &failure,
            )
        });
        let back = pass(
            destination_reader,
            source_writer,
            reports,
            &destination.name,
            &failure,
        );
        (forth.join().expect("the relay panicked"), back)
    });
    let source_ended = source_server.wait();
    let destination_ended = destination_server.wait();
    if let Some(failure) = failure.into_inner().expect("not poisoned") {
        return Err(anyhow!(failure));
    }
    passed?;
    answered?;
    source_ended.map_err(|error| anyhow!("{}: {error:#}", source.name))?;
    destination_ended.map_err(|error| anyhow!("{}: {error:#}", destination.name))?;
    Ok(())
}

/// Passes on packets from one server to the other until the first closes,
/// then closes the other's input. Records the first error a server sends.
fn pass(
    mut from: BufReader<ChildStdout>,
    to: BufWriter<ChildStdin>,
    reports: &Sender<Report>,
    name: &str,
    failure: &Mutex<Option<String>>,
) -> Result<()> {
    let mut to = Some(to);
    loop {
        let packet = match protocol::receive(&mut from) {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                failure
                    .lock()
                    .expect("not poisoned")
                    .get_or_insert_with(|| format!("{name}: {error:#}"));
                // Whatever else comes from it cannot be read as packets.
                drain(&mut from);
                break;
            }
        };
        match packet.kind {
            PacketType::Report => {
                match packet.decode::<Report>() {
                    // No one showing reports any more is no reason to stop.
                    Ok(report) => drop(reports.send(report)),
                    Err(error) => {
                        failure
                            .lock()
                            .expect("not poisoned")
                            .get_or_insert_with(|| format!("{name}: {error:#}"));
                    }
                }
                continue;
            }
            PacketType::Control => {
                if let Ok(Control::Error { message }) = packet.decode::<Control>() {
                    failure
                        .lock()
                        .expect("not poisoned")
                        .get_or_insert_with(|| format!("{name} failed: {message}"));
                }
            }
            _ => {}
        }
        if let Some(writer) = &mut to {
            // Packets go out together while more are already here, and the
            // writer is flushed before the reader would wait.
            let written = protocol::forward(writer, &packet).and_then(|()| {
                if from.buffer().is_empty() {
                    writer.flush()?;
                }
                Ok(())
            });
            if written.is_err() {
                // The other server is gone; its own end says why.
                to = None;
            }
        }
    }
    if let Some(mut writer) = to {
        let _ = writer.flush();
    }
    Ok(())
}

/// Reads what is left, so that the server never waits to write.
fn drain(from: &mut impl Read) {
    let _ = std::io::copy(from, &mut std::io::sink());
}
