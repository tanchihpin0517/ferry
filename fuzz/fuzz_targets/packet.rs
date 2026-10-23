//! Packets as a peer may send them: each read, and decoded as its kind
//! says, the manifest's stream in turn as the Destination does, never
//! panics.
#![no_main]

use ferry::protocol::{self, Control, ManifestDecoder, Needs, PacketType, Results};
use ferry::{manifest::Manifest, report::Report};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = data;
    let mut manifests = ManifestDecoder::new().unwrap();
    while let Ok(Some(packet)) = protocol::receive(&mut reader) {
        let _ = protocol::hello_build(&packet);
        match packet.kind {
            PacketType::Control => drop(packet.decode::<Control>()),
            PacketType::Manifest => drop(packet.decode::<Manifest>()),
            PacketType::ManifestStream => drop(manifests.decode(&packet)),
            PacketType::Needs => drop(packet.decode::<Needs>()),
            PacketType::Results => drop(packet.decode::<Results>()),
            PacketType::Report => drop(packet.decode::<Report>()),
            _ => {}
        }
    }
});
