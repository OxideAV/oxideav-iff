# oxideav-iff

Pure-Rust EA IFF 85 container support (8SVX audio, ILBM, …) for oxideav

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a
100% pure Rust media transcoding and streaming stack. No C libraries, no FFI
wrappers, no `*-sys` crates.

## Supported formats

### 8SVX — Amiga 8-bit Sampled Voice

Full read/write support for the Amiga `FORM / 8SVX` voice format:

| Feature                                 | Read | Write |
|-----------------------------------------|:----:|:-----:|
| `VHDR` voice header                     |  Y   |   Y   |
| Raw PCM (`sCompression = 0`)            |  Y   |   Y   |
| Fibonacci-delta (`sCompression = 1`)    |  Y   |   Y   |
| Mono (`CHAN = 2` or no CHAN chunk)      |  Y   |   Y   |
| Stereo (`CHAN = 6`, concatenated halves)|  Y   |   Y   |
| `NAME` / `AUTH` / `ANNO` / `CHRS` tags  |  Y   |   Y   |

- The exposed codec id is `pcm_s8`; Fibonacci-delta compression is transparent
  (decoded on demux, encoded on mux when `Compression::Fibonacci` is selected).
- Stereo BODY layout is the common AmigaOS convention: LEFT channel in full,
  followed by RIGHT channel in full. For Fibonacci-compressed stereo each half
  carries its own `[pad, initial_sample, nibbles…]` header.
- Fibonacci-delta table used: `[-34, -21, -13, -8, -5, -3, -2, -1, 0, 1, 2, 3,
  5, 8, 13, 21]` (16 entries, from the AmigaOS wiki / ROM Kernel Manual).
- Fibonacci-delta is lossy: round-trips reconstruct each sample within ±2 LSBs
  on smooth signals.

Example — writing a stereo Fibonacci-delta voice:

```rust
use oxideav_iff::svx::{Compression, SvxMuxer};
// `stream` describes 2-channel pcm_s8; `packet.data` is interleaved L R L R…
let mut mux = SvxMuxer::new(out, &[stream])?.with_compression(Compression::Fibonacci);
mux.write_header()?;
mux.write_packet(&packet)?;
mux.write_trailer()?;
```

## Usage

```toml
[dependencies]
oxideav-iff = "0.0"
```

## License

MIT — see [LICENSE](LICENSE).
