//! The two peers of a transfer.
//!
//! For now a connection is only opened and closed: the server checks the
//! client's hello, and waits for the client to close the connection.

use std::io::{Read, Write};

use anyhow::Result;

use crate::protocol;
use crate::session;

/// The server's side: the handshake, then nothing until the client closes the
/// connection.
pub fn serve(reader: &mut impl Read, writer: &mut impl Write, build: &str) -> Result<()> {
    session::server_handshake(reader, writer, build)?;
    while protocol::receive(reader)?.is_some() {}
    Ok(())
}
