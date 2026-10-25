//! The client, while a transfer goes: it follows the two servers on their
//! control connections, and shows what they report, until both end. What
//! one peer sends the other goes between the servers, over the stream the
//! client made; the client never touches it.
//!
//! Each server's control connection is read by a thread of its own to its
//! end, which comes as the server ends: the reports go to the display, the
//! last counts among them, before the transfer is over. A server's peer that
//! fails says so there first: the first to, of either side, is the
//! transfer's cause. A server on whose control connection nothing came for
//! [`SILENCE`] is taken for gone, as its stream would be: it sends something
//! there every second.
//!
//! A control thread follows where the threads of each side spend their
//! time: each server sends its metrics every second on its control
//! connection, and the client takes its own; see [`crate::control`]. On
//! `Ctrl-\`, or when nothing has moved for [`STILL`], it says where every
//! thread is: the client's own, and, asked on their control connections,
//! the servers'. What moves is what the servers report as it changes, such
//! as the counts of what they compared and wrote, and reports the display
//! holds up.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::control::{self, Policy, Side as Whose, View, Watch};
use crate::interrupt;
use crate::profile::{self, Metrics, Profile, QueueSample};
use crate::protocol::{self, Control};
use crate::report::Report;
use crate::stop::{Internal, Stop};
use crate::transport::stream::SILENCE;
use crate::transport::streams::Streams;
use crate::transport::{ControlConnection, ControlReader, ControlSender, Monitor, Transport};

/// How long nothing may move before the client says where every thread is.
const STILL: Duration = Duration::from_secs(60);

/// How long a server is given to end once the transfer has failed, or the
/// other has gone: one whose SSH connection is gone never says it ended.
const GIVEN: Duration = Duration::from_secs(5);

/// How long a server is given to be done once the other is: it is done in
/// a moment, as the other's end comes on the stream, or its stream is taken
/// for broken once nothing came on it for [`SILENCE`].
const LINGER: Duration = SILENCE.saturating_add(Duration::from_secs(10));

/// How a server's control connection says it is done, or ended.
enum Ending {
    /// Its peer is done, and its stream's end written: it waits to be
    /// closed.
    Done(usize),
    /// Its control connection ended: the server is gone.
    Gone(usize),
}

/// One side of a transfer: its server's control connection, the transport
/// it goes through, and how to name it.
pub struct Side {
    pub name: String,
    pub control: ControlConnection,
    /// Kept open until the transfer is over.
    pub transport: Arc<Transport>,
    /// What the dashboard shows of its SSH connection, as its server says
    /// what its stream carried; none on this machine.
    pub monitor: Option<Monitor>,
}

/// Follows the Source's server and the Destination's until both end,
/// sending their reports to `reports`, but for their profiles, which go to
/// `profiles` with the name of their side, and how full each side's queues
/// were, each second, which go to `queues`; then lets go of the stream.
/// Fails with the first error either server's peer told, or how a server
/// ended if it ended badly.
pub fn follow(
    mut source: Side,
    mut destination: Side,
    streams: Streams,
    reports: &Sender<Report>,
    profiles: &Mutex<Vec<(String, Profile)>>,
    queues: &Mutex<Vec<QueueSample>>,
) -> Result<()> {
    let (events_in, events) = crossbeam_channel::unbounded();
    // What each side reported that moves the transfer, or held the display
    // up: each added to by one thread.
    let moved = [AtomicU64::new(0), AtomicU64::new(0)];
    // When anything last came on each server's control connection, in
    // milliseconds since now: each set by one thread.
    let started = Instant::now();
    let heard = [AtomicU64::new(0), AtomicU64::new(0)];
    let silence = crate::inject::silence().unwrap_or(SILENCE);
    // The first error either server's peer tells is the transfer's.
    let stop = Stop::new(None);
    // Which server is done, or gone.
    let (ending, ends) = crossbeam_channel::unbounded();
    let senders = [source.control.sender(), destination.control.sender()];
    let names = [source.name.clone(), destination.name.clone()];
    let failed = std::thread::scope(|scope| {
        for (index, (side, whose)) in [
            (&mut source, Whose::Source),
            (&mut destination, Whose::Destination),
        ]
        .into_iter()
        .enumerate()
        {
            match side.control.take_reader() {
                Some(reader) => {
                    let name = side.name.clone();
                    let (events_in, ending) = (events_in.clone(), ending.clone());
                    let (moved, stop) = (&moved[index], &stop);
                    let heard = &heard[index];
                    let monitor = side.monitor.clone();
                    scope.spawn(move || {
                        let done = || {
                            let _ = ending.send(Ending::Done(index));
                        };
                        let heard = Heard {
                            reports,
                            events: &events_in,
                            profiles,
                            monitor: monitor.as_ref(),
                            done: &done,
                            moved,
                            stop,
                            heard,
                            started,
                        };
                        listen(reader, &name, whose, &heard);
                        let _ = ending.send(Ending::Gone(index));
                    });
                }
                None => {
                    let _ = ending.send(Ending::Gone(index));
                }
            }
        }
        let (moved, senders, names) = (&moved, &senders, &names);
        scope.spawn(move || {
            control(events, moved, senders, names, reports, queues, &mut Watch);
        });
        // Each server is done once its peer is, and its stream's end
        // written. The two end at about the same time, once each has what
        // the other sent, so once one is done, the other is given until its
        // stream would be taken for broken; once one's peer failed, both are
        // given a moment to be done, as the other's peer stops too. Once one
        // is gone without saying it was done, the other is stopped at once:
        // there is nothing more to wait for. One on whose control connection
        // nothing came for the silence allowed is taken for gone too.
        let mut ended = [false; 2];
        let mut silent = [false; 2];
        let mut given: Option<Instant> = None;
        let give = |given: &mut Option<Instant>, time| {
            let until = Instant::now() + time;
            *given = Some(given.map_or(until, |given| given.min(until)));
        };
        let mut gone = None;
        while !ended.iter().all(|ended| *ended) && given.is_none_or(|given| Instant::now() < given)
        {
            crossbeam_channel::select! {
                recv(ends) -> ending => match ending {
                    Ok(Ending::Done(index)) => {
                        ended[index] = true;
                        give(&mut given, LINGER);
                    }
                    Ok(Ending::Gone(index)) => {
                        // Not one done, then closed by this side.
                        if !ended[index] {
                            gone.get_or_insert(index);
                        }
                        ended[index] = true;
                        give(&mut given, Duration::ZERO);
                    }
                    Err(_) => break,
                },
                default(Duration::from_millis(50)) => {}
            }
            if stop.is_stopped() {
                give(&mut given, GIVEN);
            }
            let now = started.elapsed();
            for index in 0..2 {
                let last = Duration::from_millis(heard[index].load(Ordering::Relaxed));
                if !ended[index] && now.saturating_sub(last) >= silence {
                    silent[index] = true;
                    gone.get_or_insert(index);
                    ended[index] = true;
                    give(&mut given, Duration::ZERO);
                }
            }
        }
        let _ = events_in.send(Event::Done);
        // One still going, or gone silent, is told to stop, as its control
        // connection closes, and given a moment more, then let go.
        let close = |control: ControlConnection, ended: bool, silent: bool, name: &str| {
            let closed = if ended && !silent {
                control.close()
            } else {
                control.close_within(GIVEN)
            };
            if silent {
                return Err(anyhow!(
                    "nothing came on its control connection for {} s",
                    silence.as_secs()
                ))
                .context(name.to_owned());
            }
            closed.with_context(|| name.to_owned())
        };
        (
            [
                close(source.control, ended[0], silent[0], &source.name),
                close(destination.control, ended[1], silent[1], &destination.name),
            ],
            gone,
        )
    });
    let (failed, gone) = failed;
    streams.end();
    // A server's internal error is why the rest failed, whatever came first.
    if let Some(internal) = failed
        .iter()
        .position(|ended| ended.as_ref().is_err_and(|error| error.is::<Internal>()))
    {
        let [source_ended, destination_ended] = failed;
        return if internal == 0 {
            source_ended
        } else {
            destination_ended
        };
    }
    stop.result()?;
    let [source_ended, destination_ended] = failed;
    // A server gone first is why the other was stopped, whose end says
    // nothing more.
    match gone {
        Some(0) => source_ended,
        Some(_) => destination_ended,
        None => source_ended.and(destination_ended),
    }
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
    monitor: Option<&'a Monitor>,
    /// Called once it is done.
    done: &'a dyn Fn(),
    /// What it reports as the transfer moves, and a report the display
    /// holds up, add to it.
    moved: &'a AtomicU64,
    /// Its peer's failure fails it.
    stop: &'a Stop,
    /// When anything last came, in milliseconds since `started`.
    heard: &'a AtomicU64,
    started: Instant,
}

impl Heard<'_> {
    /// Something came, or would have, but for the display holding up the
    /// last.
    fn alive(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        self.heard.store(now, Ordering::Relaxed);
    }
}

/// Reads what a server, `name`d, says on its control connection, until the
/// server ends, and hands each on as `heard` says; its snapshots reported as
/// lines.
fn listen(mut reader: ControlReader, name: &str, whose: Whose, heard: &Heard) {
    let Heard {
        reports,
        events,
        profiles,
        monitor,
        done,
        moved,
        stop,
        ..
    } = *heard;
    while let Ok(Some(packet)) = protocol::receive(&mut reader) {
        heard.alive();
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
            Ok(Report::Failed(message)) => stop.fail(anyhow!("{name} failed: {message}")),
            Ok(Report::Streams(carried)) => {
                if let Some(monitor) = monitor {
                    monitor.carried(&carried);
                }
            }
            Ok(Report::Done) => done(),
            Ok(report) => {
                if matches!(report, Report::Counts(_) | Report::Listed(_)) {
                    // Only this thread adds: no read-modify-write.
                    moved.store(moved.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
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
/// standing still of the servers, so it counts as moving, and the server
/// as heard, as what it sends waits to be read. No one showing reports any
/// more is no reason to stop.
fn show(mut report: Report, heard: &Heard) {
    while let Err(crossbeam_channel::SendTimeoutError::Timeout(back)) = heard
        .reports
        .send_timeout(report, Duration::from_millis(10))
    {
        report = back;
        // Only this thread adds: no read-modify-write.
        let moved = heard.moved;
        moved.store(moved.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        heard.alive();
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
/// still. It reports what
/// holds the transfer back as it changes, naming each side by `names`, and
/// how full each side's queues were, each second, to `samples`, and for the
/// display.
#[allow(clippy::too_many_arguments)]
fn control(
    events: Receiver<Event>,
    moved: &[AtomicU64],
    senders: &[ControlSender],
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
        // With profiling off, there is nothing to say.
        let why = if interrupt::quit_asked() && profile::enabled() {
            "where every thread is, as asked:".to_owned()
        } else if !told && moved.elapsed() >= crate::inject::still().unwrap_or(STILL) {
            told = true;
            format!(
                "nothing has moved for {} s; where every thread is:",
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
                control(events, &moved, &[], &names, &reports, &samples, &mut seen);
                assert_eq!(seen.0, [Whose::Source, Whose::Destination, Whose::Source]);
            },
        );
    }
}
