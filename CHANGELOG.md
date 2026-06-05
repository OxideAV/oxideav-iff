# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
