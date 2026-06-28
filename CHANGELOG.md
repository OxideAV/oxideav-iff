# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.10](https://github.com/OxideAV/oxideav-iff/compare/v0.0.9...v0.0.10) - 2026-06-28

### Other

- MuxerMode::Acbm — stream a FORM ACBM through IlbmMuxer
- FORM ACBM / ABIT contiguous-bitmap read + round-trip
- FORM TVPP (TVPaint project) best-effort decode + iff_tvpp demuxer
- looping playback lookups for AnimPlayback
- DEEP per-component channel extraction + Dpel layout queries
- per-frame timing + AnimPlayback cumulative-timeline driver
- refresh stale iff_deep module comment for per-DBOD keyframes
- *(iff)* README — multi-image / cel-anim FORM DEEP
- DeepMovie::composite_frame — §1.3 DLOC sub-rectangle placement
- multi-image / cel-anim FORM DEEP decode + encode (§1.4/§1.3/§1.6)
- FORM DEEP RUNLENGTH (§1.5b ByteRun1) body decode + encode
- register iff_deep container demuxer + probe
- register iff_rgb8 / iff_rgbn container demuxers + probes
- encode→decode round-trip integration tests for RGB8 / RGBN / DEEP
- FORM DEEP encode — chunky + TVDC deep-raster round-trip
- FORM RGB8 / RGBN genlock-RLE encode — true-colour round-trip
- integration tests for FORM RGB8 / RGBN / DEEP true-colour decode
- top-level FORM DEEP decode + TVDC per-component-line assembler
- top-level FORM RGB8 / RGBN true-colour decode
- *(ilbm)* FORM DEEP chunky deep-raster — DGBL/DPEL/DLOC + TVDC + RGBA assembly
- FORM RGB8 24-bit genlock-RLE BODY decoder (§3.2)
- FORM RGBN 12-bit genlock-RLE BODY decoder (truecolor §3.1)
- ANIM op-8 (Anim8 short/long vertical delta) decode + encode
- honour ANHD interleave field (double-buffering) in delta decode
- op-1 XOR decodes §2.1 mask plane-subset BODY (full-frame rect)
- refresh to current status, drop per-round changelog cruft

### Fixed

- *(anim)* the §2.1 ANHD `interleave` field is now honoured during
  delta-frame reconstruction. A delta modifies the frame `interleave`
  frames back — `0` defaults to **two** frames back (the DeluxePaint
  double-buffering convention), `n` means n frames back — instead of
  always the immediately-previous frame. The decoder keeps a per-frame
  planar history and selects the referenced buffer (clamped to the seed
  for the first delta(s) per the §1.3 bootstrap). This corrects decode
  of standard double-buffered ANIMs whose deltas were computed against
  the 2-back frame. The in-tree multi-frame encoders, which compute each
  frame as a delta against the immediately-previous frame, now tag
  `interleave = 1` so a full encode → decode round-trip stays
  pixel-exact.

### Added

- *(ilbm)* **`FORM ACBM` (Amiga Contiguous BitMap) read + round-trip.**
  ACBM is the AmigaBASIC sibling of ILBM whose row-interleaved `BODY` is
  replaced by an `ABIT` chunk holding the bitplanes plane-by-plane,
  contiguously, and uncompressed (multimediawiki IFF §4.1). `parse_acbm`
  de-contiguates ABIT into the scanline-interleaved planar layout the
  shared indexed-planar renderer (extracted as `render_indexed_planar`,
  now used by both `parse_ilbm` and `parse_acbm`) expects, so EHB /
  HAM6 / HAM8 / `HasMask` / SHAM / PCHG / colour-cycling all decode
  identically to ILBM. `encode_acbm` transposes the per-row plane
  encoders' output into ABIT's plane-contiguous order and forces
  `BMHD.compression = 0`. `parse_acbm(encode_acbm(x)) == x` for any
  indexed / EHB / HAM image. Compressed ABIT, 24-bit literal layouts and
  the chunky PBM form (none of which have an ACBM analogue) are rejected.
  New `iff_acbm` demuxer (extension `.acbm`, `FORM ACBM` probe) emits one
  `rawvideo` / `Rgba` keyframe, and the streaming `IlbmMuxer` gains a
  `MuxerMode::Acbm` (same indexed palette/plane derivation as
  `IndexedAuto`; body forced uncompressed).
- *(ilbm)* **`FORM TVPP` (TVPaint project) best-effort decode** (§2,
  non-canonical / community RE). `ilbm::parse_tvpp` decodes the DEEP-
  vocabulary raster (`DGBL` / `DPEL` / `DLOC` / `DBOD` / `DCHG`) exactly as
  `parse_deep_frames` does — each `DBOD` becomes one decoded layer
  (`ilbm::TvppImage::layers: Vec<DeepFrame>`), bound to its preceding
  `DLOC` size (§1.3) else the DGBL display size — and surfaces the
  TVPP-specific `MIXR` / `BGP1` / `BGP2` chunks **raw** in
  `extra_chunks: Vec<TvppExtraChunk>` (their byte layout is not pinned
  down by any canonical reference, so no meaning is invented). A wrong
  outer FORM type, a missing `DGBL` / `DPEL` / `DBOD`, or a chunk past the
  FORM are rejected. The `iff_tvpp` container demuxer (extension `tvpp`,
  `FORM TVPP` signature probe) surfaces each layer as a `rawvideo` / `Rgba`
  keyframe.
- *(ilbm)* **DEEP per-component channel extraction + `Dpel` layout
  queries** (§1.2). `ilbm::extract_deep_channel(dpel, w, h, chunky_body,
  c_type)` pulls any one named component out of an *uncompressed* chunky
  DBOD body into a row-major `Vec<u8>` plane (scaled to 8 bits, the same
  bit replication `assemble_deep_chunky` applies to the RGB guns),
  returning `Ok(None)` when the layout lacks that component. This reaches
  channels an RGBA collapse drops — `ZBUFFER`, `MASK`, `LINEARKEY` /
  `BINARYKEY`, `BLACK`, etc. — without inventing a rendering meaning for
  them. New `Dpel` accessors describe the layout: `has_component`,
  `bit_depth_of`, `bit_offset_of` (MSB-first storage offset), and
  `has_alpha` (true only for the well-defined ALPHA / OPACITY transparency
  components, deliberately *not* the key channels).
- *(anim)* **per-frame timing + an `AnimPlayback` cumulative-timeline
  driver** (§2.1 ANHD `abstime` / `reltime`). `parse_anim` now lifts each
  frame's `reltime` (jiffy = 1/60 s delay after the previous frame) and
  `abstime` into a new `anim::AnimImage::frame_timing: Vec<FrameTiming>`
  (parallel to `frames`; the seed frame defaults to t = 0 unless its
  leading `FORM ILBM` carries an `ANHD`, per §1.3). `AnimImage::playback`
  inverts the per-frame deltas into an absolute timeline
  (`anim::AnimPlayback`): one `PlaybackFrame` per frame with its
  cumulative `start_jiffies`, display `duration_jiffies` (= the next
  frame's `reltime`; the last frame holds its own, floored to 1 so a
  looping player still advances), `start_micros` / `duration_micros`
  helpers, plus `total_jiffies` / `total_micros` and `frame_at_jiffies` /
  `frame_at_micros` scrubbers that map a wall-clock offset to the frame
  on screen (clamping past-the-end to the final frame), plus
  `frame_at_jiffies_looping` / `frame_at_micros_looping` that wrap the
  offset modulo the total length for endless playback. The
  `iff_anim` demuxer now emits packet `pts` / `dts` / `duration` from
  this timeline instead of a flat one-tick-per-frame, and
  `duration_micros()` reports the real animation length. A new
  `anim::encode_anim_op0_timed(frames, &[FrameTiming])` authors ANIMs
  with explicit per-frame `reltime` / `abstime`, the round-trip
  counterpart that `parse_anim` + `playback` decode back exactly.
- *(ilbm)* **multi-image / cel-anim `FORM DEEP`** decode + encode (§1.4 DBOD,
  §1.3 DLOC, §1.6 DCHG). A FORM DEEP may carry several DBOD frames — successive
  cels of an animation; previously `parse_deep` decoded only the first.
  `ilbm::parse_deep_frames` now walks the whole FORM into a `ilbm::DeepMovie`
  (DGBL + DPEL + optional DCHG + a `Vec<ilbm::DeepFrame>`), decoding **every**
  DBOD and binding each frame's dimensions to the DLOC that immediately
  precedes it (§1.3, consumed by the next DBOD) else the DGBL display size. The
  new `ilbm::Dchg` chunk (§1.6 `LONG FrameRate`) parses/writes the inter-frame
  timing with the two documented sentinels surfaced as
  `Dchg::AS_FAST_AS_POSSIBLE` (`0`) / `Dchg::NOT_AN_ANIMATION` (`-1`) plus
  `is_not_animation` / `delay_millis` accessors; `DeepMovie::is_animation` /
  `frame_delay_millis` fold the frame count and the DCHG sentinels into the
  "should this play?" / "at what pace?" answers. `ilbm::encode_deep_frames` is
  the inverse for the round-trippable body codings (`None` / `RunLength`),
  emitting DGBL + DPEL + optional DCHG + one DBOD per frame. The `iff_deep`
  demuxer now emits **one `rawvideo` / `Rgba` keyframe per DBOD** instead of a
  single still: with a DCHG millisecond delay it advertises a `1/1000`-second
  time base, per-frame PTS/duration, and a `duration_micros`; a still DEEP (one
  DBOD, or a `0`/`-1` DCHG sentinel) keeps the unit time base. A single-DBOD
  FORM yields a one-frame movie whose frame equals `parse_deep`'s output, so
  existing single-image callers are unaffected. `DeepMovie::composite_frame`
  blits a frame's §1.3 DLOC sub-rectangle onto a fresh
  `DGBL.DisplayWidth × DisplayHeight` RGBA canvas at the DLOC `(x, y)` pixel
  position (the §1.3 "pixel position of this image" placement), clipping pixels
  that fall outside the canvas (negative offset or running past an edge) and
  zero-filling (transparent black) the untouched area — reconstructing a
  multi-cel DEEP whose DBODs are partial sprites narrower than the display;
  `DeepMovie::display_size` surfaces the canvas geometry. Source:
  `docs/image/iff/iff-truecolor-chunks.md` §1.4 / §1.3 / §1.6.
- *(ilbm)* `FORM DEEP` **RUNLENGTH** (`DGBL.Compression == 1`) body
  decode + encode — the §1.5b best-effort coding. `ilbm::decode_deep_runlength_body`
  unpacks the whole DBOD as a single ByteRun1 (PackBits) stream to
  `width × height × pixel_bytes` bytes and assembles it as for
  NOCOMPRESSION; `ilbm::encode_deep_runlength_body` is the inverse.
  `parse_deep` / `encode_deep` now accept `DeepCompression::RunLength`,
  and the `iff_deep` demuxer decodes RUNLENGTH bodies through the
  standard registry path. §1.5b leaves the per-line-vs-whole-DBOD
  framing to a fixture probe; this decoder reads whole-DBOD framing and
  rejects a length mismatch (under-run or trailing source bytes) per
  §1.5b ¶ "fall back … ask for a fixture". HUFFMAN / DYNAMICHUFF / JPEG
  remain rejected (no documented wire layout).
- *(ilbm)* the IFF true-colour FORMs are now wired into the container
  registry: `ilbm::register` installs `iff_rgb8` / `iff_rgbn` / `iff_deep`
  demuxers, each with a `FORM`-signature probe and a matching `.rgb8` /
  `.rgbn` / `.deep` extension, so a Turbo-Silver RGB8 / RGBN or an
  Amiga-Centre-Scotland DEEP file decodes through the standard
  `ContainerRegistry::probe_input` / `open_demuxer` path (single
  `rawvideo` / `Rgba` keyframe, EOF after one packet). The RGB8 / RGBN
  demuxers apply `GenlockPolicy::default` (§3.3 load-as-a-picture
  "ignore genlock, use the coded RGB"); the DEEP demuxer decodes
  NOCOMPRESSION bodies and surfaces the same `parse_deep` error for TVDC
  and the other unsupported codings.
- *(ilbm)* `FORM DEEP` **encode**, completing the chunky deep-raster
  round-trip. `ilbm::encode_deep_chunky` packs a packed RGBA8888 image into the
  raw chunky DBOD stream (inverse of `assemble_deep_chunky`); `ilbm::encode_tvdc`
  encodes one component line to a TVDC nibble stream (inverse of `decode_tvdc`:
  running accumulator, a non-zero `table[d]` per step, runs coded as the
  zero-entry escape nibble + a 0..=15 count nibble); `ilbm::encode_deep` builds
  a complete `FORM DEEP` (DGBL + DPEL + DBOD) for `DeepCompression::None`
  (chunky) or `DeepCompression::Tvdc` (per-component lines, table supplied out
  of band per §1.5). Every DPEL component must be 8 bits for a lossless
  round-trip; sub-8-bit layouts, a missing TVDC table, an undocumented
  compression method, a mis-sized RGBA buffer, or a delta the supplied table
  can't express are each rejected with `Error::invalid`. `ilbm::Dpel::write` and
  `ilbm::Dloc::write` serialise their chunks (inverses of `parse`). The
  NOCOMPRESSION output round-trips through `parse_deep`; the TVDC output through
  `assemble_deep_tvdc` with the matching delta table.
- *(ilbm)* `FORM RGB8` / `FORM RGBN` genlock-RLE **encode**, completing the
  true-colour round-trip. `ilbm::encode_rgb8_body` / `encode_rgbn_body`
  coalesce a packed RGBA8888 image into the Turbo-Silver run-length BODY each
  form carries: maximal runs of identical pixels in flat top-to-bottom order,
  RGB8 emitting a 32-bit LONG with an inline 7-bit count (runs > 127 split into
  successive units, §3.2) and RGBN a 16-bit WORD with the §3.1 count cascade
  (1..=7 inline → BYTE for ≤ 255 → BYTE-0 + WORD for ≤ 65535). Alpha drives the
  genlock bit under `GenlockPolicy::BrushTransparency` (`a == 0` → genlocked).
  `ilbm::encode_rgb8` / `encode_rgbn` wrap the body in a complete `FORM` with a
  `compression = 4` BMHD (nPlanes 25 / 13) and the required minimal `CAMG`, so
  `parse_rgb8(encode_rgb8(x)) == x` for any 8-bit-true image and likewise for
  RGBN once 12-bit nibble quantisation is accounted for. A mis-sized RGBA
  buffer is rejected rather than silently truncating.
- *(ilbm)* top-level `FORM DEEP` decode. `ilbm::parse_deep` walks a
  complete Amiga-Centre-Scotland chunky deep-raster file: it locates DGBL
  (mandatory §1.1 global header), DPEL (mandatory §1.2 pixel layout), the
  optional DLOC placement and the first DBOD, takes the DBOD dimensions
  from the DLOC when present else the DGBL display size (§1.3), and
  assembles a packed top-to-bottom RGBA8888 `ilbm::DeepImage`.
  NOCOMPRESSION bodies decode in full via `assemble_deep_chunky`. TVDC
  per-component-line decode is wired through the new
  `ilbm::assemble_deep_tvdc` (§1.5: one TVDC line per DPEL component per
  row — Red line, then Green line, …; RED/GREEN/BLUE → guns, ALPHA/OPACITY
  → alpha) **when the caller supplies the 16-word delta table**; a
  sub-8-bit DPEL component is rejected because §1.5 pins no byte→sub-8-bit
  mapping. `parse_deep` rejects an in-FORM TVDC body (the §1.5 delta table
  is "stored with the file/companion data" and the canonical DEEP text
  names no in-FORM chunk carrying it — a documented spec gap) and the
  RUNLENGTH/HUFFMAN/DYNAMICHUFF/JPEG codings (wire layout undocumented).
- *(ilbm)* top-level `FORM RGB8` / `FORM RGBN` decode. `ilbm::parse_rgb8`
  and `ilbm::parse_rgbn` walk a complete Turbo-Silver / Imagine
  true-colour file: they locate the mandatory `BMHD` (for dimensions),
  enforce the truecolor-reference §3 invariants — `CAMG` IS REQUIRED and
  `BMHD.compression == 4` (the Turbo-Silver RLE, not ByteRun1) — then hand
  the `BODY` to the existing `decode_rgb8_body` / `decode_rgbn_body`
  genlock-RLE decoders, returning a packed top-to-bottom RGBA8888
  `ilbm::RgbTrueColor` image with a caller-chosen `GenlockPolicy`. A wrong
  outer FORM type, a missing `CAMG`/`BMHD`/`BODY`, a non-4 compression
  byte, or a chunk that overruns the FORM are each rejected with
  `Error::invalid`. This wires the previously body-only RGB8/RGBN support
  to a usable file-level entry point.
- *(ilbm)* FORM DEEP chunky deep-raster support — the structural chunks
  plus the two body codings whose wire format the staged spec fully
  pins down. `ilbm::Dgbl` parses/writes the mandatory 8-byte DGBL global
  header (display size, `DeepCompression` method, pixel aspect);
  `ilbm::Dpel` parses the DPEL pixel-element layout (a ULONG `nElements`
  followed by `(cType, cBitDepth)` pairs in MSB-first storage order) and
  reports `total_bits` / `pixel_bytes` (the pixel padded up to a byte
  boundary); `ilbm::Dloc` parses the optional DBOD-placement chunk.
  `ilbm::decode_tvdc` decodes a TVDC component line (DGBL `Compression
  == 5`, TecSoft's TVPaint addendum): the source is read one nibble at a
  time (high then low), a running accumulator `v` starts at 0, a non-zero
  `table[d]` 16-word delta advances `v` and emits it, and a zero
  `table[d]` reads the next nibble as a short-run count that re-emits the
  current `v`; the function returns the source-byte count
  (`ceil(nibbles/2)`). `ilbm::assemble_deep_chunky` turns a decompressed
  chunky body into packed RGBA8888 top-to-bottom, mapping RED/GREEN/BLUE
  to the guns and ALPHA/OPACITY to alpha (any other component is parsed
  for cursor advance only), with each component scaled from its
  `cBitDepth` to 8 bits by left-shift + MSB replication. RUNLENGTH /
  HUFFMAN / DYNAMICHUFF / JPEG bodies are not yet decoded (the canonical
  DEEP text does not spell out their wire layout). Truncated TVDC
  sources, run overshoot, undersized DPEL/DGBL/DLOC chunks, unknown
  compression/cType codes, and short chunky bodies are each rejected with
  `Error::invalid`. Source: `docs/image/iff/iff-truecolor-chunks.md` §1
  (§1.1 DGBL, §1.2 DPEL, §1.3 DLOC, §1.4 DBOD, §1.5 TVDC).
- *(ilbm)* FORM RGB8 24-bit genlock-RLE BODY decoder
  (`ilbm::decode_rgb8_body`). RGB8 is the 8-bit-per-gun sibling of RGBN:
  a flat stream of 32-bit big-endian LONG units carrying a 24-bit RGB
  value (red = MS byte), a genlock bit, and a single inline 7-bit run
  count (`1..=127`). Per §3.2 ¶ "Impulse never wrote more than a 7-bit
  repeat count, and Imagine/Light24 only read the 7-bit count", RGB8
  has **no** BYTE/WORD count cascade — a zero count is an undefined
  zero-length run and is rejected. Each 8-bit gun passes through
  unchanged; a run may spill across scanline boundaries; output is
  packed RGBA top-to-bottom. The genlock bit is interpreted via the
  shared `ilbm::GenlockPolicy` (Turbo-Silver zero-colour /
  Diamond-Light24 ignore-colour [default] / brush transparency-mask).
  Truncated streams, runs overshooting the pixel budget, and zero-count
  units are each rejected with `Error::invalid`. Source:
  `docs/image/iff/iff-truecolor-chunks.md` §3, §3.2, §3.3.
- *(ilbm)* FORM RGBN 12-bit genlock-RLE BODY decoder
  (`ilbm::decode_rgbn_body`). RGBN is Impulse's Turbo Silver / Imagine
  true-colour FORM (no CLUT): a flat stream of 16-bit big-endian WORD
  units carrying a 12-bit RGB value (red = MS nibble), a genlock bit,
  and a 3-bit run count, with the full count cascade (3-bit inline
  1..7 → trailing BYTE up to 255 → trailing WORD for larger runs).
  Each 4-bit gun is widened to RGB888 by nibble replication; a run may
  spill across scanline boundaries; output is packed RGBA top-to-bottom.
  The genlock bit is interpreted via the new `ilbm::GenlockPolicy`
  (Turbo-Silver zero-colour / Diamond-Light24 ignore-colour [default] /
  brush transparency-mask). Truncated streams, runs overshooting the
  pixel budget, missing BYTE/WORD escapes, and zero-length WORD-escape
  runs are each rejected with `Error::invalid`. Source:
  `docs/image/iff/iff-truecolor-chunks.md` §3, §3.1, §3.3.
- *(anim)* op-8 (Anim8 short / long Vertical Delta, Joe Porkka 1992)
  decode + encode. Op-8 keeps op-5's 16-longword pointer layout (8
  opcode-list pointers used) but interleaves data items inline within
  each opcode list (unlike op-7's separate data lists), so existing
  Anim5 code ports easily. Items are WORD (2 B) or LONG (4 B) per
  `ANHD.bits` bit 0; the §3.2 odd-long edge case (a plane an odd number
  of words wide, long-compressed, gets a trailing WORD column) is
  honoured. New public API: `anim::encode_anim_op8` /
  `anim::encode_op8_body` (plus `anim::apply_op8_for_test`). Source:
  `docs/image/iff/anim-op8.md`.
- *(anim)* op-1 XOR ILBM mode now decodes the §2.1 `mask` plane-subset
  BODY for the full-frame rectangle, not just the all-planes BODY.
  A sparse `mask` carries the scanline-interleaved rows of only the
  selected colour planes (ascending plane order); genuine
  sub-rectangles and plane-masked `HasMask` bitmaps stay rejected as
  undocumented wire layouts.

## [0.0.9](https://github.com/OxideAV/oxideav-iff/compare/v0.0.8...v0.0.9) - 2026-06-15

### Added

- *(aiff)* fold oxideav-aiff into oxideav-iff::aiff

### Other

- add SSND (Sound Data) chunk writer — §5.0 block-aligning body encoder
- frame_chunk framing helper + write_fver_chunk (FVER) writer
- ANIM op-1 (XOR ILBM mode) full-frame decode + encode
- COMM chunk writer + public 80-bit extended sample-rate encoder (§2.1/§3.2)
- ANIM op-4 (Generalized short/long Delta) decode + encode
- op-2/op-3 Long/Short Delta mode decode + encode (spec §1.2.2-§1.2.3, §2.2.1)
- EA IFF 85 §5 LIST/CAT group-children walker
- surface chunk::ReservedId §3 reserved-ckID classifier
- surface EA IFF 85 §3 universally-reserved ckID classifier
- generic top-level group probe primitive (FORM/LIST/CAT envelope)
- typed PCHG header surface + derived-hint consistency check
- typed Sham row-palette accessors mirroring Pchg::palette_at_line
- drop release-plz.toml — use release-plz defaults across the workspace
- cargo-fuzz harness — aiff_decode + anim_decode + pchg_parse
- §14 chunk-precedence surface (ChunkClass + Form helpers)
- structured SPRT (sprite-precedence) chunk surfacing
- structured DEST (destination-merge) chunk surfacing
- structured SAXL (Sound Accelerator) chunk surfacing
- structured §13.0 text chunks (NAME / AUTH / (c) / ANNO)
- structured MIDI (MIDI Data) chunk surfacing
- ANIM op-7 encoder + AIFF COMT/AESD/APPL surfacing + MARK/INST write
- structured INST (Instrument) chunk parsing
- structured MARK (Marker) chunk parsing

### Added

- **AIFF/AIFF-C `SSND` (Sound Data) chunk writer**
  (`docs/audio/aiff/aiff-c.txt` §5.0). `aiff::write_sound_data(&SoundData)`
  emits the §5.0 `SoundDataChunk` data portion — `offset` (u32) +
  `blockSize` (u32) + `offset` bytes of block-alignment padding +
  `soundData` — completing the AIFF round-trip story: `SSND` was the
  one read-path chunk class (`Form::sound`) without a body writer. Per
  §5.0 "offset determines where the first sample frame in the soundData
  starts", a non-zero `offset` inserts that many zero alignment bytes
  before the samples (the §5.0 "Block-Aligning Sound Data" mechanism),
  so the result round-trips through the `SSND` reader, whose `samples`
  slice begins at byte `8 + offset`. The common case
  (§5.0 "Applications that don't care about block alignment should set
  blockSize and offset to zero") emits eight zero header bytes followed
  by the raw samples. Like every other `aiff::write_*` helper it emits a
  chunk *body*; pair it with `aiff::frame_chunk(b"SSND", body)` for the
  full header + odd-length pad. Covered by 4 new unit tests in
  `src/aiff/form.rs` (zero-offset layout, full-FORM `parse` round-trip,
  non-zero-offset alignment-gap round-trip, and a `frame_chunk` →
  `ChunkIter` framing round-trip).
- **AIFF/AIFF-C `frame_chunk` framing helper + `write_fver_chunk`
  (FVER) writer** (`docs/audio/aiff/aiff-aifc-format.md` §1 / §3.1).
  Every per-chunk `aiff::write_*` helper emits a chunk *body* and
  leaves the 8-byte `ckID + ckSize` header and the odd-length pad byte
  to the caller. `aiff::frame_chunk(id, body)` factors that into one
  place — the exact inverse of `aiff::ChunkIter`: it prepends the
  4-byte `ckID` + big-endian `int32` `ckSize` header and appends a
  single `0x00` pad byte iff the body length is odd (the §1 16-bit
  alignment rule; the pad is not counted in `ckSize`), returning
  `AiffError::OversizedChunk` when the body exceeds `u32::MAX`. This
  closes the last write-side gap: `FVER` was the one read-path chunk
  class (`Form::fver_timestamp`) without a body writer.
  `aiff::write_fver_chunk(timestamp)` emits the 4-byte big-endian
  `timestamp`, and `aiff::AIFC_VERSION_1` (`0xA280_5140`) is the §3.1
  AIFF-C v1 spec timestamp every AIFC file carries. Covered by 6 new
  unit tests in `src/aiff/chunk.rs` (even/odd/empty framing, pad
  transparency through a `frame_chunk` → `ChunkIter` round-trip, and an
  `FVER` framed round-trip).
- **ANIM op-1 (XOR ILBM mode) decode + encode** for the full-frame
  case (`docs/image/iff/anim.txt` §1.2.1 / §1.3 / §2.1). op-1 is the
  original ANIM compression method: the encoder XORs every byte of the
  new frame against the previous frame's planar bitmap, producing a
  bitmap that is `0` where the frames agreed, and stores it
  run-length-encoded (`anim::encode_op1_body` / `anim::encode_anim_op1`,
  honouring `BMHD.compression` for ByteRun1 or uncompressed BODY). The
  decoder (`apply_op1`, wired into `parse_anim` via `ANHD.operation =
  1`) expands the BODY and XORs it into the running planar state — a
  zero byte in the XOR bitmap leaves the running state unchanged per
  §1.3. The §2.1 "XOR mode only" `mask` / `w` / `h` / `x` / `y` ANHD
  fields narrow the BODY to a plane subset / sub-rectangle "to
  eliminate unnecessary un-changed data"; the staged spec gives no wire
  description of that partial-BODY layout, so a plane-masked or
  partial-rectangle ANHD is rejected with `Error::unsupported` and the
  full-frame case (all planes, whole bitmap) is decoded. Covered by
  `tests/anim_op1.rs` (8 tests: ByteRun1 + uncompressed round-trips,
  sparse / 2-plane / multi-frame sequences, all-plane-mask tagging,
  no-op-XOR on identical planar buffers, partial-rectangle rejection).

- **AIFF / AIFF-C `COMM` chunk writer + 80-bit extended sample-rate
  encoder** (`docs/audio/aiff/aiff-aifc-format.md` §2.1, §3.2). Adds
  `aiff::write_common_chunk`, the round-trip inverse of
  `parse_common`: it emits the fixed 18-byte AIFF body
  (`numChannels` / `numSampleFrames` / `sampleSize` / 10-byte
  `sampleRate`) and, when the `CommonChunk` carries a
  `compression_type`, the AIFF-C extension — the 4-byte
  `compressionType` FourCC followed by the `compressionName` Pascal
  string padded so its total length (length byte + chars) is even per
  §3.2. A `None` compression name collapses to the canonical
  zero-length pstring. The writer follows the body-only convention of
  the other `write_*_chunk` functions (the FORM header / whole-chunk
  pad byte are the muxer's job). Backing this, the 80-bit IEEE-754
  extended encoder previously available only to the test suite was
  promoted to the public `aiff::encode_extended` (the exact inverse
  of `decode_extended` for finite normalised values) plus
  `aiff::encode_sample_rate`, the validating wrapper that rejects
  NaN / infinite / non-positive rates with `InvalidSampleRate` so a
  writer can never emit a COMM the parser would refuse. With this the
  required `COMM` chunk joins the existing optional-chunk writers, so
  every AIFF chunk class except the top-level FORM/SSND muxer now has
  symmetric read + write paths.
- **ANIM op-4 (Generalized short/long Delta mode) decode + encode**
  (spec §1.2.4, wire format §2.2.2). Implemented from the §2.2.2
  `SetDLTAshort` reference routine, the only normative description of
  the op-4 wire format. The DLTA opens with 16 big-endian u32
  pointers — 8 data-list pointers then 8 op-list pointers — and these
  pointers (plus the per-op column offsets) are measured in **16-bit
  words**, not bytes, because the reference routine performs `WORD*`
  pointer arithmetic (`data = deltaword + deltadata[i]`, `dest =
  planeptr + *ptr`); this is the key behavioural difference from the
  byte-offset ops 5 / 7. Each plane's op list is a flat run of
  `(offset, size)` pairs terminated by `0xFFFF`: `offset` is the
  *absolute* word position where the run begins (non-cumulative),
  `size > 0` copies `size` data words one-per-row (Uniq) and
  `size < 0` copies one data word to `|size|` rows (Same), with the
  dest stepping `nw = row_bytes / word_size` words per row down each
  vertical column. `ANHD.bits` selects the variant — bit 0
  short/long data, bit 2 separate-vs-shared info list (both
  supported), bit 5 short/long op offsets — while the XOR (bit 1) and
  horizontal (bit 4 clear) variants and any reserved high bit are
  rejected with `Error::Unsupported` since the spec gives them no
  separate wire format (§2.1 directs players to verify undefined bits
  are zero). `parse_anim` / the `iff_anim` demuxer now accept
  `ANHD.operation = 4`; the write-side surface is
  [`anim::encode_anim_op4`] (container-level) plus the lower-level
  [`anim::encode_op4_body`], both emitting the short/long-data,
  vertical, RLC, separate-info, non-XOR configuration the reference
  routine reads. Covered by `tests/anim_op4.rs` (10 tests:
  hand-built Same / Uniq decode in short + long data modes, word-unit
  pointer semantics, shared-info-list, unsupported-variant rejection,
  and encode→decode + full-container round-trips).
- **ANIM op-2 / op-3 (Long / Short Delta mode) decode + encode**
  (spec §1.2.2 / §1.2.3, wire format §2.2.1). The DLTA opens with 8
  big-endian u32 plane pointers (`0` = plane unchanged; the §2.2.1
  worked value for the first list is 32); each plane's payload is a
  list of groups whose offsets and counts are big-endian shorts and
  whose data words are longs (op 2) or shorts (op 3). A word cursor
  starts at the plane's first word; a positive offset advances the
  cursor and places one data word, a negative offset (absolute value
  = offset + 2) advances the cursor and a count short introduces that
  many contiguous data words, and `0xFFFF` terminates the plane's
  list. The bitplane is addressed as the contiguous `height ×
  row_bytes` byte array it occupies in memory, so op-2 long words may
  straddle row boundaries — the decoder gathers each plane into a
  contiguous buffer, applies the groups, and scatters the rows back.
  `parse_anim` / the `iff_anim` demuxer now accept `ANHD.operation`
  2 and 3; the new write-side surface is [`anim::encode_anim_op2`] /
  [`anim::encode_anim_op3`] (container-level, mirroring the op-5 /
  op-7 encoders) plus the lower-level [`anim::encode_op23_body`].
  The encoder collapses runs of ≥ 2 changed words into a
  negative-offset group per §1.2.2 ¶ "Strings of 2 or more long-words
  in a row which change can be run together", emits single-word
  groups otherwise, and bridges offsets wider than a positive short
  by rewriting an unchanged word in place. After a run group the
  cursor convention is "last written word" (the spec prose tracks
  the pointer at the position the data word "would be placed at" and
  never says it advances past a write); the in-tree encoder and
  decoder share that reading. 14 new integration tests in
  `tests/anim_op23.rs` — hand-crafted DLTA byte vectors pinning the
  group grammar (single words, runs, terminator, zero pointers,
  straddling long words, truncation/overrun rejection) plus
  encode → `parse_anim` round-trips for both modes.

- **EA IFF 85 §5 LIST/CAT group-children walker**
  ([`chunk::GroupChild`] + [`chunk::parse_group_children`] +
  [`chunk::prop_for_form_type`]). Appendix A's productions close the
  child grammar of the two outer group kinds — `LIST ::= "LIST"
  #{ ContentsType PROP* (FORM | LIST | CAT)* }` and `CAT ::= "CAT "
  #{ ContentsType (FORM | LIST | CAT)* }` — so a generic walker can
  decode every LIST/CAT child without per-form knowledge. The new
  [`chunk::parse_group_children`] takes a [`chunk::GroupKind`] plus
  the group's payload after its ContentsType (the caller bounds the
  slice by the declared ckSize per §5 Group CAT ¶ "programs must
  respect it's ckSize as a virtual end-of-file for reading the nested
  objects") and returns typed [`chunk::GroupChild`] entries — `Prop
  { form_type, body }` for §5 shared-property sets, `Group { kind,
  inner_type, body }` for nested FORM/LIST/CAT — enforcing every
  structural rule §5 states: PROPs only in LISTs (¶ "PROP chunks may
  appear in LISTs (not in FORMs or CATs)" / Rules for Writer Programs
  ¶ "PROPs may only appear inside LISTs"), PROPs before any nested
  group (¶ "all the PROPs must appear before any of the FORMs or
  nested LISTs and CATs"), at most one PROP per FORM type (¶ "A LIST
  may have at most one PROP of a FORM type"). §3 FILLER children are
  walked past without being surfaced ("chunks that fill space but
  have no meaningful contents"); reserved-future-version IDs and bare
  data ckIDs are rejected since the grammar admits no other child.
  `GroupKind::Form` is refused outright — §4's production admits
  `LocalChunk` children whose IDs are form-type-specific, so FORM
  bodies stay with the per-form walkers. [`chunk::prop_for_form_type`]
  is the §5 ¶ "Here are the shared properties for FORM type
  \<FormType\>" lookup joining a FORM type against the parsed child
  list. Nine new unit tests in `src/chunk.rs` (worked-example LIST
  decode, PROP-after-group rejection, duplicate-FormType rejection,
  PROP-in-CAT rejection, FORM-kind rejection, data/future-version
  ckID rejection, FILLER skip, bounds checks, odd-size pad handling)
  plus 3 integration tests in `tests/group_children.rs` building the
  §5 worked example (`LIST { PROP TEXT { FONT } FORM TEXT … }`), a
  `CAT ` of heterogeneous FORMs with the blank `JJJJ` contents ID,
  and a LIST nested inside a CAT walked recursively — all end-to-end
  from `probe_top_level_group` through the child walk. Doc reference:
  `docs/image/iff/ea-iff-85.txt` §5 "LISTs, CATs, and Shared
  Properties" lines 842–986; §6 reader/writer rules lines 1119–1196;
  Appendix A grammar lines 1244–1253.

- **EA IFF 85 §3 universally-reserved ckID classifier
  (`chunk::ReservedId`).** §3 ¶ "the following ckIDs are universally
  reserved to identify chunks with particular IFF meanings: 'LIST',
  'FORM', 'PROP', 'CAT ', and '    '. […] The IDs 'LIS1' through
  'LIS9', 'FOR1' through 'FOR9', and 'CAT1' through 'CAT9' are
  reserved for future 'version number' variations" enumerates the
  full reserved set every conforming IFF reader must recognise. The
  new [`chunk::ReservedId`] enum maps any 4-byte ckID to one of
  `Group(GroupKind)` (the three group-chunk IDs already surfaced
  by [`chunk::GroupKind`]), `Prop` (the §3 PROP property-set group
  that only appears as the first child of a LIST), `Filler` (the
  four-space ID for "chunks that fill space but have no meaningful
  contents"), or `ReservedFuture { parent, digit }` (the
  twenty-seven LIS1..9 / FOR1..9 / CAT1..9 version-number
  variants, carrying back the parent group kind and the ASCII
  digit). Two new module-level constants — [`chunk::FILLER_ID`]
  (`b"    "`) and [`chunk::PROP_ID`] (`b"PROP"`) — give the two
  previously-unnamed reserved IDs a typed home alongside the
  existing [`chunk::GROUP_FORM`] / [`chunk::GROUP_LIST`] /
  [`chunk::GROUP_CAT`] constants. [`chunk::ReservedId::classify`]
  is the free entry point; [`chunk::ChunkHeader::reserved`] and
  [`chunk::ChunkHeader::is_filler`] are the convenience accessors
  the per-form walkers can route on. Three predicates
  ([`ReservedId::is_group`], [`is_filler`], [`is_reserved_future`])
  cover the §3-aware fall-through cases, and
  [`ReservedId::all_reserved_ids`] enumerates the full set in
  spec-listed order. The classifier rejects every FORM-local
  property surfaced elsewhere in the crate (BMHD / CMAP / BODY /
  CAMG / GRAB / DEST / SPRT / CRNG / CCRT / DRNG / SHAM / PCHG /
  VHDR / CHAN / ANNO / NAME / AUTH / COMM / SSND / MARK / INST /
  COMT / AESD / APPL / MIDI / SAXL / FVER / ANHD / DLTA) so the
  data-chunk dispatch path is unaffected. Sixteen new tests cover
  the classifier surface — 9 unit tests in `src/chunk.rs`
  (`reserved_id_classifies_three_groups`,
  `reserved_id_classifies_prop_and_filler`,
  `reserved_id_classifies_all_twenty_seven_future_versions`,
  `reserved_id_rejects_non_reserved_ckid`,
  `reserved_id_rejects_boundary_version_digits`,
  `reserved_id_predicates`,
  `all_reserved_ids_covers_every_id_classify_recognises`,
  `chunk_header_reserved_and_is_filler`,
  `filler_chunk_walks_past_body_without_decoding`) plus 5
  integration tests in `tests/reserved_ids.rs` covering the
  per-id-round-trip, the constant surfaces, an end-to-end
  FILLER-before-FORM stream walk via [`chunk::skip_chunk_body`],
  the future-version parent-group routing, and the FORM-local
  rejection sweep. Doc reference: `docs/image/iff/ea-iff-85.txt`
  §3 "Chunks" lines 524–531; Appendix A C macros
  `ID_FORM`/`ID_LIST`/`ID_PROP`/`ID_CAT`/`ID_FILLER` lines
  1230–1234. The current spec defines no decoder for the
  reserved-future-version IDs; the predicate is offered so callers
  can route them to a versioning-aware fall-back path instead of
  misclassifying them as ordinary data chunks. The §3 paragraph
  cites "23 chunk IDs" as the magic number callers must account
  for, but the explicit enumeration in the same paragraph spells
  out 5 base + 3×9 = 32 distinct IDs; the enum and the helper
  match the explicit list, with the discrepancy flagged in the
  [`all_reserved_ids`] doc comment.

- **Top-level group probe primitive** ([`chunk::probe_top_level_group`]
  + [`chunk::read_top_level_group`]). EA IFF 85 §6 restricts a
  conforming file to a single `FORM`/`LIST`/`CAT ` group at offset 0
  whose first 12 bytes encode `kind` + `ckSize` + the 4-byte inner
  type ID (`FormType` for FORM/PROP, `ContentsType` for LIST/CAT).
  Today the four forms this crate handles (`8SVX` / `ILBM` / `ANIM` /
  `AIFF`/`AIFC`) each open with a near-identical hand-rolled magic
  check (`&buf[0..4] == b"FORM" && &buf[8..12] == b"<type>"`); the
  new primitive lifts that into one tested entry point that returns a
  typed [`chunk::TopLevelGroup`] (with [`chunk::GroupKind`] +
  `inner_type` 4CC + `size` + [`TopLevelGroup::declared_total_len`])
  and surfaces "starts with a non-group FourCC" as
  `Error::invalid("IFF: not a top-level group chunk (got XXXX)")`
  so callers can fall through cleanly to a non-IFF container probe.
  Cross-form regression coverage lives in
  `tests/top_level_group_probe.rs` (5 envelope checks, one per
  shipped form plus a `Cursor` round-trip).
- **ILBM `PCHG` typed-header surface.** The PCHG (Palette CHanGe)
  chunk now exposes its 20-byte fixed-layout header via a typed
  [`Pchg::header`] accessor returning an `Option<PchgHeader>`
  carrying every field the parser reads off the wire:
  [`PchgHeader::compression`], [`PchgHeader::flags`],
  [`PchgHeader::start_line`], [`PchgHeader::line_count`],
  [`PchgHeader::changed_lines`], [`PchgHeader::min_reg`],
  [`PchgHeader::max_reg`], [`PchgHeader::max_changes`], and
  [`PchgHeader::total_changes`]. A companion [`PchgKind`]
  (`Small` / `Big`) enum decodes the change-record encoding
  selector out of the `flags` word; [`Pchg::kind`] is the
  convenience accessor (zero-flag bodies report `Small` per the
  annex's documented default), and [`PchgHeader::is_compressed`]
  flags the `Compression == 1` (Huffman-compressed-records)
  variant — that wire shape still isn't decoded into per-line
  changes, but it now surfaces through the typed header for
  callers needing to fall back to [`Pchg::raw`]. The new
  [`Pchg::derive_header_hints`] re-derives the four annex hint
  fields (`changed_lines`, `min_reg`, `max_reg`, `max_changes`,
  `total_changes`) directly from the decoded [`Pchg::lines`],
  matching the annex's canonical semantics including the
  "empty-PCHG ⇒ MinReg == MaxReg == 0" rule, and
  [`Pchg::header_matches_payload`] gates the on-wire header
  against the canonical re-derivation so editors can flag hint
  drift after modifying the change list before re-encoding.
  Fifteen new tests in `tests/ilbm_pchg_header.rs` cover the
  Small + Big header round-trips, both payloads decoded through
  the typed accessor, the four-field re-derivation against
  Small/Big/empty PCHGs, the consistency check on both
  round-trip and deliberately stale-hint inputs (incorrect
  `TotalChanges` / `MinReg`), the zero-flag default-to-Small
  rule, the `is_compressed` flag on a synthetic
  `Compression == 1` body, and the short-`raw` "header is
  `None`" path for hand-crafted `Pchg` values. No behaviour
  change for existing call sites; the helpers are pure
  additions on top of the existing parser. Doc reference:
  header-layout comment in `src/ilbm.rs` PCHG section (the
  annex layout was already documented inline in the parser;
  this round only surfaces the fields the parser already reads).

- **ILBM `Sham` typed row-palette accessors.** The Sliced-HAM
  per-scanline palette descriptor now exposes typed accessors that
  match the [`Pchg::palette_at_line`] shape: [`Sham::row_palette(y)`]
  borrows the 16-entry RGB888 palette for scanline `y` without
  allocating (returning `None` past the parsed-row count, so callers
  can spot the explicit-vs-padded boundary on hand-built `Sham`
  values), [`Sham::palette_at_line(base, y)`] returns an owned
  16-entry palette mirroring the `Pchg` accessor's shape (SHAM row
  verbatim when present; `base` truncated/padded to 16 entries
  otherwise — the fallback always emits a 16-entry CMAP suitable for
  feeding directly into [`expand_ham_row`]), and
  [`Sham::is_empty`] / [`Sham::rows`] surface the
  "any explicit rows?" / "how many?" predicates without forcing
  callers to reach into the `palettes` field. Three new tests in
  `tests/ilbm_round2.rs` exercise the explicit-row path, the
  past-end fallback path (both full-length and short `base`
  palettes — the helper pads with `[0, 0, 0]` when `base` has fewer
  than 16 entries), the empty-chunk path, and the parser's
  short-chunk row-padding behaviour as observed through the typed
  accessors. No behaviour change for existing call sites; the
  helpers are pure additions and `Sham::palettes` remains
  publicly readable.

- **`cargo-fuzz` harness with three libFuzzer targets.** New `fuzz/`
  subdirectory wires the standard `cargo-fuzz` layout into the crate
  with three targets covering the highest-risk parser surfaces:
  `aiff_decode` (the top-of-stack
  `aiff::demuxer::AiffDemuxer::from_bytes` FORM AIFF / AIFC walker),
  `anim_decode` (the `anim::parse_anim` FORM ANIM walker with its
  op-0 / op-5 / op-7 delta decoders), and `pchg_parse` (the
  failure-mode-dense `ilbm::Pchg::parse` PCHG palette-change-per-line
  chunk decoder). The contract under fuzz is purely that each call
  returns a `Result` and never panics / aborts / OOMs, regardless of
  how malformed the input is. Run with
  `cargo +nightly fuzz run <target>` from the crate root; see the
  README "Fuzzing" section for the full target catalogue and the
  failure-mode notes each target was authored to keep honest.

- **AIFF-C §14 chunk-precedence surface.** A new `aiff::ChunkClass`
  enum ranks the §3.1 / §4..§13 chunk classes per the §14 ordering
  ("Highest precedence Common Chunk … Lowest precedence Application
  Specific Chunk"). The enum's `repr(u8)` is the precedence rank
  (`FVER = 0`; the §14 ranked block runs `1..=13` from `COMM` to
  `APPL`), with [`ChunkClass::rank`] returning that rank and
  [`ChunkClass::higher_precedence_than`] giving the §14 ¶ "the
  loop points in the Instrument Chunk take precedence over
  conflicting loop points found in the MIDI Data Chunk" predicate
  in one call. [`ChunkClass::ck_id`] returns the on-wire 4-byte
  ckID (with the §13.0 ¶ "the 'c' is lowercase and there is a
  space [0x20] after the close parenthesis" Copyright case
  honoured exactly as `b"(c) "`), and
  [`ChunkClass::all_in_precedence_order`] enumerates the full
  fourteen-entry table for callers that want to iterate by §14
  rank. The matching [`aiff::Form::precedence_order`] helper
  walks a parsed `Form` and emits a `Vec<ChunkClass>` of the
  classes the FORM actually contains in §14 order (regardless of
  the on-wire chunk layout — §4 of the staged AIFF-AIFC layout
  doc is explicit that chunk order inside a FORM is unspecified);
  multi-instance classes (§8.0 `SAXL`, §10.0 `MIDI`, §12.0
  `APPL`, §13.0 `ANNO`) appear once per instance and preserve the
  document-order semantics §14 ¶ "Annotation Chunk[s] -- in the
  order they appear in the FORM" requires. The companion
  [`aiff::Form::highest_precedence_class`] returns the top entry
  (always `Some` because `COMM` is mandatory) and identifies an
  AIFF-C FORM with a §3.1 `FVER` chunk as `FormatVersion`-led.
  Eight unit tests in `src/aiff/precedence.rs` and seven
  integration tests in `tests/aiff_precedence.rs` cover the
  rank-vs-spec-order invariant, the ckID round-trip, the
  §13.0 Copyright `(c) ` byte literal, the §14 worked example
  Instrument-outranks-MIDI, the §14 "Common always outranks" /
  "APPL always loses" envelope, irreflexivity of
  `higher_precedence_than`, `Ord` agreement with `rank`, a
  minimal AIFF FORM ordering, a minimal AIFF-C FORM with
  `FVER` ordering, a full thirteen-class FORM with deliberately
  scrambled on-wire layout, multi-instance multiplicities for
  `ANNO` / `APPL` / `MIDI` / `SAXL`, and the
  `highest_precedence_class` switch from `Common` to
  `FormatVersion` when `FVER` is present.
  Doc reference: `docs/audio/aiff/aiff-c.txt` §14 ("Chunk
  Precedence"), lines 1209–1259 of the staged spec text.

- **ILBM `SPRT` (sprite-precedence) chunk surfacing.** `parse_ilbm`
  now lifts the `SPRT` property (ILBM supplement §2.7) into a
  structured [`ilbm::Sprt`] (single `UWORD precedence`) and
  exposes it through `IlbmImage::sprt`. The supplement defines
  the chunk as "presence flags the ILBM as intended as a sprite"
  with a `UWORD SpritePrecedence` where "0 is the highest"
  (foremost). The Appendix A grammar slots SPRT between `[DEST]`
  and `[CAMG]` (`BMHD [CMAP] [GRAB] [DEST] [SPRT] [CAMG]`); §6
  also notes the property chunks "may actually be in any order
  but all must appear before the BODY chunk". `encode_ilbm` emits
  the two-byte payload immediately after `DEST`, before `BODY`.
  A const sentinel [`ilbm::Sprt::FOREMOST`] = `0` plus a
  [`ilbm::Sprt::is_foremost`] predicate surface the §2.7
  "0 is the highest" convention without forcing callers to
  remember the bare-int sentinel. The full unsigned-16 range
  `0..=0xFFFF` round-trips. Eleven new tests in `tests/ilbm_sprt.rs`
  cover the two-byte wire layout, foremost-zero handling,
  max-UWORD handling, short-payload rejection, the implicit
  no-SPRT default, the grammar-ordering invariant (DEST precedes
  SPRT precedes BODY), full-property-set coexistence with GRAB +
  DEST + CAMG, and parse → encode → parse byte-stability. Doc
  reference: `docs/image/iff/ilbm.txt` §2.7 + Appendix A.

- **ILBM `DEST` (destination-merge) chunk surfacing.** `parse_ilbm`
  now lifts the `DEST` property (ILBM §2.6) into a structured
  [`ilbm::Dest`] (`depth` / `pad1` / `plane_pick` / `plane_on_off` /
  `plane_mask`) and exposes it through `IlbmImage::dest`. The §6
  grammar fixes the property order as `BMHD [CMAP] [GRAB] [DEST]
  [SPRT] [CAMG] ... BODY`; `encode_ilbm` honours that slot, writing
  an eight-byte payload (`UBYTE depth`, `UBYTE pad1`, three big-endian
  `UWORD` masks) right after `GRAB`. A helper
  [`ilbm::Dest::pick_count_matches_depth`] surfaces the §2.6 soft
  expectation "the number of '1' bits should equal nPlanes" without
  rejecting non-conforming inputs at parse time (the spec frames the
  equality as an expectation, not a requirement). Round-trip is
  byte-stable, including a non-zero `pad1` byte (§2.6: "unused; for
  consistency put 0 here"). Eight new tests in `tests/ilbm_dest.rs`
  cover the wire layout, the implicit `(1 << nPlanes) - 1` default
  case, mismatch detection, and the FORM-envelope ordering invariant.

- **AIFF / AIFF-C `SAXL` (Sound Accelerator) chunk surfacing.** The
  FORM walker now decodes every `SAXL` chunk
  (`docs/audio/aiff/aiff-c.txt` §8.0 + Appendix D) into a structured
  [`aiff::SaxelChunk`] (with a `Vec<aiff::Saxel>` of `(id, data)`
  pairs) and exposes them through [`aiff::Form::saxels`] in document
  order. §8.0 explicitly permits "any number of Saxel Chunks" per
  FORM AIFC (and "Multiple Saxel Chunks are allowed in a single FORM
  AIFC file"), so the surface is a `Vec` rather than an `Option` —
  matching how §10.0 MIDI and §12.0 APPL handle the
  "any-number-per-FORM" rule. The chunk body is preserved verbatim
  as a raw byte stream — Appendix D ¶ "saxelData contains the
  specific sound accelerator data which is compression-type specific"
  and §8.0 ¶ "Under Construction" / Appendix D ¶ "Caution" emphasise
  the mechanism remained a "rough proposal" in the 1991 draft, so
  this crate does not interpret `data` against any particular
  decompressor's state-priming convention. Lightweight observers
  [`aiff::Saxel::len`] / [`aiff::Saxel::is_empty`] cover the common
  "what's the priming-data length?" inspection without re-parsing.
  Lookups are provided in both directions: [`aiff::Saxel::resolve_marker`]
  joins a saxel's `id` against a supplied [`aiff::MarkerChunk`] per
  §8.0 ¶ "id identifies the marker for which the sound accelerator
  data is to be used" (returning `None` when the id isn't a positive
  `MarkerId` per §6.0 or no marker with that id is present), and
  [`aiff::SaxelChunk::by_marker_id`] scans the chunk's saxel list
  for a matching id. The per-saxel pad byte (Appendix D ¶ "The data
  must be padded with a byte at the end as needed to make it an even
  number of bytes long. This pad byte, if present, is not included
  in size.") is honoured on parse and written on encode; end-of-chunk
  pad on the last saxel is tolerated as either present or absent,
  mirroring the MARK / COMT pstring-tail tolerance for legacy
  encoders that elided the trailing pad. The matching
  [`aiff::write_saxel_chunk`] write-side helper completes the
  round-trip story; an encoder building a FORM AIFC can now emit
  every chunk class the read path surfaces (MARK, INST, COMT, AESD,
  APPL, MIDI, SAXL + the four §13.0 text chunks). New
  `tests/aiff_saxel.rs` covers single-chunk + multiple-chunk-in-FORM
  + empty-Vec-when-absent surfacing, the empty-saxel-list intra-chunk
  case, odd-size saxelData per-saxel pad handling, the write-side
  round-trip, `resolve_marker` against a FORM-level MARK chunk plus
  the zero/negative-id MarkerId-sentinel rejection per §6.0,
  `by_marker_id` lookup, and SAXL coexisting with MARK + COMT + APPL
  + MIDI + ANNO in a single FORM. Internal `saxel.rs` tests exercise
  the same surfaces against the lower-level helpers (empty list,
  single-saxel even/odd data, empty-data, multiple saxels in
  document order with mixed pad, `by_marker_id` happy-path, three
  truncation classes, end-of-chunk pad tolerance, write-helper
  round-trip, write-helper empty-chunk, byte-for-byte write layout
  match, and write document-order preservation). Doc reference:
  `docs/audio/aiff/aiff-c.txt` §8.0 + Appendix D.

- **AIFF / AIFF-C §13.0 text chunks (`NAME` / `AUTH` / `(c) ` / `ANNO`).**
  The FORM walker now decodes the four §13.0 Text Chunks of
  `docs/audio/aiff/aiff-c.txt` into a structured [`aiff::TextChunk`]
  (with a [`aiff::TextKind`] discriminant tagging which of the four
  ckIDs the chunk came from) and surfaces them through new
  [`aiff::Form::name`] / [`aiff::Form::author`] /
  [`aiff::Form::copyright`] / [`aiff::Form::annotations`] fields.
  Per §13.0 ¶ "No more than one Name / Author / Copyright Chunk may
  exist within a FORM AIFC", `NAME` / `AUTH` / `(c) ` are
  duplicate-checked singletons (a second occurrence raises
  [`aiff::AiffError::DuplicateChunk`]); per §13.0 ¶ "Any number of
  Annotation Chunks may exist within a FORM AIFC", `ANNO` is
  accumulated into a `Vec<TextChunk>` in document order, matching how
  §10.0 MIDI and §12.0 APPL handle the "any-number-per-FORM" rule.
  The text body is preserved byte-for-byte (§13.0 ¶ "text contains
  pure ASCII characters. It is neither a pstring nor a C string");
  [`aiff::TextChunk::as_str`] returns a borrowed `&str` for valid
  UTF-8 bodies and [`aiff::TextChunk::as_string_lossy`] decodes the
  full body with `U+FFFD` substitution so MacRoman / Latin-1 bodies
  produced by older encoders are still salvageable. Empty text
  bodies (`ckDataSize == 0`) are accepted — §13.0 places no
  minimum on the text field. A matching [`aiff::write_text_chunk`]
  write-side helper completes the round-trip story; an encoder
  building a FORM AIFF / AIFC can now emit every §13.0 ckID
  alongside the COMT / MARK / INST / AESD / APPL / MIDI write paths
  added in earlier rounds. The `(c) ` ckID uses the canonical
  four-byte ASCII form `0x28 0x63 0x29 0x20` per §13.0 ¶ "the 'c' is
  lowercase and there is a space [0x20] after the close parenthesis";
  the spec uses the round-bracket character itself as the ckID glyph
  standing in for ©. New `tests/aiff_text_chunks.rs` covers
  standalone parse/write round-trips, the four-kind happy path with
  one file carrying NAME + AUTH + `(c) ` + 2 × ANNO, three
  duplicate-chunk rejection paths, a document-order check for ANNO,
  empty-body acceptance, odd-length pad-byte round-trip, and the
  `(c) ` ckID variant-resistance check. Internal `form.rs` tests
  exercise the same surfaces against the lower-level helpers.

- **AIFF / AIFF-C `MIDI` (MIDI Data) chunk surfacing.** The FORM
  walker now decodes every `MIDI` chunk (`docs/audio/aiff/aiff-c.txt`
  §10.0) into a structured [`aiff::MidiDataChunk`] and exposes them
  through [`aiff::Form::midi`] in document order. §10.0 explicitly
  permits "any number of MIDI Data Chunks" per FORM AIFC, so the
  surface is a `Vec` rather than an `Option`. The chunk body is
  preserved verbatim as a raw MIDI byte stream — the spec calls
  `MIDIdata` "a stream of MIDI data" and imposes no internal
  framing, so an MMA Standard MIDI File-style decode (MThd / MTrk
  / variable-length quantity / running status) remains the job of
  the `oxideav-midi` sibling crate. Lightweight observers
  [`aiff::MidiDataChunk::len`] /
  [`aiff::MidiDataChunk::is_empty`] /
  [`aiff::MidiDataChunk::is_sysex`] cover the common "is this a
  SysEx patch dump or something else?" classification without
  re-parsing (`is_sysex` matches the leading `0xF0` status byte
  the spec calls out as the chunk's "primary purpose"). The
  matching [`aiff::write_midi_chunk`] write-side helper completes
  the round-trip story for encoders building a FORM AIFC. Empty
  chunks (`ckDataSize == 0`) are accepted per §10.0 ¶ "MIDIData
  contains a stream of MIDI data." — the spec sets no minimum
  body length. New `tests/aiff_optional_chunks.rs` cases
  (`surfaces_single_midi_chunk`,
  `surfaces_multiple_midi_chunks_in_document_order`,
  `surfaces_zero_midi_chunks_as_empty_vec`,
  `midi_chunk_with_odd_length_round_trips_through_chunk_walker`,
  `midi_chunk_write_helper_roundtrips`,
  `empty_midi_chunk_is_accepted`,
  `midi_chunk_coexists_with_other_optional_chunks`) exercise the
  full surface plus 6 module-level unit tests
  (`parses_empty_chunk`, `preserves_byte_stream_verbatim`,
  `is_sysex_false_when_first_byte_is_not_f0`, `write_round_trips`,
  `write_round_trips_empty_chunk`, `classifies_odd_length_stream`,
  `accepts_large_body`). Doc reference:
  `docs/audio/aiff/aiff-c.txt` §10.0 MIDI DATA CHUNK.
- **ANIM op-7 (Short / Long Vertical Delta) encoder.** New
  [`anim::encode_op7_body`] builds the 64-byte pointer table + 8
  per-plane opcode lists + 8 per-plane data lists from a `prev` /
  `cur` planar-frame pair, picking Skip / Same / Uniq ops per column
  to minimise byte cost (Same for runs ≥ 2 items, Uniq otherwise,
  Skip for unchanged runs). [`anim::encode_anim_op7`] wraps it into a
  full FORM/ANIM file with leading `FORM ILBM` (seed) + per-delta
  `FORM ILBM { ANHD(op=7, bits=long_data?1:0) + DLTA }` frames. The
  short (2-byte items, `ANHD.bits` bit 0 cleared) and long (4-byte
  items, bit set) variants both round-trip through the in-tree
  [`anim::parse_anim`] decoder. The parser was extended to accept the
  `DLTA` chunk id alongside `BODY` so op-7 / op-5 / op-0 streams all
  decode via the same path. New `tests/anim_op7_encode.rs` exercises
  identical-frame elimination, sparse and full-change deltas, short
  vs long mode, and rejects `long_data=true` with `row_bytes`
  unaligned to 4. Doc reference: `docs/image/iff/anim.txt` Appendix
  Anim7 §#.# (Wolfgang Hofer, 23.6.92).
- **AIFF / AIFF-C `COMT` (Comments) chunk parsing.** The FORM walker
  now decodes the `COMT` chunk into a structured
  [`aiff::CommentsChunk`] surfaced through [`aiff::Form::comments`]
  per `docs/audio/aiff/aiff-c.txt` §7.0. Each comment carries a
  `timestamp` (seconds since 1904-01-01 UTC, the Mac epoch), a
  `MarkerId` (0 = comment is not linked to any marker, otherwise
  references the FORM's MARK entry), and a UTF-8-lossy decoded text
  body. The accompanying [`aiff::Comment::linked_marker`] returns
  `Option<i16>` so callers can distinguish linked vs free-floating
  comments without checking the marker field directly, and
  [`aiff::Comment::resolve_marker`] joins the linkage against a
  supplied [`aiff::MarkerChunk`]. At most one `COMT` per FORM per
  §7.0 — duplicates are rejected as `AiffError::DuplicateChunk
  ("COMT")`. The per-comment `text` pad byte rule (pad to even byte
  count, pad NOT included in `count`) is honoured with the same
  end-of-buffer tolerance as `MARK`.
- **AIFF / AIFF-C `AESD` (Audio Recording) chunk parsing.** The FORM
  walker now decodes the `AESD` chunk into a structured
  [`aiff::AesdChunk`] surfaced through [`aiff::Form::aesd`] per
  §11.0. The 24-byte AES channel-status block is preserved verbatim
  in `status`; [`aiff::AesdChunk::emphasis`] extracts the 3-bit
  recording-emphasis field from byte 0 bits 2..=4 the spec calls out
  as "of general interest". At most one `AESD` per FORM per §11.0;
  duplicates rejected as `AiffError::DuplicateChunk("AESD")`. The
  spec's "ckDataSize is always 24" invariant is enforced — shorter
  is `Truncated`, longer is
  `InvalidValue { what: "AESD ckSize", ... }`.
- **AIFF / AIFF-C `APPL` (Application Specific) chunk parsing.** The
  FORM walker now decodes every `APPL` chunk into an
  [`aiff::ApplicationChunk`] and collects them into
  [`aiff::Form::applications`] in document order (§12.0 explicitly
  permits any number of APPL chunks per FORM, unlike the other
  optional chunks). [`aiff::ApplicationChunk::dialect`] classifies
  the four-byte `applicationSignature` into the three §12.0
  dialects (`pdos` Apple II, `stoc` non-Apple, anything else =
  Macintosh); [`aiff::ApplicationChunk::application_name`] decodes
  the leading Pascal-string application name for `pdos` / `stoc`
  chunks (Macintosh dialect carries raw bytes with no required
  leading structure) and [`aiff::ApplicationChunk::
  payload_after_name`] returns the slice after the name, stepping
  by exactly `1 + length_byte` (§12.0 specifies chunk-level
  pad-to-even on the whole APPL but not an inner pad after the
  leading pstring).
- **`MARK` and `INST` write-side encoders.** Encoders building AIFF /
  AIFF-C files can now construct the exact wire body for these
  chunks via [`aiff::write_marker_chunk`] / [`aiff::
  write_instrument_chunk`]. The marker writer preserves document
  order, honours the §6.0 pstring pad-to-even discipline, and caps
  oversize names / lists at the wire field widths (u8 length, u16
  numMarkers); the instrument writer emits exactly 20 bytes in spec
  field order and accepts arbitrary `Loop` substructures. Companion
  [`aiff::write_comments_chunk`] / [`aiff::write_appl_chunk`] /
  [`aiff::write_aesd_chunk`] cover the other newly-surfaced
  chunks. All five round-trip through the FORM-level [`aiff::parse`]
  walker (verified by `tests/aiff_optional_chunks.rs`).
- **AIFF / AIFF-C `INST` (Instrument) chunk parsing.** The FORM
  walker now decodes the `INST` chunk into a structured
  [`aiff::InstrumentChunk`] surfaced through
  [`aiff::Form::instrument`]. Every wire field is preserved:
  `baseNote` / `lowNote` / `highNote` (MIDI 0..=127),
  `detune` (cents -50..=+50), `lowVelocity` / `highVelocity`
  (1..=127), signed-dB `gain`, plus the two `Loop`
  substructures (sustainLoop, releaseLoop) which carry a decoded
  [`aiff::PlayMode`] (`NoLooping` / `ForwardLooping` /
  `ForwardBackwardLooping`) and the two `MarkerId`s referencing the
  FORM's MARK chunk. The parser enforces every §9 invariant — at
  most one `INST` chunk per FORM (rejected as
  `AiffError::DuplicateChunk("INST")`), exact 20-byte ckDataSize
  ("ckDataSize is always 20" — shorter is `Truncated`, longer is
  `InvalidValue { what: "INST ckSize", ... }`), MIDI-note range,
  detune range, velocity range, and a known `playMode`. The
  accompanying [`aiff::InstrumentChunk::resolve_sustain_loop`] /
  [`aiff::InstrumentChunk::resolve_release_loop`] helpers join the
  loop endpoints against the FORM's [`aiff::MarkerChunk`] and apply
  §9 ¶ "beginLoop and endLoop": "The begin position must be less
  than the end position so the loop segment will have a positive
  length. [If this is not the case, then ignore this loop segment.
  No looping takes place.]" — returning `None` whenever
  `playMode == None`, an endpoint id isn't a positive marker id,
  either id isn't present in the supplied MARK list, or the begin
  marker's frame position isn't strictly less than the end marker's.
  22 new tests across the `aiff::instrument`, `aiff::form` and
  `tests/aiff_instrument.rs` surfaces.

- **AIFF / AIFF-C `MARK` (Marker) chunk parsing.** The FORM walker
  now decodes the `MARK` chunk into a structured
  [`aiff::MarkerChunk`] surfaced through [`aiff::Form::markers`].
  Each [`aiff::Marker`] carries the spec's `id` (big-endian `i16`,
  > 0, unique within the FORM), `position` (big-endian `u32` sample
  frame; for compressed AIFF-C streams the spec defines this in
  expanded-domain frames per §6.0 ¶3), and pstring `name`
  (length-prefixed with pad-to-even total). The parser enforces
  every AIFF-C §6.0 invariant — at most one `MARK` chunk per FORM
  (rejected as `AiffError::DuplicateChunk("MARK")`), positive
  `MarkerId` (rejected as `AiffError::InvalidValue { what:
  "MarkerId", ... }`), and unique-id-within-chunk (rejected as
  `AiffError::DuplicateMarkerId`). Markers are exposed in document
  order; `MarkerChunk::by_id` provides the typical
  lookup-by-id needed when the AIFF-C `INST` (instrument) chunk
  references loop endpoints by marker id. 17 new tests across the
  `aiff::marker`, `aiff::form` and `tests/aiff_markers.rs`
  surfaces.

### Changed

- `aiff::Form` gains a `markers: Option<MarkerChunk>` field —
  `None` when the FORM has no `MARK` chunk, `Some(MarkerChunk{
  markers: vec![] })` for an empty marker list (the encoder
  declared markers but had none).
- `aiff::Form` gains an `instrument: Option<InstrumentChunk>`
  field — `None` when the FORM has no `INST` chunk, `Some(_)`
  otherwise. New since the previous Unreleased entry.



## [0.0.8](https://github.com/OxideAV/oxideav-iff/compare/v0.0.7...v0.0.8) - 2026-05-30

### Other

- ANIM op-7 (Short / Long Vertical Delta) decode
- palette-cycling step helpers + per-line PCHG palette resolver
- 24-bit literal-RGB true-colour decode + encode
- DRNG DPaint IV extended range cycling chunk
- CRNG (DPaint colour-range) + CCRT (Graphicraft) chunks
- ANIM op-5 Byte Vertical Delta encoder
- add Demuxer::seek_to — sample-exact O(1) cursor reset

### Added

- **AIFF / AIFF-C (AIFC) container** support folded in from the
  retired `oxideav-aiff` crate (which was published only at v0.0.1).
  The full surface — `Chunk` / `ChunkIter` slice-based walker,
  80-bit IEEE-extended sample-rate decode, `CommonChunk` /
  `parse_common`, FORM walker (`parse` / `Form` / `SoundData`),
  PCM compression-flavour readers (`decode_pcm`,
  `is_pcm_compression`, `PcmSamples`), and the
  `AiffDemuxer` factory — is now available under the
  `oxideav_iff::aiff::*` module. The registry installs the demuxer
  under codec id `"aiff"` and claims `.aif` / `.aiff` / `.aifc`
  extensions.

  Migration: `oxideav_aiff::*` → `oxideav_iff::aiff::*`. The
  `default-features = false` standalone-build capability that
  `oxideav-aiff` exposed is intentionally not preserved here;
  `oxideav-iff` has a hard `oxideav-core` dep.

- **ANIM op-7 (Short / Long Vertical Delta) decode.** When a delta
  frame carries `ANHD.operation = 7`, the running planar state is
  patched in place by walking the DLTA chunk's 16 big-endian u32
  pointer table (8 opcode-list pointers + 8 data-list pointers, one
  pair per plane; a `0` pointer marks the plane unchanged). Per plane
  the bitplane is split into vertical columns of width `data_size`,
  controlled by `ANHD.bits` bit 0 (`0` = short 2-byte items, `1` =
  long 4-byte items); column count = `row_bytes / data_size`. Each
  column starts with an `op_count` byte (0 = column unchanged)
  followed by `op_count` opcode bytes; the three opcode classes are
  Skip (hi bit clear, non-zero — forward the dest cursor by N rows,
  no data consumed), Uniq (hi bit set — copy `byte & 0x7F` data
  items literally from the data list, one per consecutive row) and
  Same (`0x00` byte followed by a count byte — copy one data item
  `count` times to consecutive rows). Advancing one row adds
  `row_bytes` (NOT `data_size`) to the byte offset within the
  bitplane. Tested in `tests/anim_op7_decode.rs` (6 tests): short
  Skip + Uniq + Same exercise across all 4 columns of a 1-plane
  64×4 image, long-data (4-byte item) exercise across a 1-plane
  64×3 image, all-zero pointer table leaves state untouched,
  truncated pointer table errors, out-of-range opcode pointer
  errors, two-plane independent pointer-pair lookup. Op-7 encode +
  op-8 decode/encode remain open follow-ups.

- **ILBM palette-cycling step helpers + per-scanline PCHG resolver.**
  `Crng::cycle_step(palette, steps)`, `Ccrt::cycle_step(palette, steps)`
  and `Drng::cycle_step(palette, steps)` rotate the closed range
  (`[low..=high]` for `CRNG`, `[start..=end]` for `CCRT`,
  `[min..=max]` for `DRNG`) in place by `steps` ticks. `Crng` and
  `Ccrt` honour their reverse-direction flag (CRNG's `FLAG_REVERSE`,
  CCRT's `direction < 0`); DRNG cycles forward only (its wire format
  has no direction flag) and the in-tree DRNG spec material does not
  define per-tick semantics for the optional `DrngTrueCell` /
  `DrngRegCell` lists, so the cell list is left untouched and callers
  layer their own splice on top after the rotation. Each helper takes
  `steps` modulo the range length so feeding an accumulated tick
  counter into it is O(range) regardless of how large `steps` grows;
  inactive cycles, malformed ranges (`low > high`), ranges that extend
  past the palette tail, single-slot ranges, and `steps == 0 mod
  range_len` are all no-ops and the helper returns `false` to signal
  "palette unchanged." `Pchg::palette_at_line(base, y)` returns the
  cumulative PCHG-overridden palette at the start of scanline `y` by
  folding every PCHG entry whose `line <= y` over `base`; out-of-range
  register indices are skipped silently to match the parser's
  tolerance. A free `palette_for_line(image, y)` convenience wraps the
  `Option<Pchg>` plumbing so animation consumers can write a uniform
  per-row "give me the active palette" call without branching on
  whether the file carried a PCHG chunk. Tested in
  `tests/ilbm_palette_cycle.rs` (28 tests): forward / reverse single
  ticks against synthesised palettes, full-revolution identity, large
  modulo step counts, inactive / single-slot / inverted-range / past-
  palette no-ops, zero-step no-op, fwd-then-reverse round-trip,
  CCRT-direction-zero no-op, DRNG cell-list preservation across the
  rotation, PCHG before-first-override / mid-override / past-image-
  height resolution, PCHG out-of-range-index tolerance, and an
  end-to-end "PCHG override + CRNG rotation on top" composition test
  showing the two helpers compose without interfering.

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
