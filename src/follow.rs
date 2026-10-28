//! The client, while a transfer goes: it follows the two servers on their
//! control connections, and shows what they report, until both end. What
//! one peer sends the other goes between the servers, over the stream the
//! client made; the client never touches it.
//!
//! Each pipe of a server's control connection, control and its reports, is
//! read by a thread of its own to its end, which comes as the server ends:
//! the reports go to the display, the last counts among them, before the
//! transfer is over. What says the
//! server is done, or failed, goes to the client's monitor too, each server
//! a worker of it: when the transfer ends, and why, is the monitor's to
//! decide, and whether each server's host is still there; see
//! [`crate::monitor::client`].
//!
//! A control thread follows where the threads of each side spend their
//! time: each server sends its metrics every second on its control
//! connection, and the client takes its own; see [`crate::control`]. On
//! `Ctrl-\`, or when nothing has moved for [`STILL`], it says where every
//! thread is: the client's own, and, asked on their control connections,
//! the servers'. What moves is what the servers report as it changes, such
//! as the counts of what they compared and wrote, and reports the display
//! holds up.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::control::{self, Policy, Side as Whose, View, Watch};
use crate::interrupt;
use crate::monitor::client::Node;
use crate::profile::{self, Metrics, Profile, QueueSample};
use crate::protocol::{self, Control};
use crate::report::Report;
use crate::transport::{ControlConnection, ControlReader, ControlSender, Traffic, Transport};

/// How long nothing may move before the client says where every thread is.
const STILL: Duration = Duration::from_secs(60);

/// One side of a transfer: its server's control connection, the transport
/// it goes through, and how to name it.
pub struct Side {
    pub name: String,
    pub control: ControlConnection,
    /// Kept open until the transfer is over.
    pub transport: Arc<Transport>,
    /// What the dashboard shows of its SSH connection, as its server says
    /// what its stream carried; none on this machine.
    pub traffic: Option<Traffic>,
    /// The server, as the client's monitor has it.
    pub worker: Node,
}

/// Follows the Source's server and the Destination's until both control
/// connections end, sending their reports to `reports`, but for their
/// profiles, which go to `profiles` with the name of their side, and how
/// full each side's queues were, each second, which go to `queues`; and
/// telling the client's monitor when each is done, or failed.
pub fn follow(
    mut source: Side,
    mut destination: Side,
    reports: &Sender<Report>,
    profiles: &Mutex<Vec<(String, Profile)>>,
    queues: &Mutex<Vec<QueueSample>>,
) {
    let (events_in, events) = crossbeam_channel::unbounded();
    // What each side reported that moves the transfer, or held the display
    // up: each added to by one thread.
    let moved = [AtomicU64::new(0), AtomicU64::new(0)];
    let senders = [source.control.sender(), destination.control.sender()];
    let names = [source.name.clone(), destination.name.clone()];
    let workers = [source.worker.clone(), destination.worker.clone()];
    let transports = [
        Arc::clone(&source.transport),
        Arc::clone(&destination.transport),
    ];
    std::thread::scope(|scope| {
        let mut listening = Vec::new();
        for (index, (side, whose)) in [
            (&mut source, Whose::Source),
            (&mut destination, Whose::Destination),
        ]
        .into_iter()
        .enumerate()
        {
            let Some((control, reported)) = side.control.take_readers() else {
                continue;
            };
            for reader in [control, reported] {
                let name = side.name.clone();
                let events_in = events_in.clone();
                let moved = &moved[index];
                let traffic = side.traffic.clone();
                let worker = side.worker.clone();
                let here = Arc::clone(&transports[index]);
                let peer = Arc::clone(&transports[1 - index]);
                listening.push(scope.spawn(move || {
                    let heard = Heard {
                        reports,
                        events: &events_in,
                        profiles,
                        traffic: traffic.as_ref(),
                        worker: &worker,
                        moved,
                        here: &here,
                        peer: &peer,
                    };
                    listen(reader, &name, whose, &heard);
                }));
            }
        }
        let (moved, senders, workers, names) = (&moved, &senders, &workers, &names);
        scope.spawn(move || {
            control(
                events, moved, senders, workers, names, reports, queues, &mut Watch,
            );
        });
        for listening in listening {
            let _ = listening.join();
        }
        let _ = events_in.send(Event::Done);
    });
}

/// Where what a server says on its control connection goes.
struct Heard<'a> {
    /// What its peer reports, for the display.
    reports: &'a Sender<Report>,
    /// Its metrics, for the control thread.
    events: &'a Sender<Event>,
    /// Its profile, kept with its side's name.
    profiles: &'a Mutex<Vec<(String, Profile)>>,
    /// What its stream carried, for the dashboard.
    traffic: Option<&'a Traffic>,
    /// The server, as the client's monitor has it: told once it is done, or
    /// failed.
    worker: &'a Node,
    /// What it reports as the transfer moves, and a report the display
    /// holds up, add to it, from either pipe.
    moved: &'a AtomicU64,
    /// Where it runs: anything on its control connection came from there.
    here: &'a Transport,
    /// Where its peer runs: what its stream carried came from there.
    peer: &'a Transport,
}

/// Reads what a server, `name`d, says on its control connection, until the
/// server ends, or its scope does, and hands each on as `heard` says; its
/// snapshots reported as lines. All of it is word from where the server
/// runs, and what its stream carried, as it says, word from where its peer
/// does: so a host whose heartbeat waits behind the data on a slow
/// connection is not taken for lost while the data still comes.
fn listen(mut reader: ControlReader, name: &str, whose: Whose, heard: &Heard) {
    let Heard {
        reports,
        events,
        profiles,
        traffic,
        worker,
        moved,
        here,
        peer,
        ..
    } = *heard;
    loop {
        let packet = match protocol::receive(&mut reader) {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(_) => return,
        };
        let now = Instant::now();
        here.heard_from(now);
        match packet.decode::<Report>() {
            Ok(Report::Snapshot(threads)) => {
                for line in profile::describe(name, &threads) {
                    let _ = reports.send(Report::Said(line));
                }
            }
            Ok(Report::Metrics(metrics)) => {
                let _ = events.send(Event::Metrics(whose, metrics));
            }
            Ok(Report::Profile(profile)) => keep_profile(profiles, name, profile),
            Ok(Report::Failed(message)) => worker.failed(message),
            Ok(Report::Carried(carried)) => {
                if let Some(at) = carried
                    .heard
                    .and_then(|ago| now.checked_sub(Duration::from_millis(ago)))
                {
                    peer.heard_from(at);
                }
                if let Some(traffic) = traffic {
                    traffic.carried(&carried);
                }
            }
            Ok(Report::Done) => worker.done(),
            Ok(report) => {
                if matches!(report, Report::Counts(_) | Report::Listed(_)) {
                    moved.fetch_add(1, Ordering::Relaxed);
                }
                show(report, heard);
            }
            // Reports are for people: one that cannot be read is said so,
            // and the transfer goes on.
            Err(error) => {
                let _ = reports.try_send(Report::Said(format!(
                    "warning: {name} sent a report that cannot be read: {error:#}"
                )));
            }
        }
    }
}

/// Hands `report` to the display. A display that cannot keep up holds it
/// up, as it should, until it has room, and not a moment more; that is no
/// standing still of the servers, so it counts as moving. What the server
/// sends meanwhile waits to be read, and counts as come. No one showing
/// reports any more is no reason to stop.
fn show(mut report: Report, heard: &Heard) {
    while let Err(crossbeam_channel::SendTimeoutError::Timeout(back)) = heard
        .reports
        .send_timeout(report, Duration::from_millis(10))
    {
        report = back;
        heard.moved.fetch_add(1, Ordering::Relaxed);
    }
}

/// Keeps a server's profile, one for each side: a later one, which a server
/// gone wrong might send again and again, takes the place of the first.
fn keep_profile(profiles: &Mutex<Vec<(String, Profile)>>, name: &str, profile: Profile) {
    let mut profiles = profiles.lock().expect("not poisoned");
    match profiles.iter_mut().find(|(side, _)| side == name) {
        Some((_, kept)) => *kept = profile,
        None => profiles.push((name.to_owned(), profile)),
    }
}

/// What the control thread hears.
enum Event {
    /// A server's latest interval.
    Metrics(Whose, Metrics),
    /// Both servers ended.
    Done,
}

/// How often the client takes its own metrics.
const METRICS_EVERY: Duration = Duration::from_secs(1);

/// Until both servers end: follows each side's metrics, the client's own
/// taken every [`METRICS_EVERY`], and hands them to `policy` as they come;
/// and says where every thread is when `Ctrl-\` asks, or once nothing has
/// moved for [`STILL`], as `moved` counts it, once for each time it stands
/// still, as each server says, and as each of `workers` looks from outside,
/// which it says whether or not its server can answer. It reports what
/// holds the transfer back as it changes, naming each side by `names`, and
/// how full each side's queues were, each second, to `samples`, and for the
/// display.
#[allow(clippy::too_many_arguments)]
fn control(
    events: Receiver<Event>,
    moved: &[AtomicU64],
    senders: &[ControlSender],
    workers: &[Node],
    names: &[String; 2],
    reports: &Sender<Report>,
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
        moved
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
        let asked = interrupt::quit_asked();
        // With profiling off, no thread can say where it is.
        let why = if asked && profile::enabled() {
            Some("where every thread is, as asked:".to_owned())
        } else if !told && moved.elapsed() >= crate::inject::still().unwrap_or(STILL) {
            told = true;
            Some(format!(
                "nothing has moved for {} s; where every thread is:",
                moved.elapsed().as_secs()
            ))
        } else {
            None
        };
        if why.is_none() && !asked {
            continue;
        }
        if let Some(why) = why {
            let _ = reports.send(Report::Said(why));
            for line in profile::describe("the client", &profile::snapshot()) {
                let _ = reports.send(Report::Said(line));
            }
            // Each answers on its control connection; see [`listen`].
            for sender in senders {
                let _ = sender.send(&Control::Snapshot);
            }
        }
        // Said by their monitors, whether or not they can answer.
        for worker in workers {
            worker.look();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One profile is kept for each side, however many come.
    #[test]
    fn one_profile_is_kept_for_each_side() {
        let profiles = Mutex::new(Vec::new());
        let named = |role: &str| Profile {
            roles: vec![crate::profile::RoleProfile {
                role: role.to_owned(),
                threads: 1,
                states: Vec::new(),
            }],
        };
        keep_profile(&profiles, "the Source", named("first"));
        keep_profile(&profiles, "the Destination", named("other"));
        keep_profile(&profiles, "the Source", named("again"));
        let kept = profiles.into_inner().unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].1.roles[0].role, "again");
    }

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
    fn the_policy_sees_each_server_s_metrics_until_both_end() {
        crate::test_support::within(
            "the policy sees each server s metrics until both end",
            crate::test_support::PATIENCE,
            move || {
                let (events_in, events) = crossbeam_channel::unbounded();
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
                let (reports, _shown) = crossbeam_channel::bounded(64);
                let moved = [AtomicU64::new(0), AtomicU64::new(0)];
                let mut seen = Seen(Vec::new());
                let names = ["the Source".to_owned(), "the Destination".to_owned()];
                let samples = Mutex::new(Vec::new());
                control(
                    events,
                    &moved,
                    &[],
                    &[],
                    &names,
                    &reports,
                    &samples,
                    &mut seen,
                );
                assert_eq!(seen.0, [Whose::Source, Whose::Destination, Whose::Source]);
            },
        );
    }
}
