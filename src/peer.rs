//! A transfer's two peers, as the client and the server run them. The client
//! is the Source in a push and the Destination in a pull; the server is the
//! other. Whichever the client is reports what goes wrong.

use std::ffi::OsStr;
use std::io::{Read, Write};

use anyhow::Result;

use crate::destination;
use crate::protocol::{self, Control, Hello, Role};
use crate::session::{self, Connection};
use crate::source;

/// What went wrong in a transfer, for the client's exit status.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Paths the Source could not read.
    pub problems: u64,
    /// Entries that could not be made to match, or deleted.
    pub failed: u64,
}

/// What a dry run prints: each path on a line of its own.
fn printer(dry_run: bool, out: &mut impl Write) -> impl FnMut(&[u8]) -> Result<()> + '_ {
    move |path: &[u8]| -> Result<()> {
        if dry_run {
            out.write_all(path)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    }
}

/// The client's side of a push: the Source, at `root`.
pub fn push(
    connection: Connection,
    root: &OsStr,
    hello: &Hello,
    out: &mut impl Write,
) -> Result<Outcome> {
    let (mut reader, mut writer, server) = connection.split();
    let summary = source::run(
        &mut reader,
        &mut writer,
        root,
        hello.dry_run,
        hello.numeric_ids,
        true,
        &mut printer(hello.dry_run, out),
    )?;
    // Closing the connection ends the server.
    drop(writer);
    server.wait()?;
    Ok(Outcome {
        problems: summary.problems,
        failed: summary.failed,
    })
}

/// The client's side of a pull: the Destination, at `root`.
pub fn pull(
    connection: Connection,
    root: &OsStr,
    hello: &Hello,
    out: &mut impl Write,
) -> Result<Outcome> {
    let (mut reader, mut writer, server) = connection.split();
    let options = destination_options(root.as_encoded_bytes().to_vec(), hello, true);
    let summary = destination::run(
        &mut reader,
        &mut writer,
        &options,
        &mut printer(hello.dry_run, out),
    )?;
    drop(writer);
    server.wait()?;
    Ok(Outcome {
        problems: summary.problems,
        failed: summary.failed,
    })
}

fn destination_options(root: Vec<u8>, hello: &Hello, report: bool) -> destination::Options {
    destination::Options {
        root,
        dry_run: hello.dry_run,
        modify_window: hello.modify_window,
        numeric_ids: hello.numeric_ids,
        fsync: hello.fsync,
        delete: hello.delete,
        report,
    }
}

/// The server's side: the handshake, then the peer the client asked for. The
/// server prints nothing; a failure is sent to the client before it exits.
pub fn serve(reader: &mut impl Read, writer: &mut (impl Write + Send), build: &str) -> Result<()> {
    let hello = session::server_handshake(reader, writer, build)?;
    let result = match hello.role {
        Role::Destination => {
            let options = destination_options(hello.root.clone(), &hello, false);
            destination::run(reader, writer, &options, &mut |_| Ok(())).map(|_| ())
        }
        Role::Source => {
            let root: &OsStr = std::os::unix::ffi::OsStrExt::from_bytes(&hello.root[..]);
            source::run(
                reader,
                writer,
                root,
                hello.dry_run,
                hello.numeric_ids,
                false,
                &mut |_| Ok(()),
            )
            .map(|_| ())
        }
    };
    if let Err(error) = &result {
        // The client may already be gone; the error is returned either way.
        let _ = protocol::send(
            writer,
            &Control::Error {
                message: format!("{error:#}"),
            },
        );
        let _ = writer.flush();
    }
    result
}
