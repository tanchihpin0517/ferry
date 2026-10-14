//! `ferry server` on its own, as a client would start it.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn a_server_refuses_another_build() {
    // A Hello from another build, written by hand to the server's input.
    let mut hello = vec![0u8]; // control packet
    let payload = [0u8, 3, b'x', b'y', b'z', 0, 1]; // Hello { build: "xyz", destination: [], dry_run: true }
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
