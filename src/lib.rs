//! Pure-Rust reader for the Electronic Arts / Commodore IFF 85 container
//! family ("FORM / LIST / CAT" chunked format).
//!
//! IFF files are big-endian chunk trees. The top-level chunk is always a
//! group chunk — `FORM`, `LIST`, or `CAT ` — whose first 4 bytes of payload
//! are a 4-character "form type" such as `8SVX` (Amiga 8-bit sampled voice),
//! `ILBM` (Amiga picture), `AIFF` (Apple audio), `SMUS` (music score),
//! and so on.
//!
//! Today this crate handles **8SVX audio** end-to-end (identifies the
//! stream, exposes its PCM-S8 samples as packets) and **ILBM**
//! (InterLeaved BitMap, the Amiga IFF picture form) for indexed,
//! EHB and HAM6/HAM8 images including ByteRun1 (PackBits)
//! decompression. The same chunk reader and `Form` walker are
//! reusable for future AIFF / SMUS support without restructuring.

pub mod chunk;
pub mod ilbm;
pub mod svx;

use oxideav_core::ContainerRegistry;

/// Register all IFF-family demuxers with the container registry.
pub fn register(reg: &mut ContainerRegistry) {
    svx::register(reg);
    ilbm::register(reg);
}
