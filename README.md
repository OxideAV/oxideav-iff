# oxideav-iff

Pure-Rust EA IFF 85 container support for oxideav — the chunk reader
that underlies 8SVX (Amiga 8-bit sampled voice), ILBM (Amiga
InterLeaved BitMap pictures), PBM (DPaint II / Brilliance chunky
sibling), ANIM (animated ILBM), AIFF, SMUS, and friends. Today this
crate ships a full read/write implementation of FORM/8SVX, a
read-and-round-trip implementation of FORM/ILBM and FORM/PBM (1..=8
bitplanes, ByteRun1 / Auto compression, EHB, HAM6, HAM8, HasMask,
transparent-colour keying, GRAB hotspot, SHAM per-line palette, PCHG
small-format palette change list), and a read-only FORM/ANIM
implementation (op-0 literal + op-5 byte-vertical delta). The shared
chunk walker is reusable as AIFF / SMUS support is added. Zero C
dependencies.

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

- The exposed codec id is `pcm_s8`; Fibonacci-delta compression is
  transparent — decoded on demux, encoded on mux when the caller picks
  `Compression::Fibonacci`.
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
| EHB — extra-half-brite (32 → 64 entries) |  Y   |   Y   |
| HAM6 (6-plane Hold-And-Modify, 4-bit ch) |  Y   |   Y   |
| HAM8 (8-plane Hold-And-Modify, 6-bit ch) |  Y   |   Y   |
| `Masking::HasMask` plane → alpha         |  Y   |   Y   |
| `Masking::HasTransparentColor` keying    |  Y   |   Y   |
| `GRAB` hotspot (mouse-pointer anchor)    |  Y   |   Y   |
| `SHAM` Sliced HAM (per-line 16×RGB444)   |  Y   |   Y   |
| `PCHG` palette change list (small fmt)   |  Y   |   Y   |
| `PCHG` palette change list (big fmt)     |  Y   |   N*  |
| Output pixel format                      | RGBA |  -    |

`*` PCHG big-format chunks are decoded but the writer round-trips
the original raw bytes verbatim (no re-encode from the parsed entry
list).

- Public API: [`ilbm::parse_ilbm`], [`ilbm::encode_ilbm`],
  [`ilbm::IlbmImage`], [`ilbm::Bmhd`], [`ilbm::Camg`],
  [`ilbm::Grab`], [`ilbm::Sham`], [`ilbm::Pchg`],
  [`ilbm::byterun1_decode_row`] / [`ilbm::byterun1_encode_row`],
  [`ilbm::expand_ham_row`], [`ilbm::expand_ehb_palette`].
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

### PBM — DPaint II / Brilliance chunky sibling

`FORM / PBM ` (note the trailing space) shares BMHD / CMAP / CAMG
chunks with ILBM but stores the BODY as a chunky 8-bit-per-pixel byte
stream (no bitplane interleave). Read + write supported with
uncompressed and ByteRun1 BODY; HAM and `HasMask`-plane masking are
not legal in PBM and are rejected on encode/decode.

### ANIM — animated ILBM

Read-only support for `FORM / ANIM` (Aegis Animator / DPaint III):

| Feature                                  | Read | Write |
|------------------------------------------|:----:|:-----:|
| `ANHD` Animation Header (40 bytes)       |  Y   |   Y   |
| Op 0 — full literal BODY                 |  Y   |   Y   |
| Op 5 — Byte Vertical Delta (DPaint III)  |  Y   |   N   |
| Op 7 / 8 — short / long vertical deltas  |  N   |   N   |

- Public API: [`anim::parse_anim`], [`anim::encode_anim_op0`],
  [`anim::AnimImage`], [`anim::Anhd`].
- Container id: `"iff_anim"`, probes `FORM....ANIM` and matches
  `.anim` by extension. Multi-frame `rawvideo` / `Rgba` stream;
  every frame is emitted as a keyframe packet.
- The op-0 muxer is used by the test suite to round-trip multi-frame
  ANIM through the public encoder; production-quality op-5 encode is
  not yet implemented.

#### Read an ILBM picture

```rust
let bytes = std::fs::read("picture.ilbm")?;
let img = oxideav_iff::ilbm::parse_ilbm(&bytes)?;
println!("{}x{} → {} bytes RGBA", img.width, img.height, img.rgba.len());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Roadmap

The chunk walker (`chunk.rs`) is format-agnostic; AIFF (Apple audio),
SMUS (music score) and MAUD are natural follow-ons that reuse the
same FORM/LIST/CAT reader. ANIM op-5 encode and ANIM op-7/op-8
decode are open ILBM-side extensions, as are CRNG / CCRT colour-
cycling chunks.

## License

MIT - see [LICENSE](LICENSE).
