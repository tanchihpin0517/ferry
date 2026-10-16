//! The handshake that opens a server: the client's hello, and the server's
//! answer, over the server's control connection.

use std::ffi::OsStr;
use std::io::{BufRead, Read, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::{Context, Result, bail};

use crate::protocol::{self, Control, Hello, Role, Welcome};
use crate::source;
use crate::transport::start;

/// The connection closed before the server answered the hello: the server
/// did not start, or exited at once.
#[derive(Debug)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("the connection closed before the server answered")
    }
}

impl std::error::Error for Closed {}

/// The client's side: says hello and waits to be welcomed; returns where the
/// server waits for its paths.
pub fn client_handshake(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    hello: Hello,
    skipped: impl FnOnce(&[u8]),
) -> Result<Welcome> {
    protocol::send(writer, &Control::Hello(hello))?;
    // A server that is gone shows as a closed connection when its answer is
    // read, rather than as a failed write here.
    let _ = writer.flush();
    // What a shell printed before the server started is skipped; with no
    // start mark, the server never started.
    let start = start::skip(reader).context("wait for the server to start")?;
    skipped(&start.before);
    if !start.found {
        return Err(Closed.into());
    }
    let Some(answer) = protocol::receive(reader).context("wait for the server's answer")? else {
        return Err(Closed.into());
    };
    match answer.decode::<Control>()? {
        Control::Welcome(welcome) => Ok(welcome),
        Control::Refused { reason } => bail!(reason),
        Control::Error { message } => bail!("the server failed: {message}"),
        other => bail!("expected the server's answer, got {other:?}"),
    }
}

/// The server's side: accepts a hello from the same build, and refuses any
/// other, or one whose Source cannot start, telling the client why. Once the
/// hello is accepted, `joinable` gets ready for the paths, and the welcome
/// says where they reach it.
pub fn server_handshake(
    reader: &mut impl Read,
    writer: &mut impl Write,
    build: &str,
    joinable: impl FnOnce() -> Result<Welcome>,
) -> Result<Hello> {
    // First, so that the client knows where the server's output starts.
    start::write(writer).context("write the start mark")?;
    let hello = match protocol::expect::<Control>(reader).context("wait for the client's hello")? {
        Control::Hello(hello) => hello,
        other => bail!("expected a hello, got {other:?}"),
    };
    if hello.build != build {
        let reason = format!(
            "the server is build {build}, but the client is build {}",
            hello.build
        );
        return refuse(writer, reason);
    }
    // A Source checks its root before the transfer starts, as the client
    // does for a local one.
    if hello.role == Role::Source
        && let Err(error) = source::check_root(OsStr::from_bytes(&hello.root))
    {
        return refuse(writer, format!("{error:#}"));
    }
    let welcome = match joinable() {
        Ok(welcome) => welcome,
        Err(error) => return refuse(writer, format!("{error:#}")),
    };
    protocol::send(writer, &Control::Welcome(welcome))?;
    writer.flush()?;
    Ok(hello)
}

/// Tells the client why the server will not go on, and fails with the reason.
fn refuse<T>(writer: &mut impl Write, reason: String) -> Result<T> {
    protocol::send(
        writer,
        &Control::Refused {
            reason: reason.clone(),
        },
    )?;
    writer.flush()?;
    bail!(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(build: &str) -> Hello {
        Hello {
            build: build.to_owned(),
            role: crate::protocol::Role::Destination,
            root: b"dest".to_vec(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: false,
            fsync: true,
            delete: false,
            compress: true,
            compress_level: 1,
            profile: false,
        }
    }

    /// Runs the server's handshake on the client's packets, then the client's
    /// on the server's answer.
    fn handshake(client_build: &str, server_build: &str) -> (Result<Hello>, Result<Welcome>) {
        exchange(hello(client_build), server_build)
    }

    fn exchange(sent: Hello, server_build: &str) -> (Result<Hello>, Result<Welcome>) {
        let mut to_server = Vec::new();
        protocol::send(&mut to_server, &Control::Hello(sent.clone())).unwrap();
        let mut to_client = Vec::new();
        let server = server_handshake(
            &mut to_server.as_slice(),
            &mut to_client,
            server_build,
            || {
                Ok(Welcome {
                    token: "token".to_owned(),
                    socket: b"/tmp/socket".to_vec(),
                })
            },
        );

        let mut ignored = Vec::new();
        let client = client_handshake(&mut to_client.as_slice(), &mut ignored, sent, |_| {});
        (server, client)
    }

    #[test]
    fn the_same_build_is_welcomed() {
        let (server, client) = handshake("dev-1", "dev-1");
        assert_eq!(server.unwrap(), hello("dev-1"));
        assert_eq!(client.unwrap().token, "token");
    }

    #[test]
    fn another_build_is_refused_with_a_reason() {
        let (server, client) = handshake("dev-1", "dev-2");
        assert!(server.is_err());
        let message = client.unwrap_err().to_string();
        assert!(
            message.contains("dev-1") && message.contains("dev-2"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_source_is_refused_with_a_reason() {
        let sent = Hello {
            role: Role::Source,
            root: b"/nonexistent/ferry-source".to_vec(),
            ..hello("dev-1")
        };
        let (server, client) = exchange(sent, "dev-1");
        assert!(server.is_err());
        let message = client.unwrap_err().to_string();
        assert_eq!(
            message,
            "the source /nonexistent/ferry-source does not exist"
        );
    }

    #[test]
    fn a_server_gone_before_answering_is_told_apart() {
        let mut ignored = Vec::new();
        let error =
            client_handshake(&mut [].as_slice(), &mut ignored, hello("dev-1"), |_| {}).unwrap_err();
        assert!(error.is::<Closed>(), "{error:#}");
    }
}
