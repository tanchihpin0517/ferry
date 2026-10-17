//! `ferry server` on its own, as a client would start it.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn a_server_refuses_another_build() {
    // A Hello from another build, written by hand to the server's input.
    let mut hello = vec![0u8]; // control packet
    // Hello { build: "xyz", role: Destination, root: [], dry_run: true,
    // modify_window: 0, numeric_ids: false, fsync: true, delete: false,
    // compress: true, compress_level: 0, profile: false, part_entries: 0,
    // backlog_files: 0, paced_bytes: 0 }
    let payload = [
        0u8, 3, b'x', b'y', b'z', 1, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0,
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
    use ferry::protocol::{Control, Hello, Role};
    use ferry::report::Report;
    use ferry::transport::Transport;

    let temp = tempfile::tempdir().unwrap();
    let script = format!("exec '{}' server", env!("CARGO_BIN_EXE_ferry"));
    let mut control = Transport::Local.control(&script, &[]).unwrap();
    let welcome = control
        .handshake(Hello {
            build: ferry::build_info::identity(),
            role: Role::Destination,
            root: temp.path().join("d").into_os_string().into_encoded_bytes(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: false,
            fsync: true,
            delete: false,
            compress: true,
            compress_level: 1,
            profile: true,
            part_entries: 0,
            backlog_files: 0,
            paced_bytes: 0,
        })
        .unwrap();
    assert_eq!(welcome.token.len(), 32, "{}", welcome.token);
    assert!(welcome.socket.ends_with(welcome.token.as_bytes()));
    // Once its paths and the control connection's channel have come, asked
    // where its threads are, it answers on the control connection; and
    // every second it sends its metrics there, unasked.
    let join = format!(
        "exec '{}' server --join {}",
        env!("CARGO_BIN_EXE_ferry"),
        welcome.token
    );
    let mut connection = Transport::Local.connect(&welcome, Some(&join)).unwrap();
    control.attach(&mut connection);
    let mut reader = control.take_reader().unwrap();
    control.sender().send(&Control::Snapshot).unwrap();
    let (mut snapshot, mut metrics) = (false, false);
    while !(snapshot && metrics) {
        match ferry::protocol::expect::<Report>(&mut reader).unwrap() {
            Report::Snapshot(_) => snapshot = true,
            Report::Metrics(sent) => {
                assert!(sent.sequence >= 1);
                metrics = true;
            }
            other => panic!("{other:?}"),
        }
    }
    // The client goes away instead of sending the manifest, and so does the
    // server, rather than waiting for it forever.
    let error = control.close().unwrap_err().to_string();
    assert!(error.contains("exited"), "{error}");
}
