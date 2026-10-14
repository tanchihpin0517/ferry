//! A connection between the client and a server, and the handshake that opens
//! it.
//!
//! The server is a child process speaking packets on its standard input and
//! output: `ssh host … ferry server` for a remote host, or `ferry server` itself
//! for a local destination and for tests. Either way the client sees the same
//! two pipes.

use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::interrupt;
use crate::protocol::{self, Control, Hello};

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

/// The client's side: says hello and waits to be welcomed.
pub fn client_handshake(
    reader: &mut impl Read,
    writer: &mut impl Write,
    hello: Hello,
) -> Result<()> {
    protocol::send(writer, &Control::Hello(hello))?;
    writer.flush().context("send the hello")?;
    match protocol::expect::<Control>(reader).context("wait for the server's answer")? {
        Control::Welcome => Ok(()),
        Control::Refused { reason } => bail!("the server refused the connection: {reason}"),
        Control::Error { message } => bail!("the server failed: {message}"),
        other => bail!("expected the server's answer, got {other:?}"),
    }
}

/// The server's side: accepts a hello from the same build, and refuses any
/// other, telling the client why.
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
            "the client is build {}, but this server is build {build}",
            hello.build
        );
        protocol::send(
            writer,
            &Control::Refused {
                reason: reason.clone(),
            },
        )?;
        writer.flush()?;
        bail!(reason);
    }
    protocol::send(writer, &Control::Welcome)?;
    writer.flush()?;
    Ok(hello)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(build: &str) -> Hello {
        Hello {
            build: build.to_owned(),
            destination: b"dest".to_vec(),
            dry_run: true,
            modify_window: 0,
            numeric_ids: false,
            fsync: true,
            delete: false,
        }
    }

    /// Runs the server's handshake on the client's packets, then the client's
    /// on the server's answer.
    fn handshake(client_build: &str, server_build: &str) -> (Result<Hello>, Result<()>) {
        let mut to_server = Vec::new();
        protocol::send(&mut to_server, &Control::Hello(hello(client_build))).unwrap();
        let mut to_client = Vec::new();
        let server = server_handshake(&mut to_server.as_slice(), &mut to_client, server_build);

        let mut ignored = Vec::new();
        let client = client_handshake(&mut to_client.as_slice(), &mut ignored, hello(client_build));
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
}
