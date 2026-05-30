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
//! stream, exposes its PCM-S8 samples as packets), **ILBM**
//! (InterLeaved BitMap, the Amiga IFF picture form) for indexed,
//! EHB and HAM6/HAM8 images including ByteRun1 (PackBits)
//! decompression, **ANIM** animations (op-5 and op-7 vertical-delta
//! decode), and Apple **AIFF / AIFF-C (AIFC)** audio (FORM/COMM/SSND
//! walker, 80-bit IEEE extended sample-rate decode, NONE/twos/sowt/
//! raw/fl32/fl64 PCM compression-type readers).

pub mod aiff;
pub mod anim;
pub mod chunk;
pub mod ilbm;
pub mod svx;

use oxideav_core::ContainerRegistry;

/// Register all IFF-family demuxers with the container registry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    svx::register(reg);
    ilbm::register(reg);
    anim::register(reg);
    // aiff's sibling-form helper is `register_containers` (the
    // public `aiff::register` takes a full `RuntimeContext` because
    // it was the standalone-crate entry point; here we want only the
    // container half).
    aiff::demuxer::register_containers(reg);
}

/// Install every IFF-family container into a
/// [`oxideav_core::RuntimeContext`].
///
/// Convenience wrapper around [`register_containers`] that matches the
/// uniform `register(&mut RuntimeContext)` entry point every sibling
/// crate exposes. The nested `svx::register` / `ilbm::register` helpers
/// remain `&mut ContainerRegistry`-shaped because they are internal
/// per-form installers and not part of the framework-facing surface.
///
/// Also wired into [`oxideav_meta::register_all`] via the
/// [`oxideav_core::register!`] macro below.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("iff", register);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_container() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        // 8SVX (Amiga audio) extension is registered by svx::register,
        // ILBM (Amiga picture) by ilbm::register, and AIFF / AIFC
        // (Apple audio) by aiff::register; all should be wired through
        // the unified entry point.
        assert_eq!(
            ctx.containers.container_for_extension("8svx"),
            Some("iff_8svx")
        );
        assert_eq!(
            ctx.containers.container_for_extension("ilbm"),
            Some("iff_ilbm")
        );
        // The aiff sub-module installs itself under FORMAT_NAME =
        // "aiff" and claims `.aif` / `.aiff` / `.aifc` extensions.
        assert_eq!(ctx.containers.container_for_extension("aiff"), Some("aiff"));
        assert_eq!(ctx.containers.container_for_extension("aifc"), Some("aiff"));
    }
}
