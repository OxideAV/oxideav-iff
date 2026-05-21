//! FORM ILBM — Amiga InterLeaved BitMap (Jerry Morrison, 1986).
//!
//! Layout: outer EA IFF 85 group chunk → 4-byte `ILBM` form type →
//! children:
//!
//! - **`BMHD`** (BitMap HeaDer, 20 bytes): width / height / origin
//!   plus the bitplane count, masking mode, compression mode, the
//!   transparent colour index, and a hint at the source aspect /
//!   page size.
//! - **`CMAP`** (ColourMAP): a packed array of 3-byte RGB triples —
//!   one per palette index. The size of the chunk divided by three
//!   gives the entry count (typical: 2, 4, 8, 16, 32, 64 or 256).
//! - **`CAMG`** (Commodore Amiga Graphics, 4 bytes): viewport flags.
//!   We pick out only two bits worth caring about for round 1:
//!   * `0x80` — "extra-half-brite" (EHB). 32-entry palette is
//!     mirrored to a 64-entry palette where `pal[i+32] = pal[i] / 2`.
//!   * `0x800` — Hold-And-Modify (HAM). The plane count picks the
//!     mode (HAM6 = 6 bitplanes / 4 channel bits, HAM8 = 8 bitplanes
//!     / 6 channel bits). The top 2 bits of each pixel are the
//!     control op (see [`expand_ham_row`]).
//! - **`BODY`** (image data): rows of bitplane data laid out
//!   plane-by-plane, then row-by-row. Each plane's row is
//!   `((width + 15) / 16) * 2` bytes wide (rounded up to a
//!   16-bit word boundary). If the masking byte in `BMHD` is `1`
//!   (HasMask) an extra "mask plane" of the same per-row width
//!   follows the colour planes within each row. If `BMHD.compression
//!   == 1` each plane-row is RLE-compressed independently using the
//!   ByteRun1 algorithm (the same encoding TIFF calls "PackBits"):
//!   * `n` in `0..=127` — copy the next `n + 1` bytes literally.
//!   * `n == 128` — NOP / skip (some encoders emit this; we tolerate it).
//!   * `n` in `129..=255` — repeat the next byte `257 - n` times.
//!
//! **Pixel reassembly.** Bitplanes hold the colour-index bits in
//! plane order (plane 0 = LSB, plane `n-1` = MSB). Per pixel we walk
//! the planes from `n-1` down to `0`, OR-ing each plane's bit into
//! a u8 accumulator. The accumulated byte is the palette index
//! (or, for HAM, the control + channel value).
//!
//! Source: the public **EA IFF 85** standard (Electronic Arts, 1985)
//! and Jerry Morrison's **ILBM IFF Interleaved Bitmap** form spec
//! (1986). No third-party loader code was consulted.

// Bitplane / mask / scanline loops use the index to address parallel
// row-vec arrays (n bitplanes + optional mask plane × `row_bytes`).
// Iterators would require zip()s plus per-step state; the explicit
// index form keeps the spec correspondence obvious.
#![allow(clippy::needless_range_loop)]

use std::io::{Read, Seek, SeekFrom, Write};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType, Packet,
    PixelFormat, Result, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_core::{Muxer, ReadSeek};

use crate::chunk::{read_chunk_header, read_form_type, skip_chunk_body, ChunkHeader, GROUP_FORM};

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("iff_ilbm", open);
    reg.register_muxer("iff_ilbm", open_muxer);
    reg.register_extension("ilbm", "iff_ilbm");
    reg.register_extension("lbm", "iff_ilbm");
    reg.register_probe("iff_ilbm", probe);
}

fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" {
        match &p.buf[8..12] {
            b"ILBM" | b"PBM " => 100,
            _ => 0,
        }
    } else {
        0
    }
}

// ───────────────────── BMHD ─────────────────────

/// `BMHD.masking` — how the BODY's colour-index bitplanes carry an
/// alpha / cookie-cutter signal alongside the colour data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Masking {
    /// No mask data. BODY is `n_planes` rows of colour-index planes per row.
    None,
    /// Each row of BODY carries an extra plane (after the `n_planes`
    /// colour planes) holding 1 bit per pixel — set bits are opaque.
    HasMask,
    /// Pixels equal to `BMHD.transparent_colour` are transparent.
    HasTransparentColor,
    /// Lasso (an Amiga editor tool); we tolerate the value but treat
    /// the image as opaque.
    Lasso,
}

impl Masking {
    fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::None,
            1 => Self::HasMask,
            2 => Self::HasTransparentColor,
            3 => Self::Lasso,
            other => {
                return Err(Error::invalid(format!(
                    "ILBM BMHD: invalid masking byte {other} (expected 0..=3)"
                )))
            }
        })
    }

    fn to_byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::HasMask => 1,
            Self::HasTransparentColor => 2,
            Self::Lasso => 3,
        }
    }
}

/// `BMHD.compression` — `0` = uncompressed, `1` = ByteRun1 (PackBits).
///
/// The encoder-only [`Compression::Auto`] variant is not a valid BMHD byte;
/// it instructs [`encode_ilbm`] to try both modes and emit the shorter
/// output, writing the winning mode into BMHD before assembly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Compression {
    /// BODY is a literal stack of bitplane rows.
    None,
    /// Each plane-row is ByteRun1 (PackBits) compressed independently.
    /// Decoder side: see [`byterun1_decode_row`].
    #[default]
    ByteRun1,
    /// Encoder-only: try both `None` and `ByteRun1` and emit whichever
    /// produces fewer bytes. The winning mode is written into the BMHD
    /// `compression` field in the output.
    Auto,
}

impl Compression {
    fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::None,
            1 => Self::ByteRun1,
            other => {
                return Err(Error::unsupported(format!(
                    "ILBM BMHD: compression {other} not supported (0=none, 1=ByteRun1)"
                )))
            }
        })
    }

    fn to_byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::ByteRun1 => 1,
            // Auto is resolved before writing; should never reach here.
            Self::Auto => 1,
        }
    }
}

/// 20-byte BMHD chunk parsed into a strongly-typed view.
#[derive(Clone, Copy, Debug)]
pub struct Bmhd {
    pub width: u16,
    pub height: u16,
    pub x_origin: i16,
    pub y_origin: i16,
    pub n_planes: u8,
    pub masking: Masking,
    pub compression: Compression,
    /// Padding byte in the on-disk struct — we keep it round-trippable
    /// even though the spec says it must be zero.
    pub pad: u8,
    pub transparent_color: u16,
    pub x_aspect: u8,
    pub y_aspect: u8,
    pub page_width: i16,
    pub page_height: i16,
}

impl Bmhd {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 20 {
            return Err(Error::invalid(format!(
                "ILBM BMHD: need 20 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            width: u16::from_be_bytes([body[0], body[1]]),
            height: u16::from_be_bytes([body[2], body[3]]),
            x_origin: i16::from_be_bytes([body[4], body[5]]),
            y_origin: i16::from_be_bytes([body[6], body[7]]),
            n_planes: body[8],
            masking: Masking::from_byte(body[9])?,
            compression: Compression::from_byte(body[10])?,
            pad: body[11],
            transparent_color: u16::from_be_bytes([body[12], body[13]]),
            x_aspect: body[14],
            y_aspect: body[15],
            page_width: i16::from_be_bytes([body[16], body[17]]),
            page_height: i16::from_be_bytes([body[18], body[19]]),
        })
    }

    pub fn write(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[0..2].copy_from_slice(&self.width.to_be_bytes());
        out[2..4].copy_from_slice(&self.height.to_be_bytes());
        out[4..6].copy_from_slice(&self.x_origin.to_be_bytes());
        out[6..8].copy_from_slice(&self.y_origin.to_be_bytes());
        out[8] = self.n_planes;
        out[9] = self.masking.to_byte();
        out[10] = self.compression.to_byte();
        out[11] = self.pad;
        out[12..14].copy_from_slice(&self.transparent_color.to_be_bytes());
        out[14] = self.x_aspect;
        out[15] = self.y_aspect;
        out[16..18].copy_from_slice(&self.page_width.to_be_bytes());
        out[18..20].copy_from_slice(&self.page_height.to_be_bytes());
        out
    }

    /// Bytes per bitplane row (rounded up to a 16-bit word boundary).
    pub fn row_bytes(&self) -> usize {
        (self.width as usize).div_ceil(16) * 2
    }
}

// ───────────────────── CAMG ─────────────────────

/// `CAMG` viewport-mode flag bits we recognise. Other bits (interlace,
/// hires, lace, etc.) are preserved on round-trip but ignored by the
/// pixel pipeline.
pub const CAMG_HAM: u32 = 0x0800;
pub const CAMG_EHB: u32 = 0x0080;

/// A parsed `CAMG` viewport mode. `raw` retains every flag bit so a
/// round-trip preserves the original word.
#[derive(Clone, Copy, Debug, Default)]
pub struct Camg {
    pub raw: u32,
}

impl Camg {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::invalid(format!(
                "ILBM CAMG: need 4 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            raw: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
        })
    }
    pub fn is_ham(self) -> bool {
        self.raw & CAMG_HAM != 0
    }
    pub fn is_ehb(self) -> bool {
        self.raw & CAMG_EHB != 0
    }
    pub fn to_be_bytes(self) -> [u8; 4] {
        self.raw.to_be_bytes()
    }
}

// ───────────────────── GRAB ─────────────────────

/// `GRAB` chunk — mouse-pointer hotspot for sprite use. Two big-endian
/// signed 16-bit coordinates relative to the image's top-left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Grab {
    pub x: i16,
    pub y: i16,
}

impl Grab {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::invalid(format!(
                "ILBM GRAB: need 4 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            x: i16::from_be_bytes([body[0], body[1]]),
            y: i16::from_be_bytes([body[2], body[3]]),
        })
    }
    pub fn write(&self) -> [u8; 4] {
        let mut out = [0u8; 4];
        out[0..2].copy_from_slice(&self.x.to_be_bytes());
        out[2..4].copy_from_slice(&self.y.to_be_bytes());
        out
    }
}

// ───────────────────── SHAM (Sliced HAM) ─────────────────────

/// `SHAM` — Sliced-HAM extension. After a 16-bit version word the
/// chunk carries one 16-entry palette per scanline; each entry is a
/// big-endian Amiga-style 12-bit colour packed as `0x0RGB` in a `u16`
/// (low nibble of each byte ignored / treated as zero on read). The
/// SHAM palette overrides `CMAP` per row when decoding HAM6.
///
/// On-disk layout (for `version == 0`):
/// `[u16 version][height × 16 × u16 RGB444]` — `2 + height*32` bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sham {
    pub version: u16,
    /// One 16-entry palette per scanline; each entry is RGB at 8-bit.
    pub palettes: Vec<Vec<[u8; 3]>>,
}

impl Sham {
    /// Parse a SHAM chunk. `expected_height` lets the parser tolerate
    /// chunks slightly shorter than `2 + height*32` (some encoders only
    /// store palettes up to the last *changed* row); missing rows are
    /// padded by repeating the previous palette.
    pub fn parse(body: &[u8], expected_height: u32) -> Result<Self> {
        if body.len() < 2 {
            return Err(Error::invalid(format!(
                "ILBM SHAM: need at least 2 bytes, got {}",
                body.len()
            )));
        }
        let version = u16::from_be_bytes([body[0], body[1]]);
        let mut palettes = Vec::with_capacity(expected_height as usize);
        let stride = 32usize;
        let mut off = 2usize;
        let mut last: Vec<[u8; 3]> = vec![[0, 0, 0]; 16];
        for _ in 0..expected_height {
            if off + stride <= body.len() {
                let mut pal = Vec::with_capacity(16);
                for i in 0..16 {
                    let hi = body[off + i * 2];
                    let lo = body[off + i * 2 + 1];
                    // RGB444 (Amiga): 0x0RGB → expand 4-bit → 8-bit
                    // by replicating each nibble (`n*0x11`).
                    let r4 = hi & 0x0F;
                    let g4 = (lo >> 4) & 0x0F;
                    let b4 = lo & 0x0F;
                    pal.push([r4 * 0x11, g4 * 0x11, b4 * 0x11]);
                }
                last = pal.clone();
                palettes.push(pal);
                off += stride;
            } else {
                palettes.push(last.clone());
            }
        }
        Ok(Self { version, palettes })
    }

    /// Serialise for round-trip: `[u16 version][n × 16 × u16 RGB444]`.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.palettes.len() * 32);
        out.extend_from_slice(&self.version.to_be_bytes());
        for pal in &self.palettes {
            for i in 0..16 {
                let entry = pal.get(i).copied().unwrap_or([0, 0, 0]);
                let r4 = entry[0] >> 4;
                let g4 = entry[1] >> 4;
                let b4 = entry[2] >> 4;
                out.push(r4 & 0x0F);
                out.push(((g4 & 0x0F) << 4) | (b4 & 0x0F));
            }
        }
        out
    }
}

// ───────────────────── PCHG (Palette CHanGe) ─────────────────────

/// One palette entry change at a given index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PchgChange {
    /// Palette index whose RGB to overwrite (0..=255).
    pub index: u16,
    pub rgb: [u8; 3],
}

/// All changes to apply at the start of a given scanline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PchgLine {
    pub line: u32,
    pub changes: Vec<PchgChange>,
}

/// `PCHG` — Palette CHanGe list (Sebastiano Vigna). Per-scanline CMAP
/// overrides; supports two formats encoded in a 12-bit "small/big"
/// flag:
///
/// * **SmallLineChanges** (flag bit `1`) — 12-bit channel palette,
///   1 byte index + 2 bytes RGB444 per change.
/// * **BigLineChanges** (flag bit `2`) — 24-bit channel palette,
///   2 bytes index + 3 bytes RGB888 per change.
///
/// We parse both and surface the cumulative-state palette per line as
/// 8-bit RGB in [`Pchg::lines`]. The original wire bytes are kept in
/// [`Pchg::raw`] for byte-exact round-trip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pchg {
    pub raw: Vec<u8>,
    /// Per-affected-line list of palette overrides (in line order).
    pub lines: Vec<PchgLine>,
}

impl Pchg {
    pub fn parse(body: &[u8]) -> Result<Self> {
        // Header layout per the PCHG IFF Annex (Sebastiano Vigna 1994):
        // u16 Compression; u16 Flags; i16 StartLine; u16 LineCount;
        // u16 ChangedLines; u16 MinReg; u16 MaxReg; u16 MaxChanges;
        // u32 TotalChanges;
        if body.len() < 20 {
            return Err(Error::invalid(format!(
                "ILBM PCHG: header needs 20 bytes, got {}",
                body.len()
            )));
        }
        let _comp = u16::from_be_bytes([body[0], body[1]]);
        let flags = u16::from_be_bytes([body[2], body[3]]);
        let start_line = i16::from_be_bytes([body[4], body[5]]);
        let line_count = u16::from_be_bytes([body[6], body[7]]) as usize;
        let _changed_lines = u16::from_be_bytes([body[8], body[9]]);
        let _min_reg = u16::from_be_bytes([body[10], body[11]]);
        let _max_reg = u16::from_be_bytes([body[12], body[13]]);
        let _max_changes = u16::from_be_bytes([body[14], body[15]]);
        let total_changes = u32::from_be_bytes([body[16], body[17], body[18], body[19]]) as usize;

        let big = flags & 2 != 0;
        let small = flags & 1 != 0;
        if big && small {
            return Err(Error::invalid(
                "ILBM PCHG: both Small and Big flag bits set",
            ));
        }

        // Compression byte 0 = uncompressed; we don't yet support
        // compressed (Compression == 1, Huffman). The spec calls for
        // a separate sub-chunk header with tree data we don't
        // implement on round 1. Surface the raw bytes regardless so
        // round-trip preserves the chunk verbatim.
        let mut out_lines: Vec<PchgLine> = Vec::new();

        // Small / default format: ChangeStructure begins after the
        // 20-byte header. For each line in [start_line, start_line +
        // line_count) we read a u8 ChangeCount, then ChangeCount
        // entries of (u8 RegisterIndex, u16 RGB444 BE).
        // Big format: u16 ChangeCount, then (u16 RegisterIndex, 3
        // bytes RGB888).
        // Lines with ChangeCount == 0 are emitted nowhere in our
        // out_lines list.
        let mut cur = 20usize;
        let mut total_seen = 0usize;
        if !big {
            for li in 0..line_count {
                if cur >= body.len() {
                    break;
                }
                let cc = body[cur] as usize;
                cur += 1;
                if cc > 0 {
                    let mut entries = Vec::with_capacity(cc);
                    for _ in 0..cc {
                        if cur + 3 > body.len() {
                            break;
                        }
                        let reg = body[cur] as u16;
                        let hi = body[cur + 1];
                        let lo = body[cur + 2];
                        cur += 3;
                        let r4 = hi & 0x0F;
                        let g4 = (lo >> 4) & 0x0F;
                        let b4 = lo & 0x0F;
                        entries.push(PchgChange {
                            index: reg,
                            rgb: [r4 * 0x11, g4 * 0x11, b4 * 0x11],
                        });
                        total_seen += 1;
                    }
                    let line = (start_line as i32 + li as i32).max(0) as u32;
                    out_lines.push(PchgLine {
                        line,
                        changes: entries,
                    });
                }
            }
        } else {
            for li in 0..line_count {
                if cur + 2 > body.len() {
                    break;
                }
                let cc = u16::from_be_bytes([body[cur], body[cur + 1]]) as usize;
                cur += 2;
                if cc > 0 {
                    let mut entries = Vec::with_capacity(cc);
                    for _ in 0..cc {
                        if cur + 5 > body.len() {
                            break;
                        }
                        let reg = u16::from_be_bytes([body[cur], body[cur + 1]]);
                        let r = body[cur + 2];
                        let g = body[cur + 3];
                        let b = body[cur + 4];
                        cur += 5;
                        entries.push(PchgChange {
                            index: reg,
                            rgb: [r, g, b],
                        });
                        total_seen += 1;
                    }
                    let line = (start_line as i32 + li as i32).max(0) as u32;
                    out_lines.push(PchgLine {
                        line,
                        changes: entries,
                    });
                }
            }
        }
        // Tolerant: total_changes mismatch is just a header hint.
        let _ = total_changes;
        let _ = total_seen;

        Ok(Self {
            raw: body.to_vec(),
            lines: out_lines,
        })
    }
}

// ───────────────────── CRNG (Color Range) ─────────────────────

/// `CRNG` — DeluxePaint Color Range cycling chunk. A request that a
/// closed range of palette indices be rotated at a given rate. Layout
/// (8 bytes per the public EA IFF 85 supplement / DeluxePaint manual):
///
/// ```text
/// i16 pad1   (reserved, written 0)
/// i16 rate   (palette-rotation rate; one step every 16384/rate
///             vertical-blank ticks at 60 Hz)
/// i16 flags  (bit 0 = active, bit 1 = reverse)
/// u8  low    (low end of cycling range, inclusive)
/// u8  high   (high end of cycling range, inclusive)
/// ```
///
/// An ILBM may carry many `CRNG` chunks (DeluxePaint allows up to 6).
/// We preserve them in document order so a round-trip is byte-stable.
/// We do not animate; consumers may inspect [`Crng::is_active`] and
/// [`Crng::cycles_per_second`] to apply their own animation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crng {
    pub pad1: i16,
    pub rate: i16,
    pub flags: i16,
    pub low: u8,
    pub high: u8,
}

impl Crng {
    /// CRNG flag bit: range is active (cycling enabled).
    pub const FLAG_ACTIVE: i16 = 1;
    /// CRNG flag bit: cycle direction reversed (high → low).
    pub const FLAG_REVERSE: i16 = 2;

    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "ILBM CRNG: need 8 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            pad1: i16::from_be_bytes([body[0], body[1]]),
            rate: i16::from_be_bytes([body[2], body[3]]),
            flags: i16::from_be_bytes([body[4], body[5]]),
            low: body[6],
            high: body[7],
        })
    }

    pub fn write(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&self.pad1.to_be_bytes());
        out[2..4].copy_from_slice(&self.rate.to_be_bytes());
        out[4..6].copy_from_slice(&self.flags.to_be_bytes());
        out[6] = self.low;
        out[7] = self.high;
        out
    }

    /// True if the cycling range is enabled (`flags & FLAG_ACTIVE`).
    pub fn is_active(&self) -> bool {
        self.flags & Self::FLAG_ACTIVE != 0
    }

    /// True if the range cycles high → low (`flags & FLAG_REVERSE`).
    pub fn is_reverse(&self) -> bool {
        self.flags & Self::FLAG_REVERSE != 0
    }

    /// Cycle rate in steps per second on a 60 Hz vertical-blank tick.
    /// Per the DeluxePaint manual one cycle step happens every
    /// `16384 / rate` ticks; with `rate == 16384` that's once per
    /// tick (~60 steps/s); `rate == 0` means disabled.
    pub fn cycles_per_second(&self) -> f32 {
        if self.rate <= 0 {
            0.0
        } else {
            60.0 * (self.rate as f32) / 16384.0
        }
    }

    /// Number of palette entries spanned by the cycle (inclusive of
    /// both ends). Returns 0 if `low > high`.
    pub fn range_len(&self) -> u16 {
        if self.low > self.high {
            0
        } else {
            (self.high - self.low) as u16 + 1
        }
    }
}

// ───────────────────── CCRT (Color Cycling Range and Timing) ─────────────────────

/// `CCRT` — Commodore Graphicraft Color Cycling Range and Timing.
/// The Amiga Graphicraft analogue of CRNG: same intent (rotate a
/// palette range over time) with a longer / more explicit timing
/// representation. Layout (14 bytes per the EA IFF 85 supplement):
///
/// ```text
/// i16 direction (-1 = backwards, 0 = inactive, +1 = forwards)
/// u8  start     (low palette index, inclusive)
/// u8  end       (high palette index, inclusive)
/// i32 seconds   (cycle delay seconds component)
/// i32 micros    (cycle delay microseconds component, 0..1_000_000)
/// i16 pad       (reserved, written 0)
/// ```
///
/// `seconds + micros / 1_000_000` is the delay between one cycle step
/// and the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ccrt {
    pub direction: i16,
    pub start: u8,
    pub end: u8,
    pub seconds: i32,
    pub micros: i32,
    pub pad: i16,
}

impl Ccrt {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 14 {
            return Err(Error::invalid(format!(
                "ILBM CCRT: need 14 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            direction: i16::from_be_bytes([body[0], body[1]]),
            start: body[2],
            end: body[3],
            seconds: i32::from_be_bytes([body[4], body[5], body[6], body[7]]),
            micros: i32::from_be_bytes([body[8], body[9], body[10], body[11]]),
            pad: i16::from_be_bytes([body[12], body[13]]),
        })
    }

    pub fn write(&self) -> [u8; 14] {
        let mut out = [0u8; 14];
        out[0..2].copy_from_slice(&self.direction.to_be_bytes());
        out[2] = self.start;
        out[3] = self.end;
        out[4..8].copy_from_slice(&self.seconds.to_be_bytes());
        out[8..12].copy_from_slice(&self.micros.to_be_bytes());
        out[12..14].copy_from_slice(&self.pad.to_be_bytes());
        out
    }

    /// True if `direction` is non-zero (cycling is active in either
    /// direction).
    pub fn is_active(&self) -> bool {
        self.direction != 0
    }

    /// True if direction is negative (high → low).
    pub fn is_reverse(&self) -> bool {
        self.direction < 0
    }

    /// Cycle delay expressed as a single float in seconds. Returns
    /// 0.0 for negative inputs (treated as malformed).
    pub fn delay_seconds(&self) -> f64 {
        if self.seconds < 0 || self.micros < 0 {
            0.0
        } else {
            self.seconds as f64 + self.micros as f64 / 1_000_000.0
        }
    }

    /// Number of palette entries spanned by the cycle (inclusive of
    /// both ends). Returns 0 if `start > end`.
    pub fn range_len(&self) -> u16 {
        if self.start > self.end {
            0
        } else {
            (self.end - self.start) as u16 + 1
        }
    }
}

// ───────────────────── ByteRun1 (PackBits) ─────────────────────

/// Decode one ByteRun1-compressed plane-row into `out`. Reads from
/// `input` until exactly `expected` decoded bytes have been emitted.
/// Returns the number of *input* bytes consumed.
///
/// Spec (ILBM appendix C):
/// * `n` in `0..=127` — copy the next `n + 1` bytes literally.
/// * `n == 128` — NOP (no operation).
/// * `n` in `129..=255` — repeat the next byte `257 - n` times
///   (i.e. between 2 and 128 copies).
pub fn byterun1_decode_row(input: &[u8], expected: usize, out: &mut Vec<u8>) -> Result<usize> {
    let target = out.len() + expected;
    let mut i = 0usize;
    while out.len() < target {
        if i >= input.len() {
            return Err(Error::invalid(
                "ILBM ByteRun1: input exhausted before producing expected bytes",
            ));
        }
        let n = input[i] as i8;
        i += 1;
        if n >= 0 {
            // Literal run of n+1 bytes.
            let len = n as usize + 1;
            if i + len > input.len() {
                return Err(Error::invalid(
                    "ILBM ByteRun1: literal run extends past input",
                ));
            }
            if out.len() + len > target {
                return Err(Error::invalid(
                    "ILBM ByteRun1: literal run overruns row budget",
                ));
            }
            out.extend_from_slice(&input[i..i + len]);
            i += len;
        } else if n == -128 {
            // NOP byte; emitted by some encoders, ignored.
            continue;
        } else {
            // Repeat run of (1 - n) = (257 - byte) copies.
            let len = (1i32 - n as i32) as usize;
            if i >= input.len() {
                return Err(Error::invalid(
                    "ILBM ByteRun1: missing repeat byte at end of input",
                ));
            }
            if out.len() + len > target {
                return Err(Error::invalid(
                    "ILBM ByteRun1: repeat run overruns row budget",
                ));
            }
            let v = input[i];
            i += 1;
            for _ in 0..len {
                out.push(v);
            }
        }
    }
    Ok(i)
}

/// Encode one plane-row as ByteRun1 (PackBits). Greedy — pick the
/// longest run of 2..=128 equal bytes, otherwise emit a literal run of
/// 1..=128 bytes.
pub fn byterun1_encode_row(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 64 + 1);
    let mut i = 0usize;
    while i < input.len() {
        // Look for a run of identical bytes starting at `i`.
        let mut run_len = 1usize;
        while run_len < 128 && i + run_len < input.len() && input[i + run_len] == input[i] {
            run_len += 1;
        }
        if run_len >= 2 {
            // Repeat run: encode as (257 - count) byte-pair where
            // `count` is in 2..=128. Equivalent: byte = -(count-1) as i8.
            let count = run_len;
            let n = -(count as i32 - 1) as i8;
            out.push(n as u8);
            out.push(input[i]);
            i += count;
        } else {
            // Literal run: collect bytes that don't start a 3-or-more
            // repeat. Length capped at 128 (encoded as 0..=127).
            let start = i;
            i += 1;
            while i - start < 128 && i < input.len() {
                // Stop the literal if the next 3 bytes are equal —
                // that's a worthwhile repeat.
                if i + 2 < input.len() && input[i] == input[i + 1] && input[i + 1] == input[i + 2] {
                    break;
                }
                // Stop if exactly 2 equal at the end (cheap to keep,
                // since literal+repeat costs as much as one literal).
                i += 1;
            }
            let len = i - start;
            out.push((len - 1) as u8);
            out.extend_from_slice(&input[start..i]);
        }
    }
    out
}

// ───────────────────── Planar → packed ─────────────────────

/// Convert one row of bitplane data to a `width`-long `Vec<u8>` of
/// per-pixel index bytes. `planes[p]` is plane `p`'s row, each
/// `row_bytes` bytes long. Plane 0 is the LSB; plane `n-1` is the MSB.
pub fn planar_row_to_indices(planes: &[&[u8]], width: u16) -> Vec<u8> {
    let mut out = vec![0u8; width as usize];
    let n_planes = planes.len();
    for (x, slot) in out.iter_mut().enumerate() {
        let byte_idx = x / 8;
        let bit = 7 - (x % 8);
        let mut acc = 0u8;
        // Build MSB → LSB so the high plane contributes the high bit.
        for p in (0..n_planes).rev() {
            acc <<= 1;
            let row = planes[p];
            if byte_idx < row.len() && (row[byte_idx] >> bit) & 1 == 1 {
                acc |= 1;
            }
        }
        *slot = acc;
    }
    out
}

/// Inverse of [`planar_row_to_indices`] — pack a row of `width`
/// per-pixel index bytes into `n_planes` plane-rows of `row_bytes`
/// each. Used only by the encoder side.
pub fn indices_to_planar_row(indices: &[u8], n_planes: u8, row_bytes: usize) -> Vec<Vec<u8>> {
    let mut planes: Vec<Vec<u8>> = (0..n_planes).map(|_| vec![0u8; row_bytes]).collect();
    for (x, &v) in indices.iter().enumerate() {
        let byte_idx = x / 8;
        let bit = 7 - (x % 8);
        for (p, plane) in planes.iter_mut().enumerate() {
            if (v >> p) & 1 == 1 {
                plane[byte_idx] |= 1 << bit;
            }
        }
    }
    planes
}

// ───────────────────── EHB / HAM ─────────────────────

/// Mirror a 32-entry palette into the upper 32 entries by halving each
/// channel. Required when CAMG indicates extra-half-brite (EHB).
pub fn expand_ehb_palette(palette: &[[u8; 3]]) -> Vec<[u8; 3]> {
    let mut out: Vec<[u8; 3]> = palette.iter().take(32).copied().collect();
    while out.len() < 32 {
        out.push([0, 0, 0]);
    }
    for i in 0..32 {
        out.push([out[i][0] >> 1, out[i][1] >> 1, out[i][2] >> 1]);
    }
    out
}

/// Decode one HAM row of indices to a `width`-long `Vec<[u8; 3]>` of
/// RGB triples. `bits` is `4` for HAM6 (6-plane) and `6` for HAM8
/// (8-plane). The top two bits of each index encode the control op:
///
/// * `0b00` — palette lookup (low `bits` bits index `palette`).
/// * `0b01` — modify Blue channel; new value = low `bits` bits left-
///   shifted to fill 8 bits (replicating the high bits into the low).
/// * `0b10` — modify Red channel.
/// * `0b11` — modify Green channel.
///
/// State (R, G, B) carries from the previous pixel within the row.
/// Per spec the row begins from black `(0, 0, 0)` — the first pixel
/// being a "modify" only changes one channel.
pub fn expand_ham_row(indices: &[u8], palette: &[[u8; 3]], bits: u8) -> Vec<[u8; 3]> {
    debug_assert!(
        bits == 4 || bits == 6,
        "HAM bits must be 4 (HAM6) or 6 (HAM8)"
    );
    let value_mask: u8 = (1u8 << bits) - 1;
    let mut out = Vec::with_capacity(indices.len());
    let mut r: u8 = 0;
    let mut g: u8 = 0;
    let mut b: u8 = 0;
    // Replicate the channel value into the unused low bits so it spans
    // 0..=255 regardless of HAM6 (4-bit) vs HAM8 (6-bit).
    let widen = |val: u8| -> u8 {
        match bits {
            4 => (val << 4) | val,
            6 => (val << 2) | (val >> 4),
            _ => val,
        }
    };
    for &px in indices {
        let op = (px >> bits) & 0b11;
        let val = px & value_mask;
        match op {
            0b00 => {
                // Palette lookup. HAM6 uses up to 16 entries; HAM8 up to 64.
                let idx = val as usize;
                if idx < palette.len() {
                    let p = palette[idx];
                    r = p[0];
                    g = p[1];
                    b = p[2];
                } else {
                    r = 0;
                    g = 0;
                    b = 0;
                }
            }
            0b01 => b = widen(val),
            0b10 => r = widen(val),
            0b11 => g = widen(val),
            _ => unreachable!(),
        }
        out.push([r, g, b]);
    }
    out
}

// ───────────────────── In-memory ILBM image ─────────────────────

/// A fully-decoded ILBM image: width × height of packed RGBA8888 in
/// row-major order plus the parsed BMHD / CMAP / CAMG metadata. The
/// alpha plane is `0xFF` everywhere unless the file indicated a mask
/// (HasMask plane or transparent-colour key) — masked-out pixels get
/// alpha `0`.
#[derive(Clone, Debug)]
pub struct IlbmImage {
    pub width: u32,
    pub height: u32,
    /// The original BMHD (helpful for re-encode round-trip).
    pub bmhd: Bmhd,
    /// Original palette (pre-EHB expansion). Empty for true-colour
    /// files.
    pub palette: Vec<[u8; 3]>,
    /// CAMG flags (0 if absent in the source).
    pub camg: Camg,
    /// Outer FORM type: `b"ILBM"` (planar) or `b"PBM "` (chunky 8-bit
    /// per pixel — DPaint II / Brilliance variant).
    pub form_type: [u8; 4],
    /// Optional `GRAB` hotspot (mouse-pointer anchor for sprites).
    pub grab: Option<Grab>,
    /// Optional `SHAM` Sliced-HAM payload (one 16-entry RGB444 palette
    /// per scanline). Only meaningful when `camg.is_ham()` and
    /// `bmhd.n_planes == 6`.
    pub sham: Option<Sham>,
    /// Optional `PCHG` palette-change list (per-line CMAP overrides).
    pub pchg: Option<Pchg>,
    /// `CRNG` colour-range cycling descriptors (DeluxePaint). Order
    /// is preserved so round-trip is byte-stable.
    pub crngs: Vec<Crng>,
    /// `CCRT` colour-range cycling descriptors (Graphicraft variant).
    /// Order is preserved so round-trip is byte-stable.
    pub ccrts: Vec<Ccrt>,
    /// Packed RGBA bytes, row-major, top-to-bottom, 4 bytes/pixel.
    pub rgba: Vec<u8>,
}

impl Default for IlbmImage {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            bmhd: Bmhd {
                width: 0,
                height: 0,
                x_origin: 0,
                y_origin: 0,
                n_planes: 0,
                masking: Masking::None,
                compression: Compression::None,
                pad: 0,
                transparent_color: 0,
                x_aspect: 1,
                y_aspect: 1,
                page_width: 0,
                page_height: 0,
            },
            palette: Vec::new(),
            camg: Camg::default(),
            form_type: *b"ILBM",
            grab: None,
            sham: None,
            pchg: None,
            crngs: Vec::new(),
            ccrts: Vec::new(),
            rgba: Vec::new(),
        }
    }
}

// ───────────────────── parse_ilbm ─────────────────────

/// Parse an in-memory ILBM file: the outer FORM/ILBM envelope plus
/// BMHD / CMAP / (optional) CAMG / BODY children. Other chunks are
/// skipped silently — round 1 doesn't surface CRNG / GRAB / DPI / etc.
///
/// Returns the fully-decoded image with packed RGBA pixels — the
/// caller doesn't need to know about bitplanes, ByteRun1, EHB, or HAM.
pub fn parse_ilbm(bytes: &[u8]) -> Result<IlbmImage> {
    if bytes.len() < 12 {
        return Err(Error::invalid("ILBM: file shorter than FORM header"));
    }
    if &bytes[0..4] != b"FORM" {
        return Err(Error::invalid("ILBM: missing FORM signature"));
    }
    let form_type = [bytes[8], bytes[9], bytes[10], bytes[11]];
    let is_ilbm = &form_type == b"ILBM";
    let is_pbm = &form_type == b"PBM ";
    if !is_ilbm && !is_pbm {
        return Err(Error::invalid(format!(
            "ILBM: outer form type is {:?} (expected ILBM or PBM)",
            std::str::from_utf8(&form_type).unwrap_or("????")
        )));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8 + total).min(bytes.len());

    let mut bmhd: Option<Bmhd> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut camg = Camg::default();
    let mut body_data: Option<Vec<u8>> = None;
    let mut grab: Option<Grab> = None;
    let mut sham_raw: Option<Vec<u8>> = None;
    let mut pchg: Option<Pchg> = None;
    let mut crngs: Vec<Crng> = Vec::new();
    let mut ccrts: Vec<Ccrt> = Vec::new();

    let mut cursor = 12usize;
    while cursor + 8 <= body_end {
        let id = [
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ];
        let size = u32::from_be_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let payload_start = cursor + 8;
        let payload_end = payload_start + size;
        if payload_end > body_end {
            return Err(Error::invalid(format!(
                "ILBM: chunk {:?} extends past FORM ({} > {})",
                std::str::from_utf8(&id).unwrap_or("????"),
                payload_end,
                body_end
            )));
        }
        let payload = &bytes[payload_start..payload_end];
        match &id {
            b"BMHD" => bmhd = Some(Bmhd::parse(payload)?),
            b"CMAP" => {
                palette = payload
                    .chunks_exact(3)
                    .map(|c| [c[0], c[1], c[2]])
                    .collect();
            }
            b"CAMG" => camg = Camg::parse(payload)?,
            b"BODY" => body_data = Some(payload.to_vec()),
            b"GRAB" => grab = Some(Grab::parse(payload)?),
            b"SHAM" => sham_raw = Some(payload.to_vec()),
            b"PCHG" => pchg = Some(Pchg::parse(payload)?),
            b"CRNG" => crngs.push(Crng::parse(payload)?),
            b"CCRT" => ccrts.push(Ccrt::parse(payload)?),
            _ => { /* skip unknown chunks (DPI, DPPS, AUTH, ...) */ }
        }
        let padded = size + (size & 1);
        cursor = payload_start + padded;
    }

    let bmhd = bmhd.ok_or_else(|| Error::invalid("ILBM: missing BMHD chunk"))?;
    let body = body_data.ok_or_else(|| Error::invalid("ILBM: missing BODY chunk"))?;
    let sham = sham_raw
        .as_deref()
        .map(|raw| Sham::parse(raw, bmhd.height as u32))
        .transpose()?;

    let width = bmhd.width as u32;
    let height = bmhd.height as u32;
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];

    if is_pbm {
        // Chunky variant: BODY is `width` (rounded up to even) bytes
        // per row, each byte = a palette index. No bitplanes, no
        // mask plane, no HAM. Compression follows BMHD.compression.
        if camg.is_ham() {
            return Err(Error::unsupported(
                "PBM: HAM viewport on chunky form is not supported",
            ));
        }
        let stride = (bmhd.width as usize + 1) & !1;
        let mut indices_all: Vec<u8> = Vec::with_capacity(stride * bmhd.height as usize);
        match bmhd.compression {
            Compression::None => {
                let needed = stride * bmhd.height as usize;
                if body.len() < needed {
                    return Err(Error::invalid(format!(
                        "PBM uncompressed BODY: need {needed} bytes, got {}",
                        body.len()
                    )));
                }
                indices_all.extend_from_slice(&body[..needed]);
            }
            Compression::ByteRun1 => {
                let mut input = &body[..];
                for _ in 0..bmhd.height {
                    let consumed = byterun1_decode_row(input, stride, &mut indices_all)?;
                    input = &input[consumed..];
                }
            }
            // Auto is encoder-only; it is always resolved to None or
            // ByteRun1 before the BMHD byte is written, so it should
            // never appear in a file being decoded.
            Compression::Auto => {
                return Err(Error::unsupported(
                    "ILBM BMHD: compression byte 'Auto' is encoder-only, not a valid file value",
                ))
            }
        }
        let effective_palette: Vec<[u8; 3]> = if camg.is_ehb() && palette.len() <= 32 {
            expand_ehb_palette(&palette)
        } else {
            palette.clone()
        };
        for y in 0..bmhd.height as usize {
            for x in 0..bmhd.width as usize {
                let idx = indices_all[y * stride + x] as usize;
                let dst = (y * bmhd.width as usize + x) * 4;
                let p = if idx < effective_palette.len() {
                    effective_palette[idx]
                } else {
                    [0, 0, 0]
                };
                rgba[dst] = p[0];
                rgba[dst + 1] = p[1];
                rgba[dst + 2] = p[2];
                let alpha = if bmhd.masking == Masking::HasTransparentColor
                    && (idx as u16) == bmhd.transparent_color
                {
                    0
                } else {
                    0xFF
                };
                rgba[dst + 3] = alpha;
            }
        }
        return Ok(IlbmImage {
            width,
            height,
            bmhd,
            palette,
            camg,
            form_type,
            grab,
            sham,
            pchg,
            crngs,
            ccrts,
            rgba,
        });
    }

    // Planar ILBM path (existing behaviour).
    let n_planes = bmhd.n_planes as usize;
    if n_planes == 0 || n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ILBM: round 1 supports 1..=8 colour bitplanes (got {n_planes})"
        )));
    }
    let row_bytes = bmhd.row_bytes();
    let has_mask_plane = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + if has_mask_plane { 1 } else { 0 };

    // Decode BODY into a flat row-major buffer of un-interlaced bitplane
    // bytes: `(plane, row)` indexed by `row * row_bytes * planes_per_row + p * row_bytes`.
    let mut rows_planar: Vec<Vec<u8>> = Vec::with_capacity(bmhd.height as usize * planes_per_row);

    match bmhd.compression {
        Compression::None => {
            let needed = bmhd.height as usize * planes_per_row * row_bytes;
            if body.len() < needed {
                return Err(Error::invalid(format!(
                    "ILBM uncompressed BODY: need {needed} bytes, got {}",
                    body.len()
                )));
            }
            for chunk in body[..needed].chunks_exact(row_bytes) {
                rows_planar.push(chunk.to_vec());
            }
        }
        Compression::ByteRun1 => {
            let mut input = &body[..];
            for _ in 0..bmhd.height {
                for _ in 0..planes_per_row {
                    let mut row = Vec::with_capacity(row_bytes);
                    let consumed = byterun1_decode_row(input, row_bytes, &mut row)?;
                    input = &input[consumed..];
                    rows_planar.push(row);
                }
            }
        }
        Compression::Auto => {
            return Err(Error::unsupported(
                "ILBM BMHD: compression byte 'Auto' is encoder-only, not a valid file value",
            ))
        }
    }

    // Decide effective default palette (EHB-expanded if requested).
    let default_palette: Vec<[u8; 3]> = if camg.is_ehb() && palette.len() <= 32 {
        expand_ehb_palette(&palette)
    } else {
        palette.clone()
    };

    // PCHG: build per-line palette overlays. Start from `default_palette`
    // and apply changes cumulatively in line order.
    let line_palettes: Option<Vec<Vec<[u8; 3]>>> = if let Some(pchg) = &pchg {
        let mut cur_pal = default_palette.clone();
        if cur_pal.len() < 256 {
            cur_pal.resize(256, [0, 0, 0]);
        }
        let mut iter = pchg.lines.iter().peekable();
        let mut out: Vec<Vec<[u8; 3]>> = Vec::with_capacity(bmhd.height as usize);
        for y in 0..bmhd.height as u32 {
            while let Some(line) = iter.peek() {
                if line.line == y {
                    for ch in &line.changes {
                        let i = ch.index as usize;
                        if i < cur_pal.len() {
                            cur_pal[i] = ch.rgb;
                        }
                    }
                    iter.next();
                } else if line.line < y {
                    iter.next();
                } else {
                    break;
                }
            }
            out.push(cur_pal.clone());
        }
        Some(out)
    } else {
        None
    };

    for y in 0..bmhd.height as usize {
        let row_base = y * planes_per_row;
        let plane_refs: Vec<&[u8]> = (0..n_planes)
            .map(|p| rows_planar[row_base + p].as_slice())
            .collect();
        let indices = planar_row_to_indices(&plane_refs, bmhd.width);

        // Resolve to RGB.
        let row_palette: &[[u8; 3]] = if let Some(sham) = &sham {
            sham.palettes
                .get(y)
                .map(|p| p.as_slice())
                .unwrap_or(&default_palette)
        } else if let Some(lp) = &line_palettes {
            lp[y].as_slice()
        } else {
            &default_palette
        };
        let rgb_row: Vec<[u8; 3]> = if camg.is_ham() {
            let bits = match n_planes {
                6 => 4u8, // HAM6
                8 => 6u8, // HAM8
                other => {
                    return Err(Error::unsupported(format!(
                        "ILBM HAM: unsupported plane count {other} (expected 6 or 8)"
                    )))
                }
            };
            expand_ham_row(&indices, row_palette, bits)
        } else {
            indices
                .iter()
                .map(|&i| {
                    let i = i as usize;
                    if i < row_palette.len() {
                        row_palette[i]
                    } else {
                        [0, 0, 0]
                    }
                })
                .collect()
        };

        // Mask: HasMask plane takes precedence; otherwise transparent
        // colour key when configured.
        let mask_row: Option<&[u8]> = if has_mask_plane {
            Some(rows_planar[row_base + n_planes].as_slice())
        } else {
            None
        };

        for x in 0..width as usize {
            let dst = (y * width as usize + x) * 4;
            rgba[dst] = rgb_row[x][0];
            rgba[dst + 1] = rgb_row[x][1];
            rgba[dst + 2] = rgb_row[x][2];
            let alpha = if let Some(mr) = mask_row {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                if bi < mr.len() && (mr[bi] >> bit) & 1 == 1 {
                    0xFF
                } else {
                    0x00
                }
            } else if bmhd.masking == Masking::HasTransparentColor
                && !camg.is_ham()
                && (indices[x] as u16) == bmhd.transparent_color
            {
                0x00
            } else {
                0xFF
            };
            rgba[dst + 3] = alpha;
        }
    }

    Ok(IlbmImage {
        width,
        height,
        bmhd,
        palette,
        camg,
        form_type,
        grab,
        sham,
        pchg,
        crngs,
        ccrts,
        rgba,
    })
}

// ───────────────────── encode_ilbm ─────────────────────

/// Encode an [`IlbmImage`] back into a FORM/ILBM (or FORM/PBM ) byte
/// stream.
///
/// Output form selection:
/// * `image.form_type == b"PBM "` → chunky 8-bit-per-pixel BODY
///   (DPaint II / Brilliance). Requires `bmhd.n_planes == 8` and a
///   non-empty palette.
/// * everything else → planar `FORM/ILBM`. Three sub-paths:
///   * `camg.is_ham()` with `n_planes == 6` (HAM6) or `n_planes == 8`
///     (HAM8) — runs the per-row HAM state-machine encoder.
///   * `camg.is_ehb()` — quantises against a 64-entry EHB-expanded
///     palette and writes 6 bitplanes regardless of the input
///     palette length.
///   * otherwise — straight indexed encode (1..=8 bitplanes).
///
/// Optional sub-chunks `GRAB`, `SHAM`, `PCHG` are emitted when present
/// on `image`. Compression follows `image.bmhd.compression`.
pub fn encode_ilbm(image: &IlbmImage) -> Result<Vec<u8>> {
    let bmhd = image.bmhd;
    if bmhd.width == 0 || bmhd.height == 0 {
        return Err(Error::invalid("ILBM encode: zero-dimension image"));
    }
    if image.palette.is_empty() {
        return Err(Error::unsupported(
            "ILBM encode: requires an indexed palette",
        ));
    }
    let is_pbm = &image.form_type == b"PBM ";
    if is_pbm && bmhd.n_planes != 8 {
        return Err(Error::invalid(format!(
            "PBM encode: requires n_planes=8 (got {})",
            bmhd.n_planes
        )));
    }

    // Build BODY bytes per branch. When compression is Auto the body
    // encoder returns the winning bytes; we must also learn which mode
    // won so we can write the correct byte into BMHD.
    let (body_bytes, resolved_compression): (Vec<u8>, Compression) = if is_pbm {
        encode_pbm_body_resolving(image)?
    } else if image.camg.is_ham() {
        encode_ham_body_resolving(image)?
    } else if image.camg.is_ehb() {
        encode_ehb_body_resolving(image)?
    } else {
        encode_indexed_body_resolving(image)?
    };

    // Build BMHD with the resolved compression mode so the BMHD byte on
    // disk always matches the actual encoding of BODY.
    let mut bmhd_out = bmhd;
    bmhd_out.compression = resolved_compression;

    // Assemble FORM/ILBM (or FORM/PBM ).
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes()); // size patched below
    out.extend_from_slice(&image.form_type);

    // BMHD
    out.extend_from_slice(b"BMHD");
    out.extend_from_slice(&20u32.to_be_bytes());
    out.extend_from_slice(&bmhd_out.write());

    // CMAP
    let cmap_size = (image.palette.len() * 3) as u32;
    out.extend_from_slice(b"CMAP");
    out.extend_from_slice(&cmap_size.to_be_bytes());
    for c in &image.palette {
        out.extend_from_slice(c);
    }
    if cmap_size & 1 == 1 {
        out.push(0);
    }

    // CAMG (only if non-zero — saves bytes on the common path).
    if image.camg.raw != 0 {
        out.extend_from_slice(b"CAMG");
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&image.camg.to_be_bytes());
    }

    // GRAB
    if let Some(g) = image.grab {
        out.extend_from_slice(b"GRAB");
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&g.write());
    }

    // SHAM (Sliced HAM per-line palette table)
    if let Some(s) = &image.sham {
        let payload = s.write();
        let sz = payload.len() as u32;
        out.extend_from_slice(b"SHAM");
        out.extend_from_slice(&sz.to_be_bytes());
        out.extend_from_slice(&payload);
        if sz & 1 == 1 {
            out.push(0);
        }
    }

    // PCHG (palette-change list — round-trip the raw bytes verbatim)
    if let Some(p) = &image.pchg {
        let sz = p.raw.len() as u32;
        out.extend_from_slice(b"PCHG");
        out.extend_from_slice(&sz.to_be_bytes());
        out.extend_from_slice(&p.raw);
        if sz & 1 == 1 {
            out.push(0);
        }
    }

    // CRNG (DeluxePaint colour-range cycling — 8 bytes each; even-
    // sized so no pad byte). Emitted in `image.crngs` order so a
    // parse → encode round-trip is byte-stable.
    for c in &image.crngs {
        out.extend_from_slice(b"CRNG");
        out.extend_from_slice(&8u32.to_be_bytes());
        out.extend_from_slice(&c.write());
    }

    // CCRT (Graphicraft colour-cycling timing — 14 bytes each; even-
    // sized so no pad byte). Emitted in `image.ccrts` order so a
    // parse → encode round-trip is byte-stable.
    for c in &image.ccrts {
        out.extend_from_slice(b"CCRT");
        out.extend_from_slice(&14u32.to_be_bytes());
        out.extend_from_slice(&c.write());
    }

    // BODY
    let body_size = body_bytes.len() as u32;
    out.extend_from_slice(b"BODY");
    out.extend_from_slice(&body_size.to_be_bytes());
    out.extend_from_slice(&body_bytes);
    if body_size & 1 == 1 {
        out.push(0);
    }

    // Patch FORM size = total - 8.
    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Best-fit palette match by squared Euclidean distance.
fn nearest_index(palette: &[[u8; 3]], r: u8, g: u8, b: u8) -> usize {
    let mut best = 0usize;
    let mut best_d = i32::MAX;
    for (i, p) in palette.iter().enumerate() {
        let dr = r as i32 - p[0] as i32;
        let dg = g as i32 - p[1] as i32;
        let db = b as i32 - p[2] as i32;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Apply the per-plane row encoding for a list of palette indices.
/// Pushes the resulting rows (`n_planes` then optional mask) onto
/// `planar_rows`. `mask_bits` is `Some(row)` for HasMask, else `None`.
fn push_planar_row(
    planar_rows: &mut Vec<Vec<u8>>,
    indices: &[u8],
    n_planes: u8,
    row_bytes: usize,
    mask_bits: Option<&[u8]>,
) {
    let plane_rows = indices_to_planar_row(indices, n_planes, row_bytes);
    for pr in plane_rows {
        planar_rows.push(pr);
    }
    if let Some(m) = mask_bits {
        planar_rows.push(m.to_vec());
    }
}

/// Pack BODY rows (with optional ByteRun1 compression) into a single
/// byte stream. `Compression::Auto` tries both modes and returns the
/// shorter result together with the winning [`Compression`] variant.
fn pack_body(planar_rows: Vec<Vec<u8>>, compression: Compression) -> Vec<u8> {
    match compression {
        Compression::None => planar_rows.into_iter().flatten().collect(),
        Compression::ByteRun1 => planar_rows
            .iter()
            .flat_map(|row| byterun1_encode_row(row))
            .collect(),
        Compression::Auto => {
            let rle: Vec<u8> = planar_rows
                .iter()
                .flat_map(|row| byterun1_encode_row(row))
                .collect();
            let raw_len: usize = planar_rows.iter().map(|r| r.len()).sum();
            if rle.len() < raw_len {
                rle
            } else {
                planar_rows.into_iter().flatten().collect()
            }
        }
    }
}

/// Like [`pack_body`] but also returns the resolved [`Compression`]
/// mode that was actually used. For `Auto`, returns `ByteRun1` when
/// RLE wins, `None` otherwise; for explicit modes returns the mode as-is.
fn pack_body_resolving(
    planar_rows: Vec<Vec<u8>>,
    compression: Compression,
) -> (Vec<u8>, Compression) {
    match compression {
        Compression::Auto => {
            let rle: Vec<u8> = planar_rows
                .iter()
                .flat_map(|row| byterun1_encode_row(row))
                .collect();
            let raw_len: usize = planar_rows.iter().map(|r| r.len()).sum();
            if rle.len() < raw_len {
                (rle, Compression::ByteRun1)
            } else {
                let raw: Vec<u8> = planar_rows.into_iter().flatten().collect();
                (raw, Compression::None)
            }
        }
        other => (pack_body(planar_rows, other), other),
    }
}

/// Indexed (non-HAM, non-EHB) BODY encoder. Up to 8 bitplanes. When
/// `image.pchg` is `Some`, each row's palette is the cumulative state
/// at that scanline (start = `image.palette`, then PCHG entries
/// applied in line order).
///
/// Returns body bytes plus the resolved [`Compression`] mode actually
/// used (important when `bmhd.compression == Auto`).
fn encode_indexed_body_resolving(image: &IlbmImage) -> Result<(Vec<u8>, Compression)> {
    let rows = encode_indexed_planar_rows(image)?;
    Ok(pack_body_resolving(rows, image.bmhd.compression))
}

/// Build raw planar rows for indexed (non-HAM, non-EHB) body encoding.
fn encode_indexed_planar_rows(image: &IlbmImage) -> Result<Vec<Vec<u8>>> {
    let bmhd = image.bmhd;
    let n_planes = bmhd.n_planes as usize;
    if !(1..=8).contains(&n_planes) {
        return Err(Error::unsupported(format!(
            "ILBM encode: 1..=8 bitplanes for indexed (got {n_planes})"
        )));
    }
    let row_bytes = bmhd.row_bytes();
    let has_mask_plane = bmhd.masking == Masking::HasMask;
    let mut planar_rows: Vec<Vec<u8>> =
        Vec::with_capacity(bmhd.height as usize * (n_planes + has_mask_plane as usize));

    // Pre-compute per-row palette if PCHG is in play.
    let line_palettes: Option<Vec<Vec<[u8; 3]>>> = image.pchg.as_ref().map(|pchg| {
        let mut cur_pal = image.palette.clone();
        if cur_pal.len() < 256 {
            cur_pal.resize(256, [0, 0, 0]);
        }
        let mut iter = pchg.lines.iter().peekable();
        let mut out: Vec<Vec<[u8; 3]>> = Vec::with_capacity(bmhd.height as usize);
        for y in 0..bmhd.height as u32 {
            while let Some(line) = iter.peek() {
                if line.line == y {
                    for ch in &line.changes {
                        let i = ch.index as usize;
                        if i < cur_pal.len() {
                            cur_pal[i] = ch.rgb;
                        }
                    }
                    iter.next();
                } else if line.line < y {
                    iter.next();
                } else {
                    break;
                }
            }
            out.push(cur_pal.clone());
        }
        out
    });

    for y in 0..bmhd.height as usize {
        let palette: &[[u8; 3]] = if let Some(lp) = &line_palettes {
            lp[y].as_slice()
        } else {
            image.palette.as_slice()
        };
        let use_transparent_key = bmhd.masking == Masking::HasTransparentColor;
        let mut indices = vec![0u8; bmhd.width as usize];
        let mut mask = vec![0u8; row_bytes];
        for (x, idx_slot) in indices.iter_mut().enumerate() {
            let src = (y * bmhd.width as usize + x) * 4;
            let r = image.rgba[src];
            let g = image.rgba[src + 1];
            let b = image.rgba[src + 2];
            let a = image.rgba[src + 3];
            // Transparent-colour key: pixels whose alpha is below
            // 0x80 are written as the BMHD-declared transparent
            // index (the decoder zeros them on read).
            *idx_slot = if use_transparent_key && a < 0x80 {
                bmhd.transparent_color as u8
            } else {
                nearest_index(palette, r, g, b) as u8
            };
            if a >= 0x80 {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                mask[bi] |= 1 << bit;
            }
        }
        push_planar_row(
            &mut planar_rows,
            &indices,
            bmhd.n_planes,
            row_bytes,
            if has_mask_plane { Some(&mask) } else { None },
        );
    }
    Ok(planar_rows)
}

/// EHB (Extra-Half-Brite) BODY encoder. Output is 6 bitplanes; the
/// expanded 64-entry palette is `[pal[0..32], pal[i].halved...]`. We
/// quantise against the full 64-entry table per pixel, then encode
/// the chosen index 0..=63 in 6 bitplanes.
fn encode_ehb_planar_rows(image: &IlbmImage) -> Result<Vec<Vec<u8>>> {
    let bmhd = image.bmhd;
    if bmhd.n_planes != 6 {
        return Err(Error::invalid(format!(
            "EHB encode: requires n_planes=6 (got {})",
            bmhd.n_planes
        )));
    }
    let expanded = expand_ehb_palette(&image.palette);
    let row_bytes = bmhd.row_bytes();
    let has_mask_plane = bmhd.masking == Masking::HasMask;
    let mut planar_rows: Vec<Vec<u8>> =
        Vec::with_capacity(bmhd.height as usize * (6 + has_mask_plane as usize));
    for y in 0..bmhd.height as usize {
        let mut indices = vec![0u8; bmhd.width as usize];
        let mut mask = vec![0u8; row_bytes];
        for (x, idx_slot) in indices.iter_mut().enumerate() {
            let src = (y * bmhd.width as usize + x) * 4;
            let r = image.rgba[src];
            let g = image.rgba[src + 1];
            let b = image.rgba[src + 2];
            let a = image.rgba[src + 3];
            *idx_slot = nearest_index(&expanded, r, g, b) as u8;
            if a >= 0x80 {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                mask[bi] |= 1 << bit;
            }
        }
        push_planar_row(
            &mut planar_rows,
            &indices,
            6,
            row_bytes,
            if has_mask_plane { Some(&mask) } else { None },
        );
    }
    Ok(planar_rows)
}

fn encode_ehb_body_resolving(image: &IlbmImage) -> Result<(Vec<u8>, Compression)> {
    let rows = encode_ehb_planar_rows(image)?;
    Ok(pack_body_resolving(rows, image.bmhd.compression))
}

/// HAM6 / HAM8 BODY encoder. For each pixel we cost four candidate
/// ops (palette lookup + modify-R/G/B) against the running channel
/// state and emit the cheapest by squared distance to the source.
///
/// Selection rules per spec:
/// * op `0b00 val=v` — palette[v]; cost = |target − palette[v]|^2.
/// * op `0b01 val=v` — modify B = widen(v); R/G held; cost vs
///   target keeping the same R/G.
/// * op `0b10 val=v` — modify R = widen(v); G/B held.
/// * op `0b11 val=v` — modify G = widen(v); R/B held.
///
/// The widening function matches the decoder: HAM6 (`bits == 4`)
/// replicates the nibble; HAM8 (`bits == 6`) shifts left by 2 and
/// fills the bottom 2 bits with the top of the value.
fn encode_ham_body_resolving(image: &IlbmImage) -> Result<(Vec<u8>, Compression)> {
    let rows = encode_ham_planar_rows(image)?;
    Ok(pack_body_resolving(rows, image.bmhd.compression))
}

fn encode_ham_planar_rows(image: &IlbmImage) -> Result<Vec<Vec<u8>>> {
    let bmhd = image.bmhd;
    let bits = match bmhd.n_planes {
        6 => 4u8,
        8 => 6u8,
        other => {
            return Err(Error::invalid(format!(
                "HAM encode: n_planes must be 6 (HAM6) or 8 (HAM8), got {other}"
            )))
        }
    };
    let value_mask: u8 = (1u8 << bits) - 1;
    let widen = |val: u8| -> u8 {
        match bits {
            4 => (val << 4) | val,
            6 => (val << 2) | (val >> 4),
            _ => val,
        }
    };
    // Pre-compute every widened channel value once.
    let mut widened = [0u8; 64];
    for v in 0..=value_mask {
        widened[v as usize] = widen(v);
    }
    let cost = |a: u8, b: u8| -> i32 {
        let d = a as i32 - b as i32;
        d * d
    };
    let row_bytes = bmhd.row_bytes();
    let has_mask_plane = bmhd.masking == Masking::HasMask;
    let mut planar_rows: Vec<Vec<u8>> = Vec::with_capacity(
        bmhd.height as usize * (bmhd.n_planes as usize + has_mask_plane as usize),
    );
    for y in 0..bmhd.height as usize {
        // Per-row palette: SHAM overrides if present.
        let row_palette: Vec<[u8; 3]> = if let Some(s) = &image.sham {
            if let Some(p) = s.palettes.get(y) {
                p.clone()
            } else {
                image.palette.clone()
            }
        } else {
            image.palette.clone()
        };
        let mut indices = vec![0u8; bmhd.width as usize];
        let mut mask = vec![0u8; row_bytes];
        // HAM state starts from black at the start of every row.
        let mut r: u8 = 0;
        let mut g: u8 = 0;
        let mut b: u8 = 0;
        for (x, idx_slot) in indices.iter_mut().enumerate() {
            let src = (y * bmhd.width as usize + x) * 4;
            let tr = image.rgba[src];
            let tg = image.rgba[src + 1];
            let tb = image.rgba[src + 2];
            let ta = image.rgba[src + 3];

            // Candidate 1: palette lookup.
            let pal_max = (1u8 << bits) as usize;
            let pal_search = row_palette.len().min(pal_max);
            let mut best_op: u8 = 0;
            let mut best_val: u8 = 0;
            let mut best_cost: i32 = i32::MAX;
            let mut best_rgb = [r, g, b];
            for (i, p) in row_palette.iter().take(pal_search).enumerate() {
                let c = cost(tr, p[0]) + cost(tg, p[1]) + cost(tb, p[2]);
                if c < best_cost {
                    best_cost = c;
                    best_op = 0b00;
                    best_val = i as u8;
                    best_rgb = [p[0], p[1], p[2]];
                }
            }
            // Candidates 2–4: modify B / R / G holding the other two.
            // Search the channel that is being modified for the
            // closest widened value.
            // Modify B (op = 0b01): R/G held, B varies.
            for v in 0..=value_mask {
                let nb = widened[v as usize];
                let c = cost(tr, r) + cost(tg, g) + cost(tb, nb);
                if c < best_cost {
                    best_cost = c;
                    best_op = 0b01;
                    best_val = v;
                    best_rgb = [r, g, nb];
                }
            }
            // Modify R (op = 0b10): G/B held, R varies.
            for v in 0..=value_mask {
                let nr = widened[v as usize];
                let c = cost(tr, nr) + cost(tg, g) + cost(tb, b);
                if c < best_cost {
                    best_cost = c;
                    best_op = 0b10;
                    best_val = v;
                    best_rgb = [nr, g, b];
                }
            }
            // Modify G (op = 0b11): R/B held, G varies.
            for v in 0..=value_mask {
                let ng = widened[v as usize];
                let c = cost(tr, r) + cost(tg, ng) + cost(tb, b);
                if c < best_cost {
                    best_cost = c;
                    best_op = 0b11;
                    best_val = v;
                    best_rgb = [r, ng, b];
                }
            }

            *idx_slot = (best_op << bits) | (best_val & value_mask);
            r = best_rgb[0];
            g = best_rgb[1];
            b = best_rgb[2];
            if ta >= 0x80 {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                mask[bi] |= 1 << bit;
            }
        }
        push_planar_row(
            &mut planar_rows,
            &indices,
            bmhd.n_planes,
            row_bytes,
            if has_mask_plane { Some(&mask) } else { None },
        );
    }
    Ok(planar_rows)
}

/// PBM chunky BODY encoder. One palette-index byte per pixel, padded
/// to an even-byte row stride. Returns body bytes plus resolved compression.
fn encode_pbm_body_resolving(image: &IlbmImage) -> Result<(Vec<u8>, Compression)> {
    let bmhd = image.bmhd;
    let stride = (bmhd.width as usize + 1) & !1;
    let mut indices_all: Vec<u8> = Vec::with_capacity(stride * bmhd.height as usize);
    let pal: Vec<[u8; 3]> = if image.camg.is_ehb() && image.palette.len() <= 32 {
        expand_ehb_palette(&image.palette)
    } else {
        image.palette.clone()
    };
    let use_transparent_key = bmhd.masking == Masking::HasTransparentColor;
    for y in 0..bmhd.height as usize {
        let mut row = vec![0u8; stride];
        for x in 0..bmhd.width as usize {
            let src = (y * bmhd.width as usize + x) * 4;
            let r = image.rgba[src];
            let g = image.rgba[src + 1];
            let b = image.rgba[src + 2];
            let a = image.rgba[src + 3];
            row[x] = if use_transparent_key && a < 0x80 {
                bmhd.transparent_color as u8
            } else {
                nearest_index(&pal, r, g, b) as u8
            };
        }
        indices_all.extend_from_slice(&row);
    }
    // Build row slices so pack_body_resolving can try both modes.
    let rows: Vec<Vec<u8>> = indices_all
        .chunks_exact(stride)
        .map(|c| c.to_vec())
        .collect();
    Ok(pack_body_resolving(rows, bmhd.compression))
}

// ───────────────────── Demuxer ─────────────────────

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    // Outer FORM.
    let hdr = read_chunk_header(&mut *input)?.ok_or_else(|| Error::invalid("ILBM: empty file"))?;
    if hdr.id != GROUP_FORM {
        return Err(Error::invalid(format!(
            "ILBM: expected FORM chunk, got {}",
            hdr.id_str()
        )));
    }
    let form_type = read_form_type(&mut *input)?;
    if &form_type != b"ILBM" && &form_type != b"PBM " {
        return Err(Error::invalid(format!(
            "IFF: not an ILBM/PBM file (form type {:?})",
            std::str::from_utf8(&form_type).unwrap_or("????")
        )));
    }
    // Read the rest of the FORM into memory and let parse_ilbm walk it.
    // ILBM files are static images (kilobytes-to-megabytes), not
    // streams — buffering the whole FORM keeps the decode path simple.
    let body_size = hdr.size as u64 - 4;
    let mut form_body = vec![0u8; body_size as usize];
    input.read_exact(&mut form_body)?;

    // Reconstruct a contiguous buffer with the FORM header so we can
    // hand it to parse_ilbm verbatim.
    let mut full = Vec::with_capacity(8 + 4 + form_body.len());
    full.extend_from_slice(b"FORM");
    full.extend_from_slice(&hdr.size.to_be_bytes());
    full.extend_from_slice(&form_type);
    full.extend_from_slice(&form_body);

    let image = parse_ilbm(&full)?;
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(image.width);
    params.height = Some(image.height);
    params.pixel_format = Some(PixelFormat::Rgba);

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    };

    Ok(Box::new(IlbmDemuxer {
        streams: vec![stream],
        image: Some(image),
    }))
}

struct IlbmDemuxer {
    streams: Vec<StreamInfo>,
    image: Option<IlbmImage>,
}

impl Demuxer for IlbmDemuxer {
    fn format_name(&self) -> &str {
        "iff_ilbm"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        let img = self.image.take().ok_or(Error::Eof)?;
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, img.rgba);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.duration = Some(1);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }
    fn metadata(&self) -> &[(String, String)] {
        &[]
    }
    fn duration_micros(&self) -> Option<i64> {
        None
    }
}

// ───────────────────── Muxer ─────────────────────

fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(IlbmMuxer::new(output, streams)?))
}

/// Encoder mode picked by [`IlbmMuxer`] when assembling the BODY.
///
/// The muxer's default is [`MuxerMode::IndexedAuto`] — it greedily
/// builds an indexed palette from the first frame and emits 1..=8
/// bitplanes plus a `CMAP`. Switch to [`MuxerMode::Ham6`] /
/// [`MuxerMode::Ham8`] / [`MuxerMode::Ehb`] / [`MuxerMode::Pbm`] for
/// the matching ILBM viewport / form variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MuxerMode {
    /// Indexed planar `FORM/ILBM`. Plane count = ceil(log2(palette))
    /// clamped to 1..=8. CAMG omitted unless the caller sets one.
    #[default]
    IndexedAuto,
    /// HAM6 — 6 bitplanes, CAMG=HAM. Palette is the 16-entry table
    /// built from the first `write_packet`. The encoder picks per-pixel
    /// op codes to approximate the source RGBA.
    Ham6,
    /// HAM8 — 8 bitplanes, CAMG=HAM. Palette is the first 64 unique
    /// RGB triples seen in the source.
    Ham8,
    /// EHB — 6 bitplanes, CAMG=EHB. Palette is 32 unique entries
    /// expanded to 64 by halving each channel.
    Ehb,
    /// Chunky `FORM/PBM ` (DPaint II / Brilliance). 8 bits per pixel,
    /// 1 byte per pixel BODY. Caller's palette must fit in 256 entries.
    Pbm,
}

/// Container-level ILBM / PBM muxer. Accepts a single `rawvideo`
/// stream with `PixelFormat::Rgba`. The emitted file's encoder mode
/// follows [`MuxerMode`] (default [`MuxerMode::IndexedAuto`]) and
/// compression follows [`Compression`] (default
/// [`Compression::Auto`]).
pub struct IlbmMuxer {
    output: Box<dyn WriteSeek>,
    width: u32,
    height: u32,
    compression: Compression,
    mode: MuxerMode,
    masking: Masking,
    transparent_color: u16,
    written: bool,
    pending: Vec<u8>,
}

impl IlbmMuxer {
    pub fn new(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        if streams.len() != 1 {
            return Err(Error::unsupported("ILBM supports exactly one video stream"));
        }
        let s = &streams[0];
        if s.params.media_type != MediaType::Video {
            return Err(Error::invalid("ILBM stream must be video"));
        }
        if s.params.pixel_format != Some(PixelFormat::Rgba) {
            return Err(Error::unsupported(
                "ILBM muxer requires PixelFormat::Rgba (round 1)",
            ));
        }
        let width = s
            .params
            .width
            .ok_or_else(|| Error::invalid("ILBM muxer: missing width"))?;
        let height = s
            .params
            .height
            .ok_or_else(|| Error::invalid("ILBM muxer: missing height"))?;
        Ok(Self {
            output,
            width,
            height,
            compression: Compression::Auto,
            mode: MuxerMode::IndexedAuto,
            masking: Masking::None,
            transparent_color: 0,
            written: false,
            pending: Vec::new(),
        })
    }

    /// Choose a compression mode (default: `Auto` — tries both and
    /// emits the shorter result).
    pub fn with_compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }

    /// Choose the encoder mode (default: indexed planar).
    pub fn with_mode(mut self, m: MuxerMode) -> Self {
        self.mode = m;
        self
    }

    /// Configure how alpha / transparency is encoded into the BODY.
    /// `Masking::HasMask` writes an extra bit-plane per row;
    /// `Masking::HasTransparentColor` reserves a palette index keyed
    /// by `transparent_color` for fully-transparent pixels.
    /// Has no effect in [`MuxerMode::Pbm`] (chunky variant doesn't
    /// support a mask plane).
    pub fn with_masking(mut self, masking: Masking, transparent_color: u16) -> Self {
        self.masking = masking;
        self.transparent_color = transparent_color;
        self
    }
}

/// Build an RGBA→indexed quantiser keyed by exact RGB equality first,
/// then by nearest-neighbour squared-distance. Used by HAM/EHB/PBM
/// muxer paths where the palette size cap is prescribed (16/64/32/256).
fn build_palette_capped(rgba: &[u8], cap: usize) -> Vec<[u8; 3]> {
    let mut palette: Vec<[u8; 3]> = Vec::new();
    for px in rgba.chunks_exact(4) {
        let triple = [px[0], px[1], px[2]];
        if !palette.contains(&triple) && palette.len() < cap {
            palette.push(triple);
        }
    }
    palette
}

/// Build a ≤256-entry palette by collecting unique RGB triples in the
/// order they first appear in `rgba`. Returns the palette plus a
/// best-fit u8 index buffer of length `width * height`.
fn build_palette(rgba: &[u8]) -> (Vec<[u8; 3]>, Vec<u8>) {
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut indices = Vec::with_capacity(rgba.len() / 4);
    for px in rgba.chunks_exact(4) {
        let triple = [px[0], px[1], px[2]];
        let pos = palette.iter().position(|&p| p == triple);
        let idx = if let Some(p) = pos {
            p
        } else if palette.len() < 256 {
            palette.push(triple);
            palette.len() - 1
        } else {
            // Closest existing entry by squared distance.
            let mut best = 0usize;
            let mut best_d = i32::MAX;
            for (i, p) in palette.iter().enumerate() {
                let dr = triple[0] as i32 - p[0] as i32;
                let dg = triple[1] as i32 - p[1] as i32;
                let db = triple[2] as i32 - p[2] as i32;
                let d = dr * dr + dg * dg + db * db;
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            best
        };
        indices.push(idx as u8);
    }
    (palette, indices)
}

impl Muxer for IlbmMuxer {
    fn format_name(&self) -> &str {
        "iff_ilbm"
    }
    fn write_header(&mut self) -> Result<()> {
        Ok(()) // header is emitted lazily at write_trailer time
    }
    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_empty() {
            self.pending.extend_from_slice(&packet.data);
        } else {
            return Err(Error::unsupported(
                "ILBM muxer: round 1 emits one frame per file (single packet)",
            ));
        }
        Ok(())
    }
    fn write_trailer(&mut self) -> Result<()> {
        if self.written {
            return Ok(());
        }
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if self.pending.len() != expected {
            return Err(Error::invalid(format!(
                "ILBM muxer: packet size {} does not match width*height*4 = {}",
                self.pending.len(),
                expected
            )));
        }

        // Plane count, palette, CAMG flags + form type are mode-driven.
        let (palette, n_planes, camg, form_type) = match self.mode {
            MuxerMode::IndexedAuto => {
                let (pal, _) = build_palette(&self.pending);
                let np = if pal.len() <= 1 {
                    1
                } else {
                    let bits = (pal.len() as u32 - 1).next_power_of_two().trailing_zeros();
                    bits.max(1) as u8
                };
                (pal, np, Camg::default(), *b"ILBM")
            }
            MuxerMode::Ham6 => {
                // HAM6: 6 bitplanes, palette serves the op-0b00 lookup
                // (16 entries max for the 4-bit value field).
                let pal = build_palette_capped(&self.pending, 16);
                (pal, 6u8, Camg { raw: CAMG_HAM }, *b"ILBM")
            }
            MuxerMode::Ham8 => {
                // HAM8: 8 bitplanes, up to 64 palette entries.
                let pal = build_palette_capped(&self.pending, 64);
                (pal, 8u8, Camg { raw: CAMG_HAM }, *b"ILBM")
            }
            MuxerMode::Ehb => {
                // EHB: 32-entry palette mirrored to 64 by halving;
                // 6 bitplanes total.
                let pal = build_palette_capped(&self.pending, 32);
                (pal, 6u8, Camg { raw: CAMG_EHB }, *b"ILBM")
            }
            MuxerMode::Pbm => {
                let pal = build_palette_capped(&self.pending, 256);
                // PBM mandates 8 bits per pixel; n_planes = 8 even
                // when the palette is smaller, since the BODY is one
                // byte per pixel.
                (pal, 8u8, Camg::default(), *b"PBM ")
            }
        };

        if palette.is_empty() {
            return Err(Error::invalid("ILBM muxer: empty input palette"));
        }
        // PBM disallows HasMask plane (no bitplane interleave).
        let masking = if self.mode == MuxerMode::Pbm && self.masking == Masking::HasMask {
            Masking::None
        } else {
            self.masking
        };

        let bmhd = Bmhd {
            width: self.width as u16,
            height: self.height as u16,
            x_origin: 0,
            y_origin: 0,
            n_planes,
            masking,
            compression: self.compression,
            pad: 0,
            transparent_color: self.transparent_color,
            x_aspect: 1,
            y_aspect: 1,
            page_width: self.width as i16,
            page_height: self.height as i16,
        };
        let img = IlbmImage {
            width: self.width,
            height: self.height,
            bmhd,
            palette,
            camg,
            form_type,
            rgba: std::mem::take(&mut self.pending),
            ..IlbmImage::default()
        };
        let bytes = encode_ilbm(&img)?;
        self.output.write_all(&bytes)?;
        self.output.flush()?;
        self.written = true;
        Ok(())
    }
}

// Pad helper retained for symmetry with svx.rs even though parse_ilbm
// does its own buffered walk; useful if a future caller wants to
// stream chunks directly off the IFF reader.
#[allow(dead_code)]
fn pad_after<R: Seek + ?Sized>(r: &mut R, c: &ChunkHeader) -> Result<()> {
    if c.size & 1 == 1 {
        r.seek(SeekFrom::Current(1))?;
    }
    Ok(())
}

#[allow(dead_code)]
fn skip_unknown<R: Seek + ?Sized>(r: &mut R, c: &ChunkHeader) -> Result<()> {
    skip_chunk_body(r, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_palette() -> Vec<[u8; 3]> {
        vec![
            [0, 0, 0],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 0],
            [255, 0, 255],
            [0, 255, 255],
            [255, 255, 255],
        ]
    }

    /// Build a 16x4 indexed image: each row is a horizontal sweep
    /// through the 8-entry palette repeated twice.
    fn synth_indexed(w: u16, h: u16, palette: &[[u8; 3]]) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for y in 0..h {
            for x in 0..w {
                let i = (x as usize + y as usize) % palette.len();
                let p = palette[i];
                rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
            }
        }
        rgba
    }

    fn make_image(compression: Compression) -> IlbmImage {
        let palette = solid_palette();
        let rgba = synth_indexed(16, 4, &palette);
        let bmhd = Bmhd {
            width: 16,
            height: 4,
            x_origin: 0,
            y_origin: 0,
            n_planes: 3, // 8 colours = 3 bitplanes
            masking: Masking::None,
            compression,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: 16,
            page_height: 4,
        };
        IlbmImage {
            width: 16,
            height: 4,
            bmhd,
            palette,
            camg: Camg::default(),
            rgba,
            ..IlbmImage::default()
        }
    }

    #[test]
    fn roundtrip_uncompressed() {
        let img = make_image(Compression::None);
        let bytes = encode_ilbm(&img).unwrap();
        let dec = parse_ilbm(&bytes).unwrap();
        assert_eq!(dec.width, img.width);
        assert_eq!(dec.height, img.height);
        assert_eq!(dec.rgba, img.rgba, "uncompressed pixels round-trip exactly");
    }

    #[test]
    fn roundtrip_byterun1() {
        let img = make_image(Compression::ByteRun1);
        let bytes = encode_ilbm(&img).unwrap();
        let dec = parse_ilbm(&bytes).unwrap();
        assert_eq!(dec.width, img.width);
        assert_eq!(dec.height, img.height);
        assert_eq!(dec.rgba, img.rgba, "ByteRun1 pixels round-trip exactly");
    }

    #[test]
    fn byterun1_codec_basic_roundtrip() {
        let row: Vec<u8> = vec![0, 0, 0, 0, 1, 2, 3, 4, 5, 5, 5, 5, 5, 5, 9, 8, 7, 7, 7, 7];
        let enc = byterun1_encode_row(&row);
        let mut dec = Vec::new();
        let consumed = byterun1_decode_row(&enc, row.len(), &mut dec).unwrap();
        assert_eq!(consumed, enc.len());
        assert_eq!(dec, row);
    }

    #[test]
    fn byterun1_handles_max_run() {
        // 200 identical bytes — exceeds the 128-cap of one repeat run.
        let row: Vec<u8> = vec![0xAA; 200];
        let enc = byterun1_encode_row(&row);
        let mut dec = Vec::new();
        byterun1_decode_row(&enc, row.len(), &mut dec).unwrap();
        assert_eq!(dec, row);
    }

    #[test]
    fn byterun1_handles_nop_byte() {
        // NOP (0x80) followed by a literal-1 of (0x00, 0x42).
        let enc = vec![0x80, 0x00, 0x42];
        let mut dec = Vec::new();
        byterun1_decode_row(&enc, 1, &mut dec).unwrap();
        assert_eq!(dec, vec![0x42]);
    }

    #[test]
    fn planar_packed_roundtrip() {
        // 16-pixel row, 3 planes — pack arbitrary indices and unpack.
        let indices: Vec<u8> = (0..16u8).map(|x| x % 8).collect();
        let planes = indices_to_planar_row(&indices, 3, 2);
        let plane_refs: Vec<&[u8]> = planes.iter().map(|p| p.as_slice()).collect();
        let recovered = planar_row_to_indices(&plane_refs, 16);
        assert_eq!(recovered, indices);
    }

    #[test]
    fn ehb_palette_doubles_with_half_brightness() {
        let mut pal: Vec<[u8; 3]> = (0..32).map(|i| [i * 4, i * 4, i * 4]).collect();
        pal[31] = [0xFE, 0xFE, 0xFE];
        let expanded = expand_ehb_palette(&pal);
        assert_eq!(expanded.len(), 64);
        for i in 0..32 {
            assert_eq!(expanded[i + 32][0], expanded[i][0] >> 1);
            assert_eq!(expanded[i + 32][1], expanded[i][1] >> 1);
            assert_eq!(expanded[i + 32][2], expanded[i][2] >> 1);
        }
    }

    /// Hand-craft a 4-pixel HAM6 row: palette lookup, then modify B,
    /// then modify R, then modify G. Verify the resulting RGB triples
    /// reflect the carry-over state.
    #[test]
    fn ham6_row_carries_state() {
        // HAM6: top 2 bits = op, low 4 bits = value.
        // Palette index 1 is solid red.
        let palette = vec![[0u8, 0, 0], [255, 0, 0]];
        // pixel 0: op=00 val=1 → palette[1] = (255, 0, 0).
        // pixel 1: op=01 val=0xF → modify B = 0xFF → (255, 0, 255).
        // pixel 2: op=10 val=0x0 → modify R = 0   → (0, 0, 255).
        // pixel 3: op=11 val=0x8 → modify G = 0x88 → (0, 136, 255).
        let indices: Vec<u8> = vec![0b00_0001, 0b01_1111, 0b10_0000, 0b11_1000];
        let row = expand_ham_row(&indices, &palette, 4);
        assert_eq!(row[0], [255, 0, 0]);
        assert_eq!(row[1], [255, 0, 0xFF]);
        assert_eq!(row[2], [0, 0, 0xFF]);
        assert_eq!(row[3], [0, 0x88, 0xFF]);
    }

    /// Build a HAM6 file end-to-end: 8 pixels × 1 row, CAMG flag set,
    /// 6 bitplanes, palette of 16. Verify the decoder recognises HAM
    /// and reproduces the expected per-pixel RGB.
    #[test]
    fn parse_ham6_recognises_camg_flag() {
        // Hand-roll a tiny HAM6 file. Width 8, height 1, 6 planes,
        // ByteRun1 compression off.
        // Palette is unused for op != 00.
        let palette: Vec<[u8; 3]> = vec![[0; 3]; 16];
        // Indices: 4 modify-Blue followed by 4 modify-Red ramps
        // (op=01 = modify B, op=10 = modify R, op=11 = modify G,
        // op=00 = palette lookup).
        let indices: Vec<u8> = vec![
            0b01_0000, 0b01_0100, 0b01_1000, 0b01_1111, 0b10_0011, 0b10_0111, 0b10_1011, 0b10_1111,
        ];
        let row_bytes = 8_usize.div_ceil(16) * 2;
        let planar = indices_to_planar_row(&indices, 6, row_bytes);

        // Build BODY = concat of plane-rows.
        let mut body = Vec::new();
        for p in &planar {
            body.extend_from_slice(p);
        }

        // Build the FORM/ILBM file by hand: BMHD, CMAP, CAMG, BODY.
        let mut out = Vec::new();
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"ILBM");

        let bmhd = Bmhd {
            width: 8,
            height: 1,
            x_origin: 0,
            y_origin: 0,
            n_planes: 6,
            masking: Masking::None,
            compression: Compression::None,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: 8,
            page_height: 1,
        };
        out.extend_from_slice(b"BMHD");
        out.extend_from_slice(&20u32.to_be_bytes());
        out.extend_from_slice(&bmhd.write());

        out.extend_from_slice(b"CMAP");
        let cmap_size = (palette.len() * 3) as u32;
        out.extend_from_slice(&cmap_size.to_be_bytes());
        for c in &palette {
            out.extend_from_slice(c);
        }
        if cmap_size & 1 == 1 {
            out.push(0);
        }

        out.extend_from_slice(b"CAMG");
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&CAMG_HAM.to_be_bytes());

        out.extend_from_slice(b"BODY");
        let body_size = body.len() as u32;
        out.extend_from_slice(&body_size.to_be_bytes());
        out.extend_from_slice(&body);
        if body_size & 1 == 1 {
            out.push(0);
        }
        let form_size = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&form_size.to_be_bytes());

        let img = parse_ilbm(&out).unwrap();
        assert!(img.camg.is_ham(), "CAMG HAM flag should be detected");
        assert_eq!(img.width, 8);
        assert_eq!(img.height, 1);
        assert_eq!(img.bmhd.n_planes, 6);

        // Compare against expand_ham_row's reference output.
        let expected = expand_ham_row(&indices, &palette, 4);
        for (x, exp) in expected.iter().enumerate() {
            let off = x * 4;
            assert_eq!(img.rgba[off], exp[0], "pixel {x} R");
            assert_eq!(img.rgba[off + 1], exp[1], "pixel {x} G");
            assert_eq!(img.rgba[off + 2], exp[2], "pixel {x} B");
            assert_eq!(img.rgba[off + 3], 0xFF, "pixel {x} A");
        }
    }

    #[test]
    fn probe_recognises_form_ilbm() {
        let mut bytes = vec![0u8; 12];
        bytes[0..4].copy_from_slice(b"FORM");
        bytes[8..12].copy_from_slice(b"ILBM");
        let p = oxideav_core::ProbeData {
            buf: &bytes,
            ext: None,
        };
        assert_eq!(probe(&p), 100);
    }

    #[test]
    fn rejects_non_ilbm_form() {
        let mut bytes = vec![0u8; 12];
        bytes[0..4].copy_from_slice(b"FORM");
        bytes[8..12].copy_from_slice(b"AIFF");
        let err = parse_ilbm(&bytes).unwrap_err();
        assert!(matches!(err, Error::InvalidData(_)));
    }

    /// HAM8 (8-plane / 6-bit channel value) — verify the channel
    /// widening replicates the top 2 bits into the bottom 2.
    #[test]
    fn ham8_widens_channel_to_8_bits() {
        // Palette unused for op != 00.
        let palette: Vec<[u8; 3]> = vec![[0; 3]; 64];
        // Indices: op=01 (modify B) val=0b111111 → 6-bit 0x3F.
        // widen(0x3F) = (0x3F << 2) | (0x3F >> 4) = 0xFC | 0x03 = 0xFF.
        let indices = vec![0b01_111111u8];
        let row = expand_ham_row(&indices, &palette, 6);
        assert_eq!(row[0], [0, 0, 0xFF]);

        // val=0b101010 → widen = (0x2A << 2) | (0x2A >> 4) = 0xA8 | 0x02 = 0xAA.
        let indices = vec![0b01_101010u8];
        let row = expand_ham_row(&indices, &palette, 6);
        assert_eq!(row[0], [0, 0, 0xAA]);
    }

    /// Mask plane: HasMask masking should produce alpha-0 for any
    /// pixel whose mask bit is unset.
    #[test]
    fn parse_with_has_mask_alpha_keys_off_mask_plane() {
        // Build an 8x1 image, 1 plane, palette [black, white], mask
        // = 0b1010_1010 (every other pixel opaque).
        let palette: Vec<[u8; 3]> = vec![[0, 0, 0], [255, 255, 255]];
        // Colour plane: all 1s = all-white (0xFF for the high 8 bits).
        // Mask plane: 0b1010_1010 = 0xAA. Each plane row is 2 bytes
        // (16 pixels rounded to a 16-bit word, only first 8 used).
        let colour_plane = vec![0xFFu8, 0x00];
        let mask_plane = vec![0xAAu8, 0x00];

        let bmhd = Bmhd {
            width: 8,
            height: 1,
            x_origin: 0,
            y_origin: 0,
            n_planes: 1,
            masking: Masking::HasMask,
            compression: Compression::None,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: 8,
            page_height: 1,
        };

        let mut out = Vec::new();
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"BMHD");
        out.extend_from_slice(&20u32.to_be_bytes());
        out.extend_from_slice(&bmhd.write());

        out.extend_from_slice(b"CMAP");
        let cmap_size = (palette.len() * 3) as u32;
        out.extend_from_slice(&cmap_size.to_be_bytes());
        for c in &palette {
            out.extend_from_slice(c);
        }
        if cmap_size & 1 == 1 {
            out.push(0);
        }

        // BODY: one row, plane 0 (colour) then mask plane.
        let mut body = Vec::new();
        body.extend_from_slice(&colour_plane);
        body.extend_from_slice(&mask_plane);
        out.extend_from_slice(b"BODY");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
        let form_size = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&form_size.to_be_bytes());

        let img = parse_ilbm(&out).unwrap();
        assert_eq!(img.width, 8);
        assert_eq!(img.height, 1);
        // Colour: every pixel = white (palette[1]).
        // Alpha: bits of 0xAA from MSB → 1 0 1 0 1 0 1 0.
        let expected_alphas = [0xFFu8, 0, 0xFF, 0, 0xFF, 0, 0xFF, 0];
        for (x, &expected_a) in expected_alphas.iter().enumerate() {
            assert_eq!(img.rgba[x * 4], 0xFF, "R pixel {x}");
            assert_eq!(img.rgba[x * 4 + 1], 0xFF, "G pixel {x}");
            assert_eq!(img.rgba[x * 4 + 2], 0xFF, "B pixel {x}");
            assert_eq!(img.rgba[x * 4 + 3], expected_a, "alpha pixel {x}");
        }
    }
}
