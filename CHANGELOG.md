# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.7](https://github.com/OxideAV/oxideav-iff/compare/v0.0.6...v0.0.7) - 2026-05-07

### Other

- round-4 — IlbmMuxer mode select + masking + ImageMagick cross-decode
- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- round-3 — Compression::Auto RDO picker + encoder refactor
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- round 2 — PBM, GRAB, SHAM, PCHG, HAM/EHB encode, ANIM op-0/op-5

### Added

- `ilbm` round-4 features:
  - **`IlbmMuxer::with_mode`** — new `MuxerMode` enum
    (`IndexedAuto` / `Ham6` / `Ham8` / `Ehb` / `Pbm`) lets callers
    request any of the five encoder modes through the streaming
    container API. Previously only indexed planar was reachable;
    HAM/EHB/PBM were `encode_ilbm`-only. The muxer auto-builds the
    correct CAMG flags + plane count + palette cap (16 / 64 / 32 /
    256) for each mode and rejects illegal combinations (e.g. PBM
    with `HasMask` plane).
  - **`IlbmMuxer::with_masking`** — set `Masking::HasMask` or
    `Masking::HasTransparentColor` plus the keyed transparent index
    on muxer-side encodes.
  - **Indexed + PBM `HasTransparentColor` encode path** — when
    `bmhd.masking == HasTransparentColor` and a source pixel's
    alpha is `< 0x80`, the encoder writes `bmhd.transparent_color`
    directly instead of nearest-palette-matching the source RGB,
    so the decoder produces alpha-0 for those indices on read.
  - 13 new tests in `tests/ilbm_round4.rs`: 5 mux-mode round-trips
    (HAM6 grey, HAM8 grey, HAM6 colour gradient, EHB exact, PBM
    lossless uncompressed and ByteRun1), explicit HasMask +
    HasTransparentColor encoder round-trips, PBM RLE-vs-raw
    byte-savings, mode-emits-CAMG sanity, and three optional
    ImageMagick `magick convert` cross-decode tests (indexed + HAM6
    + PBM, gated by `OXIDEAV_IFF_MAGICK_CROSS=1`, silent skip when
    the binary or its `ilbmtoppm` delegate is unavailable). Indexed
    ByteRun1 and PBM cross-decoded pixels are bit-exact against
    `magick`'s output; HAM6 only checks dimensions agree.

- `ilbm` round-3 features:
  - **`Compression::Auto`** — encoder-only variant. `encode_ilbm` tries
    both uncompressed and ByteRun1 BODY for each image and emits the
    shorter result, writing the resolved mode into BMHD. `IlbmMuxer`
    now defaults to `Auto` instead of `ByteRun1`. Picks ByteRun1 for
    solid-colour / gradient images (typical >50 % savings) and raw for
    fully-random bitplane data.
  - `pack_body_resolving` internal helper — returns `(Vec<u8>, Compression)`
    so the BMHD compression byte always matches the actual encoding.
  - All BODY-encoder branches (`indexed`, `ehb`, `ham`, `pbm`) refactored
    into a planar-row builder + a resolving packer so `Auto` propagates
    through every encode path.
  - 8 new tests in `tests/ilbm_round3.rs`: Auto selects RLE for solid
    colour, Auto selects raw for pseudo-random data, HAM6/HAM8/EHB
    self-roundtrip under Auto compression, byte-savings sanity check,
    CAMG emission guard, IlbmMuxer end-to-end with Auto.

- `ilbm` round-2 features:
  - **PBM** chunky variant (`FORM / PBM `) — DPaint II / Brilliance
    8-bit-per-pixel sibling of ILBM. Read + write under the
    `iff_ilbm` container with uncompressed and ByteRun1 BODY.
  - **GRAB** chunk — mouse-pointer hotspot (i16 x, i16 y).
    Round-trip via `IlbmImage::grab`.
  - **SHAM** — Sliced HAM, one 16-entry RGB444 palette per
    scanline. The decoder applies the row's palette to HAM6
    op-`0b00` (palette lookup); the encoder honours per-row
    palette state when quantising HAM6 BODY.
  - **PCHG** — Sebastiano Vigna's palette-change list. Small format
    parsed into per-line `PchgLine` overrides; both small and big
    formats round-trip the original raw bytes for byte-exact
    encoder output. The indexed encoder honours per-row palette
    state when quantising the BODY.
  - **HAM6 / HAM8 encode** — per-row state-machine encoder picks
    the cheapest of (palette lookup, modify-R, modify-G, modify-B)
    against the running channel state by squared distance.
  - **EHB encode** — quantise against the 64-entry expanded
    palette, emit 6 bitplanes regardless of input palette length.
- `anim` module: read-only support for `FORM / ANIM` (DPaint III /
  Aegis Animator multi-frame container). Implements ANHD op 0
  (literal full BODY) and op 5 (Byte Vertical Delta). Op-0 muxer
  helper `encode_anim_op0` available for round-trip testing.
- New container registration `"iff_anim"` with `.anim` extension and
  a `FORM....ANIM` probe; demuxer emits one keyframe packet per
  decoded frame through a `rawvideo` / `Rgba` stream.

## [0.0.6](https://github.com/OxideAV/oxideav-iff/compare/v0.0.5...v0.0.6) - 2026-05-05

### Other

- add FORM/ILBM read + round-trip support
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- pin release-plz to patch-only bumps

### Added

- `ilbm` module: read/round-trip support for FORM/ILBM (Amiga
  InterLeaved BitMap, 1986 Jerry Morrison spec). BMHD / CMAP / CAMG /
  BODY chunks; uncompressed and ByteRun1 (PackBits) BODY; 1..=8
  colour bitplanes; EHB (32→64-entry palette mirroring); HAM6 and HAM8
  with running-state channel modify ops; `Masking::HasMask` plane and
  `Masking::HasTransparentColor` alpha. Public API:
  `parse_ilbm` / `encode_ilbm` / `IlbmImage` / `Bmhd` / `Camg` /
  `byterun1_{decode,encode}_row` / `expand_ham_row` /
  `expand_ehb_palette`.
- New container registration `"iff_ilbm"` with `.ilbm` / `.lbm`
  extensions and a `FORM....ILBM` probe; demuxer emits one keyframe
  packet of RGBA pixels through a `rawvideo` stream.

## [0.0.5](https://github.com/OxideAV/oxideav-iff/compare/v0.0.4...v0.0.5) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core

## [0.0.4](https://github.com/OxideAV/oxideav-iff/compare/v0.0.3...v0.0.4) - 2026-04-19

### Other

- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
- iff / 8svx: round-trip (c) copyright chunk, rewrite README
