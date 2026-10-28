//! What comes on a connection's wires once it is ready, as one end's
//! transport takes it: frames of any pipe and kind, chunks numbered anyhow,
//! bytes cut however, on any wire, the program reading now and then and the
//! pumps sending. Whatever comes, the transport takes it or says why not,
//! never panicking; and never holds more of a pipe than its window, nor of
//! control more than its guard, once it took what came.
#![no_main]

use arbitrary::{Result, Unstructured};
use ferry::transport::pipes::{
    CHUNK_HEADER, CONTROL_GUARD, Carrying, Core, DATA, HEADER, Inbound, Kind, MOST, Sending,
    Windows, stream,
};
use libfuzzer_sys::fuzz_target;

/// Small windows, so that an input can reach them.
const WINDOWS: Windows = Windows {
    manifest: 4096,
    data: 8192,
    answers: 2048,
};

/// A frame, of a pipe and kind there may or may not be, its length as said
/// or not.
fn frame(u: &mut Unstructured, numbered: &mut u64) -> Result<Vec<u8>> {
    let pipe = if u.ratio(1, 16)? {
        u.arbitrary()?
    } else {
        u.int_in_range(0..=4)?
    };
    let kind = if u.ratio(1, 16)? {
        u.arbitrary()?
    } else {
        u.int_in_range(0..=4)?
    };
    let content: Vec<u8> = if pipe == DATA && kind == Kind::Bytes as u8 && u.ratio(3, 4)? {
        // A chunk, or part of one, numbered near where they are.
        let number = match u.int_in_range(0..=5)? {
            0 => u.arbitrary()?,
            1 => u64::MAX,
            _ => {
                *numbered += 1;
                *numbered - 1 + u64::from(u.int_in_range(0..=2u8)?)
            }
        };
        let length: u32 = u.int_in_range(0..=600)?;
        let mut bytes = number.to_le_bytes().to_vec();
        bytes.push(if u.ratio(1, 16)? { 1 } else { 0 });
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.resize(CHUNK_HEADER + length as usize, 7);
        let cut = u.int_in_range(0..=bytes.len())?;
        if u.ratio(1, 4)? {
            bytes[..cut].to_vec()
        } else {
            bytes
        }
    } else {
        let length = match u.int_in_range(0..=4)? {
            0 => 0,
            1 => 4,
            2 => 8,
            3 => 12,
            _ => u.int_in_range(0..=2000)?,
        };
        let mut content = vec![0; length];
        u.fill_buffer(&mut content)?;
        content
    };
    let mut length = (content.len() as u32).to_le_bytes();
    if u.ratio(1, 32)? {
        length = u.arbitrary()?;
    }
    let mut bytes = vec![pipe, kind];
    bytes.extend_from_slice(&length);
    bytes.extend_from_slice(&content);
    Ok(bytes)
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(source) = u.arbitrary::<bool>() else {
        return;
    };
    let wires = u.int_in_range(1..=3usize).unwrap_or(1);
    let pipes = stream(source, WINDOWS);
    let mut core = Core::new(&pipes, wires);
    let mut inbounds: Vec<Inbound> = (0..wires).map(Inbound::new).collect();
    let mut sendings: Vec<Sending> = (0..wires).map(|_| Sending::default()).collect();
    let mut numbered = 0;
    while !u.is_empty() {
        let Ok(action) = u.int_in_range(0..=9u8) else {
            break;
        };
        match action {
            0..=5 => {
                let Ok(wire) = u.int_in_range(0..=wires - 1) else {
                    break;
                };
                let Ok(bytes) = frame(&mut u, &mut numbered) else {
                    break;
                };
                // Come however cut.
                let Ok(cut) = u.int_in_range(0..=bytes.len()) else {
                    break;
                };
                let mut taken = Ok(());
                for piece in [&bytes[..cut], &bytes[cut..]] {
                    taken = taken.and(inbounds[wire].take(piece, &mut core).map(|_| ()));
                }
                if taken.is_err() {
                    return;
                }
            }
            6 => {
                let Ok(pipe) = u.int_in_range(0..=3u8) else {
                    break;
                };
                if pipes[usize::from(pipe)].reads {
                    core.read_all(pipe);
                }
            }
            7 => {
                let Ok(wire) = u.int_in_range(0..=wires - 1) else {
                    break;
                };
                if let Some(frame) = core.next(wire, &mut sendings[wire]) {
                    let bytes = frame.bytes();
                    assert!(bytes.len() <= HEADER + MOST);
                    core.written(Carrying::of(&frame, &sendings[wire]));
                }
            }
            _ => {
                let Ok(wire) = u.int_in_range(0..=wires - 1) else {
                    break;
                };
                if core.wire_ended(wire, &inbounds[wire]).is_err() {
                    return;
                }
            }
        }
        // Once taken, what is held is within bounds: a window, or for
        // control, which has none, its guard.
        for (number, pipe) in pipes.iter().enumerate() {
            let (outstanding, window) = core.outstanding(number as u8);
            match window {
                Some(window) if pipe.reads => assert!(outstanding <= u64::from(window)),
                None => assert!(outstanding <= CONTROL_GUARD as u64),
                Some(_) => {}
            }
        }
    }
});
