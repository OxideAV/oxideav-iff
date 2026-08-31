#![no_main]

//! Feed arbitrary fuzz-supplied bytes through every remaining
//! whole-FORM raster walker the crate exposes (the DEEP family has its
//! own `deep_decode` target):
//!
//!  * `ilbm::parse_ilbm` — the FORM ILBM / PBM walker: BMHD / CMAP /
//!    CAMG / GRAB / DEST / SPRT / SHAM / PCHG / CRNG / CCRT / DRNG
//!    chunk parsers, ByteRun1 row expansion, the planar and chunky
//!    render passes (EHB, HAM6/HAM8 — including the assumed-HAM6
//!    inference for a missing/broken CAMG — HasMask, colour keying,
//!    the mskLasso seed fill) and the PCHG flag-less strict-validation
//!    disambiguator.
//!  * `ilbm::parse_acbm` — the FORM ACBM walker + ABIT plane
//!    de-contiguation feeding the same shared render.
//!  * `ilbm::parse_rgb8` / `ilbm::parse_rgbn` — the Turbo Silver §3
//!    genlock-RLE true-colour walkers (WORD/LONG run units, count
//!    cascade, run-spill across scanlines, pixel-budget bounds).
//!  * `ilbm::parse_tvpp` — the best-effort TVPaint project walk over
//!    the DEEP raster vocabulary.
//!  * `chunk::probe_top_level_group` + `chunk::parse_group_children` —
//!    the EA IFF 85 §5 LIST/CAT/PROP grammar walker, chained off the
//!    probed top-level envelope.
//!
//! The contract under test is purely that each call *returns*: a
//! malformed input yields `Err(oxideav_core::Error::…)`, a well-formed
//! one yields `Ok(_)`, and neither path may panic, integer-overflow
//! (in a debug build), index out of bounds, or allocate an
//! attacker-controlled buffer larger than the input actually supports.

use libfuzzer_sys::fuzz_target;
use oxideav_iff::chunk::{parse_group_children, probe_top_level_group, GroupKind};
use oxideav_iff::ilbm::GenlockPolicy;

fuzz_target!(|data: &[u8]| {
    let _ = oxideav_iff::ilbm::parse_ilbm(data);
    let _ = oxideav_iff::ilbm::parse_acbm(data);
    let _ = oxideav_iff::ilbm::parse_rgb8(data, GenlockPolicy::default());
    let _ = oxideav_iff::ilbm::parse_rgbn(data, GenlockPolicy::BrushTransparency);
    let _ = oxideav_iff::ilbm::parse_tvpp(data);
    if let Ok(Some(group)) = probe_top_level_group(data) {
        if matches!(group.kind, GroupKind::List | GroupKind::Cat) && data.len() >= 12 {
            let end = (8usize + group.size as usize).min(data.len());
            if end > 12 {
                let _ = parse_group_children(group.kind, &data[12..end]);
            }
        }
    }
});
