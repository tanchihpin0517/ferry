//! The monitor's core, on made-up events and times.

use std::time::{Duration, Instant};

use super::*;

/// The design's times, but for a silence of 30 s, said as such.
const TIMING: Timing = Timing::STANDARD;

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// A client with one host, linked, its server as a worker carried by one
/// `ssh`, and a server on this machine: the host, its worker, the carrier,
/// the local worker, and the local server's process.
struct Client {
    core: Core,
    host: ScopeId,
    remote: ScopeId,
    carrier: ScopeId,
    local: ScopeId,
    server: ScopeId,
    start: Instant,
}

fn client() -> Client {
    let start = Instant::now();
    let mut core = Core::client(TIMING);
    let host = core.open(ROOT, New::Host, start);
    core.phase(host, Phase::Linked, start);
    let remote = core.open(host, New::Worker, start);
    let carrier = core.open(
        remote,
        New::Process {
            pid: 10,
            role: Role::Carrier,
        },
        start,
    );
    let local = core.open(ROOT, New::Worker, start);
    let server = core.open(
        local,
        New::Process {
            pid: 11,
            role: Role::Worker,
        },
        start,
    );
    assert!(core.take().is_empty());
    Client {
        core,
        host,
        remote,
        carrier,
        local,
        server,
        start,
    }
}

fn ends(actions: &[Action]) -> Vec<ScopeId> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::End(scope, _) => Some(*scope),
            _ => None,
        })
        .collect()
}

fn over(actions: &[Action]) -> Option<&Reason> {
    actions.iter().find_map(|action| match action {
        Action::Over(reason) => Some(reason),
        _ => None,
    })
}

#[test]
fn a_transfer_whose_workers_are_done_ends_its_hosts_after_them() {
    let Client {
        mut core,
        host,
        remote,
        carrier,
        local,
        server,
        start,
    } = client();
    let link = core.open(host, New::Plain, start);
    core.done(remote, start);
    assert_eq!(core.take(), [Action::Close(remote)]);
    core.done(local, start);
    assert_eq!(core.take(), [Action::Close(local)]);
    // The local server ends as its control connection closes: its worker's
    // end is known.
    core.exited(server, start);
    core.worker_ended(remote, start);
    // Its worker is over, but its carrier still runs: nothing ends yet but
    // the local worker.
    let actions = core.take();
    assert_eq!(core.ending(ROOT), Some(&Reason::Done));
    assert_eq!(ends(&actions), [server, local]);
    assert!(!core.ended(host));
    core.exited(carrier, start);
    let actions = core.take();
    // The link closes once what works on the host ended, then the host, and
    // the transfer is over.
    assert_eq!(ends(&actions), [carrier, remote, link, host, ROOT]);
    assert_eq!(over(&actions), Some(&Reason::Done));
}

#[test]
fn a_host_that_goes_silent_is_lost_and_its_carriers_killed_at_once() {
    let Client {
        mut core,
        host,
        remote,
        carrier,
        local,
        server,
        start,
    } = client();
    core.heard(host, start + secs(10));
    core.tick(start + secs(39));
    assert!(core.take().is_empty());
    assert_eq!(core.next(), Some(start + secs(40)));
    core.tick(start + secs(40));
    let actions = core.take();
    let lost = Reason::Lost(Lost::Silent(secs(30)));
    assert!(actions.contains(&Action::End(host, lost.clone())));
    assert!(actions.contains(&Action::Signal(carrier, Signal::Kill)));
    assert!(actions.contains(&Action::End(remote, lost)));
    assert_eq!(
        core.ending(ROOT),
        Some(&Reason::Failed(Cause::Lost {
            host,
            how: Lost::Silent(secs(30)),
        }))
    );
    // The local server can be seen: it is waited for, not stopped.
    assert!(!actions.contains(&Action::Signal(server, Signal::Term)));
    assert!(!actions.contains(&Action::Close(local)));
    assert!(over(&actions).is_none());
    // It ends as its stream breaks, having said it was done.
    core.done(local, start + secs(41));
    core.exited(server, start + secs(41));
    let actions = core.take();
    assert!(actions.contains(&Action::Close(local)));
    assert!(matches!(
        over(&actions),
        Some(Reason::Failed(Cause::Lost { .. }))
    ));
}

#[test]
fn a_host_waited_for_to_link_is_lost_late_however_much_is_heard() {
    let start = Instant::now();
    let mut core = Core::client(TIMING);
    let host = core.open(ROOT, New::Host, start);
    // Logging in has no limit of the monitor's.
    assert_eq!(core.next(), None);
    core.phase(host, Phase::Linking, start + secs(5));
    core.heard(host, start + secs(20));
    core.tick(start + secs(34));
    assert!(core.take().is_empty());
    core.tick(start + secs(35));
    assert!(
        core.take()
            .contains(&Action::End(host, Reason::Lost(Lost::Late(secs(30)))))
    );
}

#[test]
fn a_first_ctrl_c_stops_each_side_and_a_second_soon_after_forces() {
    let Client {
        mut core,
        host,
        remote,
        carrier,
        local,
        server,
        start,
    } = client();
    core.interrupt(start);
    let actions = core.take();
    assert!(actions.contains(&Action::SendStop(host)));
    assert!(actions.contains(&Action::Close(remote)));
    assert!(actions.contains(&Action::Close(local)));
    assert!(actions.contains(&Action::Signal(server, Signal::Term)));
    // What carries a connection ends as it closes.
    assert!(!actions.contains(&Action::Signal(carrier, Signal::Term)));
    assert!(actions.contains(&Action::Note(Note::Stopping { within: secs(3) })));
    assert_eq!(core.ending(ROOT), Some(&Reason::Stopped));
    // What is awaited is said after a moment.
    core.tick(start + Duration::from_millis(1500));
    assert_eq!(
        core.take(),
        [Action::Note(Note::Awaiting(vec![remote, local]))]
    );
    core.interrupt(start + Duration::from_millis(2900));
    let actions = core.take();
    assert!(actions.contains(&Action::Signal(carrier, Signal::Kill)));
    assert!(actions.contains(&Action::Signal(server, Signal::Kill)));
    assert!(actions.contains(&Action::End(ROOT, Reason::Forced)));
    assert_eq!(over(&actions), Some(&Reason::Forced));
}

#[test]
fn a_ctrl_c_after_the_window_says_again_rather_than_forcing() {
    let Client {
        mut core,
        remote,
        local,
        start,
        ..
    } = client();
    core.interrupt(start);
    core.take();
    core.interrupt(start + secs(4));
    assert_eq!(
        core.take(),
        [
            Action::Note(Note::Awaiting(vec![remote, local])),
            Action::Note(Note::Stopping { within: secs(3) }),
        ]
    );
    // The window starts again from the press counted as a first.
    core.interrupt(start + secs(6));
    assert_eq!(over(&core.take()), Some(&Reason::Forced));
}

#[test]
fn a_request_to_terminate_never_forces_but_opens_the_window() {
    let Client {
        mut core, start, ..
    } = client();
    core.terminate(start);
    core.terminate(start + secs(1));
    assert!(over(&core.take()).is_none());
    core.interrupt(start + secs(2));
    assert_eq!(over(&core.take()), Some(&Reason::Forced));
}

#[test]
fn a_host_not_linked_yet_is_cut_when_stopped() {
    let start = Instant::now();
    let mut core = Core::client(TIMING);
    let host = core.open(ROOT, New::Host, start);
    core.phase(host, Phase::Hearing, start);
    let session = core.open(
        host,
        New::Process {
            pid: 12,
            role: Role::Carrier,
        },
        start,
    );
    core.interrupt(start + secs(1));
    let actions = core.take();
    assert!(!actions.contains(&Action::SendStop(host)));
    assert!(actions.contains(&Action::Signal(session, Signal::Kill)));
    assert!(actions.contains(&Action::End(host, Reason::Lost(Lost::Cut))));
    // Nothing else is waited for: the transfer is over, stopped.
    assert_eq!(over(&actions), Some(&Reason::Stopped));
}

#[test]
fn a_failure_waits_for_the_rest_and_a_ctrl_c_then_stops_them_keeping_the_cause() {
    let Client {
        mut core,
        host,
        remote,
        local,
        server,
        start,
        ..
    } = client();
    core.failed(local, "it broke".to_owned(), start);
    let cause = Reason::Failed(Cause::Failed {
        worker: local,
        message: "it broke".to_owned(),
    });
    assert_eq!(core.ending(ROOT), Some(&cause));
    assert!(core.take().is_empty());
    core.tick(start + secs(1));
    assert_eq!(
        core.take(),
        [Action::Note(Note::Awaiting(vec![remote, local]))]
    );
    core.interrupt(start + secs(2));
    let actions = core.take();
    assert!(actions.contains(&Action::SendStop(host)));
    assert!(actions.contains(&Action::Close(remote)));
    assert!(actions.contains(&Action::Signal(server, Signal::Term)));
    assert_eq!(core.ending(ROOT), Some(&cause));
}

#[test]
fn a_worker_that_ends_without_saying_it_was_done_fails_the_transfer() {
    let Client {
        mut core,
        local,
        server,
        start,
        ..
    } = client();
    core.exited(server, start);
    assert_eq!(
        core.ending(ROOT),
        Some(&Reason::Failed(Cause::Gone { worker: local }))
    );
}

#[test]
fn the_first_failure_is_the_cause() {
    let Client {
        mut core,
        remote,
        local,
        start,
        ..
    } = client();
    core.failed(remote, "first".to_owned(), start);
    core.failed(local, "second".to_owned(), start);
    assert_eq!(
        core.ending(ROOT),
        Some(&Reason::Failed(Cause::Failed {
            worker: remote,
            message: "first".to_owned(),
        }))
    );
}

#[test]
fn a_scope_opened_under_one_ending_ends_at_once() {
    let Client {
        mut core,
        host,
        start,
        ..
    } = client();
    core.interrupt(start);
    core.take();
    let late = core.open(
        host,
        New::Process {
            pid: 13,
            role: Role::Worker,
        },
        start,
    );
    assert_eq!(core.ending(late), Some(&Reason::Stopped));
    assert_eq!(core.take(), [Action::Signal(late, Signal::Term)]);
}

#[test]
fn pauses_are_said_not_timed_out() {
    let Client {
        mut core,
        host,
        server,
        start,
        ..
    } = client();
    core.paused(server);
    // Its host is heard from all the while.
    for minute in 1..=60 {
        core.heard(host, start + secs(minute * 20));
        core.tick(start + secs(minute * 20));
    }
    core.resumed(server);
    assert_eq!(
        core.take(),
        [
            Action::Note(Note::Paused(server)),
            Action::Note(Note::Resumed(server)),
        ]
    );
}

/// A host's monitor with two workers started.
fn host() -> (Core, ScopeId, ScopeId, ScopeId, Instant) {
    let start = Instant::now();
    let mut core = Core::host(TIMING, start);
    let listener = core.open(ROOT, New::Plain, start);
    let server = core.open(
        ROOT,
        New::Process {
            pid: 20,
            role: Role::Worker,
        },
        start,
    );
    let check = core.open(
        ROOT,
        New::Process {
            pid: 21,
            role: Role::Worker,
        },
        start,
    );
    (core, listener, server, check, start)
}

#[test]
fn a_host_monitor_not_followed_in_time_ends_what_it_started_within_the_grace() {
    let (mut core, listener, server, check, start) = host();
    assert_eq!(core.next(), Some(start + secs(30)));
    core.tick(start + secs(30));
    let actions = core.take();
    let lost = Reason::Lost(Lost::Late(secs(30)));
    assert!(actions.contains(&Action::End(ROOT, lost.clone())));
    assert!(actions.contains(&Action::End(listener, lost.clone())));
    assert!(actions.contains(&Action::Signal(server, Signal::Term)));
    assert!(actions.contains(&Action::Signal(check, Signal::Term)));
    core.exited(check, start + secs(31));
    core.take();
    assert_eq!(core.next(), Some(start + secs(60)));
    core.tick(start + secs(60));
    let actions = core.take();
    assert!(actions.contains(&Action::Signal(server, Signal::Kill)));
    assert!(actions.contains(&Action::Note(Note::Abandoned(vec![server]))));
    assert_eq!(over(&actions), Some(&lost));
}

#[test]
fn a_host_monitor_whose_workers_end_within_the_grace_ends_without_killing() {
    let (mut core, _, server, check, start) = host();
    core.phase(ROOT, Phase::Linked, start + secs(1));
    core.heard(ROOT, start + secs(5));
    core.lose(ROOT, Lost::Ended, start + secs(6));
    core.take();
    core.exited(server, start + secs(7));
    core.exited(check, start + secs(8));
    let actions = core.take();
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::Signal(_, Signal::Kill)))
    );
    assert_eq!(over(&actions), Some(&Reason::Lost(Lost::Ended)));
}

#[test]
fn a_host_monitor_told_to_stop_waits_for_its_workers_and_keeps_its_link() {
    let (mut core, _, server, check, start) = host();
    core.phase(ROOT, Phase::Linked, start);
    core.stop(start + secs(1));
    let actions = core.take();
    assert!(actions.contains(&Action::Signal(server, Signal::Term)));
    assert!(actions.contains(&Action::Signal(check, Signal::Term)));
    core.tick(start + secs(2));
    assert_eq!(
        core.take(),
        [Action::Note(Note::Awaiting(vec![server, check]))]
    );
    core.exited(server, start + secs(3));
    core.exited(check, start + secs(3));
    assert!(over(&core.take()).is_none());
    assert_eq!(core.ending(ROOT), None);
    // Once the client closes the link, nothing is left to wait for.
    core.lose(ROOT, Lost::Ended, start + secs(4));
    assert_eq!(over(&core.take()), Some(&Reason::Lost(Lost::Ended)));
}

#[test]
fn the_beat_and_silence_are_shortened_together() {
    let timing = Timing {
        beat: secs(1),
        silence: secs(6),
        ..TIMING
    };
    let start = Instant::now();
    let mut core = Core::host(timing, start);
    core.phase(ROOT, Phase::Linked, start);
    core.tick(start + secs(6));
    assert_eq!(
        over(&core.take()),
        Some(&Reason::Lost(Lost::Silent(secs(6))))
    );
}
