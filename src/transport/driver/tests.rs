//! The driver, with the session, over socketpairs: what each peer sends
//! comes once, the control messages in order, as streams hold, or break and
//! others are joined; a session closed answers a stream joined after; a
//! stream that is not one is warned of and shut down; and once its handles
//! are gone, the driver ends.

use std::io::Read;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::*;

const ID: [u8; 16] = *b"a test's session";

/// How long a test waits for what should come.
const WAIT: Duration = Duration::from_secs(60);

struct Side {
    handle: Handle,
    deliveries: Receiver<Delivery>,
    notices: Receiver<Notice>,
    thread: JoinHandle<()>,
}

fn side() -> Side {
    let (handle, deliveries, notices, thread) = start(ID);
    Side {
        handle,
        deliveries,
        notices,
        thread,
    }
}

/// Joins `left` and `right` by a socketpair numbered `stream`; returns its
/// left end again, to break it with.
fn link(left: &Side, right: &Side, stream: StreamId) -> UnixStream {
    let (one, other) = UnixStream::pair().unwrap();
    let breaker = one.try_clone().unwrap();
    left.handle.join(stream, Ends::socket(one).unwrap());
    right.handle.join(stream, Ends::socket(other).unwrap());
    breaker
}

/// What a peer sends: control messages, and blocks of a byte or so to a
/// few fragments, each saying which it is.
struct Traffic {
    controls: Vec<Vec<u8>>,
    blocks: Vec<Vec<u8>>,
}

impl Traffic {
    fn new(name: &str, controls: usize, blocks: usize) -> Self {
        Self {
            controls: (0..controls)
                .map(|i| format!("{name}'s control message {i}").into_bytes())
                .collect(),
            blocks: (0..blocks)
                .map(|i| {
                    let mut block = format!("{name}'s block {i}:").into_bytes();
                    block.resize(block.len() + (i * 7919 + 13) % (5 * FRAGMENT), i as u8);
                    block
                })
                .collect(),
        }
    }

    /// Hands it all to `side`, control messages and blocks in turn, then
    /// says the peer is done.
    fn send(&self, side: &Side) {
        let mut blocks = self.blocks.iter();
        for control in &self.controls {
            side.handle.control(control.clone());
            if let Some(block) = blocks.next() {
                side.handle.block(Bytes::new(block.clone()));
            }
        }
        for block in blocks {
            side.handle.block(Bytes::new(block.clone()));
        }
        side.handle.close();
    }

    /// Whether `received` is this: the control messages in order, each
    /// block once, in any order.
    fn check(&self, received: &Traffic) {
        assert_eq!(received.controls, self.controls);
        let mut blocks = received.blocks.clone();
        blocks.sort();
        let mut sent = self.blocks.clone();
        sent.sort();
        assert!(blocks == sent, "blocks came missing, or twice");
    }
}

/// Takes what a side delivers until its session is closed.
fn receive(deliveries: &Receiver<Delivery>) -> Traffic {
    let mut received = Traffic {
        controls: Vec::new(),
        blocks: Vec::new(),
    };
    loop {
        match deliveries
            .recv_timeout(WAIT)
            .expect("the session closed in time")
        {
            Delivery::Control(message) => received.controls.push(message.to_vec()),
            Delivery::Block(bytes) => received.blocks.push(bytes.to_vec()),
            Delivery::Closed => return received,
            Delivery::Lost => panic!("the session gave up"),
        }
    }
}

/// Exchanges traffic between two sides joined by `streams` streams, while
/// `meanwhile` is called every couple of milliseconds until both sessions
/// are closed, with the streams' left ends, to break and join them; and
/// checks that each side got what the other sent. Returns the sides.
fn exchange(
    streams: StreamId,
    mut meanwhile: impl FnMut(&Side, &Side, &mut Vec<(StreamId, UnixStream)>),
) -> (Side, Side) {
    let left = side();
    let right = side();
    let mut links: Vec<(StreamId, UnixStream)> = (0..streams)
        .map(|stream| (stream, link(&left, &right, stream)))
        .collect();
    let from_left = Traffic::new("left", 150, 300);
    let from_right = Traffic::new("right", 100, 200);
    from_left.send(&left);
    from_right.send(&right);
    let to_left = {
        let deliveries = left.deliveries.clone();
        std::thread::spawn(move || receive(&deliveries))
    };
    let to_right = {
        let deliveries = right.deliveries.clone();
        std::thread::spawn(move || receive(&deliveries))
    };
    while !(to_left.is_finished() && to_right.is_finished()) {
        meanwhile(&left, &right, &mut links);
        std::thread::sleep(Duration::from_millis(2));
    }
    from_right.check(&to_left.join().unwrap());
    from_left.check(&to_right.join().unwrap());
    (left, right)
}

/// The notices a side gave, so far.
fn notices(side: &Side) -> Vec<Notice> {
    side.notices.try_iter().collect()
}

/// Waits for a side to give `notice`, passing others.
fn noticed(side: &Side, notice: &Notice) {
    while side.notices.recv_timeout(WAIT).expect("noticed in time") != *notice {}
}

/// Drops a side's handle, and waits for its driver to end.
fn end(side: Side) {
    let Side { handle, thread, .. } = side;
    drop(handle);
    thread.join().unwrap();
}

#[test]
fn what_each_peer_sends_comes_once_over_streams_that_hold() {
    let (left, right) = exchange(3, |_, _, _| {});
    assert_eq!(notices(&left), []);
    assert_eq!(notices(&right), []);
    end(left);
    end(right);
}

/// Streams are broken, one at a time, both ways or only one, as the data
/// flows; each is noticed on both sides, and another joined in its place,
/// numbered anew.
#[test]
fn what_each_peer_sends_comes_once_as_streams_break_and_others_join() {
    let mut next = 3;
    let mut broken = 0;
    let (left, right) = exchange(3, |left, right, links| {
        if broken >= 20 {
            return;
        }
        let (_, end) = links.remove(broken % links.len());
        let how = if broken % 3 == 2 {
            Shutdown::Write
        } else {
            Shutdown::Both
        };
        end.shutdown(how).unwrap();
        broken += 1;
        links.push((next, link(left, right, next)));
        next += 1;
    });
    // Breaks are noticed; none is warned of.
    for side in [&left, &right] {
        let noticed = notices(side);
        assert!(
            noticed
                .iter()
                .all(|notice| matches!(notice, Notice::Broke(_))),
            "{noticed:?}"
        );
        assert!(!noticed.is_empty());
    }
    assert!(broken >= 2, "the data flowed past {broken} breaks only");
    end(left);
    end(right);
}

/// Every stream breaks at once; the sessions wait for others, which are
/// joined a while after.
#[test]
fn every_stream_breaks_and_others_join_in_the_grace() {
    let mut done = false;
    let (left, right) = exchange(2, |left, right, links| {
        if done {
            return;
        }
        done = true;
        for (_, end) in links.drain(..) {
            end.shutdown(Shutdown::Both).unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
        for stream in 2..4 {
            links.push((stream, link(left, right, stream)));
        }
    });
    for side in [&left, &right] {
        let noticed = notices(side);
        assert!(noticed.contains(&Notice::Broke(0)), "{noticed:?}");
        assert!(noticed.contains(&Notice::Broke(1)), "{noticed:?}");
    }
    end(left);
    end(right);
}

/// Reads the frames from `end` until it ends.
fn frames(end: UnixStream) -> Vec<Frame> {
    let mut reader = BufReader::new(end);
    let mut frames = Vec::new();
    while let Some(frame) = frame::read(&mut reader).unwrap() {
        frames.push(frame);
    }
    frames
}

/// A session closed says its end on a stream joined after, should the
/// other side have missed it, then closes it.
#[test]
fn a_session_closed_answers_a_stream_joined_after() {
    let (left, right) = exchange(1, |_, _, _| {});
    let (one, other) = UnixStream::pair().unwrap();
    left.handle.join(7, Ends::socket(one).unwrap());
    let hello = Frame::Hello {
        session: ID,
        stream: 7,
    };
    assert_eq!(frames(other), [hello, Frame::End]);
    end(left);
    end(right);
}

/// A stream on which what comes is not a frame is shut down, and why is
/// told; the session goes on over the others.
#[test]
fn a_stream_that_is_not_one_is_warned_of_and_shut() {
    let left = side();
    let right = side();
    let _link = link(&left, &right, 0);
    let (one, mut other) = UnixStream::pair().unwrap();
    left.handle.join(9, Ends::socket(one).unwrap());
    std::io::Write::write_all(&mut other, &[9, 0, 0, 0, 0]).unwrap();
    noticed(
        &left,
        &Notice::Warn("stream 9: a frame of kind 9".to_owned()),
    );
    noticed(&left, &Notice::Broke(9));
    // Shut down: what it was told, if anything, ends.
    let mut said = Vec::new();
    other.read_to_end(&mut said).unwrap();
    let from_left = Traffic::new("left", 20, 20);
    let from_right = Traffic::new("right", 20, 20);
    from_left.send(&left);
    from_right.send(&right);
    from_right.check(&receive(&left.deliveries));
    from_left.check(&receive(&right.deliveries));
    end(left);
    end(right);
}

/// A stream numbered as one still there is refused, and closed at once.
#[test]
fn a_stream_joined_again_by_its_number_is_refused() {
    let left = side();
    let (first, _kept) = UnixStream::pair().unwrap();
    left.handle.join(0, Ends::socket(first).unwrap());
    let (second, other) = UnixStream::pair().unwrap();
    left.handle.join(0, Ends::socket(second).unwrap());
    assert_eq!(frames(other), []);
    let warned = left.notices.recv_timeout(WAIT).unwrap();
    assert_eq!(
        warned,
        Notice::Warn("stream 0 was joined again, as another still there".to_owned())
    );
    end(left);
}

/// Once every handle is gone, the driver shuts its streams down, and ends.
#[test]
fn once_its_handles_are_gone_the_driver_shuts_its_streams_and_ends() {
    let left = side();
    let (one, other) = UnixStream::pair().unwrap();
    left.handle.join(0, Ends::socket(one).unwrap());
    let mut reader = BufReader::new(other);
    let hello = Frame::Hello {
        session: ID,
        stream: 0,
    };
    assert_eq!(frame::read(&mut reader).unwrap(), Some(hello));
    end(left);
    assert_eq!(frame::read(&mut reader).unwrap(), None);
}
