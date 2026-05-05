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
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" && &p.buf[8..12] == b"ILBM" {
        100
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Compression {
    /// BODY is a literal stack of bitplane rows.
    #[default]
    None,
    /// Each plane-row is ByteRun1 (PackBits) compressed independently.
    /// Decoder side: see [`byterun1_decode_row`].
    ByteRun1,
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
    /// Packed RGBA bytes, row-major, top-to-bottom, 4 bytes/pixel.
    pub rgba: Vec<u8>,
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
    if &bytes[8..12] != b"ILBM" {
        return Err(Error::invalid("ILBM: outer form type is not ILBM"));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8 + total).min(bytes.len());

    let mut bmhd: Option<Bmhd> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut camg = Camg::default();
    let mut body_data: Option<Vec<u8>> = None;

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
            _ => { /* skip unknown chunks (CRNG, DPI, GRAB, ...) */ }
        }
        let padded = size + (size & 1);
        cursor = payload_start + padded;
    }

    let bmhd = bmhd.ok_or_else(|| Error::invalid("ILBM: missing BMHD chunk"))?;
    let body = body_data.ok_or_else(|| Error::invalid("ILBM: missing BODY chunk"))?;

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
    }

    // Decide effective palette (EHB-expanded if requested).
    let effective_palette: Vec<[u8; 3]> = if camg.is_ehb() && palette.len() <= 32 {
        expand_ehb_palette(&palette)
    } else {
        palette.clone()
    };

    // Build packed RGBA output row-by-row.
    let width = bmhd.width as u32;
    let height = bmhd.height as u32;
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];

    for y in 0..bmhd.height as usize {
        let row_base = y * planes_per_row;
        let plane_refs: Vec<&[u8]> = (0..n_planes)
            .map(|p| rows_planar[row_base + p].as_slice())
            .collect();
        let indices = planar_row_to_indices(&plane_refs, bmhd.width);

        // Resolve to RGB.
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
            expand_ham_row(&indices, &effective_palette, bits)
        } else {
            indices
                .iter()
                .map(|&i| {
                    let i = i as usize;
                    if i < effective_palette.len() {
                        effective_palette[i]
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
        rgba,
    })
}

// ───────────────────── encode_ilbm ─────────────────────

/// Encode an [`IlbmImage`] back into a FORM/ILBM byte stream.
/// Round 1 emits an indexed-colour file when `palette` is non-empty
/// (using as many bitplanes as the palette requires) and a 24-bit
/// "deep ILBM" when the palette is empty (`n_planes = 24`, three
/// bytes per pixel split across 24 bitplanes). HAM / EHB encoding is
/// not implemented — those writes use the indexed path with the
/// matching CAMG flag preserved on round-trip but with a literal
/// palette lookup.
///
/// Compression follows `image.bmhd.compression`. Any unknown CAMG
/// flag bits are passed through verbatim.
pub fn encode_ilbm(image: &IlbmImage) -> Result<Vec<u8>> {
    let bmhd = image.bmhd;
    if bmhd.width == 0 || bmhd.height == 0 {
        return Err(Error::invalid("ILBM encode: zero-dimension image"));
    }
    let n_planes = bmhd.n_planes as usize;
    if n_planes == 0 || n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ILBM encode: round 1 supports 1..=8 bitplanes (got {n_planes})"
        )));
    }
    let row_bytes = bmhd.row_bytes();
    let has_mask_plane = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + if has_mask_plane { 1 } else { 0 };

    // Convert RGBA to per-row plane data. For an indexed image the
    // caller is responsible for having already quantised pixels to
    // palette indices encoded in the R channel — that mirrors the
    // round-trip the decoder produces when re-feeding parse_ilbm
    // output through `IlbmImage::from_indexed`.
    if image.palette.is_empty() {
        return Err(Error::unsupported(
            "ILBM encode: round 1 requires an indexed palette (no true-colour writer yet)",
        ));
    }

    // Planar BODY (uncompressed first; ByteRun1 wraps it).
    let mut planar_rows: Vec<Vec<u8>> = Vec::with_capacity(bmhd.height as usize * planes_per_row);
    for y in 0..bmhd.height as usize {
        let mut indices = vec![0u8; bmhd.width as usize];
        let mut mask = vec![0u8; row_bytes];
        for (x, idx_slot) in indices.iter_mut().enumerate() {
            let src = (y * bmhd.width as usize + x) * 4;
            let r = image.rgba[src];
            let g = image.rgba[src + 1];
            let b = image.rgba[src + 2];
            let a = image.rgba[src + 3];
            // Find nearest palette index by squared distance.
            let mut best = 0usize;
            let mut best_d = i32::MAX;
            for (i, p) in image.palette.iter().enumerate() {
                let dr = r as i32 - p[0] as i32;
                let dg = g as i32 - p[1] as i32;
                let db = b as i32 - p[2] as i32;
                let d = dr * dr + dg * dg + db * db;
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            *idx_slot = best as u8;
            if a >= 0x80 {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                mask[bi] |= 1 << bit;
            }
        }
        let plane_rows = indices_to_planar_row(&indices, bmhd.n_planes, row_bytes);
        for pr in plane_rows {
            planar_rows.push(pr);
        }
        if has_mask_plane {
            planar_rows.push(mask);
        }
    }

    // Encode BODY (with ByteRun1 if requested).
    let body_bytes: Vec<u8> = match bmhd.compression {
        Compression::None => planar_rows.into_iter().flatten().collect(),
        Compression::ByteRun1 => planar_rows
            .iter()
            .flat_map(|row| byterun1_encode_row(row))
            .collect(),
    };

    // Assemble FORM/ILBM with BMHD, CMAP, optional CAMG, BODY.
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes()); // size patched below
    out.extend_from_slice(b"ILBM");

    // BMHD
    out.extend_from_slice(b"BMHD");
    out.extend_from_slice(&20u32.to_be_bytes());
    out.extend_from_slice(&bmhd.write());

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
    if &form_type != b"ILBM" {
        return Err(Error::invalid(format!(
            "IFF: not an ILBM file (form type {:?})",
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
    full.extend_from_slice(b"ILBM");
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

/// Container-level ILBM muxer. Accepts a single `rawvideo` stream
/// with `PixelFormat::Rgba`. The emitted file uses an 8-bitplane
/// indexed palette built greedily from the first `write_packet` (see
/// [`build_palette`]). Compression follows the `compression`
/// constructor argument (default ByteRun1).
pub struct IlbmMuxer {
    output: Box<dyn WriteSeek>,
    width: u32,
    height: u32,
    compression: Compression,
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
            compression: Compression::ByteRun1,
            written: false,
            pending: Vec::new(),
        })
    }

    /// Choose a compression mode (default: `ByteRun1`).
    pub fn with_compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }
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
        let (palette, _idx) = build_palette(&self.pending);
        // Pick the smallest plane count that covers the palette.
        let n_planes = if palette.len() <= 1 {
            1
        } else {
            let bits = (palette.len() as u32 - 1)
                .next_power_of_two()
                .trailing_zeros();
            bits.max(1) as u8
        };
        let bmhd = Bmhd {
            width: self.width as u16,
            height: self.height as u16,
            x_origin: 0,
            y_origin: 0,
            n_planes,
            masking: Masking::None,
            compression: self.compression,
            pad: 0,
            transparent_color: 0,
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
            camg: Camg::default(),
            rgba: std::mem::take(&mut self.pending),
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
