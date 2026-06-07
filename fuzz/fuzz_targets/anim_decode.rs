#![no_main]

//! Feed arbitrary fuzz-supplied bytes through `anim::parse_anim`.
//!
//! `parse_anim` is the FORM ANIM walker: it loads a first FORM ILBM
//! frame (BMHD + CMAP + CAMG + BODY) and then applies subsequent
//! ANHD + DLTA delta frames using one of three vertical-delta
//! operations (operation 0 literal, operation 5 byte-vertical-RLC,
//! operation 7 short / long vertical delta).
//!
//! The contract under test is purely that the call *returns*: a
//! malformed stream yields `Err(oxideav_core::Error::…)`, a well-formed
//! one yields `Ok(AnimImage)`, and neither path may panic,
//! integer-overflow (in a debug build), index out of bounds, or try to
//! allocate an attacker-controlled buffer larger than the input
//! actually supports. The chunk walker, the per-frame BODY/DLTA size
//! arithmetic, and the three delta decoders each have their own
//! failure-mode surface; this target exercises all of them.
//!
//! The return value is intentionally discarded.

use libfuzzer_sys::fuzz_target;
use oxideav_iff::anim::parse_anim;

fuzz_target!(|data: &[u8]| {
    let _ = parse_anim(data);
});
