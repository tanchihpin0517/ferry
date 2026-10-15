//! A connection between the client and a server, and the handshake that opens
//! it.
//!
//! The server is a child process speaking packets on its standard input and
//! output: `ssh host … ferry server`. A local copy needs no server: both
//! peers run in the client.

use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::interrupt;
use crate::protocol::{self, Control, Hello, Role};
use crate::source;

pub struct Connection {
    child: Child,
    pub reader: BufReader<ChildStdout>,
    pub writer: BufWriter<ChildStdin>,
}

impl Connection {
    /// Starts the server. Its standard error stays the client's, so `ssh`
    /// prompts and errors reach the terminal.
    pub fn spawn(mut command: Command) -> Result<Self> {
        let mut child = interrupt::unblock_in_child(&mut command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("start {:?}", command.get_program()))?;
        let reader = BufReader::new(child.stdout.take().expect("piped"));
        let writer = BufWriter::new(child.stdin.take().expect("piped"));
        Ok(Self {
            child,
            reader,
            writer,
        })
    }

    /// Splits the connection so that one thread can write while another reads.
    pub fn split(self) -> (BufReader<ChildStdout>, BufWriter<ChildStdin>, Server) {
        (self.reader, self.writer, Server(self.child))
    }
}

/// The server's process, once its pipes are split off.
pub struct Server(Child);

impl Server {
    pub fn wait(mut self) -> Result<()> {
        let status = self.0.wait().context("wait for the server")?;
        if !status.success() {
            bail!("the server exited with {status}");
        }
        Ok(())
    }
}

/// The connection closed before the server answered the hello: the server
/// did not start, or exited at once.
#[derive(Debug)]
struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("the connection closed before the server answered")
    }
}

impl std::error::Error for Closed {}

impl Connection {
    /// Says hello to the server and waits to be welcomed. A server that exits
    /// before it answers, such as one that could not be started, is reported
    /// with how it exited; what it printed is already on standard error.
    pub fn handshake(&mut self, hello: Hello) -> Result<()> {
        match client_handshake(&mut self.reader, &mut self.writer, hello) {
            Err(error) if error.is::<Closed>() => match self.child.wait() {
                Ok(status) => bail!("the server did not start ({status})"),
                Err(_) => bail!("the server did not start"),
            },
            result => result,
        }
    }
}

/// The client's side: says hello and waits to be welcomed.
pub fn client_handshake(
    reader: &mut impl Read,
    writer: &mut impl Write,
    hello: Hello,
) -> Result<()> {
    protocol::send(writer, &Control::Hello(hello))?;
    // A server that is gone shows as a closed connection when its answer is
    // read, rather than as a failed write here.
    let _ = writer.flush();
    let Some(answer) = protocol::receive(reader).context("wait for the server's answer")? else {
        return Err(Closed.into());
    };
    match answer.decode::<Control>()? {
        Control::Welcome => Ok(()),
        Control::Refused { reason } => bail!(reason),
        Control::Error { message } => bail!("the server failed: {message}"),
        other => bail!("expected the server's answer, got {other:?}"),
    }
}

/// The server's side: accepts a hello from the same build, and refuses any
/// other, or one whose Source cannot start, telling the client why.
pub fn server_handshake(
    reader: &mut impl Read,
    writer: &mut impl Write,
    build: &str,
) -> Result<Hello> {
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
    protocol::send(writer, &Control::Welcome)?;
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
            compress_level: None,
        }
    }

    /// Runs the server's handshake on the client's packets, then the client's
    /// on the server's answer.
    fn handshake(client_build: &str, server_build: &str) -> (Result<Hello>, Result<()>) {
        exchange(hello(client_build), server_build)
    }

    fn exchange(sent: Hello, server_build: &str) -> (Result<Hello>, Result<()>) {
        let mut to_server = Vec::new();
        protocol::send(&mut to_server, &Control::Hello(sent.clone())).unwrap();
        let mut to_client = Vec::new();
        let server = server_handshake(&mut to_server.as_slice(), &mut to_client, server_build);

        let mut ignored = Vec::new();
        let client = client_handshake(&mut to_client.as_slice(), &mut ignored, sent);
        (server, client)
    }

    #[test]
    fn the_same_build_is_welcomed() {
        let (server, client) = handshake("dev-1", "dev-1");
        assert_eq!(server.unwrap(), hello("dev-1"));
        client.unwrap();
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
        let error = client_handshake(&mut [].as_slice(), &mut ignored, hello("dev-1")).unwrap_err();
        assert!(error.is::<Closed>(), "{error:#}");
    }
}
