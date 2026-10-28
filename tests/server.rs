//! `ferry server` on its own, as a client would start it.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use ferry::protocol::Message;
use ferry::transport::pipes::{self, Kind};

/// Writes `message` on the control pipe of a control connection past its
/// handshake, framed as the transport does.
fn send_control<M: Message>(input: &mut impl Write, message: &M) {
    let packet = ferry::protocol::encode(message).unwrap();
    input
        .write_all(&pipes::frame(pipes::CONTROL, Kind::Bytes, &packet))
        .unwrap();
}

/// The next message on the control pipe of a control connection past its
/// handshake, what came of it so far in `taken`: the frames of other pipes,
/// and of the transport's own, skipped.
fn expect_control<M: Message>(output: &mut impl Read, taken: &mut Vec<u8>) -> anyhow::Result<M> {
    loop {
        let mut reader = &taken[..];
        if let Ok(Some(packet)) = ferry::protocol::receive(&mut reader) {
            let used = taken.len() - reader.len();
            let message = packet.decode::<M>();
            taken.drain(..used);
            return message;
        }
        let mut header = [0; pipes::HEADER];
        output.read_exact(&mut header)?;
        let length = u32::from_le_bytes(header[2..].try_into().unwrap()) as usize;
        let mut content = vec![0; length];
        output.read_exact(&mut content)?;
        if header[0] == pipes::CONTROL && header[1] == Kind::Bytes as u8 {
            taken.extend_from_slice(&content);
        }
    }
}

#[test]
fn a_server_refuses_another_build() {
    // A Hello from another build, written by hand to the server's input.
    let mut hello = vec![0u8]; // control packet
    // Hello { build: "xyz", role: Destination, root: [], dry_run: true,
    // modify_window: 0, numeric_ids: false, fsync: true, delete: false,
    // compress: true, compress_level: 0, profile: false, batch_entries: 0,
    // backlog_files: 0, paced_bytes: 0, cpus: 0 }
    let payload = [
        0u8, 3, b'x', b'y', b'z', 1, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0,
    ];
    hello.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    hello.extend_from_slice(&payload);

    let mut server = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    server.stdin.take().unwrap().write_all(&hello).unwrap();
    let output = server.wait_with_output().unwrap();
    assert!(!output.status.success());
    // A Refused packet whose reason names both builds, between the marks.
    use ferry::transport::start::{END, MARK};
    assert!(output.stdout.starts_with(MARK) && output.stdout.ends_with(END));
    let packet = &output.stdout[MARK.len()..output.stdout.len() - END.len()];
    assert_eq!(packet[0], 0);
    let reason = String::from_utf8_lossy(&packet[5..]);
    assert!(
        reason.contains("xyz") && reason.contains("dev-"),
        "{reason}"
    );
}

#[test]
fn a_server_ends_with_its_control_connection() {
    use ferry::monitor::client::Node;
    use ferry::protocol::{Control, Hello, Role};
    use ferry::report::Report;
    use ferry::transport::Transport;

    let temp = tempfile::tempdir().unwrap();
    let script = format!("exec '{}' server", env!("CARGO_BIN_EXE_ferry"));
    let mut control = Transport::Local(Node::detached().unwrap())
        .control(&script, &Node::detached().unwrap())
        .unwrap();
    let welcome = control
        .handshake(Hello {
            build: ferry::build_info::identity(),
            role: Role::Destination,
            root: temp.path().join("d").into_os_string().into_encoded_bytes(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: false,
            delete: false,
            one_file_system: false,
            compress: true,
            compress_level: 1,
            profile: true,
            batch_entries: 0,
            backlog_files: 0,
            paced_bytes: 0,
            data_credit: 0,
            cpus: 0,
            place: 0,
        })
        .unwrap();
    // A socket named by a token of 32 hexadecimal digits.
    let socket = String::from_utf8(welcome.socket.clone()).unwrap();
    let token = socket.rsplit('/').next().unwrap();
    assert_eq!(token.len(), 32, "{socket}");
    assert!(
        token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{socket}"
    );
    control
        .sender()
        .send(&Control::Proceed(ferry::protocol::Proceed {
            guard: ferry::protocol::Guard::default(),
            streams: 1,
        }))
        .unwrap();
    // Once told to go on, asked where its threads are, it answers on the
    // control pipe; and every second it sends its metrics there, unasked,
    // and what its stream carried, before the stream comes. What its peer
    // reports goes on the other pipe.
    let (mut reader, _reports) = control.take_readers().unwrap();
    control.sender().send(&Control::Snapshot).unwrap();
    let (mut snapshot, mut metrics) = (false, false);
    while !(snapshot && metrics) {
        match ferry::protocol::expect::<Report>(&mut reader).unwrap() {
            Report::Snapshot(_) => snapshot = true,
            Report::Metrics(sent) => {
                assert!(sent.sequence >= 1);
                metrics = true;
            }
            Report::Queues(_)
            | Report::Counts(_)
            | Report::Transferring(_)
            | Report::Carried(_) => {}
            other => panic!("{other:?}"),
        }
    }
    // The client goes away instead of sending the manifest, and so does the
    // server, rather than waiting for it forever.
    let error = control.close().unwrap_err().to_string();
    assert!(error.contains("exited"), "{error}");
}

#[test]
fn a_server_not_told_to_go_on_ends_having_changed_nothing() {
    use ferry::monitor::client::Node;
    use ferry::protocol::{Hello, Role};
    use ferry::transport::Transport;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("d");
    let script = format!("exec '{}' server", env!("CARGO_BIN_EXE_ferry"));
    let mut control = Transport::Local(Node::detached().unwrap())
        .control(&script, &Node::detached().unwrap())
        .unwrap();
    let welcome = control
        .handshake(Hello {
            build: ferry::build_info::identity(),
            role: Role::Destination,
            root: root.clone().into_os_string().into_encoded_bytes(),
            dry_run: false,
            modify_window: 0,
            numeric_ids: false,
            delete: true,
            one_file_system: false,
            compress: true,
            compress_level: 1,
            profile: false,
            batch_entries: 0,
            backlog_files: 0,
            paced_bytes: 0,
            data_credit: 0,
            cpus: 0,
            place: 0,
        })
        .unwrap();
    // It says what its root is, not there yet, under the directory that is.
    assert!(!welcome.root.exists);
    assert!(
        welcome.root.canonical.ends_with(b"/d"),
        "{:?}",
        welcome.root
    );
    assert!(!welcome.root.chain.is_empty());
    // The client refuses, and closes the connection rather than say to go
    // on: the server ends, having made nothing.
    control.close().unwrap();
    assert!(!root.exists());
    assert!(
        !std::path::Path::new(std::ffi::OsStr::new(
            &String::from_utf8(welcome.socket).unwrap()
        ))
        .exists()
    );
}

/// Once it has answered the hello, a server on whose control connection
/// nothing comes, though it stays open, does not end by its silence, which
/// is its monitor's to judge, but once its lifeline breaks, as its monitor
/// is gone: before the client said to go on, and after, as it waits for the
/// stream.
#[test]
fn a_server_ends_once_its_lifeline_breaks_not_by_silence() {
    use std::io::BufReader;
    use std::time::{Duration, Instant};

    use ferry::protocol::{self, Control, Guard, Hello, Role};
    use ferry::transport::start;

    for go_on in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (lifeline, stderr) = ferry::transport::lifeline::pair().unwrap();
        let mut server = Command::new(env!("CARGO_BIN_EXE_ferry"))
            .args(["server", "--lifeline"])
            .env("FERRY_TEST_SILENCE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let mut input = server.stdin.take().unwrap();
        let mut output = BufReader::new(server.stdout.take().unwrap());
        let hello = Hello {
            build: ferry::build_info::identity(),
            role: Role::Destination,
            root: temp.path().join("d").into_os_string().into_encoded_bytes(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: false,
            delete: false,
            one_file_system: false,
            compress: true,
            compress_level: 1,
            profile: false,
            batch_entries: 0,
            backlog_files: 0,
            paced_bytes: 0,
            data_credit: 0,
            cpus: 0,
            place: 0,
        };
        protocol::send(&mut input, &Control::Hello(hello)).unwrap();
        assert!(start::skip(&mut output).unwrap().found);
        assert!(matches!(
            protocol::expect::<Control>(&mut output).unwrap(),
            Control::Welcome(_)
        ));
        start::expect_end(&mut output).unwrap();
        if go_on {
            send_control(
                &mut input,
                &Control::Proceed(protocol::Proceed {
                    guard: Guard::default(),
                    streams: 1,
                }),
            );
        }
        // Nothing more comes, though the connection stays open, longer
        // than any silence there was.
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            server.try_wait().unwrap().is_none(),
            "the server ended by its silence ({go_on})"
        );
        drop(lifeline);
        let began = Instant::now();
        while server.try_wait().unwrap().is_none() {
            if began.elapsed() > Duration::from_secs(30) {
                let _ = server.kill();
                panic!("the server stayed though its lifeline broke ({go_on})");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(input);
    }
}

/// A stream that ends before it says which one it is fails the server,
/// which says so, and ends: rather than wait for that stream, which never
/// comes, as the other side, its own streams cut too, has ended.
#[test]
fn a_stream_that_ends_before_its_header_fails_the_server() {
    use std::io::{BufReader, Read};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use ferry::protocol::{self, Control, Guard, Hello, Role};
    use ferry::report::Report;
    use ferry::transport::{STRAIGHT, start};

    let temp = tempfile::tempdir().unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = server.stdin.take().unwrap();
    let mut output = BufReader::new(server.stdout.take().unwrap());
    let hello = Hello {
        build: ferry::build_info::identity(),
        role: Role::Destination,
        root: temp.path().join("d").into_os_string().into_encoded_bytes(),
        dry_run: true,
        modify_window: 0,
        numeric_ids: false,
        delete: false,
        one_file_system: false,
        compress: true,
        compress_level: 1,
        profile: false,
        batch_entries: 0,
        backlog_files: 0,
        paced_bytes: 0,
        data_credit: 0,
        cpus: 0,
        place: 0,
    };
    protocol::send(&mut input, &Control::Hello(hello)).unwrap();
    assert!(start::skip(&mut output).unwrap().found);
    let Control::Welcome(welcome) = protocol::expect::<Control>(&mut output).unwrap() else {
        panic!("not welcomed");
    };
    start::expect_end(&mut output).unwrap();
    send_control(
        &mut input,
        &Control::Proceed(protocol::Proceed {
            guard: Guard::default(),
            streams: 1,
        }),
    );
    // A stream comes straight, and is answered, then ends at once.
    let socket = std::str::from_utf8(&welcome.socket).unwrap();
    let mut stream = UnixStream::connect(socket).unwrap();
    std::io::Write::write_all(&mut stream, &[STRAIGHT]).unwrap();
    let mut mark = vec![0u8; start::MARK.len()];
    stream.read_exact(&mut mark).unwrap();
    drop(stream);
    // The server says why, on control, and ends.
    let began = Instant::now();
    let mut taken = Vec::new();
    let said = loop {
        match expect_control::<Report>(&mut output, &mut taken) {
            Ok(Report::Failed(message)) => break message,
            Ok(_) => {}
            Err(error) => panic!("the server said nothing of why: {error:#}"),
        }
        assert!(began.elapsed() < Duration::from_secs(30), "no failure said");
    };
    assert!(said.contains("before it said which one it is"), "{said}");
    while server.try_wait().unwrap().is_none() {
        if began.elapsed() > Duration::from_secs(30) {
            let _ = server.kill();
            panic!("the server stayed, waiting for its stream");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!server.wait().unwrap().success());
    drop(input);
}
