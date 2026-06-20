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

// ───────────────────── DEST (Destination Merge) ─────────────────────

/// `DEST` — destination-merge property describing how to scatter source
/// bitplanes into a deeper destination image. Eight bytes on disk:
///
/// ```text
/// UBYTE depth        // # bitplanes in the original source
/// UBYTE pad1         // 0 on write; ignored on read
/// UWORD planePick    // 1 bit = "consume next source plane here"
/// UWORD planeOnOff   // default bit when planePick bit is 0
/// UWORD planeMask    // 1 bit = write to destination bitplane
/// ```
///
/// All `UWORD` fields are big-endian; only the low `depth` bits matter
/// for the destination bitmap, higher-order bits are unused. With no
/// `DEST` chunk the implicit default is `planePick == planeMask ==
/// (1 << nPlanes) - 1` and `planeOnOff == 0` (i.e. one-to-one mapping
/// of source planes into the destination, every plane written, zero
/// fill where no source plane).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dest {
    /// Number of bitplanes in the source image (the `nPlanes` carried
    /// by `BMHD`).
    pub depth: u8,
    /// Pad byte from the on-disk layout. Spec says "put 0 here"; kept
    /// so a parse → write round-trip preserves the original byte.
    pub pad1: u8,
    /// `planePick` mask: a `1` bit consumes the next source bitplane
    /// into the destination bitplane at the same bit position. The
    /// count of `1` bits is expected to equal `depth`.
    pub plane_pick: u16,
    /// `planeOnOff` mask: at destination bitplanes whose `planePick`
    /// bit is `0`, this bit is broadcast to every pixel of that plane
    /// instead of pulling from a source plane.
    pub plane_on_off: u16,
    /// `planeMask` mask: a `1` bit means "write to this destination
    /// bitplane"; a `0` bit means "leave the destination bitplane
    /// untouched" (the receiver's existing pixels remain).
    pub plane_mask: u16,
}

impl Dest {
    /// Parse a `DEST` chunk body. Eight bytes required; the spec
    /// fixes the layout to `depth/pad1/planePick/planeOnOff/planeMask`.
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "ILBM DEST: need 8 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            depth: body[0],
            pad1: body[1],
            plane_pick: u16::from_be_bytes([body[2], body[3]]),
            plane_on_off: u16::from_be_bytes([body[4], body[5]]),
            plane_mask: u16::from_be_bytes([body[6], body[7]]),
        })
    }
    /// Serialise to the 8-byte on-disk form.
    pub fn write(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = self.depth;
        out[1] = self.pad1;
        out[2..4].copy_from_slice(&self.plane_pick.to_be_bytes());
        out[4..6].copy_from_slice(&self.plane_on_off.to_be_bytes());
        out[6..8].copy_from_slice(&self.plane_mask.to_be_bytes());
        out
    }

    /// True when `plane_pick` has exactly `depth` `1` bits set in its
    /// low `depth` positions. The spec phrases this as a soft
    /// expectation; callers building synthetic `DEST` chunks can use
    /// this to sanity-check their wire bytes.
    pub fn pick_count_matches_depth(&self) -> bool {
        let mask = if self.depth >= 16 {
            0xFFFFu16
        } else {
            (1u16 << self.depth).wrapping_sub(1)
        };
        (self.plane_pick & mask).count_ones() == self.depth as u32
    }
}

// ───────────────────── SPRT (Sprite Precedence) ─────────────────────

/// `SPRT` — Sprite Precedence property. A single big-endian
/// `UWORD` carrying the sprite layering hint defined by the ILBM
/// supplement §2.7: the chunk's presence flags the ILBM "as
/// intended as a sprite", and its `precedence` is "relative
/// precedence, 0 is the highest" (foremost). Reader programs may
/// honour the precedence or override it; the supplement also notes
/// that mapping an ILBM into an Amiga hardware sprite has setup
/// rules of its own (e.g. a 2-plane sprite uses
/// `transparentColor == 0` and remaps `CMAP` registers 1..=3 to
/// the hardware sprite's three colour registers).
///
/// On-disk layout: two bytes, no pad needed (size 2 is even).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sprt {
    /// Relative sprite precedence. `0` denotes the foremost
    /// (highest-priority) sprite; larger values sit further back.
    /// The spec uses `UWORD`, so the full unsigned 16-bit range
    /// `0..=0xFFFF` is legal.
    pub precedence: u16,
}

impl Sprt {
    /// Sentinel for the supplement's "foremost" sprite — the
    /// `precedence == 0` slot.
    pub const FOREMOST: u16 = 0;

    /// Parse a `SPRT` chunk body. Two big-endian bytes; the
    /// supplement fixes the chunk to a single `UWORD
    /// SpritePrecedence`.
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 2 {
            return Err(Error::invalid(format!(
                "ILBM SPRT: need 2 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            precedence: u16::from_be_bytes([body[0], body[1]]),
        })
    }

    /// Serialise to the 2-byte on-disk form.
    pub fn write(&self) -> [u8; 2] {
        self.precedence.to_be_bytes()
    }

    /// `true` when this sprite holds the supplement's
    /// foremost-precedence slot (`precedence == 0`).
    pub fn is_foremost(&self) -> bool {
        self.precedence == Self::FOREMOST
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

    /// True when no per-scanline palette entries are present (e.g. the
    /// chunk decoded but every row would just re-use `CMAP`). Useful
    /// for callers wanting to skip the SHAM render fast-path.
    pub fn is_empty(&self) -> bool {
        self.palettes.is_empty()
    }

    /// Number of scanlines covered by an explicit SHAM palette. May be
    /// less than the image height when the chunk was short — the
    /// parser pads in that case but a caller that wants the "explicit
    /// stride" can compare against this.
    pub fn rows(&self) -> usize {
        self.palettes.len()
    }

    /// Borrow the SHAM palette for scanline `y` without allocating.
    /// Returns `None` when `y` is past the last stored palette (which
    /// only happens for callers that bypassed [`Sham::parse`]'s
    /// `expected_height` padding — the parsed-from-bytes path always
    /// has a palette per row in `0..expected_height`).
    pub fn row_palette(&self, y: u32) -> Option<&[[u8; 3]]> {
        self.palettes.get(y as usize).map(|v| v.as_slice())
    }

    /// Resolve the effective 16-entry palette to use when expanding
    /// HAM6 op-`0b00` lookups on scanline `y`.
    ///
    /// When a SHAM palette exists for `y` the SHAM palette is returned
    /// verbatim (16 RGB entries, RGB444 widened to 8-bit by the
    /// parser). When SHAM is short — fewer parsed rows than the
    /// requested `y` — the caller-supplied `base` palette is returned
    /// truncated/padded to 16 entries with `[0, 0, 0]` as fallback.
    ///
    /// Mirrors [`Pchg::palette_at_line`] in shape: per-row palette
    /// resolution returning an owned 16-entry CMAP that can be fed
    /// directly into [`expand_ham_row`]. The `base` fallback exists
    /// for parity with that helper; callers that want the raw
    /// "explicit-only or nothing" view should use [`Sham::row_palette`]
    /// instead.
    ///
    /// [`expand_ham_row`]: crate::ilbm::expand_ham_row
    pub fn palette_at_line(&self, base: &[[u8; 3]], y: u32) -> Vec<[u8; 3]> {
        match self.palettes.get(y as usize) {
            Some(pal) => pal.clone(),
            None => {
                let mut out: Vec<[u8; 3]> = base.iter().take(16).copied().collect();
                while out.len() < 16 {
                    out.push([0, 0, 0]);
                }
                out
            }
        }
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

/// Which of the two PCHG change-record encodings a chunk uses, decoded
/// from the 16-bit `Flags` field in the PCHG header.
///
/// Per the PCHG IFF Annex header layout, two flag bits select the
/// per-change record encoding:
///
/// * Flag bit `1` (`0x0001`) — `SmallLineChanges`: 12-bit channel
///   palette; each change record is `(u8 RegisterIndex, u16
///   RGB444 big-endian)` for a 3-byte payload.
/// * Flag bit `2` (`0x0002`) — `BigLineChanges`: 24-bit channel
///   palette; each change record is `(u16 RegisterIndex, u8 R, u8 G,
///   u8 B)` for a 5-byte payload, with a 2-byte `u16` ChangeCount
///   instead of the Small format's 1-byte count.
///
/// The two bits are mutually exclusive — both being set is a malformed
/// PCHG and rejected by [`Pchg::parse`]. When neither bit is set the
/// annex defines the encoding to default to Small; we report that as
/// [`PchgKind::Small`] so callers can rely on a non-`Option` accessor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PchgKind {
    /// 12-bit channel encoding — 1-byte RegisterIndex + 2-byte RGB444
    /// per change, 1-byte ChangeCount per line.
    Small,
    /// 24-bit channel encoding — 2-byte RegisterIndex + 3-byte RGB888
    /// per change, 2-byte ChangeCount per line.
    Big,
}

/// Decoded PCHG header — the 20-byte fixed-layout prefix in front of
/// every PCHG chunk's change records.
///
/// All fields are surfaced verbatim as parsed off the wire. Together
/// they describe the change-record encoding ([`Self::kind`]), the
/// scanline range the chunk covers (`start_line` / `line_count`), and
/// the four header hints the annex defines as upper-bound summaries of
/// the change records that follow (`changed_lines` / `min_reg` /
/// `max_reg` / `max_changes` / `total_changes`).
///
/// The header hints aren't load-bearing for decode — [`Pchg::parse`] is
/// permissive about mismatches because historical PCHG-generating tools
/// have been inconsistent about them — but they're useful to callers
/// authoring PCHG-aware editors that want to validate or re-derive the
/// hints after editing the per-line change list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PchgHeader {
    /// `Compression` (u16) — `0` = uncompressed change records (the
    /// only mode [`Pchg::parse`] actually decodes), `1` = the
    /// annex-defined Huffman-compressed mode (not yet decoded; the
    /// raw bytes still round-trip via [`Pchg::raw`]).
    pub compression: u16,
    /// `Flags` (u16) — the raw 16-bit flag word. Bit 0 selects Small,
    /// bit 1 selects Big; higher bits are reserved. [`Self::kind`]
    /// returns the [`PchgKind`] derived from this field.
    pub flags: u16,
    /// `StartLine` (i16) — first scanline (zero-based, may be
    /// negative to address a row above the ILBM origin) covered by
    /// the change records.
    pub start_line: i16,
    /// `LineCount` (u16) — total number of scanlines the change
    /// records cover, starting at `start_line`. Lines with zero
    /// changes contribute an empty per-line record but still count.
    pub line_count: u16,
    /// `ChangedLines` (u16) — number of scanlines in `LineCount`
    /// whose per-line record carries at least one change (i.e. lines
    /// with ChangeCount > 0). An optimisation hint.
    pub changed_lines: u16,
    /// `MinReg` (u16) — smallest palette register index touched by
    /// any change record in the chunk.
    pub min_reg: u16,
    /// `MaxReg` (u16) — largest palette register index touched by
    /// any change record in the chunk.
    pub max_reg: u16,
    /// `MaxChanges` (u16) — highest per-line `ChangeCount` seen in
    /// the chunk.
    pub max_changes: u16,
    /// `TotalChanges` (u32) — sum of every per-line `ChangeCount`
    /// across every covered scanline.
    pub total_changes: u32,
}

impl PchgHeader {
    /// Decode the [`PchgKind`] from [`Self::flags`].
    ///
    /// Returns `Big` when flag bit 1 is set, otherwise `Small` (the
    /// annex's documented default when no flag bits are set, and the
    /// only valid choice when bit 0 is set). [`Pchg::parse`] rejects
    /// the both-bits-set case before this struct is constructed, so
    /// the choice here is unambiguous on any header produced by the
    /// parser.
    pub fn kind(&self) -> PchgKind {
        if self.flags & 2 != 0 {
            PchgKind::Big
        } else {
            PchgKind::Small
        }
    }

    /// True when `Compression == 1`, the annex-defined Huffman-
    /// compressed change-record encoding. [`Pchg::parse`] does not
    /// decode that variant yet; callers detecting this should fall
    /// back to the raw bytes via [`Pchg::raw`] if they need the
    /// original payload.
    pub fn is_compressed(&self) -> bool {
        self.compression == 1
    }
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

    /// Return the cumulative palette state at the start of scanline
    /// `y`, given a starting `base` palette.
    ///
    /// Walks every entry in [`Pchg::lines`] whose `line` is `<= y` and
    /// applies its register overwrites in document order, leaving the
    /// rest of `base` untouched. Out-of-range indices (`>= base.len()`)
    /// are skipped silently — same tolerance as the parser, since
    /// PCHG-generating tools have historically been permissive about
    /// the upper-bound register count.
    ///
    /// Convenience: callers walking every scanline can fold the state
    /// themselves and avoid re-walking the whole list per `y`; this
    /// helper is intended for one-off "show me the palette at row N"
    /// queries from animation viewers.
    pub fn palette_at_line(&self, base: &[[u8; 3]], y: u32) -> Vec<[u8; 3]> {
        let mut cur = base.to_vec();
        for entry in &self.lines {
            if entry.line > y {
                break;
            }
            for ch in &entry.changes {
                let i = ch.index as usize;
                if i < cur.len() {
                    cur[i] = ch.rgb;
                }
            }
        }
        cur
    }

    /// Decode the 20-byte PCHG header from [`Self::raw`] into a typed
    /// [`PchgHeader`].
    ///
    /// Returns `None` when `raw` is shorter than 20 bytes — that can
    /// only happen for `Pchg` values built by hand (e.g. in tests)
    /// since [`Pchg::parse`] rejects short bodies before construction.
    /// On any `Pchg` produced by the parser this always returns
    /// `Some`.
    pub fn header(&self) -> Option<PchgHeader> {
        if self.raw.len() < 20 {
            return None;
        }
        let r = &self.raw;
        Some(PchgHeader {
            compression: u16::from_be_bytes([r[0], r[1]]),
            flags: u16::from_be_bytes([r[2], r[3]]),
            start_line: i16::from_be_bytes([r[4], r[5]]),
            line_count: u16::from_be_bytes([r[6], r[7]]),
            changed_lines: u16::from_be_bytes([r[8], r[9]]),
            min_reg: u16::from_be_bytes([r[10], r[11]]),
            max_reg: u16::from_be_bytes([r[12], r[13]]),
            max_changes: u16::from_be_bytes([r[14], r[15]]),
            total_changes: u32::from_be_bytes([r[16], r[17], r[18], r[19]]),
        })
    }

    /// Convenience: [`PchgHeader::kind`] for the underlying header,
    /// or `None` when the raw buffer is too short for a header. On
    /// any parser-produced `Pchg` this always returns `Some`.
    pub fn kind(&self) -> Option<PchgKind> {
        self.header().map(|h| h.kind())
    }

    /// Compute the four PCHG header-hint fields (`ChangedLines`,
    /// `MinReg`, `MaxReg`, `MaxChanges`, `TotalChanges`) directly
    /// from [`Self::lines`].
    ///
    /// The annex defines these as upper-bound summaries an encoder
    /// fills in for downstream readers. This helper computes the
    /// canonical values from the decoded change records so callers
    /// can either validate a parsed header against the records that
    /// follow it ([`Self::header_matches_payload`]) or re-derive the
    /// hints after editing the change list before re-encoding.
    ///
    /// Returns the tuple
    /// `(changed_lines, min_reg, max_reg, max_changes, total_changes)`
    /// with `min_reg` / `max_reg` set to `0` when no changes are
    /// present (matching the annex's treatment of empty PCHGs as
    /// `MinReg == MaxReg == 0`).
    pub fn derive_header_hints(&self) -> (u16, u16, u16, u16, u32) {
        let mut changed_lines: u16 = 0;
        let mut min_reg: u16 = u16::MAX;
        let mut max_reg: u16 = 0;
        let mut max_changes: u16 = 0;
        let mut total_changes: u32 = 0;
        for line in &self.lines {
            if line.changes.is_empty() {
                continue;
            }
            changed_lines = changed_lines.saturating_add(1);
            let cc = line.changes.len() as u32;
            total_changes = total_changes.saturating_add(cc);
            let cc16 = u16::try_from(cc).unwrap_or(u16::MAX);
            if cc16 > max_changes {
                max_changes = cc16;
            }
            for ch in &line.changes {
                if ch.index < min_reg {
                    min_reg = ch.index;
                }
                if ch.index > max_reg {
                    max_reg = ch.index;
                }
            }
        }
        if changed_lines == 0 {
            min_reg = 0;
            max_reg = 0;
        }
        (changed_lines, min_reg, max_reg, max_changes, total_changes)
    }

    /// True when every header hint in [`Self::header`] agrees with
    /// the corresponding canonical value derived from
    /// [`Self::lines`].
    ///
    /// Specifically:
    ///
    /// * `changed_lines` matches the number of lines with a non-empty
    ///   change list.
    /// * `min_reg` / `max_reg` bracket every change record's
    ///   `RegisterIndex` (when no changes are present, both must be
    ///   `0`).
    /// * `max_changes` matches the longest per-line change list.
    /// * `total_changes` matches the sum of every per-line change
    ///   count.
    ///
    /// Returns `false` when the header is absent (raw too short to
    /// decode) or when any hint disagrees. Mirrors the validation
    /// surface other AIFF/ILBM chunks expose (e.g. `MarkerChunk`'s
    /// `id`-uniqueness check) so editors can flag hint drift after
    /// modifying the change list.
    pub fn header_matches_payload(&self) -> bool {
        let Some(h) = self.header() else {
            return false;
        };
        let (cl, lo, hi, mc, tc) = self.derive_header_hints();
        h.changed_lines == cl
            && h.min_reg == lo
            && h.max_reg == hi
            && h.max_changes == mc
            && h.total_changes == tc
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

    /// Apply `steps` palette-cycle ticks to `palette` in place.
    ///
    /// Rotates the closed range `[low..=high]` by `steps` slots. Each
    /// tick moves the *contents* of every slot in the window one
    /// position forward (or backward when [`is_reverse`] is set):
    ///
    /// * Forward: `pal[low+i]` becomes `old_pal[low + (i - 1).mod range_len]`.
    /// * Reverse: `pal[low+i]` becomes `old_pal[low + (i + 1).mod range_len]`.
    ///
    /// Returns `false` (and leaves `palette` unchanged) when the cycle
    /// has nothing to do — inactive flag, zero-length range, malformed
    /// `low > high`, range tail past the palette length, or
    /// `range_len == 1` (one-slot rotation is the identity). Returns
    /// `true` when the rotation was actually applied.
    ///
    /// `steps` is taken modulo `range_len()`, so very large step counts
    /// reduce to one in-range walk. This makes it cheap to skip ahead by
    /// arbitrary tick counts without an O(steps) loop.
    ///
    /// The caller is responsible for picking how many ticks to apply
    /// per wall-clock frame; [`cycles_per_second`] gives the
    /// DPaint-spec rate at 60 Hz vertical blank.
    ///
    /// [`is_reverse`]: Self::is_reverse
    /// [`cycles_per_second`]: Self::cycles_per_second
    pub fn cycle_step(&self, palette: &mut [[u8; 3]], steps: u32) -> bool {
        if !self.is_active() {
            return false;
        }
        let len = self.range_len() as usize;
        if len < 2 {
            return false;
        }
        if self.high as usize >= palette.len() {
            return false;
        }
        let lo = self.low as usize;
        let hi = self.high as usize;
        let k = (steps as usize) % len;
        if k == 0 {
            return false;
        }
        // Forward: each value moves +1 slot per tick (so reading the
        // pre-rotation buffer, slot `lo + i` after one tick should hold
        // what slot `lo + (i + len - 1) % len` held before — the value
        // that was one position "behind" it). Reverse is the inverse.
        let tmp: Vec<[u8; 3]> = palette[lo..=hi].to_vec();
        for i in 0..len {
            let src = if self.is_reverse() {
                (i + k) % len
            } else {
                (i + len - k) % len
            };
            palette[lo + i] = tmp[src];
        }
        true
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

    /// Apply `steps` palette-cycle ticks to `palette` in place,
    /// rotating the closed range `[start..=end]` per [`direction`].
    ///
    /// Forward (`direction > 0`) moves slot contents toward higher
    /// indices; reverse (`direction < 0`) moves them toward lower
    /// indices; `direction == 0` is a no-op (matches [`is_active`] ==
    /// `false`).
    ///
    /// Returns `false` (palette unchanged) when the cycle is inactive,
    /// the range is malformed (`start > end`), the range tail lies past
    /// the palette length, the range spans fewer than two slots, or
    /// `steps` reduces to 0 mod `range_len()`. Returns `true` when the
    /// rotation actually mutated the palette.
    ///
    /// `steps` is taken modulo `range_len()` so callers can pass an
    /// accumulated tick counter without ever paying an O(steps) cost.
    /// Use [`delay_seconds`] to convert wall-clock time into a tick
    /// count for the next frame.
    ///
    /// [`direction`]: Self::direction
    /// [`is_active`]: Self::is_active
    /// [`delay_seconds`]: Self::delay_seconds
    pub fn cycle_step(&self, palette: &mut [[u8; 3]], steps: u32) -> bool {
        if !self.is_active() {
            return false;
        }
        let len = self.range_len() as usize;
        if len < 2 {
            return false;
        }
        if self.end as usize >= palette.len() {
            return false;
        }
        let lo = self.start as usize;
        let hi = self.end as usize;
        let k = (steps as usize) % len;
        if k == 0 {
            return false;
        }
        let tmp: Vec<[u8; 3]> = palette[lo..=hi].to_vec();
        for i in 0..len {
            let src = if self.is_reverse() {
                (i + k) % len
            } else {
                (i + len - k) % len
            };
            palette[lo + i] = tmp[src];
        }
        true
    }
}

// ───────────────────── DRNG (DPaint IV Extended Range Cycling) ─────────────────────

/// A true-colour cell inside a [`Drng`] descriptor: at the given
/// palette-index `cell` the cycling sequence inserts the explicit RGB
/// triple `(r, g, b)` rather than re-using the current `CMAP` entry.
///
/// Per the public DeluxePaint IV manual / EA IFF 85 supplement, true-
/// colour cells let an extended range cycle through colours that have
/// no permanent home in the 32-entry palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrngTrueCell {
    pub cell: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A palette-register cell inside a [`Drng`] descriptor: at the given
/// `cell` slot the cycle uses the current contents of `CMAP[index]`
/// (i.e. follows that palette register's live value, rather than a
/// frozen RGB triple).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrngRegCell {
    pub cell: u8,
    pub index: u8,
}

/// `DRNG` — DeluxePaint IV Extended Range Cycling. A super-set of
/// [`Crng`] that lets the cycling range insert *true-colour* RGB
/// samples and/or *follow* live palette registers at arbitrary
/// positions inside the `[min, max]` index window.
///
/// Layout (variable-length, per the public EA IFF 85 DPaint IV
/// supplement / DeluxePaint IV manual):
///
/// ```text
/// u8  min          (low palette index of the cycle, inclusive)
/// u8  max          (high palette index of the cycle, inclusive)
/// i16 rate         (palette-rotation rate; one step every
///                   16384 / rate vertical-blank ticks at 60 Hz)
/// i16 flags        (bit 0 = active, bit 2 = DP_RGB    (has true cells),
///                                  bit 3 = DP_REGS   (has register cells))
/// u8  ntrue        (number of DrngTrueCell entries that follow)
/// u8  nregs        (number of DrngRegCell  entries that follow)
/// DrngTrueCell × ntrue        (each 4 bytes: cell, r, g, b)
/// DrngRegCell  × nregs        (each 2 bytes: cell, index)
/// ```
///
/// The chunk is therefore `8 + 4*ntrue + 2*nregs` bytes; the parser
/// validates that the payload length matches `ntrue` / `nregs`.
///
/// As with [`Crng`] and [`Ccrt`], this crate does *not* animate; the
/// cycle descriptor is preserved verbatim so consumers can render
/// their own palette walks against `image.palette`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Drng {
    pub min: u8,
    pub max: u8,
    pub rate: i16,
    pub flags: i16,
    pub trues: Vec<DrngTrueCell>,
    pub regs: Vec<DrngRegCell>,
}

impl Drng {
    /// DRNG flag bit: range is active (cycling enabled).
    pub const FLAG_ACTIVE: i16 = 0x0001;
    /// DRNG flag bit: range carries true-colour `DrngTrueCell` entries.
    pub const FLAG_DP_RGB: i16 = 0x0004;
    /// DRNG flag bit: range carries palette-register `DrngRegCell`
    /// entries.
    pub const FLAG_DP_REGS: i16 = 0x0008;

    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "ILBM DRNG: need 8-byte header, got {}",
                body.len()
            )));
        }
        let min = body[0];
        let max = body[1];
        let rate = i16::from_be_bytes([body[2], body[3]]);
        let flags = i16::from_be_bytes([body[4], body[5]]);
        let ntrue = body[6] as usize;
        let nregs = body[7] as usize;
        let need = 8 + 4 * ntrue + 2 * nregs;
        if body.len() < need {
            return Err(Error::invalid(format!(
                "ILBM DRNG: need {need} bytes for ntrue={ntrue} nregs={nregs}, got {}",
                body.len()
            )));
        }
        let mut trues = Vec::with_capacity(ntrue);
        let mut cursor = 8;
        for _ in 0..ntrue {
            trues.push(DrngTrueCell {
                cell: body[cursor],
                r: body[cursor + 1],
                g: body[cursor + 2],
                b: body[cursor + 3],
            });
            cursor += 4;
        }
        let mut regs = Vec::with_capacity(nregs);
        for _ in 0..nregs {
            regs.push(DrngRegCell {
                cell: body[cursor],
                index: body[cursor + 1],
            });
            cursor += 2;
        }
        Ok(Self {
            min,
            max,
            rate,
            flags,
            trues,
            regs,
        })
    }

    /// Serialise this `DRNG` into its on-disk byte form (no chunk
    /// header — just the payload).
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 4 * self.trues.len() + 2 * self.regs.len());
        out.push(self.min);
        out.push(self.max);
        out.extend_from_slice(&self.rate.to_be_bytes());
        out.extend_from_slice(&self.flags.to_be_bytes());
        // ntrue / nregs are u8 — DPaint IV manual caps each list at 255
        // entries (a 6-bit cell index can address at most 64 slots in
        // practice).  Clamp defensively rather than truncate.
        out.push(self.trues.len().min(u8::MAX as usize) as u8);
        out.push(self.regs.len().min(u8::MAX as usize) as u8);
        for c in &self.trues {
            out.push(c.cell);
            out.push(c.r);
            out.push(c.g);
            out.push(c.b);
        }
        for c in &self.regs {
            out.push(c.cell);
            out.push(c.index);
        }
        out
    }

    /// True if the cycling range is enabled (`flags & FLAG_ACTIVE`).
    pub fn is_active(&self) -> bool {
        self.flags & Self::FLAG_ACTIVE != 0
    }

    /// True if the descriptor advertises at least one `DrngTrueCell`.
    /// Matches the `DP_RGB` flag bit but is also robust against
    /// generators that set the flag without writing any cells (or
    /// vice-versa).
    pub fn has_true_cells(&self) -> bool {
        !self.trues.is_empty() || (self.flags & Self::FLAG_DP_RGB != 0)
    }

    /// True if the descriptor advertises at least one `DrngRegCell`.
    pub fn has_reg_cells(&self) -> bool {
        !self.regs.is_empty() || (self.flags & Self::FLAG_DP_REGS != 0)
    }

    /// Cycle rate in steps per second on a 60 Hz vertical-blank tick
    /// (mirrors [`Crng::cycles_per_second`]).
    pub fn cycles_per_second(&self) -> f32 {
        if self.rate <= 0 {
            0.0
        } else {
            60.0 * (self.rate as f32) / 16384.0
        }
    }

    /// Number of palette entries spanned by the cycle (inclusive of
    /// both ends). Returns 0 if `min > max`.
    pub fn range_len(&self) -> u16 {
        if self.min > self.max {
            0
        } else {
            (self.max - self.min) as u16 + 1
        }
    }

    /// Apply `steps` palette-cycle ticks to `palette` in place,
    /// rotating the closed range `[min..=max]` forward.
    ///
    /// DRNG is the DeluxePaint IV super-set of [`Crng`]: in addition to
    /// rotating the palette slots in `[min..=max]`, it can splice
    /// *true-colour cells* (frozen RGB values) and *register cells*
    /// (live mirrors of other palette slots) at arbitrary positions
    /// inside the range. The cells are *positional* — they describe
    /// "at cell index `cell` substitute this RGB / this register". The
    /// in-tree spec material defines the cell list but does not specify
    /// the per-tick semantics for how cells animate alongside the
    /// rotation, so this helper does the conservative thing: it rotates
    /// the contiguous range exactly as [`Crng::cycle_step`] would, and
    /// leaves the cell list untouched. Callers that want cell-aware
    /// animation can layer their own splice on top by walking
    /// [`Drng::trues`] / [`Drng::regs`] after the rotation.
    ///
    /// DRNG's wire format has no reverse-direction flag; the rotation is
    /// always forward (toward higher indices). Returns `false` (and
    /// leaves the palette untouched) on inactive flag, malformed range
    /// (`min > max`), range past the palette tail, single-slot range,
    /// or `steps` reducing to 0 mod `range_len()`. Returns `true` when
    /// the rotation mutated the palette.
    ///
    /// [`Crng::cycle_step`]: Crng::cycle_step
    /// [`Drng::trues`]: Self::trues
    /// [`Drng::regs`]: Self::regs
    pub fn cycle_step(&self, palette: &mut [[u8; 3]], steps: u32) -> bool {
        if !self.is_active() {
            return false;
        }
        let len = self.range_len() as usize;
        if len < 2 {
            return false;
        }
        if self.max as usize >= palette.len() {
            return false;
        }
        let lo = self.min as usize;
        let hi = self.max as usize;
        let k = (steps as usize) % len;
        if k == 0 {
            return false;
        }
        let tmp: Vec<[u8; 3]> = palette[lo..=hi].to_vec();
        for i in 0..len {
            let src = (i + len - k) % len;
            palette[lo + i] = tmp[src];
        }
        true
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

// ───────────────────── True-colour 24-bit ─────────────────────

/// Decode a `BMHD.n_planes == 24` true-colour ILBM `BODY` into a
/// pre-allocated `rgba` buffer. Each scanline emits 24 plane-rows of
/// `bmhd.row_bytes()` bytes: 8 red planes (bit 0 first, LSB-first), then
/// 8 green planes, then 8 blue planes. ByteRun1 (`Compression::ByteRun1`)
/// is applied per-plane-per-row exactly as in the indexed planar path.
///
/// HasMask / HasTransparentColor are ignored in this mode — alpha is
/// always written as `0xFF`. The EGFF / fileformat.info ILBM article
/// states the masking byte is "almost always 0" on true-colour files
/// because there is no `BMHD.transparent_color` lookup for literal RGB.
fn decode_truecolor24_into(bmhd: &Bmhd, body: &[u8], rgba: &mut [u8]) -> Result<()> {
    let row_bytes = bmhd.row_bytes();
    let width = bmhd.width as usize;
    let height = bmhd.height as usize;
    // 24 colour planes; mask plane is illegal for true-colour bodies.
    if bmhd.masking == Masking::HasMask {
        return Err(Error::unsupported(
            "ILBM 24-bit true-colour: HasMask plane is not defined for literal-RGB BODY",
        ));
    }
    let planes_per_row = 24usize;

    let mut rows_planar: Vec<Vec<u8>> = Vec::with_capacity(height * planes_per_row);
    match bmhd.compression {
        Compression::None => {
            let needed = height * planes_per_row * row_bytes;
            if body.len() < needed {
                return Err(Error::invalid(format!(
                    "ILBM 24-bit uncompressed BODY: need {needed} bytes, got {}",
                    body.len()
                )));
            }
            for chunk in body[..needed].chunks_exact(row_bytes) {
                rows_planar.push(chunk.to_vec());
            }
        }
        Compression::ByteRun1 => {
            let mut input = body;
            for _ in 0..height {
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

    for y in 0..height {
        let row_base = y * planes_per_row;
        // Three 8-plane groups: red (planes 0..=7), green (8..=15), blue (16..=23).
        for x in 0..width {
            let byte_idx = x / 8;
            let bit = 7 - (x % 8);
            let mut r: u8 = 0;
            let mut g: u8 = 0;
            let mut b: u8 = 0;
            // Plane k inside a channel contributes bit k (k=0 is LSB).
            for k in 0..8 {
                let pr = &rows_planar[row_base + k];
                let pg = &rows_planar[row_base + 8 + k];
                let pb = &rows_planar[row_base + 16 + k];
                if byte_idx < pr.len() && (pr[byte_idx] >> bit) & 1 == 1 {
                    r |= 1 << k;
                }
                if byte_idx < pg.len() && (pg[byte_idx] >> bit) & 1 == 1 {
                    g |= 1 << k;
                }
                if byte_idx < pb.len() && (pb[byte_idx] >> bit) & 1 == 1 {
                    b |= 1 << k;
                }
            }
            let dst = (y * width + x) * 4;
            rgba[dst] = r;
            rgba[dst + 1] = g;
            rgba[dst + 2] = b;
            rgba[dst + 3] = 0xFF;
        }
    }
    Ok(())
}

/// Build the 24 plane-rows for one scanline of a true-colour ILBM
/// encode. `rgba_row` carries `width * 4` bytes of source RGBA pixels.
/// Plane layout matches [`decode_truecolor24_into`]: red bit 0 first,
/// red bit 7 last; then green LSB→MSB; then blue LSB→MSB. Each plane
/// row is `row_bytes` bytes wide (rounded up to a 16-bit word).
fn encode_truecolor24_row(rgba_row: &[u8], width: u16, row_bytes: usize) -> Vec<Vec<u8>> {
    let mut planes: Vec<Vec<u8>> = (0..24).map(|_| vec![0u8; row_bytes]).collect();
    for x in 0..width as usize {
        let byte_idx = x / 8;
        let bit = 7 - (x % 8);
        let r = rgba_row[x * 4];
        let g = rgba_row[x * 4 + 1];
        let b = rgba_row[x * 4 + 2];
        for k in 0..8 {
            if (r >> k) & 1 == 1 {
                planes[k][byte_idx] |= 1 << bit;
            }
            if (g >> k) & 1 == 1 {
                planes[8 + k][byte_idx] |= 1 << bit;
            }
            if (b >> k) & 1 == 1 {
                planes[16 + k][byte_idx] |= 1 << bit;
            }
        }
    }
    planes
}

// ───────────────────── Palette helpers ─────────────────────

/// Resolve the effective palette at the start of scanline `y` for the
/// given image.
///
/// When `image.pchg` is `Some`, the returned palette is the cumulative
/// PCHG state at `y` — `image.palette` with every PCHG register
/// override whose `line <= y` applied in document order. When PCHG is
/// absent the call returns `image.palette` verbatim. EHB / HAM
/// expansion is *not* applied; this is the raw, pre-expansion CMAP
/// state suitable as input to [`expand_ehb_palette`] or
/// [`expand_ham_row`].
///
/// `y >= image.height` clamps to the image's last row's state — the
/// PCHG list is exhausted and every entry has already been folded.
///
/// This is a thin convenience wrapper around [`Pchg::palette_at_line`]
/// that hides the `Option<Pchg>` plumbing; consumers writing a "render
/// scanline `y` with cycling" path call it once per row.
pub fn palette_for_line(image: &IlbmImage, y: u32) -> Vec<[u8; 3]> {
    match &image.pchg {
        Some(pchg) => pchg.palette_at_line(&image.palette, y),
        None => image.palette.clone(),
    }
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
    /// Optional `DEST` destination-merge descriptor. Captures how the
    /// source's `nPlanes` bitplanes scatter into a deeper destination
    /// bitmap (Amiga "merge into a `depth`-deep viewport" pattern).
    pub dest: Option<Dest>,
    /// Optional `SPRT` sprite-precedence flag. Presence marks the
    /// ILBM "as intended as a sprite"; the wrapped `precedence`
    /// follows the ILBM supplement §2.7 (`0 = foremost`).
    pub sprt: Option<Sprt>,
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
    /// `DRNG` extended-range cycling descriptors (DeluxePaint IV).
    /// Order is preserved so round-trip is byte-stable.
    pub drngs: Vec<Drng>,
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
            dest: None,
            sprt: None,
            sham: None,
            pchg: None,
            crngs: Vec::new(),
            ccrts: Vec::new(),
            drngs: Vec::new(),
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
    let mut dest: Option<Dest> = None;
    let mut sprt: Option<Sprt> = None;
    let mut sham_raw: Option<Vec<u8>> = None;
    let mut pchg: Option<Pchg> = None;
    let mut crngs: Vec<Crng> = Vec::new();
    let mut ccrts: Vec<Ccrt> = Vec::new();
    let mut drngs: Vec<Drng> = Vec::new();

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
            b"DEST" => dest = Some(Dest::parse(payload)?),
            b"SPRT" => sprt = Some(Sprt::parse(payload)?),
            b"SHAM" => sham_raw = Some(payload.to_vec()),
            b"PCHG" => pchg = Some(Pchg::parse(payload)?),
            b"CRNG" => crngs.push(Crng::parse(payload)?),
            b"CCRT" => ccrts.push(Ccrt::parse(payload)?),
            b"DRNG" => drngs.push(Drng::parse(payload)?),
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
            dest,
            sprt,
            sham,
            pchg,
            crngs,
            ccrts,
            drngs,
            rgba,
        });
    }

    // 24-bit literal-RGB ILBM path. fileformat.info / EGFF §3.3.4 (and
    // Encyclopedia of Graphics File Formats, Murray & vanRyper 1996, ch.
    // "IFF File Format Summary") specify that when `BMHD.BitPlanes == 24`
    // and no `CMAP` is present the BODY holds literal RGB pixels with
    // bitplanes laid out as 8 red planes (LSB first), then 8 green, then
    // 8 blue per scanline. Mask-plane (HasMask) and transparent-colour
    // keying are not meaningful in this mode — alpha is always opaque.
    if bmhd.n_planes == 24 {
        decode_truecolor24_into(&bmhd, &body, &mut rgba)?;
        return Ok(IlbmImage {
            width,
            height,
            bmhd,
            palette,
            camg,
            form_type,
            grab,
            dest,
            sprt,
            sham,
            pchg,
            crngs,
            ccrts,
            drngs,
            rgba,
        });
    }

    // Planar indexed ILBM path (existing behaviour).
    let n_planes = bmhd.n_planes as usize;
    if n_planes == 0 || n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ILBM: indexed planar supports 1..=8 colour bitplanes (got {n_planes}); \
             use n_planes=24 for literal-RGB true-colour"
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
        dest,
        sprt,
        sham,
        pchg,
        crngs,
        ccrts,
        drngs,
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
    let is_pbm = &image.form_type == b"PBM ";
    let is_truecolor24 = !is_pbm && bmhd.n_planes == 24;
    if !is_truecolor24 && image.palette.is_empty() {
        return Err(Error::unsupported(
            "ILBM encode: indexed paths require a non-empty palette \
             (use n_planes=24 for literal-RGB true-colour with no CMAP)",
        ));
    }
    if is_pbm && bmhd.n_planes != 8 {
        return Err(Error::invalid(format!(
            "PBM encode: requires n_planes=8 (got {})",
            bmhd.n_planes
        )));
    }
    if is_truecolor24 && (image.camg.is_ham() || image.camg.is_ehb()) {
        return Err(Error::invalid(
            "ILBM 24-bit true-colour: HAM/EHB CAMG flags are exclusive to indexed planar bodies",
        ));
    }

    // Build BODY bytes per branch. When compression is Auto the body
    // encoder returns the winning bytes; we must also learn which mode
    // won so we can write the correct byte into BMHD.
    let (body_bytes, resolved_compression): (Vec<u8>, Compression) = if is_pbm {
        encode_pbm_body_resolving(image)?
    } else if is_truecolor24 {
        encode_truecolor24_body_resolving(image)?
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

    // CMAP — emit only when a palette is present. True-colour 24-bit
    // ILBM files normally omit CMAP entirely (literal RGB pixels).
    if !image.palette.is_empty() {
        let cmap_size = (image.palette.len() * 3) as u32;
        out.extend_from_slice(b"CMAP");
        out.extend_from_slice(&cmap_size.to_be_bytes());
        for c in &image.palette {
            out.extend_from_slice(c);
        }
        if cmap_size & 1 == 1 {
            out.push(0);
        }
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

    // DEST — destination-merge descriptor (ILBM §2.6). Eight bytes;
    // even-sized so no pad byte. Emitted in the position fixed by the
    // spec grammar `BMHD [CMAP] [GRAB] [DEST] [SPRT] [CAMG]`.
    if let Some(d) = image.dest {
        out.extend_from_slice(b"DEST");
        out.extend_from_slice(&8u32.to_be_bytes());
        out.extend_from_slice(&d.write());
    }

    // SPRT — sprite-precedence flag (ILBM supplement §2.7). Two
    // bytes; even-sized so no pad byte. Spec grammar slots SPRT
    // between [DEST] and [CAMG]; Appendix A §6 also notes the
    // property chunks "may actually be in any order but all must
    // appear before the BODY chunk", so the placement is
    // grammar-faithful regardless of the order existing files use.
    if let Some(s) = image.sprt {
        out.extend_from_slice(b"SPRT");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&s.write());
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

    // DRNG (DPaint IV extended-range cycling — variable length:
    // 8-byte header + 4*ntrue + 2*nregs cell bytes. With nregs odd the
    // payload is odd-length and needs a pad byte). Emitted in
    // `image.drngs` order so a parse → encode round-trip is byte-stable.
    for d in &image.drngs {
        let payload = d.write();
        let sz = payload.len() as u32;
        out.extend_from_slice(b"DRNG");
        out.extend_from_slice(&sz.to_be_bytes());
        out.extend_from_slice(&payload);
        if sz & 1 == 1 {
            out.push(0);
        }
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

/// 24-bit true-colour planar encoder. Walks the source RGBA buffer
/// scanline-by-scanline, emits 24 plane-rows per scanline (R LSB→MSB,
/// G LSB→MSB, B LSB→MSB) and lets [`pack_body_resolving`] apply the
/// caller-chosen [`Compression`] (including `Auto`, which picks the
/// shorter of literal vs. ByteRun1 across the whole BODY).
fn encode_truecolor24_body_resolving(image: &IlbmImage) -> Result<(Vec<u8>, Compression)> {
    let bmhd = image.bmhd;
    let row_bytes = bmhd.row_bytes();
    let width = bmhd.width;
    let stride = bmhd.width as usize * 4;
    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(bmhd.height as usize * 24);
    for y in 0..bmhd.height as usize {
        let src = &image.rgba[y * stride..(y + 1) * stride];
        let plane_rows = encode_truecolor24_row(src, width, row_bytes);
        for pr in plane_rows {
            rows.push(pr);
        }
    }
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
    /// True-colour planar `FORM/ILBM` — 24 bitplanes (8 R, 8 G, 8 B),
    /// no `CMAP`, literal-RGB pixels per fileformat.info / EGFF §3.3.4.
    /// Output preserves the full source RGB; alpha is dropped because
    /// 24-bit ILBM has no defined mask-plane or transparent-colour key.
    /// LightWave 3D / NewTek Toaster IFF24 is the historical producer.
    TrueColor24,
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
            MuxerMode::TrueColor24 => {
                // No CMAP — literal RGB. 24 bitplanes (8 R, 8 G, 8 B).
                (Vec::new(), 24u8, Camg::default(), *b"ILBM")
            }
        };

        if palette.is_empty() && self.mode != MuxerMode::TrueColor24 {
            return Err(Error::invalid("ILBM muxer: empty input palette"));
        }
        // PBM disallows HasMask plane (no bitplane interleave). True-colour
        // 24-bit ILBM has no defined mask-plane or transparent-colour key,
        // so force-None for both flavours of masking on that path.
        let masking = if (self.mode == MuxerMode::Pbm && self.masking == Masking::HasMask)
            || self.mode == MuxerMode::TrueColor24
        {
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

// ───────────────────── FORM RGBN — Turbo Silver / Imagine 12-bit ─────────────────────
//
// RGBN is a distinct EA IFF 85 FORM type from Impulse's *Turbo Silver*
// (later *Imagine*), almost identical to FORM ILBM: same `BMHD`, an
// (unused) `CMAP`, and a `CAMG` viewport word. It differs from ILBM only
// in the **BODY** encoding and two `BMHD` fields:
//
//   * `BMHD.compression` is **4** — a Turbo-Silver-specific RLE, *not*
//     ILBM ByteRun1.
//   * `BMHD.nPlanes` is the nominal **13** (12 colour bits + 1 genlock),
//     even though the body is really chunky 12-bit RGB rather than 13
//     bitplanes.
//
// The BODY is a stream of 16-bit big-endian WORD units, each carrying a
// 12-bit RGB value (red = most-significant nibble, then green, then
// blue), one genlock bit, and a run-length count:
//
//   bit:  15 .............. 4   3        2 1 0
//         [ 12-bit RGB value ] [genlock] [3-bit count]
//
// Count cascade (the canonical RGBN sample's decode):
//   * 3-bit inline count holds runs 1..7.
//   * If the run > 7, the 3-bit field is 0 and a following **BYTE** holds
//     the count (up to 255).
//   * If the run > 255, that BYTE is 0 and a following **WORD** holds the
//     larger count. Runs > 65536 are not supported.
//
// Pixels are filled left-to-right within a scanline, top to bottom; a
// single run can spill across the right edge into the next scanline (the
// body is a flat pixel stream of `width * height` entries). The 12-bit
// RGB value is widened to RGB888 by bit replication (`x << 4 | x`).
//
// Source: docs/image/iff/iff-truecolor-chunks.md §3, §3.1, §3.3.

/// How the RGBN/RGB8 **genlock** bit is interpreted when expanding a
/// coded run into output pixels (§3.3 of the truecolor doc).
///
/// The [`Default`] is [`IgnoreUseColour`](GenlockPolicy::IgnoreUseColour):
/// the least-surprising choice for a still-image decode, where every coded
/// RGB value reaches the output and no pixel is silently blacked or made
/// transparent. A caller wanting genlock / brush semantics opts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GenlockPolicy {
    /// *Turbo Silver* "picture" semantics: a set genlock bit writes the
    /// **zero colour** (transparent-to-genlock black) into the pixel —
    /// emitted here as opaque black `(0, 0, 0, 0xFF)`. The RGB value in
    /// the coded unit is ignored for genlocked pixels.
    TurboSilverZeroColour,
    /// *Diamond / Light24* "load as picture" semantics: the genlock bit
    /// is **ignored** and the coded RGB value is always used (opaque).
    #[default]
    IgnoreUseColour,
    /// *Diamond / Light24* "load as brush" semantics: the genlock bit
    /// **marks pixels that are not part of the brush** — i.e. a
    /// transparency mask. Genlocked pixels get alpha `0`; the RGB value
    /// is still widened and stored under the transparent alpha.
    BrushTransparency,
}

/// Widen a 4-bit gun value (0..=15) to 8 bits by nibble replication, so
/// `0xF → 0xFF` and `0x0 → 0x00` map the 12-bit RGB range onto the full
/// 8-bit range.
#[inline]
fn widen4(x: u16) -> u8 {
    let n = (x & 0x0F) as u8;
    (n << 4) | n
}

/// Decode an RGBN (`compression == 4`) BODY of `width * height` 12-bit
/// genlock-RLE pixels into packed RGBA8888, row-major, top-to-bottom.
///
/// `genlock` selects how the genlock bit maps to output colour / alpha
/// (see [`GenlockPolicy`]). The function validates that the run stream
/// fills *exactly* `width * height` pixels — a stream that runs out early
/// or whose final run overshoots the pixel budget is rejected with
/// [`Error::invalid`], so a truncated or malformed body never silently
/// yields a partial frame or writes out of bounds.
pub fn decode_rgbn_body(
    width: u16,
    height: u16,
    body: &[u8],
    genlock: GenlockPolicy,
) -> Result<Vec<u8>> {
    let total: usize = width as usize * height as usize;
    let mut rgba = vec![0u8; total * 4];
    let mut filled = 0usize;
    let mut pos = 0usize;

    while filled < total {
        if pos + 2 > body.len() {
            return Err(Error::invalid(format!(
                "RGBN BODY: stream ended after {filled} of {total} pixels (need another WORD unit)"
            )));
        }
        let w = u16::from_be_bytes([body[pos], body[pos + 1]]);
        pos += 2;

        let rgb12 = w >> 4;
        let lock = w & 0x0008 != 0;
        let mut count = (w & 0x0007) as usize;

        // Count cascade: 3-bit 0 → BYTE; BYTE 0 → WORD.
        if count == 0 {
            if pos >= body.len() {
                return Err(Error::invalid(
                    "RGBN BODY: 3-bit count was 0 but no BYTE count follows",
                ));
            }
            count = body[pos] as usize;
            pos += 1;
            if count == 0 {
                if pos + 2 > body.len() {
                    return Err(Error::invalid(
                        "RGBN BODY: BYTE count was 0 but no WORD count follows",
                    ));
                }
                count = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
                pos += 1; // advance one then …
                pos += 1; // … the second WORD byte (keep arithmetic obvious)
                if count == 0 {
                    return Err(Error::invalid(
                        "RGBN BODY: WORD escape count is 0 (a zero-length run is undefined)",
                    ));
                }
            }
        }

        if filled + count > total {
            return Err(Error::invalid(format!(
                "RGBN BODY: run of {count} overshoots pixel budget ({filled} filled, {total} total)"
            )));
        }

        // Resolve this run's RGBA quadruple once, then splat it.
        let r = widen4(rgb12 >> 8);
        let g = widen4(rgb12 >> 4);
        let b = widen4(rgb12);
        let (or, og, ob, oa) = match genlock {
            GenlockPolicy::IgnoreUseColour => (r, g, b, 0xFF),
            GenlockPolicy::TurboSilverZeroColour => {
                if lock {
                    (0, 0, 0, 0xFF)
                } else {
                    (r, g, b, 0xFF)
                }
            }
            GenlockPolicy::BrushTransparency => {
                if lock {
                    (r, g, b, 0x00)
                } else {
                    (r, g, b, 0xFF)
                }
            }
        };

        for _ in 0..count {
            let dst = filled * 4;
            rgba[dst] = or;
            rgba[dst + 1] = og;
            rgba[dst + 2] = ob;
            rgba[dst + 3] = oa;
            filled += 1;
        }
    }

    Ok(rgba)
}

/// Decode an RGB8 (`compression == 4`) BODY of `width * height` 24-bit
/// genlock-RLE pixels into packed RGBA8888, row-major, top-to-bottom.
///
/// RGB8 is the 24-bit-per-pixel sibling of RGBN (§3.2 of the truecolor
/// doc): every coded unit is a **32-bit big-endian LONG**, MSB→LSB:
///
/// ```text
///  bit:  31 ................. 8   7      6 .... 0
///        [   24-bit RGB value   ] [genlock] [7-bit count]
/// ```
///
/// Red is the most-significant gun, then green, then blue (LSBs); each gun
/// is already a full 8 bits so no widening is needed. Unlike RGBN's
/// 3-bit-with-BYTE/WORD-cascade count, RGB8 carries a single inline **7-bit
/// repeat count** (runs `1..=127`): per §3.2 ¶ "Impulse never wrote more
/// than a 7-bit repeat count, and Imagine/Light24 only read the 7-bit
/// count", so there is no escape cascade. A `count` of `0` is therefore an
/// undefined zero-length run and is rejected.
///
/// `genlock` selects how the genlock bit maps to output colour / alpha
/// (see [`GenlockPolicy`]) — identical semantics to [`decode_rgbn_body`].
/// The function validates that the run stream fills *exactly*
/// `width * height` pixels — a stream that runs out early or whose final
/// run overshoots the pixel budget is rejected with [`Error::invalid`], so
/// a truncated or malformed body never silently yields a partial frame or
/// writes out of bounds. A single run may spill across the right edge into
/// the next scanline (the body is a flat pixel stream).
pub fn decode_rgb8_body(
    width: u16,
    height: u16,
    body: &[u8],
    genlock: GenlockPolicy,
) -> Result<Vec<u8>> {
    let total: usize = width as usize * height as usize;
    let mut rgba = vec![0u8; total * 4];
    let mut filled = 0usize;
    let mut pos = 0usize;

    while filled < total {
        if pos + 4 > body.len() {
            return Err(Error::invalid(format!(
                "RGB8 BODY: stream ended after {filled} of {total} pixels (need another LONG unit)"
            )));
        }
        let w = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;

        let rgb24 = w >> 8;
        let lock = w & 0x0000_0080 != 0;
        let count = (w & 0x0000_007F) as usize;

        if count == 0 {
            return Err(Error::invalid(
                "RGB8 BODY: 7-bit count is 0 (a zero-length run is undefined; RGB8 has no count escape)",
            ));
        }

        if filled + count > total {
            return Err(Error::invalid(format!(
                "RGB8 BODY: run of {count} overshoots pixel budget ({filled} filled, {total} total)"
            )));
        }

        // Resolve this run's RGBA quadruple once, then splat it. Each gun is
        // already a full byte (red = MSB, then green, then blue = LSB).
        let r = (rgb24 >> 16) as u8;
        let g = (rgb24 >> 8) as u8;
        let b = rgb24 as u8;
        let (or, og, ob, oa) = match genlock {
            GenlockPolicy::IgnoreUseColour => (r, g, b, 0xFF),
            GenlockPolicy::TurboSilverZeroColour => {
                if lock {
                    (0, 0, 0, 0xFF)
                } else {
                    (r, g, b, 0xFF)
                }
            }
            GenlockPolicy::BrushTransparency => {
                if lock {
                    (r, g, b, 0x00)
                } else {
                    (r, g, b, 0xFF)
                }
            }
        };

        for _ in 0..count {
            let dst = filled * 4;
            rgba[dst] = or;
            rgba[dst + 1] = og;
            rgba[dst + 2] = ob;
            rgba[dst + 3] = oa;
            filled += 1;
        }
    }

    Ok(rgba)
}

// ─────────────────── FORM RGB8 / RGBN — top-level decode ───────────────────
//
// The `decode_rgb8_body` / `decode_rgbn_body` functions above decode a bare
// Turbo-Silver run-length BODY once the dimensions are known. These two
// `parse_*` wrappers walk a complete `FORM RGB8` / `FORM RGBN` file: they
// locate the mandatory `BMHD` (for dimensions), enforce the two RGB-form
// invariants the truecolor reference (§3) pins down — `CAMG` IS REQUIRED and
// `BMHD.compression == 4` — and then hand the `BODY` to the matching body
// decoder. The result is a packed top-to-bottom RGBA8888 image, the same
// shape `parse_ilbm` produces, so a downstream image umbrella can treat every
// IFF raster the same way.
//
// Source: docs/image/iff/iff-truecolor-chunks.md §3, §3.1, §3.2, §3.3.

/// A decoded Turbo-Silver / Imagine true-colour image (`FORM RGB8` or
/// `FORM RGBN`).
#[derive(Clone, Debug)]
pub struct RgbTrueColor {
    /// `true` for `FORM RGB8` (24-bit), `false` for `FORM RGBN` (12-bit).
    pub is_rgb8: bool,
    pub width: u16,
    pub height: u16,
    /// Packed RGBA, row-major, top-to-bottom, 4 bytes/pixel.
    pub rgba: Vec<u8>,
}

/// Locate `BMHD` (width/height/compression byte), whether a `CAMG` chunk was
/// present, and the `BODY` payload inside a `FORM RGB8` / `FORM RGBN` file.
///
/// Returns `(width, height, compression_byte, have_camg, body)`.
#[allow(clippy::type_complexity)]
fn walk_rgb_form<'a>(
    bytes: &'a [u8],
    expect_form: &[u8; 4],
) -> Result<(u16, u16, u8, bool, &'a [u8])> {
    if bytes.len() < 12 || &bytes[0..4] != b"FORM" {
        return Err(Error::invalid("RGB8/RGBN: missing FORM signature"));
    }
    if &bytes[8..12] != expect_form {
        return Err(Error::invalid(format!(
            "RGB8/RGBN: outer form type is {:?} (expected {:?})",
            std::str::from_utf8(&bytes[8..12]).unwrap_or("????"),
            std::str::from_utf8(expect_form).unwrap_or("????"),
        )));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8 + total).min(bytes.len());

    let mut dims: Option<(u16, u16, u8)> = None;
    let mut have_camg = false;
    let mut body: Option<&[u8]> = None;

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
                "RGB8/RGBN: chunk {:?} extends past FORM",
                std::str::from_utf8(&id).unwrap_or("????")
            )));
        }
        let payload = &bytes[payload_start..payload_end];
        match &id {
            b"BMHD" => {
                if payload.len() < 20 {
                    return Err(Error::invalid("RGB8/RGBN BMHD: need 20 bytes"));
                }
                dims = Some((
                    u16::from_be_bytes([payload[0], payload[1]]),
                    u16::from_be_bytes([payload[2], payload[3]]),
                    payload[10],
                ));
            }
            b"CAMG" => have_camg = true,
            b"BODY" => body = Some(payload),
            _ => { /* CMAP (unused on RGB8/RGBN), DPI, ... skipped */ }
        }
        let padded = size + (size & 1);
        cursor = payload_start + padded;
    }

    let (w, h, compression) =
        dims.ok_or_else(|| Error::invalid("RGB8/RGBN: missing BMHD chunk"))?;
    // §3: "CAMG chunk IS REQUIRED."
    if !have_camg {
        return Err(Error::invalid(
            "RGB8/RGBN: CAMG chunk is required for Turbo-Silver true-colour FORMs",
        ));
    }
    // §3: BMHD.compression == 4 (Turbo-Silver-specific RLE, not ByteRun1).
    if compression != 4 {
        return Err(Error::invalid(format!(
            "RGB8/RGBN: BMHD.compression is {compression} (expected 4 for Turbo-Silver RLE)"
        )));
    }
    let body = body.ok_or_else(|| Error::invalid("RGB8/RGBN: missing BODY chunk"))?;
    Ok((w, h, compression, have_camg, body))
}

/// Parse a complete `FORM RGB8` file (§3.2) into a packed-RGBA image,
/// applying the given [`GenlockPolicy`] to the genlock bit.
pub fn parse_rgb8(bytes: &[u8], genlock: GenlockPolicy) -> Result<RgbTrueColor> {
    let (width, height, _comp, _camg, body) = walk_rgb_form(bytes, b"RGB8")?;
    let rgba = decode_rgb8_body(width, height, body, genlock)?;
    Ok(RgbTrueColor {
        is_rgb8: true,
        width,
        height,
        rgba,
    })
}

/// Parse a complete `FORM RGBN` file (§3.1) into a packed-RGBA image,
/// applying the given [`GenlockPolicy`] to the genlock bit.
pub fn parse_rgbn(bytes: &[u8], genlock: GenlockPolicy) -> Result<RgbTrueColor> {
    let (width, height, _comp, _camg, body) = walk_rgb_form(bytes, b"RGBN")?;
    let rgba = decode_rgbn_body(width, height, body, genlock)?;
    Ok(RgbTrueColor {
        is_rgb8: false,
        width,
        height,
        rgba,
    })
}

// ───────────────────────── FORM DEEP — chunky deep raster ─────────────────
//
// FORM DEEP (Amiga Centre Scotland, 1991; used by TVPaint) carries *chunky*
// — not bitplaned — deep / true-colour pixels: each pixel's components sit in
// consecutive bytes, described once by a DPEL chunk, with no CLUT. The chunk
// vocabulary is:
//
//   FORM DEEP
//      DGBL  global info (mandatory, first): display size, compression, aspect
//      DPEL  pixel-element layout: per-component type + bit depth
//      DLOC  optional DBOD placement (w/h/x/y)
//      DBOD  the pixel data, compressed per DGBL.Compression
//      DCHG  optional cel-anim frame timing
//
// This module implements the structural chunks (DGBL/DPEL/DLOC) plus the two
// body codings whose wire format the staged spec fully pins down:
// NOCOMPRESSION (raw chunky stream) and TVDC (Compression == 5, TecSoft's
// 16-word delta + short-run RLE addendum). RUNLENGTH/HUFFMAN/DYNAMICHUFF/JPEG
// are not yet decoded — the canonical DEEP text does not spell out their wire
// layout (RUNLENGTH is explicitly flagged undocumented).
//
// Source: docs/image/iff/iff-truecolor-chunks.md §1 (§1.1 DGBL, §1.2 DPEL,
// §1.3 DLOC, §1.4 DBOD, §1.5 TVDC). No third-party loader code was consulted.

/// DEEP DBOD compression method, from the DGBL `Compression` field
/// (§1.1 of the truecolor doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeepCompression {
    /// `0` — raw chunky stream, no compression.
    None,
    /// `1` — run-length (the canonical DEEP text does not spell out the
    /// scheme; treat as undocumented and probe before relying on it).
    RunLength,
    /// `2` — Huffman.
    Huffman,
    /// `3` — dynamic Huffman.
    DynamicHuffman,
    /// `4` — JPEG.
    Jpeg,
    /// `5` — TVDC (TecSoft addendum): 16-word delta table + short-run RLE,
    /// applied line-by-line per DPEL component. See [`decode_tvdc`].
    Tvdc,
}

impl DeepCompression {
    /// Map a DGBL `Compression` value to its enum, rejecting unknown codes.
    pub fn from_u16(v: u16) -> Result<Self> {
        Ok(match v {
            0 => DeepCompression::None,
            1 => DeepCompression::RunLength,
            2 => DeepCompression::Huffman,
            3 => DeepCompression::DynamicHuffman,
            4 => DeepCompression::Jpeg,
            5 => DeepCompression::Tvdc,
            other => {
                return Err(Error::invalid(format!(
                    "DEEP DGBL: unknown Compression {other} (expected 0..=5)"
                )))
            }
        })
    }

    /// The numeric DGBL `Compression` value for this method.
    pub fn to_u16(self) -> u16 {
        match self {
            DeepCompression::None => 0,
            DeepCompression::RunLength => 1,
            DeepCompression::Huffman => 2,
            DeepCompression::DynamicHuffman => 3,
            DeepCompression::Jpeg => 4,
            DeepCompression::Tvdc => 5,
        }
    }
}

/// DEEP global information — the `DGBL` chunk (§1.1). Always the first chunk
/// in a FORM DEEP. Eight bytes on the wire: two UWORDs, one UWORD, two UBYTEs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dgbl {
    /// Width of the source display, in pixels.
    pub display_width: u16,
    /// Height of the source display, in pixels.
    pub display_height: u16,
    /// DBOD compression method.
    pub compression: DeepCompression,
    /// Pixel aspect-ratio width term.
    pub x_aspect: u8,
    /// Pixel aspect-ratio height term.
    pub y_aspect: u8,
}

impl Dgbl {
    /// Parse a `DGBL` chunk body (8 bytes).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "DEEP DGBL: chunk is {} bytes, need at least 8",
                body.len()
            )));
        }
        Ok(Dgbl {
            display_width: u16::from_be_bytes([body[0], body[1]]),
            display_height: u16::from_be_bytes([body[2], body[3]]),
            compression: DeepCompression::from_u16(u16::from_be_bytes([body[4], body[5]]))?,
            x_aspect: body[6],
            y_aspect: body[7],
        })
    }

    /// Serialise to the 8-byte `DGBL` wire form.
    pub fn write(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&self.display_width.to_be_bytes());
        out[2..4].copy_from_slice(&self.display_height.to_be_bytes());
        out[4..6].copy_from_slice(&self.compression.to_u16().to_be_bytes());
        out[6] = self.x_aspect;
        out[7] = self.y_aspect;
        out
    }
}

/// DEEP pixel component type — the `cType` field of a DPEL element (§1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeepCType {
    Red,
    Green,
    Blue,
    Alpha,
    Yellow,
    Cyan,
    Magenta,
    Black,
    Mask,
    ZBuffer,
    Opacity,
    LinearKey,
    BinaryKey,
}

impl DeepCType {
    /// Map a DPEL `cType` value (§1.2 table) to its enum.
    pub fn from_u16(v: u16) -> Result<Self> {
        Ok(match v {
            1 => DeepCType::Red,
            2 => DeepCType::Green,
            3 => DeepCType::Blue,
            4 => DeepCType::Alpha,
            5 => DeepCType::Yellow,
            6 => DeepCType::Cyan,
            7 => DeepCType::Magenta,
            8 => DeepCType::Black,
            9 => DeepCType::Mask,
            10 => DeepCType::ZBuffer,
            11 => DeepCType::Opacity,
            12 => DeepCType::LinearKey,
            13 => DeepCType::BinaryKey,
            other => {
                return Err(Error::invalid(format!(
                    "DEEP DPEL: unknown cType {other} (expected 1..=13)"
                )))
            }
        })
    }

    /// The numeric `cType` value.
    pub fn to_u16(self) -> u16 {
        match self {
            DeepCType::Red => 1,
            DeepCType::Green => 2,
            DeepCType::Blue => 3,
            DeepCType::Alpha => 4,
            DeepCType::Yellow => 5,
            DeepCType::Cyan => 6,
            DeepCType::Magenta => 7,
            DeepCType::Black => 8,
            DeepCType::Mask => 9,
            DeepCType::ZBuffer => 10,
            DeepCType::Opacity => 11,
            DeepCType::LinearKey => 12,
            DeepCType::BinaryKey => 13,
        }
    }
}

/// One DPEL pixel-component descriptor: a `(cType, cBitDepth)` pair (§1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DpelElement {
    /// Component type.
    pub c_type: DeepCType,
    /// Number of bits this component occupies in the pixel.
    pub c_bit_depth: u16,
}

/// DEEP pixel-element layout — the `DPEL` chunk (§1.2). Describes, in storage
/// order (MSB-first), the components that make up one pixel. The whole pixel
/// is padded up to a byte boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dpel {
    /// The components, in MSB-first storage order.
    pub elements: Vec<DpelElement>,
}

impl Dpel {
    /// Parse a `DPEL` chunk body: a ULONG `nElements` followed by
    /// `nElements` `(UWORD cType, UWORD cBitDepth)` pairs.
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::invalid(
                "DEEP DPEL: chunk too small for the nElements ULONG",
            ));
        }
        let n = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        // Each element is 4 bytes (two UWORDs). Reject a count that the body
        // can't possibly hold before allocating anything.
        let need = 4usize
            .checked_add(
                n.checked_mul(4)
                    .ok_or_else(|| Error::invalid("DEEP DPEL: nElements * 4 overflows"))?,
            )
            .ok_or_else(|| Error::invalid("DEEP DPEL: header + payload overflows"))?;
        if body.len() < need {
            return Err(Error::invalid(format!(
                "DEEP DPEL: {n} elements need {need} bytes, chunk is {}",
                body.len()
            )));
        }
        let mut elements = Vec::with_capacity(n);
        for i in 0..n {
            let off = 4 + i * 4;
            let c_type = DeepCType::from_u16(u16::from_be_bytes([body[off], body[off + 1]]))?;
            let c_bit_depth = u16::from_be_bytes([body[off + 2], body[off + 3]]);
            elements.push(DpelElement {
                c_type,
                c_bit_depth,
            });
        }
        Ok(Dpel { elements })
    }

    /// Total bits across every component (before byte padding).
    pub fn total_bits(&self) -> u32 {
        self.elements.iter().map(|e| u32::from(e.c_bit_depth)).sum()
    }

    /// Bytes occupied by one pixel: the summed component bits rounded up to a
    /// byte boundary (§1.2 "the whole pixel is padded up to a byte boundary").
    pub fn pixel_bytes(&self) -> usize {
        (self.total_bits() as usize).div_ceil(8)
    }
}

/// DEEP display location — the optional `DLOC` chunk (§1.3): the width/height
/// of the *following* DBOD plus its placement. Eight bytes: two UWORDs then
/// two WORDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dloc {
    /// Width of the following DBOD, in pixels.
    pub w: u16,
    /// Height of the following DBOD, in pixels.
    pub h: u16,
    /// X pixel position of this image.
    pub x: i16,
    /// Y pixel position of this image.
    pub y: i16,
}

impl Dloc {
    /// Parse a `DLOC` chunk body (8 bytes).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 8 {
            return Err(Error::invalid(format!(
                "DEEP DLOC: chunk is {} bytes, need at least 8",
                body.len()
            )));
        }
        Ok(Dloc {
            w: u16::from_be_bytes([body[0], body[1]]),
            h: u16::from_be_bytes([body[2], body[3]]),
            x: i16::from_be_bytes([body[4], body[5]]),
            y: i16::from_be_bytes([body[6], body[7]]),
        })
    }
}

/// Decode a TVDC component line (DEEP `Compression == 5`, §1.5).
///
/// TVDC (TecSoft's addendum for TVPaint) is a modified delta compression that
/// reads the source **one nibble at a time, high nibble first then low**, and
/// maintains a running accumulator `v` that starts at `0` for each line:
///
/// - look up `table[d]` for nibble `d` (0..=15), a signed 16-word delta table
///   supplied alongside the data;
/// - if `table[d] != 0`: `v += table[d]`; emit `v` (low 8 bits) as the next
///   output byte;
/// - if `table[d] == 0`: the **next nibble** is a run count; the *current* `v`
///   is emitted that many **more** times (short-run RLE).
///
/// The function emits exactly `size` output bytes and returns the number of
/// **source bytes** consumed (`(nibble_pos + 1) / 2`, i.e. the nibble count
/// rounded up to whole bytes), so the caller can advance to the next line.
/// A source that runs out of nibbles before `size` bytes are produced is
/// rejected with [`Error::invalid`].
pub fn decode_tvdc(
    source: &[u8],
    table: &[i16; 16],
    size: usize,
    out: &mut Vec<u8>,
) -> Result<usize> {
    // Nibble cursor: nibble index 2*k is the high nibble of byte k, 2*k+1 the
    // low nibble. `read_nibble` advances and bounds-checks.
    let mut nib = 0usize;
    let max_nibbles = source.len() * 2;
    let read_nibble = |nib: &mut usize| -> Result<u8> {
        if *nib >= max_nibbles {
            return Err(Error::invalid(
                "DEEP TVDC: source ran out of nibbles before the line was filled",
            ));
        }
        let byte = source[*nib / 2];
        let n = if *nib & 1 == 0 {
            byte >> 4
        } else {
            byte & 0x0F
        };
        *nib += 1;
        Ok(n)
    };

    let mut v: i32 = 0;
    let mut produced = 0usize;
    while produced < size {
        let d = read_nibble(&mut nib)? as usize;
        let delta = table[d];
        if delta != 0 {
            v = v.wrapping_add(i32::from(delta));
            out.push((v & 0xFF) as u8);
            produced += 1;
        } else {
            // Zero delta → next nibble is a run count: emit current v that
            // many more times.
            let run = read_nibble(&mut nib)? as usize;
            if produced + run > size {
                return Err(Error::invalid(format!(
                    "DEEP TVDC: run of {run} overshoots the {size}-byte line ({produced} produced)"
                )));
            }
            for _ in 0..run {
                out.push((v & 0xFF) as u8);
            }
            produced += run;
        }
    }
    // Source bytes used = nibble count rounded up to whole bytes.
    Ok(nib.div_ceil(2))
}

/// Assemble a decompressed DEEP **chunky** body into packed RGBA8888,
/// row-major, top-to-bottom (§1.2 + §1.4).
///
/// `body` is the per-pixel chunky stream after any DGBL decompression: each
/// pixel occupies [`Dpel::pixel_bytes`] bytes, with the DPEL components packed
/// MSB-first and the pixel padded up to a byte boundary. Each component is
/// scaled from its `cBitDepth` up to 8 bits by left-shift + MSB replication.
/// RED/GREEN/BLUE map to the RGB guns; ALPHA / OPACITY map to alpha; any other
/// component is parsed (to keep the bit cursor correct) but does not reach the
/// output. A pixel with no alpha-bearing component is fully opaque.
pub fn assemble_deep_chunky(dpel: &Dpel, width: u16, height: u16, body: &[u8]) -> Result<Vec<u8>> {
    let pixel_bytes = dpel.pixel_bytes();
    if pixel_bytes == 0 {
        return Err(Error::invalid(
            "DEEP: DPEL describes a zero-bit pixel (no components)",
        ));
    }
    let total: usize = width as usize * height as usize;
    let need = total
        .checked_mul(pixel_bytes)
        .ok_or_else(|| Error::invalid("DEEP: width * height * pixel_bytes overflows"))?;
    if body.len() < need {
        return Err(Error::invalid(format!(
            "DEEP: chunky body is {} bytes, need {need} ({width}x{height} @ {pixel_bytes} B/pixel)",
            body.len()
        )));
    }

    let mut rgba = vec![0u8; total * 4];
    for p in 0..total {
        let pixel = &body[p * pixel_bytes..p * pixel_bytes + pixel_bytes];
        // Walk components MSB-first across the pixel's bits.
        let mut bit_cursor = 0u32;
        let (mut r, mut g, mut b) = (0u8, 0u8, 0u8);
        // A pixel with no alpha-bearing component is fully opaque.
        let mut a = 0xFFu8;
        for el in &dpel.elements {
            let depth = el.c_bit_depth;
            let raw = read_bits_msb(pixel, bit_cursor, depth);
            bit_cursor += u32::from(depth);
            let scaled = scale_to_u8(raw, depth);
            match el.c_type {
                DeepCType::Red => r = scaled,
                DeepCType::Green => g = scaled,
                DeepCType::Blue => b = scaled,
                DeepCType::Alpha | DeepCType::Opacity => a = scaled,
                _ => {} // parsed for cursor advance; not mapped to output
            }
        }
        let dst = p * 4;
        rgba[dst] = r;
        rgba[dst + 1] = g;
        rgba[dst + 2] = b;
        rgba[dst + 3] = a;
    }
    Ok(rgba)
}

/// Read `depth` bits (1..=16) MSB-first starting at bit offset `start` within
/// a byte slice, returning them right-aligned in a u16. Bits beyond the slice
/// read as 0 (the caller has already size-checked the pixel buffer).
fn read_bits_msb(bytes: &[u8], start: u32, depth: u16) -> u16 {
    let mut acc: u16 = 0;
    for i in 0..depth {
        let bit_index = start + u32::from(i);
        let byte_index = (bit_index / 8) as usize;
        let bit_in_byte = 7 - (bit_index % 8);
        let bit = bytes
            .get(byte_index)
            .map(|&v| (v >> bit_in_byte) & 1)
            .unwrap_or(0);
        acc = (acc << 1) | u16::from(bit);
    }
    acc
}

/// Scale a `depth`-bit value up to a full 8-bit channel by left-shifting into
/// the high bits and replicating the most-significant bits into the low bits,
/// so the full input range maps onto the full `0..=255` output range.
fn scale_to_u8(value: u16, depth: u16) -> u8 {
    if depth == 0 {
        return 0;
    }
    if depth >= 8 {
        // Take the top 8 bits of a deeper component.
        return (value >> (depth - 8)) as u8;
    }
    let mut out = (value << (8 - depth)) as u8;
    // Replicate the high bits down to fill the low bits.
    let mut filled = depth;
    while filled < 8 {
        out |= out >> filled;
        filled *= 2;
    }
    out
}

// ─────────────────── FORM DEEP — top-level decode ───────────────────
//
// `assemble_deep_chunky` turns a *decompressed* chunky body into RGBA;
// `decode_tvdc` decompresses one TVDC component line. `parse_deep` walks a
// complete `FORM DEEP` file: DGBL (global header, mandatory first), DPEL
// (pixel-element layout, mandatory), optional DLOC (per-DBOD dimensions),
// and DBOD (the pixel data). It assembles the first DBOD into a packed
// top-to-bottom RGBA8888 image.
//
// Coverage:
//   * NOCOMPRESSION (DGBL.Compression == 0): the DBOD is a raw chunky
//     stream → handed straight to assemble_deep_chunky.
//   * TVDC (== 5): the per-component-line decoder (decode_tvdc) is wired
//     via assemble_deep_tvdc, BUT the 16-word delta table TVDC needs is
//     "supplied alongside the data / stored with the file" (§1.5) and the
//     canonical DEEP text does not name a chunk that carries it inside the
//     FORM. parse_deep therefore decodes TVDC only when the caller supplies
//     the table (assemble_deep_tvdc); the chunk-walking parse_deep returns
//     an Error for an in-FORM TVDC body, flagging the documented gap.
//   * RUNLENGTH / HUFFMAN / DYNAMICHUFF / JPEG: wire layout undocumented in
//     the staged spec → rejected.
//
// Source: docs/image/iff/iff-truecolor-chunks.md §1 (§1.1 DGBL, §1.2 DPEL,
// §1.3 DLOC, §1.4 DBOD, §1.5 TVDC).

/// A decoded `FORM DEEP` image.
#[derive(Clone, Debug)]
pub struct DeepImage {
    /// Parsed DGBL global header.
    pub dgbl: Dgbl,
    /// Parsed DPEL pixel-element layout.
    pub dpel: Dpel,
    /// Optional DLOC placement of the decoded DBOD.
    pub dloc: Option<Dloc>,
    pub width: u16,
    pub height: u16,
    /// Packed RGBA, row-major, top-to-bottom, 4 bytes/pixel.
    pub rgba: Vec<u8>,
}

/// Assemble a TVDC-compressed DEEP body (§1.5) into packed RGBA8888.
///
/// TVDC is applied **line by line, per DPEL component**: the body is, for
/// each row, one TVDC-compressed line per component (a Red line, then a
/// Green line, …). Each compressed line decodes to `width` output bytes —
/// the 8-bit component values for that row. `table` is the per-stream
/// 16-word signed delta dictionary TVPaint stores alongside the data.
///
/// Only components whose `cBitDepth == 8` are supported here: TVDC emits one
/// output **byte** per pixel per line, so a non-8-bit DPEL component has no
/// documented byte→sub-8-bit mapping (the staged §1.5 does not pin one).
/// Such a layout is rejected with [`Error::invalid`].
pub fn assemble_deep_tvdc(
    dpel: &Dpel,
    width: u16,
    height: u16,
    table: &[i16; 16],
    body: &[u8],
) -> Result<Vec<u8>> {
    for el in &dpel.elements {
        if el.c_bit_depth != 8 {
            return Err(Error::invalid(format!(
                "DEEP TVDC: component bit depth {} is not 8 (TVDC emits one byte per \
                 component per pixel; sub-8-bit packing is undocumented in §1.5)",
                el.c_bit_depth
            )));
        }
    }
    let w = width as usize;
    let h = height as usize;
    let total = w * h;
    let mut rgba = vec![0u8; total * 4];
    // A pixel with no alpha-bearing component is fully opaque.
    for px in rgba.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }

    let mut src = 0usize;
    let mut line = Vec::with_capacity(w);
    for y in 0..h {
        for el in &dpel.elements {
            line.clear();
            let used = decode_tvdc(&body[src..], table, w, &mut line)?;
            src += used;
            let row_base = y * w;
            for (x, &v) in line.iter().enumerate() {
                let dst = (row_base + x) * 4;
                match el.c_type {
                    DeepCType::Red => rgba[dst] = v,
                    DeepCType::Green => rgba[dst + 1] = v,
                    DeepCType::Blue => rgba[dst + 2] = v,
                    DeepCType::Alpha | DeepCType::Opacity => rgba[dst + 3] = v,
                    _ => {} // parsed for stream advance; not mapped to output
                }
            }
        }
    }
    Ok(rgba)
}

/// Walk a complete `FORM DEEP` file (§1) into a [`DeepImage`].
///
/// Locates DGBL (mandatory, the §1.1 global header), DPEL (mandatory, the
/// §1.2 pixel layout), the optional DLOC placement, and the first DBOD body.
/// Dimensions come from the DLOC preceding the DBOD if present, else from the
/// DGBL display size (§1.3). The DBOD is assembled per DGBL.Compression:
/// NOCOMPRESSION is decoded; every other method (RUNLENGTH / HUFFMAN /
/// DYNAMICHUFF / JPEG / TVDC) is rejected here — see the module comment for
/// the TVDC delta-table gap and use [`assemble_deep_tvdc`] when the caller
/// has the table.
pub fn parse_deep(bytes: &[u8]) -> Result<DeepImage> {
    if bytes.len() < 12 || &bytes[0..4] != b"FORM" {
        return Err(Error::invalid("DEEP: missing FORM signature"));
    }
    if &bytes[8..12] != b"DEEP" {
        return Err(Error::invalid(format!(
            "DEEP: outer form type is {:?} (expected DEEP)",
            std::str::from_utf8(&bytes[8..12]).unwrap_or("????")
        )));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8 + total).min(bytes.len());

    let mut dgbl: Option<Dgbl> = None;
    let mut dpel: Option<Dpel> = None;
    // The DLOC that most recently preceded the captured DBOD.
    let mut pending_dloc: Option<Dloc> = None;
    let mut dbod_dloc: Option<Dloc> = None;
    let mut dbod: Option<&[u8]> = None;

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
                "DEEP: chunk {:?} extends past FORM",
                std::str::from_utf8(&id).unwrap_or("????")
            )));
        }
        let payload = &bytes[payload_start..payload_end];
        match &id {
            b"DGBL" => dgbl = Some(Dgbl::parse(payload)?),
            b"DPEL" => dpel = Some(Dpel::parse(payload)?),
            b"DLOC" => pending_dloc = Some(Dloc::parse(payload)?),
            // Capture only the first DBOD; subsequent ones (multi-image
            // FORM DEEP) are ignored here.
            b"DBOD" if dbod.is_none() => {
                dbod = Some(payload);
                dbod_dloc = pending_dloc.take();
            }
            _ => { /* DCHG (cel-anim timing), unknown chunks skipped */ }
        }
        let padded = size + (size & 1);
        cursor = payload_start + padded;
    }

    let dgbl = dgbl.ok_or_else(|| Error::invalid("DEEP: missing DGBL chunk"))?;
    let dpel = dpel.ok_or_else(|| Error::invalid("DEEP: missing DPEL chunk"))?;
    let dbod = dbod.ok_or_else(|| Error::invalid("DEEP: missing DBOD chunk"))?;

    // §1.3: DLOC gives the DBOD's dimensions; absent it, the DGBL display size.
    let (width, height) = match dbod_dloc {
        Some(dl) => (dl.w, dl.h),
        None => (dgbl.display_width, dgbl.display_height),
    };

    let rgba = match dgbl.compression {
        DeepCompression::None => assemble_deep_chunky(&dpel, width, height, dbod)?,
        DeepCompression::Tvdc => {
            return Err(Error::invalid(
                "DEEP: TVDC body cannot be decoded from the FORM alone — the §1.5 16-word \
                 delta table is stored with the file/companion data and the canonical DEEP \
                 text names no chunk that carries it in-FORM. Use assemble_deep_tvdc with \
                 the table supplied by the caller.",
            ));
        }
        other => {
            return Err(Error::invalid(format!(
                "DEEP: DGBL Compression {} body coding is not decoded (wire layout \
                 undocumented in the staged spec)",
                other.to_u16()
            )));
        }
    };

    Ok(DeepImage {
        dgbl,
        dpel,
        dloc: dbod_dloc,
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ───────────────── FORM RGBN 12-bit genlock-RLE body ─────────────────

    /// One coded WORD with a 3-bit inline count: red/green/blue nibbles,
    /// genlock flag, and a 1..=7 run. Panics on out-of-range count so the
    /// helper can't silently mis-encode a test fixture.
    fn rgbn_word(r: u16, g: u16, b: u16, lock: bool, count: u16) -> [u8; 2] {
        assert!((1..=7).contains(&count), "inline count must be 1..=7");
        let rgb12 = (r & 0xF) << 8 | (g & 0xF) << 4 | (b & 0xF);
        let w = rgb12 << 4 | (u16::from(lock) << 3) | count;
        w.to_be_bytes()
    }

    #[test]
    fn rgbn_inline_run_widens_12bit_to_rgb888() {
        // 2x1 image: one red run of 1, one white run of 1.
        let mut body = Vec::new();
        body.extend_from_slice(&rgbn_word(0xF, 0x0, 0x0, false, 1)); // red
        body.extend_from_slice(&rgbn_word(0xF, 0xF, 0xF, false, 1)); // white
        let rgba = decode_rgbn_body(2, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba, vec![255, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn rgbn_nibble_replication_maps_mid_value() {
        // 0x8 → (0x8 << 4) | 0x8 = 0x88; verifies bit-replication widening.
        let body = rgbn_word(0x8, 0x0, 0x0, false, 1);
        let rgba = decode_rgbn_body(1, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(&rgba[..3], &[0x88, 0x00, 0x00]);
    }

    #[test]
    fn rgbn_inline_run_fills_multiple_pixels() {
        // A single run of 7 green pixels fills a 7x1 row.
        let body = rgbn_word(0x0, 0xF, 0x0, false, 7);
        let rgba = decode_rgbn_body(7, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba.len(), 7 * 4);
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, [0, 255, 0, 255]);
        }
    }

    #[test]
    fn rgbn_byte_count_cascade_handles_run_over_7() {
        // Run of 200 (> 7): 3-bit field 0, then a BYTE count of 200.
        let rgb12 = 0xF00u16; // pure red
        let w = rgb12 << 4; // count nibble = 0, no genlock
        let mut body = w.to_be_bytes().to_vec();
        body.push(200);
        let rgba = decode_rgbn_body(200, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba.len(), 200 * 4);
        assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&rgba[796..800], &[255, 0, 0, 255]);
    }

    #[test]
    fn rgbn_word_count_cascade_handles_run_over_255() {
        // Run of 300 (> 255): 3-bit field 0, BYTE 0, then WORD count 300.
        let rgb12 = 0x0F0u16; // pure green
        let w = rgb12 << 4;
        let mut body = w.to_be_bytes().to_vec();
        body.push(0); // BYTE escape
        body.extend_from_slice(&300u16.to_be_bytes()); // WORD count
        let rgba = decode_rgbn_body(300, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba.len(), 300 * 4);
        assert_eq!(&rgba[1196..1200], &[0, 255, 0, 255]);
    }

    #[test]
    fn rgbn_run_spills_across_scanlines() {
        // A 2x2 image filled by a single run of 4 blue pixels: the run
        // crosses the scanline boundary (the body is a flat pixel stream).
        let body = rgbn_word(0x0, 0x0, 0xF, false, 4);
        let rgba = decode_rgbn_body(2, 2, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba.len(), 16);
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, [0, 0, 255, 255]);
        }
    }

    #[test]
    fn rgbn_genlock_turbo_silver_writes_zero_colour() {
        // A genlocked unit under Turbo-Silver semantics emits opaque
        // black regardless of the coded RGB.
        let body = rgbn_word(0xF, 0xF, 0xF, true, 1);
        let rgba = decode_rgbn_body(1, 1, &body, GenlockPolicy::TurboSilverZeroColour).unwrap();
        assert_eq!(rgba, vec![0, 0, 0, 255]);
        // Same unit with IgnoreUseColour keeps the white colour.
        let rgba2 = decode_rgbn_body(1, 1, &body, GenlockPolicy::IgnoreUseColour).unwrap();
        assert_eq!(rgba2, vec![255, 255, 255, 255]);
    }

    #[test]
    fn rgbn_genlock_brush_marks_transparency() {
        // Under brush semantics a genlocked pixel gets alpha 0 but keeps
        // its widened RGB; a non-genlocked pixel stays opaque.
        let mut body = Vec::new();
        body.extend_from_slice(&rgbn_word(0xF, 0x0, 0x0, true, 1)); // masked-out red
        body.extend_from_slice(&rgbn_word(0x0, 0xF, 0x0, false, 1)); // opaque green
        let rgba = decode_rgbn_body(2, 1, &body, GenlockPolicy::BrushTransparency).unwrap();
        assert_eq!(&rgba[0..4], &[255, 0, 0, 0]);
        assert_eq!(&rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn rgbn_truncated_stream_is_rejected() {
        // Body claims to start a unit but provides only 1 of 2 WORD bytes.
        let body = [0xFFu8];
        let err = decode_rgbn_body(4, 1, &body, GenlockPolicy::default());
        assert!(err.is_err());
        // A run that fills fewer pixels than the frame needs is also an error.
        let short = rgbn_word(0xF, 0x0, 0x0, false, 1); // 1 pixel for a 4-pixel frame
        assert!(decode_rgbn_body(4, 1, &short, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn rgbn_overshoot_run_is_rejected() {
        // A run of 7 into a 3-pixel frame overshoots the budget.
        let body = rgbn_word(0xF, 0x0, 0x0, false, 7);
        assert!(decode_rgbn_body(3, 1, &body, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn rgbn_missing_byte_escape_is_rejected() {
        // 3-bit count 0 with no following BYTE.
        let rgb12 = 0xF00u16;
        let w = rgb12 << 4;
        let body = w.to_be_bytes();
        assert!(decode_rgbn_body(1, 1, &body, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn rgbn_zero_word_escape_count_is_rejected() {
        // BYTE 0 then WORD 0 → undefined zero-length run.
        let rgb12 = 0xF00u16;
        let w = rgb12 << 4;
        let mut body = w.to_be_bytes().to_vec();
        body.push(0); // BYTE escape
        body.extend_from_slice(&0u16.to_be_bytes()); // WORD 0
        assert!(decode_rgbn_body(1, 1, &body, GenlockPolicy::default()).is_err());
    }

    /// Build one RGB8 LONG unit: 24-bit RGB (r:8 g:8 b:8) << 8, genlock bit
    /// at 0x80, 7-bit run count in the low 7 bits.
    fn rgb8_long(r: u8, g: u8, b: u8, lock: bool, count: u8) -> [u8; 4] {
        let rgb = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        let mut w = rgb << 8;
        if lock {
            w |= 0x0000_0080;
        }
        w |= u32::from(count) & 0x7F;
        w.to_be_bytes()
    }

    #[test]
    fn rgb8_inline_run_keeps_full_8bit_guns() {
        // Red then white, one pixel each — guns pass through unchanged.
        let mut body = Vec::new();
        body.extend_from_slice(&rgb8_long(0xFF, 0x00, 0x00, false, 1));
        body.extend_from_slice(&rgb8_long(0x12, 0x34, 0x56, false, 1));
        let rgba = decode_rgb8_body(2, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(&rgba[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
        assert_eq!(&rgba[4..8], &[0x12, 0x34, 0x56, 0xFF]);
    }

    #[test]
    fn rgb8_inline_run_fills_multiple_pixels() {
        // A 7-bit count of 5 fills five consecutive pixels.
        let body = rgb8_long(0x00, 0x80, 0xFF, false, 5);
        let rgba = decode_rgb8_body(5, 1, &body, GenlockPolicy::default()).unwrap();
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, &[0x00, 0x80, 0xFF, 0xFF]);
        }
    }

    #[test]
    fn rgb8_max_inline_count_127() {
        // The full 7-bit count (127) is the largest legal run.
        let body = rgb8_long(0x11, 0x22, 0x33, false, 127);
        let rgba = decode_rgb8_body(127, 1, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba.len(), 127 * 4);
        assert_eq!(&rgba[126 * 4..], &[0x11, 0x22, 0x33, 0xFF]);
    }

    #[test]
    fn rgb8_run_spills_across_scanlines() {
        // A 2x2 frame filled by a single run of 4 — the run crosses the
        // first scanline boundary into the second row.
        let body = rgb8_long(0xAB, 0xCD, 0xEF, false, 4);
        let rgba = decode_rgb8_body(2, 2, &body, GenlockPolicy::default()).unwrap();
        assert_eq!(rgba.len(), 4 * 4);
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, &[0xAB, 0xCD, 0xEF, 0xFF]);
        }
    }

    #[test]
    fn rgb8_genlock_turbo_silver_writes_zero_colour() {
        // A genlocked pixel becomes opaque black under Turbo-Silver policy;
        // the coded RGB is ignored.
        let body = rgb8_long(0xFF, 0xFF, 0xFF, true, 1);
        let rgba = decode_rgb8_body(1, 1, &body, GenlockPolicy::TurboSilverZeroColour).unwrap();
        assert_eq!(&rgba[0..4], &[0x00, 0x00, 0x00, 0xFF]);
        // Same unit under the default policy keeps the coded white.
        let rgba2 = decode_rgb8_body(1, 1, &body, GenlockPolicy::IgnoreUseColour).unwrap();
        assert_eq!(&rgba2[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn rgb8_genlock_brush_marks_transparency() {
        // Brush policy: genlocked pixel gets alpha 0 but keeps its RGB.
        let mut body = Vec::new();
        body.extend_from_slice(&rgb8_long(0xFF, 0x00, 0x00, true, 1)); // masked-out
        body.extend_from_slice(&rgb8_long(0x00, 0xFF, 0x00, false, 1)); // opaque
        let rgba = decode_rgb8_body(2, 1, &body, GenlockPolicy::BrushTransparency).unwrap();
        assert_eq!(&rgba[0..4], &[0xFF, 0x00, 0x00, 0x00]);
        assert_eq!(&rgba[4..8], &[0x00, 0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn rgb8_truncated_stream_is_rejected() {
        // A 3-byte body cannot even form one LONG unit.
        let body = [0u8, 0, 0];
        assert!(decode_rgb8_body(4, 1, &body, GenlockPolicy::default()).is_err());
        // One unit (1 pixel) for a 4-pixel frame underruns.
        let short = rgb8_long(0xFF, 0x00, 0x00, false, 1);
        assert!(decode_rgb8_body(4, 1, &short, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn rgb8_overshoot_run_is_rejected() {
        // A run of 7 into a 3-pixel frame overshoots the budget.
        let body = rgb8_long(0xFF, 0x00, 0x00, false, 7);
        assert!(decode_rgb8_body(3, 1, &body, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn rgb8_zero_count_is_rejected() {
        // A 7-bit count of 0 has no escape cascade in RGB8 → undefined.
        let body = rgb8_long(0xFF, 0x00, 0x00, false, 0);
        assert!(decode_rgb8_body(1, 1, &body, GenlockPolicy::default()).is_err());
    }

    // ───────────── FORM RGB8 / RGBN top-level decode (parse_rgb*) ─────────────

    /// Build a minimal IFF FORM envelope around the given chunks.
    fn iff_form(form_type: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(form_type);
        for (id, payload) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
            if payload.len() & 1 == 1 {
                body.push(0);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// A 20-byte BMHD with the given dimensions, plane count and compression.
    fn rgb_bmhd(w: u16, h: u16, n_planes: u8, compression: u8) -> Vec<u8> {
        let mut b = vec![0u8; 20];
        b[0..2].copy_from_slice(&w.to_be_bytes());
        b[2..4].copy_from_slice(&h.to_be_bytes());
        b[8] = n_planes;
        b[10] = compression;
        b[14] = 1;
        b[15] = 1;
        b
    }

    #[test]
    fn parse_rgb8_full_form() {
        let mut bdy = Vec::new();
        bdy.extend_from_slice(&rgb8_long(0x11, 0x22, 0x33, false, 4));
        let file = iff_form(
            b"RGB8",
            &[
                (b"BMHD", rgb_bmhd(4, 1, 25, 4)),
                (b"CAMG", vec![0, 0, 0, 0]),
                (b"BODY", bdy),
            ],
        );
        let img = parse_rgb8(&file, GenlockPolicy::default()).unwrap();
        assert!(img.is_rgb8);
        assert_eq!((img.width, img.height), (4, 1));
        for px in img.rgba.chunks_exact(4) {
            assert_eq!(px, &[0x11, 0x22, 0x33, 0xFF]);
        }
    }

    #[test]
    fn parse_rgbn_full_form() {
        let mut bdy = Vec::new();
        bdy.extend_from_slice(&rgbn_word(0xF, 0x0, 0xA, false, 3));
        let file = iff_form(
            b"RGBN",
            &[
                (b"BMHD", rgb_bmhd(3, 1, 13, 4)),
                (b"CAMG", vec![0, 0, 0, 0]),
                (b"BODY", bdy),
            ],
        );
        let img = parse_rgbn(&file, GenlockPolicy::default()).unwrap();
        assert!(!img.is_rgb8);
        assert_eq!((img.width, img.height), (3, 1));
        for px in img.rgba.chunks_exact(4) {
            assert_eq!(px, &[0xFF, 0x00, 0xAA, 0xFF]);
        }
    }

    #[test]
    fn parse_rgb8_requires_camg() {
        let mut bdy = Vec::new();
        bdy.extend_from_slice(&rgb8_long(0x11, 0x22, 0x33, false, 1));
        let file = iff_form(b"RGB8", &[(b"BMHD", rgb_bmhd(1, 1, 25, 4)), (b"BODY", bdy)]);
        assert!(parse_rgb8(&file, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn parse_rgb8_requires_compression_4() {
        let mut bdy = Vec::new();
        bdy.extend_from_slice(&rgb8_long(0x11, 0x22, 0x33, false, 1));
        let file = iff_form(
            b"RGB8",
            &[
                (b"BMHD", rgb_bmhd(1, 1, 25, 1)),
                (b"CAMG", vec![0, 0, 0, 0]),
                (b"BODY", bdy),
            ],
        );
        assert!(parse_rgb8(&file, GenlockPolicy::default()).is_err());
    }

    #[test]
    fn parse_rgb_wrong_form_type_rejected() {
        let mut bdy = Vec::new();
        bdy.extend_from_slice(&rgb8_long(0x11, 0x22, 0x33, false, 1));
        let file = iff_form(
            b"RGBN",
            &[
                (b"BMHD", rgb_bmhd(1, 1, 25, 4)),
                (b"CAMG", vec![0, 0, 0, 0]),
                (b"BODY", bdy),
            ],
        );
        // Body parsed as RGBN would mis-decode; asking parse_rgb8 must reject.
        assert!(parse_rgb8(&file, GenlockPolicy::default()).is_err());
    }

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

    // ───────────────────── FORM DEEP — chunky deep raster ─────────────────

    #[test]
    fn deep_dgbl_roundtrips() {
        let d = Dgbl {
            display_width: 320,
            display_height: 200,
            compression: DeepCompression::Tvdc,
            x_aspect: 10,
            y_aspect: 11,
        };
        let bytes = d.write();
        assert_eq!(Dgbl::parse(&bytes).unwrap(), d);
    }

    #[test]
    fn deep_dgbl_compression_codes() {
        for (v, want) in [
            (0u16, DeepCompression::None),
            (1, DeepCompression::RunLength),
            (2, DeepCompression::Huffman),
            (3, DeepCompression::DynamicHuffman),
            (4, DeepCompression::Jpeg),
            (5, DeepCompression::Tvdc),
        ] {
            assert_eq!(DeepCompression::from_u16(v).unwrap(), want);
            assert_eq!(want.to_u16(), v);
        }
        assert!(DeepCompression::from_u16(6).is_err());
    }

    #[test]
    fn deep_dgbl_rejects_short_chunk() {
        assert!(Dgbl::parse(&[0u8; 7]).is_err());
    }

    #[test]
    fn deep_dpel_rgb888_layout() {
        // nElements = 3, each RED/GREEN/BLUE @ 8 bits.
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes());
        for c in [1u16, 2, 3] {
            body.extend_from_slice(&c.to_be_bytes());
            body.extend_from_slice(&8u16.to_be_bytes());
        }
        let dpel = Dpel::parse(&body).unwrap();
        assert_eq!(dpel.elements.len(), 3);
        assert_eq!(dpel.elements[0].c_type, DeepCType::Red);
        assert_eq!(dpel.elements[1].c_type, DeepCType::Green);
        assert_eq!(dpel.elements[2].c_type, DeepCType::Blue);
        assert_eq!(dpel.total_bits(), 24);
        assert_eq!(dpel.pixel_bytes(), 3);
    }

    #[test]
    fn deep_dpel_rgba_8_8_8_4_pads_to_byte() {
        // RGBA 8:8:8:4 → 28 bits → 4 bytes (alpha padded).
        let mut body = Vec::new();
        body.extend_from_slice(&4u32.to_be_bytes());
        for (c, d) in [(1u16, 8u16), (2, 8), (3, 8), (4, 4)] {
            body.extend_from_slice(&c.to_be_bytes());
            body.extend_from_slice(&d.to_be_bytes());
        }
        let dpel = Dpel::parse(&body).unwrap();
        assert_eq!(dpel.total_bits(), 28);
        assert_eq!(dpel.pixel_bytes(), 4);
        assert_eq!(dpel.elements[3].c_type, DeepCType::Alpha);
    }

    #[test]
    fn deep_dpel_rejects_undersized_payload() {
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes()); // claims 3 elements
        body.extend_from_slice(&1u16.to_be_bytes()); // only one partial element
        assert!(Dpel::parse(&body).is_err());
    }

    #[test]
    fn deep_dpel_unknown_ctype_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&99u16.to_be_bytes());
        body.extend_from_slice(&8u16.to_be_bytes());
        assert!(Dpel::parse(&body).is_err());
    }

    #[test]
    fn deep_dloc_parses() {
        let body = [0, 64, 0, 48, 0xFF, 0xF6, 0, 10]; // w=64,h=48,x=-10,y=10
        let dloc = Dloc::parse(&body).unwrap();
        assert_eq!(dloc.w, 64);
        assert_eq!(dloc.h, 48);
        assert_eq!(dloc.x, -10);
        assert_eq!(dloc.y, 10);
    }

    #[test]
    fn deep_assemble_chunky_rgb888() {
        // 2x1 RGB888: pixel0 = (10,20,30), pixel1 = (200,100,50).
        let mut dpel_body = Vec::new();
        dpel_body.extend_from_slice(&3u32.to_be_bytes());
        for c in [1u16, 2, 3] {
            dpel_body.extend_from_slice(&c.to_be_bytes());
            dpel_body.extend_from_slice(&8u16.to_be_bytes());
        }
        let dpel = Dpel::parse(&dpel_body).unwrap();
        let body = [10u8, 20, 30, 200, 100, 50];
        let rgba = assemble_deep_chunky(&dpel, 2, 1, &body).unwrap();
        assert_eq!(rgba, vec![10, 20, 30, 0xFF, 200, 100, 50, 0xFF]);
    }

    #[test]
    fn deep_assemble_chunky_rgba_with_alpha() {
        // 1x1 RGBA 8:8:8:8: (1,2,3,4).
        let mut dpel_body = Vec::new();
        dpel_body.extend_from_slice(&4u32.to_be_bytes());
        for c in [1u16, 2, 3, 4] {
            dpel_body.extend_from_slice(&c.to_be_bytes());
            dpel_body.extend_from_slice(&8u16.to_be_bytes());
        }
        let dpel = Dpel::parse(&dpel_body).unwrap();
        let body = [1u8, 2, 3, 4];
        let rgba = assemble_deep_chunky(&dpel, 1, 1, &body).unwrap();
        assert_eq!(rgba, vec![1, 2, 3, 4]);
    }

    #[test]
    fn deep_assemble_chunky_4bit_guns_scale() {
        // 1x1 RGB444 packed into 12 bits → 2 bytes (padded). (0xF,0x0,0x8).
        let mut dpel_body = Vec::new();
        dpel_body.extend_from_slice(&3u32.to_be_bytes());
        for c in [1u16, 2, 3] {
            dpel_body.extend_from_slice(&c.to_be_bytes());
            dpel_body.extend_from_slice(&4u16.to_be_bytes());
        }
        let dpel = Dpel::parse(&dpel_body).unwrap();
        assert_eq!(dpel.pixel_bytes(), 2);
        // bits MSB-first: R=1111 G=0000 B=1000 pad=0000 → 0xF0 0x80
        let body = [0xF0u8, 0x80];
        let rgba = assemble_deep_chunky(&dpel, 1, 1, &body).unwrap();
        // 0xF → 0xFF, 0x0 → 0x00, 0x8 → 0x88 (replicate high nibble).
        assert_eq!(&rgba[0..4], &[0xFF, 0x00, 0x88, 0xFF]);
    }

    #[test]
    fn deep_assemble_chunky_rejects_short_body() {
        let mut dpel_body = Vec::new();
        dpel_body.extend_from_slice(&3u32.to_be_bytes());
        for c in [1u16, 2, 3] {
            dpel_body.extend_from_slice(&c.to_be_bytes());
            dpel_body.extend_from_slice(&8u16.to_be_bytes());
        }
        let dpel = Dpel::parse(&dpel_body).unwrap();
        assert!(assemble_deep_chunky(&dpel, 2, 1, &[1, 2, 3]).is_err());
    }

    // ───────────────────── TVDC line decompression (§1.5) ─────────────────

    #[test]
    fn tvdc_pure_delta_line() {
        // Table: nibble 1 → +1, nibble 2 → -1; nibble 0 reserved as the
        // run sentinel (table[0] = 0).
        let mut table = [0i16; 16];
        table[1] = 1;
        table[2] = -1;
        // Source nibbles: 1 1 1 2  → v: 1,2,3,2  (high then low nibble).
        // bytes: 0x11, 0x12
        let source = [0x11u8, 0x12];
        let mut out = Vec::new();
        let used = decode_tvdc(&source, &table, 4, &mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3, 2]);
        assert_eq!(used, 2); // 4 nibbles = 2 bytes
    }

    #[test]
    fn tvdc_short_run_rle() {
        // table[1] = +5; table[0] = 0 (sentinel). Nibbles: 1 (v=5, emit),
        // then 0 (sentinel) 3 (run=3 → emit v three more times).
        let mut table = [0i16; 16];
        table[1] = 5;
        // bytes: 0x10, 0x30
        let source = [0x10u8, 0x30];
        let mut out = Vec::new();
        let used = decode_tvdc(&source, &table, 4, &mut out).unwrap();
        assert_eq!(out, vec![5, 5, 5, 5]);
        assert_eq!(used, 2);
    }

    #[test]
    fn tvdc_odd_nibble_rounds_up_byte_count() {
        // Three nibbles consumed (1 1 1) → used = ceil(3/2) = 2 bytes.
        let mut table = [0i16; 16];
        table[1] = 1;
        let source = [0x11u8, 0x10];
        let mut out = Vec::new();
        let used = decode_tvdc(&source, &table, 3, &mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3]);
        assert_eq!(used, 2);
    }

    #[test]
    fn tvdc_rejects_truncated_source() {
        let mut table = [0i16; 16];
        table[1] = 1;
        let source = [0x11u8]; // only 2 nibbles, need 4 outputs
        let mut out = Vec::new();
        assert!(decode_tvdc(&source, &table, 4, &mut out).is_err());
    }

    #[test]
    fn tvdc_rejects_run_overshoot() {
        let mut table = [0i16; 16];
        table[1] = 5;
        // 1 (emit) then 0 (sentinel) F (run=15) → overshoots a 2-byte line.
        let source = [0x10u8, 0xF0];
        let mut out = Vec::new();
        assert!(decode_tvdc(&source, &table, 2, &mut out).is_err());
    }

    #[test]
    fn tvdc_accumulator_wraps_to_byte() {
        // Repeated +200 deltas: v=200, 400&0xFF=144, 600&0xFF=88.
        let mut table = [0i16; 16];
        table[1] = 200;
        let source = [0x11u8, 0x10];
        let mut out = Vec::new();
        decode_tvdc(&source, &table, 3, &mut out).unwrap();
        assert_eq!(out, vec![200, 144, 88]);
    }

    // ───────────────────── FORM DEEP top-level decode ─────────────────────

    /// A DPEL body for the given `(cType, cBitDepth)` components.
    fn deep_dpel(elems: &[(u16, u16)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(elems.len() as u32).to_be_bytes());
        for (ct, depth) in elems {
            b.extend_from_slice(&ct.to_be_bytes());
            b.extend_from_slice(&depth.to_be_bytes());
        }
        b
    }

    /// An 8-byte DGBL body.
    fn deep_dgbl(dw: u16, dh: u16, compression: u16) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b[0..2].copy_from_slice(&dw.to_be_bytes());
        b[2..4].copy_from_slice(&dh.to_be_bytes());
        b[4..6].copy_from_slice(&compression.to_be_bytes());
        b[6] = 1; // x aspect
        b[7] = 1; // y aspect
        b
    }

    /// An 8-byte DLOC body.
    fn deep_dloc(w: u16, h: u16, x: i16, y: i16) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b[0..2].copy_from_slice(&w.to_be_bytes());
        b[2..4].copy_from_slice(&h.to_be_bytes());
        b[4..6].copy_from_slice(&x.to_be_bytes());
        b[6..8].copy_from_slice(&y.to_be_bytes());
        b
    }

    #[test]
    fn parse_deep_nocompression_rgb888() {
        // 2x1 RGB888 chunky body, dimensions from DGBL display size.
        let file = iff_form(
            b"DEEP",
            &[
                (b"DGBL", deep_dgbl(2, 1, 0)),
                (b"DPEL", deep_dpel(&[(1, 8), (2, 8), (3, 8)])),
                (b"DBOD", vec![10, 20, 30, 200, 100, 50]),
            ],
        );
        let img = parse_deep(&file).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![10, 20, 30, 0xFF, 200, 100, 50, 0xFF]);
        assert_eq!(img.dgbl.compression, DeepCompression::None);
    }

    #[test]
    fn parse_deep_dloc_overrides_dimensions() {
        // DGBL says 8x8 but the DLOC narrows the DBOD to 2x1.
        let file = iff_form(
            b"DEEP",
            &[
                (b"DGBL", deep_dgbl(8, 8, 0)),
                (b"DPEL", deep_dpel(&[(1, 8), (2, 8), (3, 8)])),
                (b"DLOC", deep_dloc(2, 1, 0, 0)),
                (b"DBOD", vec![1, 2, 3, 4, 5, 6]),
            ],
        );
        let img = parse_deep(&file).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert!(img.dloc.is_some());
        assert_eq!(&img.rgba[0..4], &[1, 2, 3, 0xFF]);
    }

    #[test]
    fn parse_deep_missing_dgbl_rejected() {
        let file = iff_form(
            b"DEEP",
            &[
                (b"DPEL", deep_dpel(&[(1, 8), (2, 8), (3, 8)])),
                (b"DBOD", vec![1, 2, 3]),
            ],
        );
        assert!(parse_deep(&file).is_err());
    }

    #[test]
    fn parse_deep_tvdc_in_form_is_a_documented_gap() {
        // A TVDC DBOD cannot be decoded from the FORM alone (no in-FORM table).
        let file = iff_form(
            b"DEEP",
            &[
                (b"DGBL", deep_dgbl(2, 1, 5)), // compression = 5 = TVDC
                (b"DPEL", deep_dpel(&[(1, 8), (2, 8), (3, 8)])),
                (b"DBOD", vec![0, 0]),
            ],
        );
        assert!(parse_deep(&file).is_err());
    }

    #[test]
    fn parse_deep_jpeg_body_rejected() {
        let file = iff_form(
            b"DEEP",
            &[
                (b"DGBL", deep_dgbl(2, 1, 4)), // compression = 4 = JPEG
                (b"DPEL", deep_dpel(&[(1, 8), (2, 8), (3, 8)])),
                (b"DBOD", vec![0, 0]),
            ],
        );
        assert!(parse_deep(&file).is_err());
    }

    #[test]
    fn assemble_deep_tvdc_per_component_lines() {
        // 3x1 RGB888, TVDC: one Red line, one Green line, one Blue line.
        // Table: nibble 1 → +1, nibble 2 → -5; nibble 0 = run sentinel.
        let mut table = [0i16; 16];
        table[1] = 1;
        table[2] = -5;
        let dpel = Dpel::parse(&deep_dpel(&[(1, 8), (2, 8), (3, 8)])).unwrap();

        // Red line: nibbles 1 1 1 → v = 1,2,3.   bytes 0x11 0x10
        // Green line: nibbles 2 2 2 → v = -5,-10,-15 → &0xFF = 251,246,241
        //             bytes 0x22 0x20
        // Blue line: nibbles 1 0 2 → 1 emits v=1; 0 = run, next nibble 2 =>
        //            emit current v (1) two more times → 1,1,1. bytes 0x10 0x20
        let mut body = Vec::new();
        body.extend_from_slice(&[0x11, 0x10]); // red
        body.extend_from_slice(&[0x22, 0x20]); // green
        body.extend_from_slice(&[0x10, 0x20]); // blue
        let rgba = assemble_deep_tvdc(&dpel, 3, 1, &table, &body).unwrap();
        assert_eq!(&rgba[0..4], &[1, 251, 1, 0xFF]);
        assert_eq!(&rgba[4..8], &[2, 246, 1, 0xFF]);
        assert_eq!(&rgba[8..12], &[3, 241, 1, 0xFF]);
    }

    #[test]
    fn assemble_deep_tvdc_rejects_sub_8bit_component() {
        let table = [0i16; 16];
        let dpel = Dpel::parse(&deep_dpel(&[(1, 4), (2, 4), (3, 4)])).unwrap();
        assert!(assemble_deep_tvdc(&dpel, 1, 1, &table, &[0, 0]).is_err());
    }
}
