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
//! Beside the relay, a control thread follows where the threads of each
//! side spend their time: each server sends its metrics every second on its
//! control connection, and the client takes its own; see [`crate::control`].
//! On `Ctrl-\`, or when nothing has passed between the servers for
//! [`STILL`], it says where every thread is: the client's own, and, asked on
//! their control connections, the servers'.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use crate::control::{self, Policy, Side as Whose, View, Watch};
use crate::interrupt;
use crate::profile::{self, Metrics, Profile, QueueSample};
use crate::protocol::{self, Control, PacketType};
use crate::report::Report;
use crate::transport::{
    Connection, ControlConnection, ControlReader, ControlSender, Receiving, Sending, Transport,
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
/// to `profiles` with the name of their side, and how full each side's
/// queues were, each second, which go to `queues`. Fails with the first error
/// either server sent, or how a server ended if it ended badly.
pub fn relay(
    mut source: Side,
    mut destination: Side,
    reports: &SyncSender<Report>,
    profiles: &Mutex<Vec<(String, Profile)>>,
    queues: &Mutex<Vec<QueueSample>>,
) -> Result<()> {
    let (events_in, events) = mpsc::channel();
    for (side, whose) in [
        (&mut source, Whose::Source),
        (&mut destination, Whose::Destination),
    ] {
        if let Some(reader) = side.control.take_reader() {
            listen(
                reader,
                side.name.clone(),
                whose,
                reports.clone(),
                events_in.clone(),
            );
        }
    }
    let senders = [source.control.sender(), destination.control.sender()];
    let (source_receiving, source_sending, source_paths) = source.connection.split("Source");
    let (destination_receiving, destination_sending, destination_paths) =
        destination.connection.split("Destination");
    let failure = Mutex::new(None);
    // Packets passed in each direction, for the watcher to see them move.
    let passed = [AtomicU64::new(0), AtomicU64::new(0)];
    let names = [source.name.clone(), destination.name.clone()];
    std::thread::scope(|scope| {
        scope.spawn(|| {
            control(
                events, &passed, &senders, &names, reports, queues, &mut Watch,
            );
        });
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
        let _ = events_in.send(Event::Done);
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
    reports: &SyncSender<Report>,
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
                    Ok(report) => {
                        // A display that cannot keep up holds this up, as it
                        // should; that is no standing still of the servers,
                        // so it counts as moving. No one showing reports any
                        // more is no reason to stop.
                        let mut report = report;
                        while let Err(mpsc::TrySendError::Full(back)) = reports.try_send(report) {
                            report = back;
                            passed.store(passed.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
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

/// What the control thread hears.
enum Event {
    /// A server's latest interval.
    Metrics(Whose, Metrics),
    /// The relay is done.
    Done,
}

/// How often the client takes its own metrics.
const METRICS_EVERY: Duration = Duration::from_secs(1);

/// Until the relay is done: follows each side's metrics, the client's own
/// taken every [`METRICS_EVERY`], and hands them to `policy` as they come;
/// and says where every thread is when `Ctrl-\` asks, or once nothing has
/// passed for [`STILL`], once for each time it stands still. It reports what
/// holds the transfer back as it changes, naming each side by `names`, and
/// how full each side's queues were, each second, to `samples`, and for the
/// display.
fn control(
    events: Receiver<Event>,
    passed: &[AtomicU64; 2],
    senders: &[ControlSender],
    names: &[String; 2],
    reports: &SyncSender<Report>,
    samples: &Mutex<Vec<QueueSample>>,
    policy: &mut dyn Policy,
) {
    let started = Instant::now();
    let sample = |side: &str, metrics: &Metrics| {
        if metrics.queues.is_empty() {
            return;
        }
        samples.lock().expect("not poisoned").push(QueueSample {
            side: side.to_owned(),
            at_nanos: started.elapsed().as_nanos() as u64,
            queues: metrics.queues.clone(),
        });
        let _ = reports.send(Report::Levels {
            side: side.to_owned(),
            queues: metrics.queues.clone(),
        });
    };
    let mut view = View::default();
    let mut meter = profile::enabled().then(profile::Meter::new);
    let mut metered = Instant::now();
    let count = || {
        passed
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .sum::<u64>()
    };
    let mut last = count();
    let mut moved = Instant::now();
    let mut told = false;
    let mut busy = String::new();
    loop {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::Metrics(whose, metrics)) => {
                sample(&names[usize::from(whose == Whose::Destination)], &metrics);
                view.add(whose, metrics);
                policy.observe(&view);
                let now = control::busiest(&view).map_or_else(String::new, |(side, _, state)| {
                    // As short as it can be: it ends a line of its own.
                    let name = &names[usize::from(side == Whose::Destination)];
                    let name = name.strip_prefix("the ").unwrap_or(name);
                    format!("{name}: {}", state.name())
                });
                if now != busy {
                    busy.clone_from(&now);
                    let _ = reports.send(Report::Busy(now));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Event::Done) | Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(meter) = &mut meter
            && metered.elapsed() >= METRICS_EVERY
        {
            metered = Instant::now();
            let metrics = meter.take();
            sample("the client", &metrics);
            view.add(Whose::Client, metrics);
            policy.observe(&view);
        }
        let now = count();
        if now != last {
            last = now;
            moved = Instant::now();
            told = false;
        }
        // With profiling off, there is nothing to say.
        let why = if interrupt::quit_asked() && profile::enabled() {
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

/// Reads what a server says on its control connection, in a thread of its
/// own until the server ends: its snapshots, reported as lines, and its
/// metrics, for the control thread.
fn listen(
    mut reader: ControlReader,
    name: String,
    whose: Whose,
    reports: SyncSender<Report>,
    events: Sender<Event>,
) {
    std::thread::spawn(move || {
        while let Ok(Some(packet)) = protocol::receive(&mut reader) {
            match packet.decode::<Report>() {
                Ok(Report::Snapshot(threads)) => {
                    for line in profile::describe(&name, &threads) {
                        let _ = reports.send(Report::Said(line));
                    }
                }
                Ok(Report::Metrics(metrics)) => {
                    let _ = events.send(Event::Metrics(whose, metrics));
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy that keeps whose intervals it saw.
    struct Seen(Vec<Whose>);

    impl Policy for Seen {
        fn observe(&mut self, view: &View) {
            for whose in [Whose::Source, Whose::Destination] {
                if let Some(latest) = view.latest(whose)
                    && latest.sequence as usize
                        > self.0.iter().filter(|seen| **seen == whose).count()
                {
                    self.0.push(whose);
                }
            }
        }
    }

    #[test]
    fn the_policy_sees_each_server_s_metrics_until_the_relay_is_done() {
        let (events_in, events) = mpsc::channel();
        let metrics = |sequence| Metrics {
            sequence,
            nanos: 1_000_000_000,
            roles: Vec::new(),
            queues: Vec::new(),
        };
        events_in
            .send(Event::Metrics(Whose::Source, metrics(1)))
            .unwrap();
        events_in
            .send(Event::Metrics(Whose::Destination, metrics(1)))
            .unwrap();
        events_in
            .send(Event::Metrics(Whose::Source, metrics(2)))
            .unwrap();
        events_in.send(Event::Done).unwrap();
        let (reports, _shown) = mpsc::sync_channel(64);
        let passed = [AtomicU64::new(0), AtomicU64::new(0)];
        let mut seen = Seen(Vec::new());
        let names = ["the Source".to_owned(), "the Destination".to_owned()];
        let samples = Mutex::new(Vec::new());
        control(events, &passed, &[], &names, &reports, &samples, &mut seen);
        assert_eq!(seen.0, [Whose::Source, Whose::Destination, Whose::Source]);
    }
}
