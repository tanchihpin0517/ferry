//! A transfer's two peers, as the client and the server run them. The client
//! is the Source in a push and the Destination in a pull; the server is the
//! other. Whichever the client is reports, to the client's display; the other
//! reports nowhere. A local copy runs both in the client, joined by pipes,
//! over the same protocol, and the Source reports.

use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Read, Write};

use anyhow::Result;

use crate::destination;
use crate::protocol::{self, Control, Hello, Role};
use crate::report::Reporter;
use crate::session::{self, Connection};
use crate::source;

/// The client's side of a push: the Source, at `root`.
pub fn push(
    connection: Connection,
    root: &OsStr,
    hello: &Hello,
    reporter: &Reporter,
) -> Result<()> {
    let (mut reader, mut writer, server) = connection.split();
    source::run(
        &mut reader,
        &mut writer,
        &source_options(root, hello, reporter.clone()),
    )?;
    // Closing the connection ends the server.
    drop(writer);
    server.wait()
}

/// The client's side of a pull: the Destination, at `root`.
pub fn pull(
    connection: Connection,
    root: &OsStr,
    hello: &Hello,
    reporter: &Reporter,
) -> Result<()> {
    let (mut reader, mut writer, server) = connection.split();
    let options = destination_options(root.as_encoded_bytes().to_vec(), hello, reporter.clone());
    destination::run(&mut reader, &mut writer, &options)?;
    drop(writer);
    server.wait()
}

/// A local copy: the Source here, and the Destination in a thread, joined by
/// a pipe each way. The Source reports.
pub fn local(
    source_root: &OsStr,
    destination_root: &OsStr,
    hello: &Hello,
    reporter: &Reporter,
) -> Result<()> {
    let (from_source, to_destination) = std::io::pipe()?;
    let (from_destination, to_source) = std::io::pipe()?;
    let options = destination_options(
        destination_root.as_encoded_bytes().to_vec(),
        hello,
        Reporter::none(),
    );
    std::thread::scope(|scope| {
        let destination = scope.spawn(move || -> Result<()> {
            let mut reader = BufReader::new(from_source);
            let mut writer = BufWriter::new(to_source);
            let result = destination::run(&mut reader, &mut writer, &options);
            if let Err(error) = &result {
                let _ = protocol::send(
                    &mut writer,
                    &Control::Error {
                        message: format!("{error:#}"),
                    },
                );
                let _ = writer.flush();
            }
            result.map(|_| ())
        });
        let mut reader = BufReader::new(from_destination);
        let mut writer = BufWriter::new(to_destination);
        let sent = source::run(
            &mut reader,
            &mut writer,
            &source_options(source_root, hello, reporter.clone()),
        );
        // Closing both pipes ends the Destination, even one stuck writing.
        drop(writer);
        drop(reader);
        let finished = destination.join().expect("the destination panicked");
        sent?;
        finished
    })
}

fn source_options<'a>(root: &'a OsStr, hello: &Hello, reporter: Reporter) -> source::Options<'a> {
    source::Options {
        root,
        dry_run: hello.dry_run,
        numeric_ids: hello.numeric_ids,
        compress: hello.compress,
        compress_level: hello.compress_level,
        reporter,
    }
}

fn destination_options(root: Vec<u8>, hello: &Hello, reporter: Reporter) -> destination::Options {
    destination::Options {
        root,
        dry_run: hello.dry_run,
        modify_window: hello.modify_window,
        numeric_ids: hello.numeric_ids,
        fsync: hello.fsync,
        delete: hello.delete,
        reporter,
    }
}

/// The server's side: the handshake, then the peer the client asked for. The
/// server prints nothing; a failure is sent to the client before it exits.
pub fn serve(reader: &mut impl Read, writer: &mut (impl Write + Send), build: &str) -> Result<()> {
    let hello = session::server_handshake(reader, writer, build)?;
    let result = match hello.role {
        Role::Destination => {
            let options = destination_options(hello.root.clone(), &hello, Reporter::none());
            destination::run(reader, writer, &options).map(|_| ())
        }
        Role::Source => {
            let root: &OsStr = std::os::unix::ffi::OsStrExt::from_bytes(&hello.root[..]);
            source::run(
                reader,
                writer,
                &source_options(root, &hello, Reporter::none()),
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
