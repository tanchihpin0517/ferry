//! Heartbeats: each SSH connection carries a heartbeat channel beside its
//! path, and both sides send a heartbeat on it every few seconds. A channel
//! on which nothing comes for long, or which closes without its end, says
//! that its SSH connection is gone, and fails the connection; see the
//! [architecture](../../../docs/design/architecture.md#connections).
//!
//! A heartbeat channel has nothing else on it, so no heartbeat ever waits
//! behind data: silence means the connection is gone, not that data waits.
//! On it, each side writes one byte, [`BEAT`], every interval, and
//! [`END`] once, when the connection is done.

use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A heartbeat.
const BEAT: u8 = 0;
/// The last byte on a channel: the connection is done.
const END: u8 = 1;

/// How often a heartbeat goes, and how long a silence fails the connection.
#[derive(Debug, Clone, Copy)]
pub struct Times {
    pub interval: Duration,
    pub silence: Duration,
}

/// The times the client runs with.
pub const TIMES: Times = Times {
    interval: Duration::from_secs(5),
    silence: Duration::from_secs(30),
};

/// The times a server runs with: it waits longer than the client, so that
/// the client, which tells you, notices a silence first. A server's own
/// silence is only for when the client is gone.
pub const SERVER_TIMES: Times = Times {
    interval: Duration::from_secs(5),
    silence: Duration::from_secs(40),
};

/// One heartbeat channel: what comes from the far side, what goes to it, and
/// how a failure names it, such as `SSH connection 2 of 4`.
pub struct Channel {
    pub name: String,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

/// What to do when a channel says its connection is gone.
pub type OnFailure = Box<dyn FnOnce(String) + Send>;

/// The heartbeats of one connection, until [`Heartbeats::end`].
pub struct Heartbeats {
    shared: Arc<Shared>,
    writers: Vec<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    /// Rung when the connection is done.
    changed: Condvar,
}

struct State {
    /// The connection is done: nothing fails it any more.
    ending: bool,
    /// Taken by the first failure.
    on_failure: Option<OnFailure>,
    /// When something last came on each channel.
    last: Vec<Instant>,
    /// Which channels the far side has ended.
    ended: Vec<bool>,
}

/// Starts sending heartbeats on every channel, and watching for them.
/// `on_failure` runs once, with what went wrong, if a channel falls silent
/// for `times.silence`, or closes without its end, before
/// [`Heartbeats::end`].
pub fn start(channels: Vec<Channel>, times: Times, on_failure: OnFailure) -> Heartbeats {
    let now = Instant::now();
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            ending: false,
            on_failure: Some(on_failure),
            last: vec![now; channels.len()],
            ended: vec![false; channels.len()],
        }),
        changed: Condvar::new(),
    });
    let mut names = Vec::new();
    let mut writers = Vec::new();
    for (index, channel) in channels.into_iter().enumerate() {
        let Channel {
            name,
            reader,
            writer,
        } = channel;
        names.push(name.clone());
        let reading = Arc::clone(&shared);
        std::thread::spawn(move || watch(reader, index, &name, &reading));
        let writing = Arc::clone(&shared);
        writers.push(std::thread::spawn(move || beat(writer, times, &writing)));
    }
    let checking = Arc::clone(&shared);
    std::thread::spawn(move || check(&names, times, &checking));
    Heartbeats { shared, writers }
}

impl Heartbeats {
    /// The connection is done: ends every channel, so that the far side
    /// knows, and stops watching them.
    pub fn end(self) {
        {
            let mut state = self.shared.state.lock().expect("not poisoned");
            state.ending = true;
            self.shared.changed.notify_all();
        }
        for writer in self.writers {
            let _ = writer.join();
        }
    }
}

/// Fails the connection, the first time, unless it is done.
fn fail(shared: &Shared, message: String) {
    let mut state = shared.state.lock().expect("not poisoned");
    if state.ending {
        return;
    }
    if let Some(on_failure) = state.on_failure.take() {
        // Run under the lock, so that once `end` returns, it never runs.
        on_failure(message);
    }
}

/// Sends a heartbeat every interval until the connection is done, then the
/// end, and closes the channel.
fn beat(mut writer: Box<dyn Write + Send>, times: Times, shared: &Shared) {
    loop {
        let ending = {
            let state = shared.state.lock().expect("not poisoned");
            let (state, _) = shared
                .changed
                .wait_timeout_while(state, times.interval, |state| !state.ending)
                .expect("not poisoned");
            state.ending
        };
        let byte = if ending { END } else { BEAT };
        // A channel that cannot be written to is gone; the far side's
        // silence, or its closing, says so.
        if writer
            .write_all(&[byte])
            .and_then(|()| writer.flush())
            .is_err()
            || ending
        {
            return;
        }
    }
}

/// Reads what comes on channel `index` until its end, or until it closes,
/// which, before its end, fails the connection.
fn watch(mut reader: Box<dyn Read + Send>, index: usize, name: &str, shared: &Shared) {
    let mut buffer = [0u8; 64];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => {
                fail(shared, format!("{name}: dropped"));
                return;
            }
            Ok(read) => read,
        };
        let mut state = shared.state.lock().expect("not poisoned");
        state.last[index] = Instant::now();
        if buffer[..read].contains(&END) {
            state.ended[index] = true;
            return;
        }
    }
}

/// Fails the connection once a channel not yet ended falls silent.
fn check(names: &[String], times: Times, shared: &Shared) {
    let period = (times.silence / 10).min(Duration::from_secs(1));
    loop {
        let silent = {
            let state = shared.state.lock().expect("not poisoned");
            let (state, _) = shared
                .changed
                .wait_timeout_while(state, period, |state| !state.ending)
                .expect("not poisoned");
            if state.ending {
                return;
            }
            (0..names.len())
                .find(|&index| !state.ended[index] && state.last[index].elapsed() > times.silence)
        };
        if let Some(index) = silent {
            fail(
                shared,
                format!(
                    "{}: no heartbeat for {} s",
                    names[index],
                    times.silence.as_secs()
                ),
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SHORT: Times = Times {
        interval: Duration::from_millis(20),
        silence: Duration::from_millis(200),
    };

    /// Two ends of a heartbeat channel, joined by pipes.
    fn pair(name: &str) -> (Channel, Channel) {
        let (here_reader, there_writer) = std::io::pipe().unwrap();
        let (there_reader, here_writer) = std::io::pipe().unwrap();
        (
            Channel {
                name: name.to_owned(),
                reader: Box::new(here_reader),
                writer: Box::new(here_writer),
            },
            Channel {
                name: name.to_owned(),
                reader: Box::new(there_reader),
                writer: Box::new(there_writer),
            },
        )
    }

    fn reporting() -> (OnFailure, mpsc::Receiver<String>) {
        let (sender, failures) = mpsc::channel();
        (
            Box::new(move |message| {
                let _ = sender.send(message);
            }),
            failures,
        )
    }

    #[test]
    fn heartbeats_keep_a_connection_until_both_sides_end() {
        let (here, there) = pair("SSH connection 1 of 1");
        let (here_failure, here_failures) = reporting();
        let (there_failure, there_failures) = reporting();
        let here = start(vec![here], SHORT, here_failure);
        let there = start(vec![there], SHORT, there_failure);
        // Longer than a silence: the heartbeats keep it alive.
        std::thread::sleep(SHORT.silence * 3);
        here.end();
        std::thread::sleep(SHORT.silence * 2);
        there.end();
        assert!(here_failures.try_recv().is_err());
        assert!(there_failures.try_recv().is_err());
    }

    #[test]
    fn silence_fails_the_connection() {
        // The far side never sends anything, and never closes.
        let (here, _there) = pair("SSH connection 2 of 4");
        let (on_failure, failures) = reporting();
        let started = Instant::now();
        let _heartbeats = start(vec![here], SHORT, on_failure);
        let failure = failures.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(failure, "SSH connection 2 of 4: no heartbeat for 0 s");
        assert!(started.elapsed() >= SHORT.silence);
    }

    #[test]
    fn a_channel_closed_without_its_end_fails_at_once() {
        let (here, there) = pair("SSH connection 1 of 2");
        let (on_failure, failures) = reporting();
        let _heartbeats = start(vec![here], SHORT, on_failure);
        drop(there);
        let failure = failures.recv_timeout(SHORT.silence / 2).unwrap();
        assert_eq!(failure, "SSH connection 1 of 2: dropped");
    }
}
