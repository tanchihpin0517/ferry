use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::*;
use crate::protocol::{self, Control};
use crate::test_support::{PATIENCE, within};
use crate::transport::pipes::{self, Connection, Kind};
use crate::transport::start;

/// A wire on `socket`, within a scope of its own.
fn socket(socket: UnixStream) -> Wire {
    Wire::socket(socket, &Scope::new().unwrap()).unwrap()
}

/// A wire on two pipes, within a scope of its own: what it reads, and what
/// it writes.
fn pipes(input: io::PipeReader, output: io::PipeWriter) -> Wire {
    Wire::new(
        File::from(OwnedFd::from(input)),
        File::from(OwnedFd::from(output)),
        None,
        &Scope::new().unwrap(),
    )
    .unwrap()
}

/// `message` on control, then control's end, as the far end writes them.
fn control_then_end(message: &Control) -> Vec<u8> {
    let mut bytes = pipes::frame(
        pipes::CONTROL,
        Kind::Bytes,
        &protocol::encode(message).unwrap(),
    );
    bytes.extend(pipes::frame(pipes::CONTROL, Kind::End, b""));
    bytes
}

/// What a shell printed before the start mark is skipped; what came after,
/// with the mark, read ahead, goes on the pipes first; a wire that ends
/// before its start mark never started.
#[test]
fn a_wire_skips_to_its_start_mark() {
    within("a wire skips to its start mark", PATIENCE, || {
        let (one, mut other) = UnixStream::pair().unwrap();
        let mut wire = socket(one);
        other.write_all(b"noise from a shell\n").unwrap();
        other.write_all(start::MARK).unwrap();
        other.write_all(&control_then_end(&Control::Done)).unwrap();
        wire.started().unwrap();
        let mut connection = Connection::open(vec![wire], &pipes::link()).unwrap();
        let mut reading = connection.reader(pipes::CONTROL);
        assert!(matches!(
            protocol::expect::<Control>(&mut reading).unwrap(),
            Control::Done
        ));
        assert!(protocol::receive(&mut reading).unwrap().is_none());
        drop(other);

        let (one, mut other) = UnixStream::pair().unwrap();
        let mut wire = socket(one);
        other.write_all(b"no mark").unwrap();
        drop(other);
        let error = wire.started().unwrap_err();
        assert!(
            error.to_string().contains("ended before it started"),
            "{error}"
        );
    });
}

/// On pipes, which cannot be shut down, the far end still reads to the end:
/// each pipe's end goes, and the writing end closes as the connection is
/// let go of.
#[test]
fn on_pipes_the_end_comes_as_the_writing_end_closes() {
    within(
        "on pipes the end comes as the writing end closes",
        PATIENCE,
        || {
            let (from_other, mut to_this) = std::io::pipe().unwrap();
            let (from_this, to_other) = std::io::pipe().unwrap();
            let connection = Connection::open(vec![pipes(from_other, to_other)], &pipes::link());
            let connection = connection.unwrap();
            connection
                .writer(pipes::CONTROL)
                .write_all(&protocol::encode(&Control::Done).unwrap())
                .unwrap();
            connection.finish().unwrap();
            // The far end's control ends too, so that all ended both ways.
            to_this
                .write_all(&pipes::frame(pipes::CONTROL, Kind::End, b""))
                .unwrap();
            let mut from_this = from_this;
            let reading = std::thread::spawn(move || {
                let mut came = Vec::new();
                from_this.read_to_end(&mut came).unwrap();
                came
            });
            std::thread::sleep(Duration::from_millis(50));
            drop(connection);
            assert_eq!(reading.join().unwrap(), control_then_end(&Control::Done));
            drop(to_this);
        },
    );
}

/// A pipe ends with its end, though what carries it stays open, as a pipe a
/// joining process still holds: the reader waits for nothing more.
#[test]
fn the_end_ends_the_pipe_though_what_carries_it_stays_open() {
    within(
        "the end ends the pipe though what carries it stays open",
        PATIENCE,
        || {
            let (from_other, mut to_this) = std::io::pipe().unwrap();
            let (_from_this, to_other) = std::io::pipe().unwrap();
            let mut connection =
                Connection::open(vec![pipes(from_other, to_other)], &pipes::link()).unwrap();
            let mut reading = connection.reader(pipes::CONTROL);
            to_this
                .write_all(&control_then_end(&Control::Done))
                .unwrap();
            let started = Instant::now();
            assert!(matches!(
                protocol::expect::<Control>(&mut reading).unwrap(),
                Control::Done
            ));
            assert!(protocol::receive(&mut reading).unwrap().is_none());
            assert!(started.elapsed() < Duration::from_secs(5));
            // Still open.
            drop(to_this);
        },
    );
}

/// A wire let go of stops its pipes' readers, waiting, at once, though what
/// carries it cannot be shut down, as a pipe.
#[test]
fn a_wire_let_go_of_stops_its_reader_at_once() {
    within(
        "a wire let go of stops its reader at once",
        PATIENCE,
        || {
            // On pipes, which cannot be shut down as a socket is.
            let (from_other, _to_this) = std::io::pipe().unwrap();
            let (_from_this, to_other) = std::io::pipe().unwrap();
            let mut connection =
                Connection::open(vec![pipes(from_other, to_other)], &pipes::link()).unwrap();
            let mut reading = connection.reader(pipes::CONTROL);
            let carried = connection.carried().remove(0);
            let letting = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                carried.let_go();
            });
            let started = Instant::now();
            let error = match protocol::receive(&mut reading) {
                Err(error) => format!("{error:#}"),
                Ok(_) => panic!("read"),
            };
            assert!(error.contains("let go"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(5));
            letting.join().unwrap();
        },
    );
}
