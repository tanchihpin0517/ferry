//! Packets as a peer may send them: each read, and decoded as its kind
//! says, never panics; nor does what it sends read as the manifest's
//! records, as the Destination reads them.
#![no_main]

use ferry::protocol::{self, Control, FileHash, Monitor, Needs, PacketType, Results};
use ferry::{manifest::Manifest, report::Report};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = data;
    while let Ok(Some(packet)) = protocol::receive(&mut reader) {
        let _ = protocol::hello_build(&packet);
        match packet.kind {
            PacketType::Control => drop(packet.decode::<Control>()),
            PacketType::Manifest => drop(packet.decode::<Manifest>()),
            PacketType::FileHash => drop(packet.decode::<FileHash>()),
            PacketType::Needs => drop(packet.decode::<Needs>()),
            PacketType::Results => drop(packet.decode::<Results>()),
            PacketType::Report => drop(packet.decode::<Report>()),
            PacketType::Monitor => drop(packet.decode::<Monitor>()),
            _ => {}
        }
    }
    let mut records = std::io::BufReader::new(data);
    while let Ok(Some(_)) = protocol::read_record::<Manifest>(&mut records) {}
});
