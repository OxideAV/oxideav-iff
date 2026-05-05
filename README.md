# oxideav-iff

Pure-Rust EA IFF 85 container support for oxideav — the chunk reader
that underlies 8SVX (Amiga 8-bit sampled voice), ILBM (Amiga
InterLeaved BitMap pictures), AIFF, SMUS, and friends. Today this
crate ships a full read/write implementation of FORM/8SVX plus a
read-and-roundtrip implementation of FORM/ILBM (1..=8 bitplanes,
ByteRun1 compression, EHB, HAM6, HAM8, HasMask, transparent-colour
keying). The shared chunk walker is reusable as AIFF / SMUS / ANIM
support is added. Zero C dependencies.

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
| 1..=8 bitplane indexed colour            |  Y   |   Y   |
| EHB — extra-half-brite (32 → 64 entries) |  Y   |   N   |
| HAM6 (6-plane Hold-And-Modify, 4-bit ch) |  Y   |   N   |
| HAM8 (8-plane Hold-And-Modify, 6-bit ch) |  Y   |   N   |
| `Masking::HasMask` plane → alpha         |  Y   |   Y   |
| `Masking::HasTransparentColor` keying    |  Y   |   N   |
| Output pixel format                      | RGBA |  -    |

- Public API: [`ilbm::parse_ilbm`], [`ilbm::encode_ilbm`],
  [`ilbm::IlbmImage`], [`ilbm::Bmhd`], [`ilbm::Camg`],
  [`ilbm::byterun1_decode_row`] / [`ilbm::byterun1_encode_row`],
  [`ilbm::expand_ham_row`], [`ilbm::expand_ehb_palette`].
- Container id: `"iff_ilbm"`, probes `FORM....ILBM` and matches
  `.ilbm` / `.lbm` by extension. Single-stream `rawvideo` / `Rgba`.
- Round 1 omits HAM / EHB encode (the writer emits indexed colour
  through up to 8 bitplanes regardless of CAMG flags) and the
  `CRNG` / `CCRT` colour-cycling chunks.

#### Read an ILBM picture

```rust
let bytes = std::fs::read("picture.ilbm")?;
let img = oxideav_iff::ilbm::parse_ilbm(&bytes)?;
println!("{}x{} → {} bytes RGBA", img.width, img.height, img.rgba.len());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Roadmap

The chunk walker (`chunk.rs`) is format-agnostic; ANIM (animated
ILBM), AIFF (Apple audio), SMUS (music score) and MAUD are natural
follow-ons that reuse the same FORM/LIST/CAT reader. PBM (an 8-bit
chunky sibling of ILBM under the same outer envelope), the GRAB chunk
(hotspot) and SHAM / PCHG (per-scanline palette changes) are also
natural ILBM-side extensions.

## License

MIT - see [LICENSE](LICENSE).
