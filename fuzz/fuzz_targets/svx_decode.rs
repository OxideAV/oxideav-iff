#![no_main]

//! Feed arbitrary fuzz-supplied bytes through the two `FORM 8SVX`
//! read surfaces:
//!
//!  * `svx::parse_voice` — the structural walker: FORM/8SVX envelope,
//!    VHDR Voice8Header, CHAN / PAN / ATAK / RLSE / SEQN / FADE typed
//!    chunk parsers, text chunks, Fibonacci-delta BODY expansion, the
//!    stereo concatenated-halves split, and the per-octave doubling
//!    series split (with its overflow-checked `hi * (2^ct - 1)` total).
//!  * the registered `iff_8svx` demuxer — the streaming path with its
//!    body-capacity clamp, loop/octave frame accounting, metadata
//!    string decode, and interleaving of stereo halves.
//!
//! The failure-mode surface is the chunk-size/pad arithmetic, the
//! 20-byte VHDR field decode, the 6-byte EGPoint / 8-byte SEQN segment
//! array strides, the Fibonacci nibble expansion (2 samples per byte),
//! and the octave doubling series a forged `ctOctave` can inflate.
//!
//! The contract under test is purely that each call *returns*: a
//! malformed input yields `Err(oxideav_core::Error::…)`, a well-formed
//! one yields `Ok(_)`, and neither path may panic, integer-overflow
//! (in a debug build), index out of bounds, or allocate an
//! attacker-controlled buffer larger than the input actually supports.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{ContainerRegistry, ReadSeek};
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let _ = oxideav_iff::svx::parse_voice(data);

    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    if let Ok(mut dmx) = reg.open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver) {
        // Drain a bounded number of packets so a decodable input also
        // exercises the packetiser without letting the fuzzer stall.
        for _ in 0..64 {
            if dmx.next_packet().is_err() {
                break;
            }
        }
    }
});
