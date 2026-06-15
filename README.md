# oxideav-iff

Pure-Rust EA IFF 85 container support for oxideav — the chunk reader
that underlies the entire `FORM / LIST / CAT` family. Today this
crate ships:

- **FORM/8SVX** — full read/write (Amiga 8-bit sampled voice).
- **FORM/ILBM** — read+round-trip (1..=8 indexed bitplanes **and
  24-bit literal-RGB true-colour**, ByteRun1 / Auto compression,
  EHB, HAM6, HAM8, HasMask, transparent-colour keying, GRAB hotspot,
  DEST destination-merge (depth / planePick / planeOnOff / planeMask),
  SPRT sprite-precedence flag, SHAM per-line palette **with typed
  row-palette accessors (`row_palette` / `palette_at_line` /
  `is_empty` / `rows`) mirroring the `Pchg::palette_at_line`
  shape**, PCHG Small + Big palette-change list **with a typed
  `PchgHeader` accessor surfacing every 20-byte header field
  (`compression` / `flags` / `start_line` / `line_count` /
  `changed_lines` / `min_reg` / `max_reg` / `max_changes` /
  `total_changes`), a `PchgKind` (Small / Big) enum derived from
  the flag word, and a `derive_header_hints` re-derivation helper
  + `header_matches_payload` consistency check for callers
  validating or re-deriving the header hints after editing the
  change list**, CRNG / CCRT / DRNG colour-cycling descriptors).
- **FORM/PBM** — read+round-trip (DPaint II / Brilliance chunky sibling).
- **FORM/ANIM** — op-0 literal + op-2/op-3 Long/Short Delta
  (encode+decode) + op-5 byte-vertical delta (encode+decode) +
  op-7 Short/Long Vertical Delta (encode+decode). The op-7 encoder
  picks Skip / Same / Uniq ops
  per column to minimise byte cost (Same for runs ≥ 2, Uniq
  otherwise, Skip for unchanged runs); both short (2-byte items)
  and long (4-byte items, `ANHD.bits` bit 0 set) variants
  round-trip through the in-tree decoder. The op-2/op-3 group
  grammar (§2.2.1 — positive offset + one data word, negative
  offset + counted contiguous run, `0xFFFF` terminator) addresses
  each bitplane as the contiguous word array it occupies in
  memory, so op-2 long words may straddle row boundaries. The
  container walker accepts both `BODY` and `DLTA` chunk ids so
  op-0 / op-2 / op-3 / op-5 / op-7 streams decode through the
  same path.
- **FORM/AIFF and FORM/AIFC** — Apple AIFF / AIFF-C (read):
  COMM/SSND/FVER/MARK/INST/COMT/AESD/APPL/MIDI/SAXL/NAME/AUTH/(c) /ANNO
  walker, 80-bit IEEE-extended sample-rate decode, PCM
  compression-flavour readers for `NONE` / `twos` / `sowt` /
  `raw ` / `fl32` / `FL32` / `fl64` / `FL64`, structured `MARK`
  chunk parsing
  (`MarkerChunk` → id / sample-frame position / pstring name per
  marker, with one-per-FORM enforcement and unique-id validation),
  structured `INST` (instrument) chunk parsing (`InstrumentChunk`
  → MIDI baseNote/lowNote/highNote + detune + lowVelocity /
  highVelocity + signed-dB gain + sustainLoop / releaseLoop with
  `resolve_sustain_loop` / `resolve_release_loop` joining the loop
  endpoints against the MARK list per §9), structured `COMT`
  (comments) chunk parsing (`CommentsChunk` → per-comment
  timestamp + optional `MarkerId` linkage + UTF-8 text body, with
  `resolve_marker` joining the linkage against MARK per §7.0),
  structured `AESD` (audio recording) chunk parsing (`AesdChunk` →
  24-byte AES channel-status block + byte-0 recording-emphasis
  field per §11.0), and structured `APPL` (application-specific)
  chunk parsing (`ApplicationChunk` → 4-byte signature + raw data
  + `pdos`/`stoc`/Macintosh dialect classification +
  application-name decode for the `pdos`/`stoc` dialects per
  §12.0; multiple APPL chunks per FORM are permitted and surfaced
  in document order), and structured `MIDI` (MIDI Data) chunk
  parsing (`MidiDataChunk` → raw MIDI byte stream preserved
  verbatim per §10.0, with `is_sysex` / `len` / `is_empty`
  classifiers; multiple MIDI chunks per FORM are permitted and
  surfaced in document order, an event-level Standard MIDI File
  decode belongs in the `oxideav-midi` sibling crate), structured
  §8.0 / Appendix D `SAXL` (Sound Accelerator) chunk parsing
  (`SaxelChunk` → `Vec<Saxel>` with each entry pairing a `MarkerId`
  with a compression-type-specific `data` byte-stream preserved
  verbatim per Appendix D ¶ "saxelData contains the specific sound
  accelerator data which is compression-type specific", plus
  `Saxel::resolve_marker` / `SaxelChunk::by_marker_id` lookups;
  multiple SAXL chunks per FORM are permitted per §8.0 ¶ "Multiple
  Saxel Chunks are allowed in a single FORM AIFC file" and surfaced
  in document order via `Form::saxels`), and
  structured §13.0 text-chunk parsing (`TextChunk` → `kind`
  discriminant for `NAME` / `AUTH` / `(c) ` / `ANNO` + raw text
  bytes preserved verbatim per §13.0 ¶ "pure ASCII […] neither a
  pstring nor a C string", with `as_str` / `as_string_lossy`
  decode helpers; `NAME` / `AUTH` / `(c) ` are at-most-one-per-FORM
  singletons surfaced via `Form::name` / `Form::author` /
  `Form::copyright`, `ANNO` is "any-number-per-FORM" per §13.0 and
  surfaced via `Form::annotations` in document order). Write-side
  encoders for the required `COMM` chunk (`write_common_chunk`,
  emitting the 18-byte AIFF body or the AIFF-C body with
  `compressionType` FourCC + even-padded Pascal-string
  `compressionName`, round-trippable through `parse_common`; backed
  by `encode_sample_rate` / `encode_extended`, the validating inverse
  of the 80-bit IEEE-extended sample-rate decoder) plus the core
  `SSND` sound-data chunk (`write_sound_data`, emitting the §5.0
  `offset` + `blockSize` + alignment-padding + `soundData` body — a
  non-zero `offset` inserts that many zero block-alignment bytes
  before the first sample frame per §5.0 "Block-Aligning Sound Data",
  round-trippable through the `SSND` reader whose `samples` slice
  begins at byte `8 + offset`), `MARK`,
  `INST`, `COMT`, `AESD`, `APPL`, `MIDI`,
  `SAXL`, and the four §13.0 text chunks are also available so
  callers building an AIFF / AIFC file can emit every chunk class
  round-trippably, with `write_fver_chunk` (the `FVER` Format Version
  body, `AIFC_VERSION_1 = 0xA2805140`) closing the last write-side gap.
  Each `write_*` helper emits a chunk *body*; the `frame_chunk(id,
  body)` helper is the exact inverse of the `ChunkIter` walker —
  prepending the 8-byte `ckID + ckSize` header and appending the §1
  odd-length `0x00` pad byte — so a caller assembling a FORM does not
  re-derive the big-endian size encoding and the pad rule per chunk.
  Codec-bearing
  `compressionType` FourCCs (`ima4`, `ulaw`, `alaw`, …) are
  recognised in the parser but routed through sibling codec crates
  rather than decoded here.

Shared chunk primitives (`chunk` module): `ChunkHeader` +
`read_chunk_header` (8-byte header + clean-EOF convention),
`GroupKind` + `TopLevelGroup` + `probe_top_level_group` /
`read_top_level_group` (front-half magic check that decodes the
single top-level `FORM` / `LIST` / `CAT ` envelope every IFF file
opens with, surfacing the inner `FormType` / `ContentsType` 4CC and
the declared envelope length without committing to any specific
form-type), `read_body` / `skip_chunk_body` / `skip_pad` (pad-byte
aware body walkers), `ReservedId` + `ReservedId::classify` /
`ChunkHeader::reserved` / `ChunkHeader::is_filler` (EA IFF 85 §3
universally-reserved ckID classifier covering `FORM` / `LIST` /
`CAT ` / `PROP` / four-space FILLER + the 27 reserved-future-version
IDs `LIS1..9` / `FOR1..9` / `CAT1..9`), and the matching
`FILLER_ID` / `PROP_ID` constants. On top of the classifier sits the
§5 group-children walker — `GroupChild` + `parse_group_children` +
`prop_for_form_type` — which decodes the closed child grammars
`LIST ::= ContentsType PROP* (FORM|LIST|CAT)*` and
`CAT ::= ContentsType (FORM|LIST|CAT)*`, enforcing the §5 structural
rules (PROPs before any nested group, at most one PROP per FORM
type, no PROP inside a CAT, §3 FILLER skipped, reserved-future /
data ckIDs rejected) and surfacing each child's subtype ID + body
slice so LIST/CAT files walk recursively without any per-form
knowledge.

Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-container = "0.1"
oxideav-iff = "0.0"
```

## Supported formats

### 8SVX — Amiga 8-bit Sampled Voice

Full read and write support for `FORM / 8SVX`:

| Feature                                  | Read | Write |
|------------------------------------------|:----:|:-----:|
| `VHDR` voice header                      |  Y   |   Y   |
| Raw PCM (`sCompression = 0`)             |  Y   |   Y   |
| Fibonacci-delta (`sCompression = 1`)     |  Y   |   Y   |
| Mono (no `CHAN` chunk, or `CHAN = 2`)    |  Y   |   Y   |
| Stereo (`CHAN = 6`, concatenated halves) |  Y   |   Y   |
| `NAME` / `AUTH` / `ANNO` / `(c) ` / `CHRS` tags | Y | Y |
| Sample-exact seek (`Demuxer::seek_to`)    |  Y   |  —   |

- The exposed codec id is `pcm_s8`; Fibonacci-delta compression is
  transparent — decoded on demux, encoded on mux when the caller picks
  `Compression::Fibonacci`.
- `seek_to(0, pts)` is sample-exact: 8SVX is keyframe-only PCM and the
  whole BODY is expanded into a flat interleaved frame buffer on
  `open()`, so seek is a constant-time cursor reset. Out-of-range
  targets clamp to `[0, total_frames]`. Works uniformly across raw and
  Fibonacci-delta bodies because the cursor indexes the decoded
  buffer, not compressed bytes.
- Stereo BODY layout follows the common AmigaOS convention: the LEFT
  channel in full, then the RIGHT channel in full. For Fibonacci
  stereo each half carries its own `[pad, initial_sample, nibbles...]`
  header and is decoded independently.
- Fibonacci-delta table:
  `[-34, -21, -13, -8, -5, -3, -2, -1, 0, 1, 2, 3, 5, 8, 13, 21]` (16
  entries, from the Amiga ROM Kernel Manual / AmigaOS wiki). A 4-bit
  code cannot address a 17th entry.
- Fibonacci-delta is lossy; round-trips reconstruct each sample within
  +-2 LSBs on smooth signals.

## Quick use

### Read an 8SVX voice

```rust
use oxideav_container::ContainerRegistry;
use oxideav_core::Error;

let mut containers = ContainerRegistry::new();
oxideav_iff::register(&mut containers);

let input: Box<dyn oxideav_container::ReadSeek> = Box::new(
    std::io::Cursor::new(std::fs::read("voice.8svx")?),
);
let mut dmx = containers.open_demuxer("iff_8svx", input)?;
let stream = &dmx.streams()[0];
assert_eq!(stream.params.codec_id.as_str(), "pcm_s8");

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            // pkt.data is interleaved pcm_s8 (mono or stereo L R L R ...).
        }
        Err(Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Write a stereo Fibonacci-delta voice

```rust
use oxideav_iff::svx::{Compression, SvxMuxer};

// `stream` describes 2-channel pcm_s8; `packet.data` is interleaved
// L R L R ... at 8 bits per sample.
let mut mux = SvxMuxer::new(out, &[stream])?
    .with_compression(Compression::Fibonacci);
mux.write_header()?;
mux.write_packet(&packet)?;
mux.write_trailer()?;
```

### Container / codec IDs

- Container: `"iff_8svx"`, probes `FORM....8SVX` and matches `.8svx` /
  `.iff` by extension.
- Codec (inside the stream): `"pcm_s8"`.

### ILBM — Amiga InterLeaved BitMap

Read + round-trip support for `FORM / ILBM`:

| Feature                                  | Read | Write |
|------------------------------------------|:----:|:-----:|
| `BMHD` bitmap header (20 bytes)          |  Y   |   Y   |
| `CMAP` palette (R, G, B triples)         |  Y   |   Y   |
| `CAMG` viewport flags (HAM, EHB)         |  Y   |   Y   |
| `BODY` uncompressed planar               |  Y   |   Y   |
| `BODY` ByteRun1 (PackBits) compression   |  Y   |   Y   |
| `BODY` Auto-picker (RDO, picks shorter)  |  -   |   Y   |
| 1..=8 bitplane indexed colour            |  Y   |   Y   |
| 24-bit literal-RGB true-colour (no CMAP) |  Y   |   Y   |
| EHB — extra-half-brite (32 → 64 entries) |  Y   |   Y   |
| HAM6 (6-plane Hold-And-Modify, 4-bit ch) |  Y   |   Y   |
| HAM8 (8-plane Hold-And-Modify, 6-bit ch) |  Y   |   Y   |
| `Masking::HasMask` plane → alpha         |  Y   |   Y   |
| `Masking::HasTransparentColor` keying    |  Y   |   Y   |
| `GRAB` hotspot (mouse-pointer anchor)    |  Y   |   Y   |
| `DEST` destination-merge (depth/pick/on/mask) |  Y   |   Y   |
| `SPRT` sprite precedence (UWORD, 0=foremost) |  Y   |   Y   |
| `SHAM` Sliced HAM (per-line 16×RGB444)   |  Y   |   Y   |
| `PCHG` palette change list (small fmt)   |  Y   |   Y   |
| `PCHG` palette change list (big fmt)     |  Y   |   N*  |
| `CRNG` DPaint colour-range cycling       |  Y   |   Y   |
| `CCRT` Graphicraft colour-cycling timing |  Y   |   Y   |
| `DRNG` DPaint IV extended range cycling  |  Y   |   Y   |
| `IlbmMuxer` mode select (HAM/EHB/PBM)    |  -   |   Y   |
| Output pixel format                      | RGBA |  -    |

`*` PCHG big-format chunks are decoded but the writer round-trips
the original raw bytes verbatim (no re-encode from the parsed entry
list).

- Public API: [`ilbm::parse_ilbm`], [`ilbm::encode_ilbm`],
  [`ilbm::IlbmImage`], [`ilbm::Bmhd`], [`ilbm::Camg`],
  [`ilbm::Grab`], [`ilbm::Dest`] /
  [`ilbm::Dest::pick_count_matches_depth`],
  [`ilbm::Sprt`] / [`ilbm::Sprt::is_foremost`],
  [`ilbm::Sham`], [`ilbm::Pchg`] /
  [`ilbm::Pchg::palette_at_line`], [`ilbm::Crng`] /
  [`ilbm::Crng::cycle_step`], [`ilbm::Ccrt`] /
  [`ilbm::Ccrt::cycle_step`], [`ilbm::Drng`] / [`ilbm::DrngTrueCell`]
  / [`ilbm::DrngRegCell`] / [`ilbm::Drng::cycle_step`],
  [`ilbm::palette_for_line`],
  [`ilbm::byterun1_decode_row`] / [`ilbm::byterun1_encode_row`],
  [`ilbm::expand_ham_row`], [`ilbm::expand_ehb_palette`],
  [`ilbm::IlbmMuxer`] (with [`ilbm::MuxerMode`] selecting indexed /
  HAM6 / HAM8 / EHB / PBM and [`ilbm::IlbmMuxer::with_masking`]
  selecting `HasMask` / `HasTransparentColor`).
- Container id: `"iff_ilbm"`, probes `FORM....ILBM` (and
  `FORM....PBM `) and matches `.ilbm` / `.lbm` by extension.
  Single-stream `rawvideo` / `Rgba`.
- HAM encode picks the cheapest of (palette-lookup, modify-R,
  modify-G, modify-B) per pixel by squared channel distance against
  the running channel state. EHB encode quantises against a 64-entry
  expanded palette and emits 6 bitplanes regardless of input palette
  length.
- `Compression::Auto` (the muxer default) tries both `None` and
  `ByteRun1` and emits whichever produces fewer bytes; the winning
  mode is recorded in BMHD so the file always self-describes correctly.
  Solid-colour and gradient images typically save >50 % over raw;
  pseudo-random images fall back to uncompressed.
- The `IlbmMuxer` streaming API exposes every encoder mode the
  one-shot `encode_ilbm` supports: pick `MuxerMode::IndexedAuto`
  (default — 1..=8 bitplanes, palette greedy-built from the first
  packet), `MuxerMode::Ham6` / `MuxerMode::Ham8` (CAMG-flagged Hold-
  And-Modify), `MuxerMode::Ehb` (32→64 EHB palette mirror),
  `MuxerMode::Pbm` (chunky `FORM/PBM `), or
  `MuxerMode::TrueColor24` (24-bit literal-RGB ILBM, no CMAP).
- True-colour ILBM follows the EGFF §3.3.4 layout: `BMHD.n_planes == 24`,
  no `CMAP`, 8 red bitplanes (LSB→MSB), then 8 green, then 8 blue per
  scanline. ByteRun1 packs each plane row independently, exactly as in
  the indexed planar path. `Masking::HasMask` is not defined for
  literal-RGB BODY and the decoder rejects it; alpha is always
  `0xFF` on decode and is dropped on encode (24-bit ILBM has no
  transparent-colour key either).
- Cross-validated end-to-end against ImageMagick's `magick convert`
  (delegate `ilbmtoppm` → PPM → pixel-compare). Set
  `OXIDEAV_IFF_MAGICK_CROSS=1` to enable the cross-decode tests; they
  silently skip when the binary or its delegate isn't installed so CI
  stays green on hosts without ImageMagick.

### PBM — DPaint II / Brilliance chunky sibling

`FORM / PBM ` (note the trailing space) shares BMHD / CMAP / CAMG
chunks with ILBM but stores the BODY as a chunky 8-bit-per-pixel byte
stream (no bitplane interleave). Read + write supported with
uncompressed and ByteRun1 BODY; HAM and `HasMask`-plane masking are
not legal in PBM and are rejected on encode/decode.

### ANIM — animated ILBM

Read + round-trip support for `FORM / ANIM` (Aegis Animator / DPaint III):

| Feature                                  | Read | Write |
|------------------------------------------|:----:|:-----:|
| `ANHD` Animation Header (40 bytes)       |  Y   |   Y   |
| Op 0 — full literal BODY                 |  Y   |   Y   |
| Op 1 — XOR ILBM mode (full-frame)        |  Y   |   Y   |
| Op 2 — Long Delta mode                   |  Y   |   Y   |
| Op 3 — Short Delta mode                  |  Y   |   Y   |
| Op 4 — Generalized short/long Delta      |  Y   |   Y   |
| Op 5 — Byte Vertical Delta (DPaint III)  |  Y   |   Y   |
| Op 7 — Short / Long Vertical Delta       |  Y   |   Y   |
| Op 8 — Short / Long Vertical Delta (32b) |  N   |   N   |

- Public API: [`anim::parse_anim`], [`anim::encode_anim_op0`],
  [`anim::encode_anim_op1`], [`anim::encode_op1_body`],
  [`anim::encode_anim_op2`], [`anim::encode_anim_op3`],
  [`anim::encode_op23_body`],
  [`anim::encode_anim_op4`], [`anim::encode_op4_body`],
  [`anim::encode_anim_op5`], [`anim::encode_op5_body`],
  [`anim::encode_anim_op7`], [`anim::encode_op7_body`],
  [`anim::AnimImage`], [`anim::Anhd`].
- Container id: `"iff_anim"`, probes `FORM....ANIM` and matches
  `.anim` by extension. Multi-frame `rawvideo` / `Rgba` stream;
  every frame is emitted as a keyframe packet.
- Op-1 (XOR ILBM mode) is the original ANIM compression method
  (§1.2.1 / §1.3): a delta frame stores the byte-for-byte XOR of the
  new frame against the previous frame's planar bitmap, run-length-
  encoded (ByteRun1 or uncompressed per `BMHD.compression`). The
  decoder expands that BODY and XORs it into the running planar state;
  a zero byte in the XOR bitmap leaves the running byte unchanged
  (§1.3). Only the **full-frame** case (whole bitmap, all colour
  planes) is decoded — the §2.1 "XOR mode only" ANHD `mask` plane-
  selector and `w`/`h`/`x`/`y` rectangular changed-area narrow the BODY
  to a plane subset / sub-rectangle whose wire layout the staged spec
  does not describe, so a plane-masked or partial-rectangle ANHD is
  rejected with `Error::unsupported`. The encoder tags each delta with
  the all-planes `mask` and full-frame `w`/`h` so a round-trip stays in
  the documented case.
- Op-0 (full literal BODY) and op-5 (Byte Vertical Delta) round-trip
  through the public encoder. Op-5 emits the canonical
  pointer-table + per-plane column op-stream: each column's run-
  length encoder picks repeat (3 bytes) for runs ≥ 3 same bytes and
  literal (1 + cnt bytes) otherwise; skip-runs (≤ 0x7F) and
  repeat-runs (≤ 0xFF) split on cap.
- Op-7 (Short / Long Vertical Delta) is decoded into the running
  planar state. The DLTA payload begins with 16 big-endian u32
  pointers — 8 opcode-list pointers followed by 8 data-list pointers,
  one pair per plane (`0` = plane unchanged). Each plane is split
  into vertical columns whose width is the data-item size, controlled
  by `ANHD.bits` bit 0 (`0` = short 2-byte items, `1` = long 4-byte
  items); column count = `row_bytes / data_size`. Per column an
  `op_count` byte introduces a list of opcodes; the three classes are
  Skip (hi bit clear, non-zero — advance dest cursor by N rows; no
  data consumed), Uniq (hi bit set — copy `byte & 0x7F` data items
  literally from the data list, one per consecutive row) and Same
  (`0x00` byte followed by a count byte — copy one data item `count`
  times to consecutive rows). Advancing one row adds `row_bytes` to
  the byte offset within the bitplane (not `data_size`).
- Op-2 (Long Delta) / op-3 (Short Delta) follow the §2.2.1 group
  grammar: an 8-slot plane-pointer table (`0` = plane unchanged),
  then per plane a list of groups — a positive offset short advances
  a word cursor and places one data word, a negative offset short
  (absolute value = offset + 2) advances the cursor and a count
  short introduces that many contiguous data words, `0xFFFF`
  terminates the plane. Data words are big-endian longs in op 2 and
  shorts in op 3; the plane is addressed as its contiguous
  `height × row_bytes` byte image, so op-2 long words may straddle
  row boundaries. After a run the cursor convention is "last written
  word" (the spec prose tracks the pointer at the position the data
  word "would be placed at" and never says a write advances it);
  encoder and decoder share that reading. The encoder collapses runs
  of ≥ 2 changed words into one negative-offset group per §1.2.2 and
  bridges gaps wider than a positive short by rewriting an unchanged
  word in place.
- Op-4 (Generalized short/long Delta) follows the §2.2.2
  `SetDLTAshort` reference routine. The DLTA opens with 16 big-endian
  u32 pointers — 8 data-list pointers then 8 op-list pointers — and,
  crucially, those pointers (and the per-op column offsets) are
  measured in **16-bit words**, not bytes (the routine does `WORD*`
  arithmetic `data = deltaword + deltadata[i]`, `dest = planeptr +
  *ptr`), unlike ops 5/7 whose pointers are byte offsets. Each plane's
  op list is a flat run of `(offset, size)` pairs terminated by
  `0xFFFF`: `offset` is the *absolute* word position of the run's first
  row (each op restarts from its own offset, non-cumulative), `size > 0`
  is a Uniq run of `size` per-row data words, `size < 0` is a Same run
  writing one data word to `|size|` rows; descending a column steps the
  dest by `nw = row_bytes / word_size` words per row. `ANHD.bits`
  selects the variant — bit 0 short/long data, bit 2 separate-vs-shared
  info list (both supported), bit 5 short/long op offsets; the decoder
  rejects the XOR (bit 1) and horizontal (bit 4 clear) variants the
  spec gives no separate wire description for, plus any reserved high
  bit per §2.1 "Player code should check undefined bits … to assure
  they are zero". Op-1 (XOR) and op-8 are open follow-ups.

#### Read an ILBM picture

```rust
let bytes = std::fs::read("picture.ilbm")?;
let img = oxideav_iff::ilbm::parse_ilbm(&bytes)?;
println!("{}x{} → {} bytes RGBA", img.width, img.height, img.rgba.len());
# Ok::<(), Box<dyn std::error::Error>>(())
```

- `CRNG` (DeluxePaint colour-range cycling), `CCRT` (Graphicraft
  colour-cycling timing) and `DRNG` (DeluxePaint IV extended range
  cycling) chunks are parsed and round-tripped byte-stable. Each
  entry exposes accessors for the spec-documented derived quantities
  — `Crng::cycles_per_second()` (rate / 16384 × 60 Hz),
  `Crng::is_active()` / `Crng::is_reverse()`, `Crng::range_len()`;
  `Ccrt::delay_seconds()`, `Ccrt::is_active()` / `Ccrt::is_reverse()`,
  `Ccrt::range_len()`; `Drng::cycles_per_second()`,
  `Drng::is_active()`, `Drng::has_true_cells()` /
  `Drng::has_reg_cells()`, `Drng::range_len()`. `Drng` additionally
  preserves the variable-length cell lists (`DrngTrueCell` —
  `(cell, r, g, b)`, `DrngRegCell` — `(cell, index)`) verbatim and in
  document order. Multiple `CRNG` / `CCRT` / `DRNG` chunks per file
  are preserved in document order so a parse → encode produces the
  same byte stream.
- Each cycling descriptor now exposes a `cycle_step(palette, steps)`
  helper that rotates the in-range slots of a caller-owned palette in
  place: `Crng` and `Ccrt` honour their reverse-direction flag; `Drng`
  rotates forward only (its wire format has no direction flag) and
  leaves the positional `DrngTrueCell` / `DrngRegCell` lists untouched
  for the caller to splice in. `steps` is taken modulo
  `range_len()` so very large accumulated tick counts are O(range) to
  apply. Inactive cycles, malformed ranges, ranges past the palette
  tail and zero-net-step rotations are all silent no-ops returning
  `false`. `Pchg::palette_at_line(base, y)` (and the free
  `palette_for_line(image, y)` wrapper that handles the `Option<Pchg>`)
  fold every PCHG override whose `line <= y` over a starting palette,
  so animation viewers can compose per-scanline state + per-tick
  rotation without re-implementing the bookkeeping.

### AIFF / AIFF-C marker chunks

`MARK` chunks are parsed into a structured
[`aiff::MarkerChunk`] surface exposed via
[`aiff::Form::markers`]:

| Field            | On-wire                | API                                  |
|------------------|------------------------|--------------------------------------|
| numMarkers       | big-endian `u16`       | `markers.len()`                      |
| `Marker.id`      | big-endian `i16` (>0)  | `Marker::id`                         |
| `Marker.position`| big-endian `u32` frame | `Marker::position`                   |
| `Marker.name`    | pstring (len+chars+pad)| `Marker::name` (UTF-8 lossy)         |

The parser enforces every constraint AIFF-C §6.0 imposes:

- At most one `MARK` chunk per FORM ([`AiffError::DuplicateChunk("MARK")`]).
- Every `MarkerId` strictly positive ([`AiffError::InvalidValue`]).
- All `MarkerId`s unique inside the chunk ([`AiffError::DuplicateMarkerId`]).
- pstring `length + chars` rounded up to an even total; missing pad
  at end-of-chunk is tolerated (mirrors the chunk walker's existing
  EOF tolerance for the outer ckSize pad byte).

Markers are preserved in document order; spec is explicit that
"markers need not be ordered in any particular manner" so we don't
re-sort. `MarkerChunk::by_id(id)` is a convenience lookup that
[`InstrumentChunk::resolve_sustain_loop`] (below) uses internally to
join the sampler loop endpoints back against this list.

### AIFF / AIFF-C instrument chunk

`INST` chunks are parsed into a structured
[`aiff::InstrumentChunk`] surface exposed via
[`aiff::Form::instrument`]:

| Field                     | On-wire                    | API                              |
|---------------------------|----------------------------|----------------------------------|
| baseNote                  | `char` (MIDI 0..=127)      | `InstrumentChunk::base_note`     |
| detune                    | `char` signed (cents -50..+50) | `InstrumentChunk::detune`    |
| lowNote / highNote        | `char` (MIDI 0..=127)      | `low_note` / `high_note`         |
| lowVelocity / highVelocity| `char` (1..=127)           | `low_velocity` / `high_velocity` |
| gain                      | big-endian `i16` (dB)      | `InstrumentChunk::gain`          |
| sustainLoop / releaseLoop | 6-byte `Loop`              | `sustain_loop` / `release_loop`  |

Each `Loop` exposes the decoded [`aiff::PlayMode`]
(`NoLooping` / `ForwardLooping` / `ForwardBackwardLooping`) and the
two `MarkerId`s referencing the FORM's MARK chunk.

The parser enforces every constraint AIFF-C §9 imposes:

- At most one `INST` chunk per FORM ([`AiffError::DuplicateChunk("INST")`]).
- Exact 20-byte ckDataSize ("ckDataSize is always 20" — shorter is
  [`AiffError::Truncated`], longer is
  [`AiffError::InvalidValue { what: "INST ckSize" }`]).
- MIDI note range `0..=127` on baseNote / lowNote / highNote
  ([`AiffError::InvalidValue`]).
- detune cent range `-50..=+50` ([`AiffError::InvalidValue`]).
- Velocity range `1..=127` on lowVelocity / highVelocity
  ("1 [lowest velocity] through 127 [highest velocity]").
- playMode in `0..=2`.

[`InstrumentChunk::resolve_sustain_loop`] /
[`InstrumentChunk::resolve_release_loop`] join a loop's `MarkerId`
endpoints against the FORM's [`aiff::MarkerChunk`] and apply §9 ¶
"beginLoop and endLoop": "The begin position must be less than the
end position so the loop segment will have a positive length. [If
this is not the case, then ignore this loop segment. No looping
takes place.]" Returns `None` whenever `playMode == None`, an
endpoint id isn't a positive marker id, either id isn't present
in the supplied MARK list, or the begin marker's frame position
isn't strictly less than the end marker's — letting the caller
ask "what does the spec say to actually play?" without
re-implementing the bookkeeping.

### AIFF / AIFF-C text chunks

`NAME`, `AUTH`, `(c) `, and `ANNO` are the four §13.0 text chunks.
They share an identical wire layout — a four-byte ckID, a four-byte
big-endian `ckSize`, and a flat run of bytes whose length is the
`ckSize` value — and the parser surfaces them through structured
[`aiff::TextChunk`] entries on the [`aiff::Form`] tree:

| ckID    | Field                | Cardinality           | Surface                       |
|---------|----------------------|-----------------------|-------------------------------|
| `NAME`  | name of the sound    | at most one per FORM  | `Form::name: Option<TextChunk>` |
| `AUTH`  | author name(s)       | at most one per FORM  | `Form::author: Option<TextChunk>` |
| `(c) `  | copyright notice     | at most one per FORM  | `Form::copyright: Option<TextChunk>` |
| `ANNO`  | free-form annotation | any number per FORM   | `Form::annotations: Vec<TextChunk>` |

A duplicate `NAME` / `AUTH` / `(c) ` raises
[`AiffError::DuplicateChunk`]; multiple `ANNO` chunks are
accumulated in document order, mirroring how the §10.0 MIDI and
§12.0 APPL chunks handle the "any-number-per-FORM" rule.

The text body itself is preserved byte-for-byte — §13.0 ¶ "text
contains pure ASCII characters. It is neither a pstring nor a C
string. The number of characters in text is determined by
ckDataSize" — so no trailing-NUL trimming or pstring-length read
happens here. [`TextChunk::as_str`] returns a borrowed `&str` for
valid UTF-8 bodies and [`TextChunk::as_string_lossy`] decodes the
full body with `U+FFFD` substitution so MacRoman / Latin-1 bodies
produced by older encoders are still salvageable. Empty text
bodies (`ckDataSize == 0`) are accepted — §13.0 places no minimum
on the text field. The matching write-side helper
[`aiff::write_text_chunk`] emits the raw text bytes; the chunk
header and any odd-length pad byte are the caller's responsibility
(matching every other AIFF write-side helper).

The on-wire ckID for the Copyright chunk is the four ASCII bytes
`0x28 0x63 0x29 0x20`, i.e. `(`, lowercase `c`, `)`, space — per
§13.0 ¶ "the 'c' is lowercase and there is a space [0x20] after
the close parenthesis." The spec uses the round-bracket character
itself as the on-wire stand-in for ©; downstream code that wants
the © glyph should decode the text body, not the ckID.

### AIFF / AIFF-C SAXL (Sound Accelerator) chunks

`SAXL` chunks are parsed into a structured
[`aiff::SaxelChunk`] surface exposed via [`aiff::Form::saxels`]:

| Field            | On-wire                | API                                  |
|------------------|------------------------|--------------------------------------|
| numSaxels        | big-endian `u16`       | `saxels[i].saxels.len()`             |
| `Saxel.id`       | big-endian `i16`       | `Saxel::id`                          |
| `Saxel.size`     | big-endian `u16`       | `Saxel::len()`                       |
| `Saxel.data`     | byte[size] verbatim    | `Saxel::data`                        |

§8.0 / Appendix D permits "any number of Saxel Chunks" per FORM AIFC
(unlike `MARK` / `INST` / `COMT` / `AESD` which are at-most-one) so
the FORM walker accumulates them in document order via
[`aiff::Form::saxels`]. Within a chunk the saxels themselves are
also preserved in document order — Appendix D ¶ "The saxels need
not be ordered in any particular manner" so we don't re-sort.

[`aiff::Saxel::resolve_marker`] joins a saxel's `id` against the
FORM's [`aiff::MarkerChunk`] per §8.0 ¶ "id identifies the marker
for which the sound accelerator data is to be used"; it returns
`None` when the id isn't a positive `MarkerId` per §6.0 or when no
marker with that id exists in the supplied chunk.
[`aiff::SaxelChunk::by_marker_id`] is a convenience reverse-lookup
that scans the chunk's saxel list for a matching `id`.

The `data` payload is preserved byte-for-byte — Appendix D ¶
"saxelData contains the specific sound accelerator data which is
compression-type specific" — so the parser does NOT interpret it
against any particular algorithm. §8.0 ¶ "Under Construction" /
Appendix D ¶ "Caution" emphasise the mechanism remained a
"rough proposal" in the 1991 draft, so callers wiring an actual
decompressor's state-priming entry point own the algorithm-specific
decode (the ACE2 / ACE8 / MAC3 / MAC6 "previous 48 sample frames"
convention Appendix D describes lives in the codec crate, not
here).

The matching write-side helper [`aiff::write_saxel_chunk`] emits
the body bytes; the chunk header (`'SAXL' + ckSize`) and any
odd-length outer pad byte are the caller's responsibility,
matching every other AIFF write-side helper in this module.

### AIFF-C §14 chunk precedence

§14 of the AIFF-C spec ranks every chunk class the spec defines so
that callers can resolve overlapping information cleanly — the §14
worked example is loop endpoints carried both by the Instrument
Chunk and by MIDI System-Exclusive bytes inside a MIDI Data Chunk:

| Precedence              | Class                | ckID     |
|-------------------------|----------------------|----------|
| §3.1 sentinel           | `FormatVersion`      | `FVER`   |
| Highest                 | `Common`             | `COMM`   |
|                         | `Instrument`         | `INST`   |
|                         | `Saxel`              | `SAXL`   |
|                         | `Comments`           | `COMT`   |
|                         | `Marker`             | `MARK`   |
|                         | `SoundData`          | `SSND`   |
|                         | `Name`               | `NAME`   |
|                         | `Author`             | `AUTH`   |
|                         | `Copyright`          | `(c) `   |
| §14 ¶ document order    | `Annotation`         | `ANNO`   |
|                         | `AudioRecording`     | `AESD`   |
|                         | `MidiData`           | `MIDI`   |
| Lowest                  | `ApplicationSpecific`| `APPL`   |

The surface is the [`aiff::ChunkClass`] enum (`#[repr(u8)]`, where
the integer value is the precedence rank — lower = higher
precedence) plus three helpers:

- [`aiff::ChunkClass::rank`] returns the rank as a `u8`.
- [`aiff::ChunkClass::higher_precedence_than`] is the §14 ¶ "the
  loop points in the Instrument Chunk take precedence over
  conflicting loop points found in the MIDI Data Chunk" predicate.
- [`aiff::ChunkClass::ck_id`] returns the on-wire 4-byte ckID — the
  §13.0 ¶ "the 'c' is lowercase and there is a space [0x20] after
  the close parenthesis" Copyright tag is exactly `b"(c) "`.
- [`aiff::ChunkClass::all_in_precedence_order`] enumerates the
  fourteen-entry table for callers iterating by rank.

The matching [`aiff::Form::precedence_order`] and
[`aiff::Form::highest_precedence_class`] helpers walk a parsed
`Form` and emit a `Vec<ChunkClass>` of the classes the FORM
actually contains in §14 order. The §4 layout-doc ¶ "chunk order
inside a FORM is unspecified" rule means the on-wire chunk
sequence is irrelevant: precedence_order always reports the §14
ordering. Multi-instance classes (§8.0 `SAXL`, §10.0 `MIDI`,
§12.0 `APPL`, §13.0 `ANNO`) appear once per instance and preserve
the document-order semantics §14 ¶ "Annotation Chunk[s] -- in the
order they appear in the FORM" requires.

## Roadmap

The chunk walker (`chunk.rs`) is format-agnostic; SMUS (music score)
and MAUD are natural follow-ons that reuse the same FORM/LIST/CAT
reader. ANIM op-7 (short / long vertical delta) decode landed in
r192; r209 ships the matching **op-7 encoder** (short + long data
variants) and extends the container walker to accept the `DLTA`
chunk id alongside `BODY` so all delta streams decode through the
same path. r276 lands **op-2 / op-3 (Long / Short Delta mode)**
decode + encode from the spec's §1.2.2 / §1.2.3 / §2.2.1 — see the
ANIM section above. r287 lands **op-4 (Generalized short/long
Delta mode)** decode + encode from §1.2.4 / §2.2.2 (the
`SetDLTAshort` reference routine — word-unit pointers, absolute
`(offset, size)` op pairs, Same/Uniq run classes, `ANHD.bits`
variant selection). r302 lands **op-1 (XOR ILBM mode)** decode +
encode for the full-frame case from §1.2.1 / §1.3 / §2.1 — see the
ANIM section above. Remaining ANIM gaps: the op-1 partial-rectangle
/ plane-masked variant (§2.1 `mask` / `w` / `h` / `x` / `y` "XOR mode
only" fields) needs a staged wire description of how the partial BODY
is laid out before it can be decoded; op-8 and DEEP / TVPP / RGB8 /
RGBN true-colour IFF chunks remain blocked on docs — none has a spec
section staged in `docs/image/iff/` (the only ANIM appendix present
covers op-7, and op-8 is name-dropped without a wire format).

AIFF-side r209 surfaces the previously-skipped optional chunks
end-to-end: **COMT** (timestamped comments, optional `MarkerId`
linkage), **AESD** (24-byte AES channel-status block with the
recording-emphasis field exposed), and **APPL** (application-
specific, `pdos` / `stoc` / Macintosh dialect classification).
r215 adds **MIDI** (§10.0 MIDI Data) chunk surfacing — multiple
chunks per FORM in document order, raw byte stream preserved
verbatim with `is_sysex` / `len` / `is_empty` classifiers plus a
write-side helper; the SMF event-level decode lives in the
`oxideav-midi` sibling crate. r220 surfaces the §13.0 **text
chunks** (`NAME` / `AUTH` / `(c) ` / `ANNO`) — see the dedicated
section above. r227 surfaces **SAXL** (§8.0 / Appendix D Sound
Accelerator) chunks — `SaxelChunk` → `Vec<Saxel>` with each entry
linking a `MarkerId` to a compression-type-specific priming-data
byte stream, multiple chunks per FORM in document order, plus a
write-side helper and a `Saxel::resolve_marker` / `SaxelChunk::by_marker_id`
lookup pair. The §8.0 ¶ "Under Construction" / Appendix D ¶
"Caution" status of the mechanism means the parser preserves
`data` verbatim rather than interpreting it against any specific
compression algorithm. Write-side encoders for **MARK**, **INST**,
**COMT**, **AESD**, **APPL**, **MIDI**, **SAXL**, and the §13.0
text chunks complete the round-trip story; encoders building a
FORM AIFF / AIFC can now emit every chunk class the read path
surfaces. The remaining AIFF-C §x.x surfaces are saturated — Apple
shipped 13 chunk classes (FVER, COMM, SSND, MARK, INST, COMT,
AESD, APPL, MIDI, SAXL, NAME, AUTH, (c)  + ANNO) and this crate
now reads + writes every one of them. r243 surfaces the §14
**chunk-precedence** rules through the [`aiff::ChunkClass`]
ranked enum and the [`aiff::Form::precedence_order`] /
[`aiff::Form::highest_precedence_class`] helpers — see the
"AIFF-C §14 chunk precedence" section above for the ranked
table and the §14 worked example mapping.

## Fuzzing

A [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) harness
lives in [`fuzz/`](fuzz/) with three libFuzzer targets covering the
highest-risk parser surface of the crate:

* `aiff_decode` — feeds arbitrary bytes to
  `aiff::demuxer::AiffDemuxer::from_bytes`, the top-of-stack entry
  point that walks the entire Apple FORM AIFF / FORM AIFC chunk
  tree (FORM header + COMM common + SSND sound data + optional
  MARK / INST / COMT / AESD / APPL / MIDI / SAXL / NAME / AUTH /
  `(c) ` / ANNO metadata). The classic overflow spots are the
  32-bit chunk-size field, the 80-bit IEEE-extended sample-rate
  decode, and the per-chunk pad-byte arithmetic — this target
  keeps them honest.
* `anim_decode` — feeds arbitrary bytes to `anim::parse_anim`, the
  FORM ANIM walker that loads a first FORM ILBM frame and then
  applies subsequent ANHD + DLTA delta frames using one of three
  vertical-delta operations (op-0 literal, op-5 byte-vertical-RLC,
  op-7 short / long vertical delta). Each delta decoder has its
  own per-frame BODY/DLTA size arithmetic and its own
  failure-mode surface.
* `pchg_parse` — feeds arbitrary bytes to `ilbm::Pchg::parse`, the
  PCHG (Palette CHanGes per scan-line) chunk decoder from the
  Vigna 1994 IFF Annex. PCHG is the most failure-mode-dense
  single chunk class the crate parses: a 20-byte tabular header,
  an optional ByteRun1 / SmallLineChanges compression mode, a
  per-line change mask, and small-or-big change-record variants
  that drive cumulative-state palette reconstruction.

The contract under test for every target is purely that the call
*returns*: a malformed input yields `Err(oxideav_core::Error::…)`,
a well-formed one yields `Ok(_)`, and neither path may panic,
integer-overflow (in a debug build), index out of bounds, or
allocate an attacker-controlled buffer larger than the input
actually supports.

To run a target:

```sh
cargo install cargo-fuzz
cd crates/oxideav-iff
cargo +nightly fuzz run aiff_decode
# or anim_decode / pchg_parse
```

The harness builds under nightly Rust (libFuzzer needs nightly's
`-Z` flags); see the `cargo-fuzz` book for corpus management,
artifact triage, and coverage-guided minimisation.

## License

MIT - see [LICENSE](LICENSE).
