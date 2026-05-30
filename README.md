# oxideav-iff

Pure-Rust EA IFF 85 container support for oxideav — the chunk reader
that underlies the entire `FORM / LIST / CAT` family. Today this
crate ships:

- **FORM/8SVX** — full read/write (Amiga 8-bit sampled voice).
- **FORM/ILBM** — read+round-trip (1..=8 indexed bitplanes **and
  24-bit literal-RGB true-colour**, ByteRun1 / Auto compression,
  EHB, HAM6, HAM8, HasMask, transparent-colour keying, GRAB hotspot,
  SHAM per-line palette, PCHG small-format palette change list,
  CRNG / CCRT / DRNG colour-cycling descriptors).
- **FORM/PBM** — read+round-trip (DPaint II / Brilliance chunky sibling).
- **FORM/ANIM** — op-0 literal + op-5 byte-vertical delta
  (encode+decode) + op-7 Short/Long Vertical Delta (decode).
- **FORM/AIFF and FORM/AIFC** — Apple AIFF / AIFF-C (read):
  COMM/SSND/FVER walker, 80-bit IEEE-extended sample-rate decode,
  PCM compression-flavour readers for `NONE` / `twos` / `sowt` /
  `raw ` / `fl32` / `FL32` / `fl64` / `FL64`. Codec-bearing
  `compressionType` FourCCs (`ima4`, `ulaw`, `alaw`, …) are
  recognised in the parser but routed through sibling codec
  crates rather than decoded here.

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
  [`ilbm::Grab`], [`ilbm::Sham`], [`ilbm::Pchg`] /
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
| Op 5 — Byte Vertical Delta (DPaint III)  |  Y   |   Y   |
| Op 7 — Short / Long Vertical Delta       |  Y   |   N   |
| Op 8 — Short / Long Vertical Delta (32b) |  N   |   N   |

- Public API: [`anim::parse_anim`], [`anim::encode_anim_op0`],
  [`anim::encode_anim_op5`], [`anim::encode_op5_body`],
  [`anim::AnimImage`], [`anim::Anhd`].
- Container id: `"iff_anim"`, probes `FORM....ANIM` and matches
  `.anim` by extension. Multi-frame `rawvideo` / `Rgba` stream;
  every frame is emitted as a keyframe packet.
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
  the byte offset within the bitplane (not `data_size`). Op-7 encode
  + op-8 are open follow-ups.

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

## Roadmap

The chunk walker (`chunk.rs`) is format-agnostic; AIFF (Apple audio),
SMUS (music score) and MAUD are natural follow-ons that reuse the
same FORM/LIST/CAT reader. ANIM op-7 (short / long vertical delta)
decode landed in r192; op-7 encode + op-8 decode/encode remain open
ILBM-side extensions; DEEP / TVPP / RGB8 / RGBN true-colour IFF
chunks are the next ILBM-side decode candidates.

## License

MIT - see [LICENSE](LICENSE).
