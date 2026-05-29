# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **ILBM 24-bit true-colour (literal-RGB) decode + encode.** When
  `BMHD.n_planes == 24` the BODY carries 8 red bitplanes (LSB-first),
  then 8 green, then 8 blue per scanline with no `CMAP` chunk, per the
  EGFF / fileformat.info §3.3.4 description of NewTek / LightWave Toaster
  IFF24 files. Both `Compression::None` and `Compression::ByteRun1` are
  supported (per-plane-per-row, identical to the indexed planar path);
  `Compression::Auto` picks the shorter of the two. `Masking::HasMask`
  is undefined for literal-RGB and is rejected at decode/encode time;
  the `HAM` / `EHB` CAMG flags are also rejected because they describe
  6/8-plane indexed viewports. Alpha is dropped on encode (always
  `0xFF` on decode) — 24-bit ILBM has no transparent-colour key. New
  `MuxerMode::TrueColor24` reaches the encoder through the streaming
  `IlbmMuxer` API; the muxer emits a CAMG-free, CMAP-free ILBM file
  with `n_planes = 24`. Tested in `tests/ilbm_truecolor24.rs`
  (12 tests): raw + ByteRun1 + Auto round-trips, Auto beats raw on a
  solid fill, no-CMAP emit, full 256-value sweep per channel,
  HasMask + n_planes=24 decode rejection, HAM/EHB + n_planes=24 encode
  rejection, alpha-dropped-to-opaque, redundant-CMAP-tolerated decode,
  indexed-encode-without-palette still rejected, end-to-end through
  the streaming muxer.

- `ilbm::Drng` (DeluxePaint IV extended range cycling, variable-length
  chunk: 8-byte header `min, max, rate, flags, ntrue, nregs` followed
  by `ntrue` × `DrngTrueCell` (`cell, r, g, b`) and `nregs` ×
  `DrngRegCell` (`cell, index`)). A super-set of `CRNG` that lets the
  cycle window step through true-colour RGB samples and/or follow live
  palette registers at arbitrary positions inside `[min, max]`.
  `parse_ilbm` collects every `DRNG` chunk into `IlbmImage::drngs`
  (order preserved); `encode_ilbm` re-emits them right after the
  `CCRT` block so a parse → encode is byte-stable. Accessors:
  `Drng::cycles_per_second()` (same `rate / 16384 × 60` Hz as `Crng`),
  `Drng::is_active()`, `Drng::has_true_cells()` /
  `Drng::has_reg_cells()` (honour both the cell list and the `DP_RGB`
  / `DP_REGS` flag bits — robust against generators that set the flag
  without writing any cells), `Drng::range_len()`. Cell-list lengths
  are clamped to `u8::MAX` on encode; the parser rejects truncated
  payloads and short headers (`< 8` bytes) rather than tolerating
  malformed input. Tested in `tests/ilbm_drng.rs` (13 tests): empty
  cell lists, true-cell-only, reg-cell-only, both lists together,
  multi-chunk order preservation, byte-stable re-encode, accessor
  corner cases (inactive, zero rate, inverted range, flag-without-
  cells), short / truncated payload rejection, and a mixed
  CRNG + CCRT + DRNG single-file round-trip.

- `ilbm::Crng` (DeluxePaint colour-range cycling, 8-byte chunk:
  `pad1, rate, flags, low, high`) and `ilbm::Ccrt` (Commodore
  Graphicraft colour-cycling timing, 14-byte chunk: `direction,
  start, end, seconds, micros, pad`) parse/round-trip support.
  `parse_ilbm` collects every `CRNG` / `CCRT` chunk it sees into
  `IlbmImage::crngs` / `IlbmImage::ccrts` (order preserved);
  `encode_ilbm` re-emits them between PCHG and BODY so a parse →
  encode is byte-stable. Each struct exposes spec-derived accessors
  — `Crng::cycles_per_second()` (`rate / 16384 × 60` Hz),
  `Crng::is_active()`, `Crng::is_reverse()`, `Crng::range_len()`;
  `Ccrt::delay_seconds()` (`seconds + micros/1e6`), `Ccrt::is_active()`,
  `Ccrt::is_reverse()`, `Ccrt::range_len()`. Inverted ranges
  (`low > high` / `start > end`) and out-of-spec negative timing
  components clamp to safe defaults rather than wrap. Tested in
  `tests/ilbm_crng_ccrt.rs` (13 tests): single-chunk round-trip,
  multi-chunk order preservation, byte-stable re-encode, inactive /
  reversed flags, short-payload rejection, mixed CRNG+CCRT,
  unknown-chunk skipping. No animation is performed; consumers
  walk `image.crngs` / `image.ccrts` to apply their own palette
  rotation.

- `anim::encode_anim_op5(frames)` and `anim::encode_op5_body(prev,
  cur, bmhd)` — ANIM op-5 (Byte Vertical Delta) encoder. Walks each
  plane's columns top-to-bottom; emits skip ops (1..=0x7F rows
  unchanged), repeat ops (`0x80, cnt, v` for 3..=0xFF same bytes), or
  literal ops (`0x80 | cnt`, then `cnt` bytes for 1..=0x7F differing
  bytes), splitting at run-length caps. Pointer table populates only
  the plane slots that actually carry deltas; identical frames yield
  a 32-byte BODY (just the empty table). Tested in
  `tests/anim_op5_encode.rs` (10 tests): identical-frame trivial
  case, sparse 4×4-corner delta round-trip, sparse-delta byte-count
  beats op-0 by ≥ 20 % on 64×64, long skip-run (300 rows) crosses
  the 0x7F cap correctly, long repeat-run (300 rows) crosses the
  0xFF cap correctly, 2-bitplane indexed round-trip, 4-frame
  bouncing-dot sequence pixel-exact, `encode_op5_body` pointer-table
  has slot 0 = 32 + slots 1..=7 = 0 when only plane 0 dirty,
  `encode_op5_body` rejects > 8 colour planes with `Unsupported`.

- `SvxDemuxer::seek_to(stream_index, pts)` — sample-exact seek across
  `FORM / 8SVX` bodies. 8SVX is keyframe-only `pcm_s8` (Fibonacci-delta
  is decompressed at `open()`), so seek is a constant-time cursor
  reset over the in-memory interleaved frame buffer; the returned pts
  equals `pts.clamp(0, total_frames)` with no keyframe quantisation.
  Works uniformly across raw and Fibonacci-compressed bodies.
  Integration tests in `tests/seek.rs` cover seek-to-zero, half-second
  exact landing, past-EOF clamping, invalid stream index, and seek
  through a Fibonacci body against the demuxer-decoded reference.

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
