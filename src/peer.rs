//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and relays between them. Both peers report to the
//! client, in report packets.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;

use crate::destination;
use crate::protocol::{self, Control, Role};
use crate::session;
use crate::source;

/// The server's side: the handshake, then the peer the client asked for. The
/// server prints nothing; a failure is sent to the client before it exits.
pub fn serve(reader: &mut impl Read, writer: &mut (impl Write + Send), build: &str) -> Result<()> {
    let hello = session::server_handshake(reader, writer, build)?;
    let result = match hello.role {
        Role::Destination => {
            let options = destination::Options {
                root: hello.root.clone(),
                dry_run: hello.dry_run,
                modify_window: hello.modify_window,
                numeric_ids: hello.numeric_ids,
                fsync: hello.fsync,
                delete: hello.delete,
                report: true,
            };
            destination::run(reader, writer, &options).map(|_| ())
        }
        Role::Source => {
            let options = source::Options {
                root: OsStr::from_bytes(&hello.root),
                dry_run: hello.dry_run,
                numeric_ids: hello.numeric_ids,
                compress: hello.compress,
                compress_level: hello.compress_level,
                report: true,
            };
            source::run(reader, writer, &options).map(|_| ())
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
