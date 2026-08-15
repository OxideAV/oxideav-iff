#![no_main]

//! Feed arbitrary fuzz-supplied bytes through the three `FORM DEEP`
//! whole-FORM walkers:
//!
//!  * `ilbm::parse_deep_frames` — DGBL / DPEL / DLOC / DBOD / DCHG chunk
//!    walk plus the NOCOMPRESSION chunky assembly and the §1.5b RUNLENGTH
//!    (ByteRun1) expansion, per DBOD frame.
//!  * `ilbm::parse_deep_frames_with_tvdc_table` — the same walk with the
//!    §1.5 TVDC per-component-line nibble decoder enabled via a fixed
//!    caller-supplied 16-word delta table (the table lives outside the
//!    FORM, so the fuzzer pins one; the wire decoder under test is
//!    identical for any table).
//!  * `ilbm::extract_deep_jpeg_frames` — the §1.5b JPEG surfacing walk
//!    (DGBL Compression == 4 gate + SOI/EOI framing validation per DBOD).
//!
//! The failure-mode surface is the chunk-size/pad arithmetic of the walk,
//! the DGBL/DPEL/DLOC struct parsers, the per-coding expansion bounds
//! (ByteRun1 64x / TVDC 15x allocation guards), and the per-pixel bit
//! cursor of the chunky assembly.
//!
//! The contract under test is purely that each call *returns*: a malformed
//! input yields `Err(oxideav_core::Error::…)`, a well-formed one yields
//! `Ok(_)`, and neither path may panic, integer-overflow (in a debug
//! build), index out of bounds, or allocate an attacker-controlled buffer
//! larger than the input actually supports.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Deltas covering both signs plus wide jumps so fuzz-built TVDC bodies
    // can reach any byte value; nibble 0 stays the §1.5 run escape.
    const TABLE: [i16; 16] = [
        0, 1, -1, 2, -2, 4, -4, 8, -8, 16, -16, 32, -32, 64, -64, 128,
    ];
    let _ = oxideav_iff::ilbm::parse_deep_frames(data);
    let _ = oxideav_iff::ilbm::parse_deep_frames_with_tvdc_table(data, &TABLE);
    let _ = oxideav_iff::ilbm::extract_deep_jpeg_frames(data);
});
