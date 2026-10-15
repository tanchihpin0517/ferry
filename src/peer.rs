//! A peer as a server runs it: the Source or the Destination, as the
//! client's hello asks. The client never runs a peer itself; it starts a
//! server for each side and relays between them. Both peers report to the
//! client, in report packets.

use std::ffi::OsStr;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;

use crate::destination;
use crate::interrupt;
use crate::protocol::{self, Control, Hello, Role};
use crate::session;
use crate::source;
use crate::transport;

/// The server: over its control connection, its standard input and output,
/// it shakes hands and opens a socket for its path to join, then runs the
/// peer the client asked for over the path. It prints nothing; a failure goes
/// to the client. It lives as long as its control connection: should that
/// close first, the client is gone, and the server stops.
pub fn serve(build: &str) -> Result<()> {
    let mut joining = None;
    let hello = session::server_handshake(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        build,
        || {
            let listening = transport::listen()?;
            let token = listening.token().to_owned();
            joining = Some(listening);
            Ok(token)
        },
    )?;
    let joining = joining.expect("the handshake got ready for the path");
    std::thread::spawn(|| {
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
        interrupt::stop(1);
    });
    // The path's process stays until `_held` closes, as the server ends.
    let (input, output, _held) = joining.accept()?;
    drop(joining);
    run(
        hello,
        &mut BufReader::new(input),
        &mut BufWriter::new(output),
    )
}

/// Runs the peer `hello` asks for over a path.
fn run(hello: Hello, reader: &mut impl Read, writer: &mut (impl Write + Send)) -> Result<()> {
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
