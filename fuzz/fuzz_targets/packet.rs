//! Packets as a peer may send them: each read, and decoded as its kind
//! says, never panics; nor does what it sends read as records, the
//! manifest's, as the Destination reads them, or the answers, as the
//! Source does.
#![no_main]

use ferry::protocol::{self, Answer, Control, FileHash, Monitor, PacketType};
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
            PacketType::Answer => drop(packet.decode::<Answer>()),
            PacketType::Report => drop(packet.decode::<Report>()),
            PacketType::Monitor => drop(packet.decode::<Monitor>()),
            _ => {}
        }
    }
    let mut records = std::io::BufReader::new(data);
    while let Ok(Some(_)) = protocol::read_record::<Manifest>(&mut records) {}
    let mut records = std::io::BufReader::new(data);
    while let Ok(Some(_)) = protocol::read_record::<Answer>(&mut records) {}
});
