//! `ferry server` on its own, as a client would start it.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn a_server_refuses_another_build() {
    // A Hello from another build, written by hand to the server's input.
    let mut hello = vec![0u8]; // control packet
    // Hello { build: "xyz", role: Destination, root: [], dry_run: true,
    // modify_window: 0, numeric_ids: false, fsync: true, delete: false,
    // compress: true, compress_level: 0 }
    let payload = [0u8, 3, b'x', b'y', b'z', 1, 0, 1, 0, 0, 1, 0, 1, 0];
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
    // A Refused packet whose reason names both builds.
    assert_eq!(output.stdout[0], 0);
    let reason = String::from_utf8_lossy(&output.stdout[5..]);
    assert!(
        reason.contains("xyz") && reason.contains("dev-"),
        "{reason}"
    );
}

#[test]
fn a_server_ends_with_its_control_connection() {
    use ferry::protocol::{Hello, Role};
    use ferry::transport::Transport;

    let temp = tempfile::tempdir().unwrap();
    let script = format!("exec '{}' server", env!("CARGO_BIN_EXE_ferry"));
    let mut control = Transport::Local.control(&script).unwrap();
    let token = control
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
        })
        .unwrap();
    assert_eq!(token.len(), 32, "{token}");
    // No path ever joins; the client goes away instead, and so does the
    // server, rather than waiting for a path forever.
    let error = control.close().unwrap_err().to_string();
    assert!(error.contains("exited"), "{error}");
}
