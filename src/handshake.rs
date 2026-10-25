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
/// server waits for its streams.
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
    // The answer is between the start mark and the end mark: what a shell
    // printed before it is skipped; with no start mark, the server never
    // answered.
    let start = start::skip(reader).context("wait for the server's answer")?;
    skipped(&start.before);
    if !start.found {
        return Err(Closed.into());
    }
    let answer = protocol::receive(reader)
        .context("read the server's answer")?
        .context("the server's answer ended early")?;
    start::expect_end(reader).context("read the server's answer")?;
    match answer.decode::<Control>()? {
        Control::Welcome(welcome) => Ok(welcome),
        Control::Refused { reason } => bail!(reason),
        Control::Error { message } => bail!("the server failed: {message}"),
        other => bail!("expected the server's answer, got {other:?}"),
    }
}

/// The server's side: accepts a hello from the same build, and refuses any
/// other, or one whose Source cannot start, telling the client why. Once the
/// hello is accepted, `joinable` gets ready for the streams, and the welcome
/// says where they reach it, and what the hello's root is.
pub fn server_handshake(
    reader: &mut impl Read,
    writer: &mut impl Write,
    build: &str,
    joinable: impl FnOnce(&Hello) -> Result<Welcome>,
) -> Result<Hello> {
    let packet = protocol::receive(reader)
        .context("wait for the client's hello")?
        .context("the connection closed before the client's hello")?;
    // The build first, as one of another build may lay the rest out
    // otherwise.
    if let Some(theirs) = protocol::hello_build(&packet)
        && theirs != build
    {
        let reason = format!("the server is build {build}, but the client is build {theirs}");
        return refuse(writer, reason);
    }
    let hello = match packet.decode::<Control>() {
        Ok(Control::Hello(hello)) => hello,
        Ok(other) => bail!("expected a hello, got {other:?}"),
        Err(error) => {
            return refuse(
                writer,
                format!("the client's hello could not be read: {error:#}"),
            );
        }
    };
    // A Source checks its root before the transfer starts, as the client
    // does for a local one.
    if hello.role == Role::Source
        && let Err(error) = source::check_root(OsStr::from_bytes(&hello.root))
    {
        return refuse(writer, format!("{error:#}"));
    }
    let welcome = match joinable(&hello) {
        Ok(welcome) => welcome,
        Err(error) => return refuse(writer, format!("{error:#}")),
    };
    answer(writer, &Control::Welcome(welcome))?;
    Ok(hello)
}

/// Sends the client the server's answer, between the marks, in one write.
fn answer(writer: &mut impl Write, answer: &Control) -> Result<()> {
    writer.write_all(&start::framed(&protocol::encode(answer)?))?;
    writer.flush()?;
    Ok(())
}

/// Tells the client why the server will not go on, and fails with the reason.
fn refuse<T>(writer: &mut impl Write, reason: String) -> Result<T> {
    answer(
        writer,
        &Control::Refused {
            reason: reason.clone(),
        },
    )?;
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
            delete: false,
            one_file_system: false,
            compress: true,
            compress_level: 1,
            profile: false,
            part_entries: 0,
            backlog_files: 0,
            paced_bytes: 0,
            in_flight_entries: 0,
            data_credit: 0,
            cpus: 0,
            place: 0,
            session: [0; 16],
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
            |_| {
                Ok(Welcome {
                    socket: b"/tmp/socket".to_vec(),
                    root: crate::protocol::RootIdentity::default(),
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
        assert_eq!(client.unwrap().socket, b"/tmp/socket");
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

    /// A hello of another build, laid out otherwise past its build, is
    /// refused, saying both builds; one of this build that cannot be read is
    /// refused, saying so, not left without an answer.
    #[test]
    fn a_hello_laid_out_otherwise_is_refused_with_a_reason() {
        let run = |payload: &[u8]| {
            let mut to_server = vec![protocol::PacketType::Control as u8];
            to_server.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            to_server.extend_from_slice(payload);
            let mut to_client = Vec::new();
            let server =
                server_handshake(&mut to_server.as_slice(), &mut to_client, "dev-1", |_| {
                    unreachable!("never welcomed")
                });
            assert!(server.is_err());
            let mut ignored = Vec::new();
            client_handshake(
                &mut to_client.as_slice(),
                &mut ignored,
                hello("dev-x"),
                |_| {},
            )
            .unwrap_err()
            .to_string()
        };
        let mut other = vec![0, 5];
        other.extend_from_slice(b"dev-x");
        other.extend_from_slice(&[0xff, 0xff, 0xff, 7, 1]);
        let message = run(&other);
        assert!(
            message.contains("dev-1") && message.contains("dev-x"),
            "{message}"
        );
        let mut same = vec![0, 5];
        same.extend_from_slice(b"dev-1");
        same.extend_from_slice(&[0xff, 0xff, 0xff]);
        let message = run(&same);
        assert!(message.contains("hello could not be read"), "{message}");
    }

    /// The layout every build keeps: a hello is variant 0, its build first;
    /// a refusal is variant 2, its reason alone.
    #[test]
    fn the_hello_and_the_refusal_keep_their_layout() {
        let hello = postcard::to_stdvec(&Control::Hello(hello("dev-1"))).unwrap();
        assert_eq!(&hello[..7], b"\x00\x05dev-1");
        let refused = postcard::to_stdvec(&Control::Refused {
            reason: "no".to_owned(),
        })
        .unwrap();
        assert_eq!(refused, b"\x02\x02no");
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
    fn what_a_shell_prints_around_the_answer_is_skipped() {
        let (_, mut to_client) = {
            let mut to_server = Vec::new();
            protocol::send(&mut to_server, &Control::Hello(hello("dev-1"))).unwrap();
            let mut answer = Vec::new();
            let server = server_handshake(&mut to_server.as_slice(), &mut answer, "dev-1", |_| {
                Ok(Welcome {
                    socket: Vec::new(),
                    root: crate::protocol::RootIdentity::default(),
                })
            });
            (server, answer)
        };
        let mut stream = b"\x1b[33m WARN\x1b[0m busy\n".to_vec();
        stream.append(&mut to_client.clone());
        stream.extend_from_slice(b" WARN later\n");
        let mut said = Vec::new();
        let welcome = client_handshake(
            &mut stream.as_slice(),
            &mut Vec::new(),
            hello("dev-1"),
            |before| said = before.to_vec(),
        )
        .unwrap();
        assert!(welcome.socket.is_empty());
        assert_eq!(said, b"\x1b[33m WARN\x1b[0m busy\n");
        // Something written into the answer is found out, not read as it.
        to_client.insert(start::MARK.len() + 3, b'!');
        let error = client_handshake(
            &mut to_client.as_slice(),
            &mut Vec::new(),
            hello("dev-1"),
            |_| {},
        )
        .unwrap_err();
        assert!(!error.is::<Closed>(), "{error:#}");
    }

    #[test]
    fn a_server_gone_before_answering_is_told_apart() {
        let mut ignored = Vec::new();
        let error =
            client_handshake(&mut [].as_slice(), &mut ignored, hello("dev-1"), |_| {}).unwrap_err();
        assert!(error.is::<Closed>(), "{error:#}");
    }
}
