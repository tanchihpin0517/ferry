//! A transfer's two peers, as the client and the server run them.
//!
//! For now the client is always the Source and the server the Destination
//! (a push).

use std::ffi::OsStr;
use std::io::{Read, Write};

use anyhow::Result;

use crate::destination::{self, Options};
use crate::protocol::{self, Control};
use crate::session::{self, Connection};
use crate::source::{self, Summary};

/// The client's side of a push: runs the Source, writing each needed path to
/// `out` in a dry run.
pub fn push(
    connection: Connection,
    root: &OsStr,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<Summary> {
    let (mut reader, mut writer, server) = connection.split();
    let mut on_need = |path: &[u8]| -> Result<()> {
        if dry_run {
            out.write_all(path)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    };
    let summary = source::run(&mut reader, &mut writer, root, dry_run, &mut on_need)?;
    // Closing the connection ends the server.
    drop(writer);
    server.wait()?;
    Ok(summary)
}

/// The server's side: the handshake, then the Destination. A failure is sent
/// to the client before the server exits.
pub fn serve(reader: &mut impl Read, writer: &mut (impl Write + Send), build: &str) -> Result<()> {
    let hello = session::server_handshake(reader, writer, build)?;
    let options = Options {
        root: hello.destination,
        dry_run: hello.dry_run,
        modify_window: hello.modify_window,
    };
    let result = destination::run(reader, writer, &options, &mut |_| Ok(()));
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
