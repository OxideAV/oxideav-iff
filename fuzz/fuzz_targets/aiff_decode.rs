#![no_main]

//! Feed arbitrary fuzz-supplied bytes through `AiffDemuxer::from_bytes`.
//!
//! `AiffDemuxer::from_bytes` is the top-of-stack entry point for an
//! Apple FORM AIFF / FORM AIFC container: it walks the entire chunk
//! tree (FORM header + COMM common + SSND sound data + optional
//! MARK / INST / COMT / AESD / APPL / MIDI / SAXL / NAME / AUTH /
//! (c)  / ANNO metadata chunks) before returning a demuxer the caller
//! can pull a packet from.
//!
//! The contract under test is purely that the call *returns*: a
//! malformed stream yields `Err(oxideav_core::Error::…)`, a well-formed
//! one yields `Ok(AiffDemuxer)`, and neither path may panic,
//! integer-overflow (in a debug build), index out of bounds, or try to
//! allocate an attacker-controlled buffer larger than the input
//! actually supports. The 32-bit chunk-size field, the per-chunk
//! pad-byte arithmetic, and the 80-bit IEEE-extended sample-rate decode
//! are the classic failure-mode spots; this target keeps them honest.
//!
//! The return value is intentionally discarded.

use libfuzzer_sys::fuzz_target;
use oxideav_iff::aiff::demuxer::AiffDemuxer;

fuzz_target!(|data: &[u8]| {
    let _ = AiffDemuxer::from_bytes(data.to_vec());
});
