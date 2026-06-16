//! FORM ANIM — animated ILBM, the Aegis Animator / DPaint III sequence
//! container.
//!
//! Layout (from "ANIM IFF Cel Animation", Gary Bonham 1988):
//!
//! ```text
//! FORM <size> ANIM
//!   FORM <size> ILBM        # frame 0 — full ILBM (BMHD/CMAP/CAMG/BODY)
//!   FORM <size> ILBM        # frame 1+ — carries an ANHD chunk
//!     ANHD <40>             #   Animation Header (op + flags + dims)
//!     BODY <size>           #   delta payload (op-specific layout)
//!   ...
//! ```
//!
//! `ANHD.operation` selects the delta encoder. The operations
//! commonly seen in the wild:
//!
//! * **op 0** — full BODY (uncompressed, same shape as the leading
//!   FORM ILBM frame). We treat this as a fresh frame.
//! * **op 2 / op 3** — Long / Short Delta mode (spec §1.2.2 / §1.2.3,
//!   wire format §2.2.1). The DLTA begins with 8 big-endian u32
//!   pointers (one per bitplane; `0` = plane unchanged). Each plane's
//!   data is a list of groups; offsets and counts are big-endian
//!   shorts, data words are longs (op 2) or shorts (op 3). The
//!   bitplane is treated as the contiguous word array Amiga memory
//!   holds (`height * row_bytes` bytes — long words may straddle row
//!   boundaries when `row_bytes` isn't a multiple of 4). A word
//!   cursor starts pointing at the first word of the plane; per group
//!   a positive offset advances the cursor and the following data
//!   word is placed there, a negative offset (absolute value =
//!   offset + 2) advances the cursor and the following count short
//!   says how many contiguous data words follow; `0xFFFF` terminates
//!   the plane's list.
//! * **op 5** — Byte Vertical Delta. For each bitplane the delta is a
//!   list of `width / 8` columns of "ops"; each op is a 1-byte count
//!   plus N bytes of either repeats (top bit set) or literal columns
//!   walked top-to-bottom.
//! * **op 7** — Short/Long Vertical Delta. The bitplane is split into
//!   vertical columns whose width is the data-item size (2 bytes if
//!   `ANHD.bits` bit 0 = 0, "short"; 4 bytes if bit 0 = 1, "long").
//!   The DLTA chunk begins with 16 big-endian u32 pointers (8 opcode
//!   pointers + 8 data pointers); per plane the opcode and data lists
//!   live at independent offsets. Each column starts with an
//!   `op_count` byte, then `op_count` opcode bytes; the three opcode
//!   classes are Skip (hi bit clear, non-zero — forward dest cursor),
//!   Uniq (hi bit set — copy `byte & 0x7F` data items literally), and
//!   Same (`0x00` byte followed by a count byte — copy one data item
//!   `count` times). The "dest" cursor walks rows by adding
//!   `row_bytes` (NOT data-item width) per step, and the column starts
//!   at byte offset `column_index * data_size` within the plane row.
//!
//! Round 2 implements op 0 + op 5 (the format DPaint III emits);
//! round 192 adds op 7 (short / long vertical delta) decode; round
//! 276 adds op 2 / op 3 (Long / Short Delta) decode + encode. Other
//! operations surface `Error::Unsupported` for diagnosability.
//!
//! Source: the public **ANIM IFF Cel Animation** spec (Gary Bonham,
//! 1988). No third-party loader code consulted.

// 2D pixel/plane loops where the index is used to address multiple
// parallel arrays (planar rows × column bytes × frame state). Per-
// element iterators would require zip(), enumerate(), and an extra
// helper just to compute the address; the explicit-index form is
// the clearer expression.
#![allow(clippy::needless_range_loop)]

use std::io::Read;

use oxideav_core::ReadSeek;
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType, Packet,
    PixelFormat, Result, StreamInfo, TimeBase,
};

use crate::chunk::{read_chunk_header, read_form_type, GROUP_FORM};
use crate::ilbm::{
    byterun1_decode_row, byterun1_encode_row, expand_ehb_palette, expand_ham_row,
    indices_to_planar_row, parse_ilbm, planar_row_to_indices, Bmhd, Camg, Compression, IlbmImage,
    Masking,
};

/// Install the FORM/ANIM demuxer into a container registry. The
/// registered codec id matches the seed-frame's `rawvideo`+RGBA shape;
/// every decoded frame is emitted as a single keyframe packet at
/// `pts = i * rel_time`. Round 2 doesn't ship a muxer here — the
/// `anim::encode_anim_op0` helper is the only writer (used by tests).
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("iff_anim", open);
    reg.register_extension("anim", "iff_anim");
    reg.register_probe("iff_anim", probe_data);
}

fn probe_data(p: &oxideav_core::ProbeData) -> u8 {
    probe(p.buf)
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let hdr = read_chunk_header(&mut *input)?.ok_or_else(|| Error::invalid("ANIM: empty file"))?;
    if hdr.id != GROUP_FORM {
        return Err(Error::invalid(format!(
            "ANIM: expected FORM chunk, got {}",
            hdr.id_str()
        )));
    }
    let form_type = read_form_type(&mut *input)?;
    if &form_type != b"ANIM" {
        return Err(Error::invalid(format!(
            "IFF: not an ANIM file (form type {:?})",
            std::str::from_utf8(&form_type).unwrap_or("????")
        )));
    }
    let body_size = hdr.size as u64 - 4;
    let mut form_body = vec![0u8; body_size as usize];
    input.read_exact(&mut form_body)?;
    let mut full = Vec::with_capacity(8 + 4 + form_body.len());
    full.extend_from_slice(b"FORM");
    full.extend_from_slice(&hdr.size.to_be_bytes());
    full.extend_from_slice(b"ANIM");
    full.extend_from_slice(&form_body);
    let anim = parse_anim(&full)?;

    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(anim.width);
    params.height = Some(anim.height);
    params.pixel_format = Some(PixelFormat::Rgba);
    let frames_count = anim.frames.len() as i64;
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 60),
        duration: Some(frames_count),
        start_time: Some(0),
        params,
    };
    Ok(Box::new(AnimDemuxer {
        streams: vec![stream],
        frames: anim.frames.into_iter().map(|f| f.rgba).collect(),
        next: 0,
    }))
}

struct AnimDemuxer {
    streams: Vec<StreamInfo>,
    frames: Vec<Vec<u8>>,
    next: usize,
}

impl Demuxer for AnimDemuxer {
    fn format_name(&self) -> &str {
        "iff_anim"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        if self.next >= self.frames.len() {
            return Err(Error::Eof);
        }
        let i = self.next;
        let data = std::mem::take(&mut self.frames[i]);
        self.next += 1;
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(i as i64);
        pkt.dts = Some(i as i64);
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

/// `ANHD` — Animation Header. 40-byte chunk per spec, but only a few
/// fields drive decoding.
#[derive(Clone, Copy, Debug, Default)]
pub struct Anhd {
    /// Compression operation (`0..=8`); we implement `0` (full BODY)
    /// and `5` (byte vertical delta).
    pub operation: u8,
    /// Mask flag — unused for op 5.
    pub mask: u8,
    pub w: u16,
    pub h: u16,
    pub x: i16,
    pub y: i16,
    pub abs_time: u32,
    pub rel_time: u32,
    /// Interleave count. `0` is interpreted as `2` per spec (double-
    /// buffering: a delta references the frame two back).
    pub interleave: u8,
    pub pad0: u8,
    pub bits: u32,
}

impl Anhd {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 24 {
            return Err(Error::invalid(format!(
                "ANIM ANHD: need ≥24 bytes, got {}",
                body.len()
            )));
        }
        Ok(Self {
            operation: body[0],
            mask: body[1],
            w: u16::from_be_bytes([body[2], body[3]]),
            h: u16::from_be_bytes([body[4], body[5]]),
            x: i16::from_be_bytes([body[6], body[7]]),
            y: i16::from_be_bytes([body[8], body[9]]),
            abs_time: u32::from_be_bytes([body[10], body[11], body[12], body[13]]),
            rel_time: u32::from_be_bytes([body[14], body[15], body[16], body[17]]),
            interleave: body[18],
            pad0: body[19],
            bits: u32::from_be_bytes([body[20], body[21], body[22], body[23]]),
        })
    }

    pub fn write(&self, body_size: u32) -> [u8; 40] {
        // ANHD body: above fields + 16 reserved bytes (zero-fill).
        // `body_size` is a hint stored in the header so a player can
        // skip the body without scanning chunks.
        let mut out = [0u8; 40];
        out[0] = self.operation;
        out[1] = self.mask;
        out[2..4].copy_from_slice(&self.w.to_be_bytes());
        out[4..6].copy_from_slice(&self.h.to_be_bytes());
        out[6..8].copy_from_slice(&self.x.to_be_bytes());
        out[8..10].copy_from_slice(&self.y.to_be_bytes());
        out[10..14].copy_from_slice(&self.abs_time.to_be_bytes());
        out[14..18].copy_from_slice(&self.rel_time.to_be_bytes());
        out[18] = self.interleave;
        out[19] = self.pad0;
        out[20..24].copy_from_slice(&self.bits.to_be_bytes());
        // The `body_size` hint is stored in bytes 24..28 in some
        // encoders' interpretation. Spec is fuzzy; we keep it for
        // forward compat but place it in the reserved region.
        out[24..28].copy_from_slice(&body_size.to_be_bytes());
        out
    }
}

/// Decoded ANIM container — the leading frame plus the delta-decoded
/// follow-on frames.
#[derive(Clone, Debug)]
pub struct AnimImage {
    /// Width/height shared by all frames (taken from the leading BMHD).
    pub width: u32,
    pub height: u32,
    /// Frame `0` is always the seed. Each subsequent frame is the
    /// running state after applying its delta.
    pub frames: Vec<IlbmImage>,
}

/// Probe: a `FORM .... ANIM` magic at the start.
pub fn probe(buf: &[u8]) -> u8 {
    if buf.len() >= 12 && &buf[0..4] == b"FORM" && &buf[8..12] == b"ANIM" {
        100
    } else {
        0
    }
}

/// Parse a FORM/ANIM container.
///
/// Currently supports `ANHD.operation = 0` (literal full BODY),
/// `ANHD.operation = 1` (XOR ILBM mode, full-frame rectangle —
/// all-planes or §2.1 `mask` plane-subset),
/// `ANHD.operation = 2` / `= 3` (Long / Short Delta mode),
/// `ANHD.operation = 4` (Generalized short/long Delta mode),
/// `ANHD.operation = 5` (Byte Vertical Delta) and `ANHD.operation = 7`
/// (Short / Long Vertical Delta). Other operations
/// return `Error::Unsupported`.
///
/// Each delta frame's §2.1 `ANHD.interleave` field selects which earlier
/// frame the delta modifies: `interleave = n` references the buffer `n`
/// frames back, and `interleave = 0` defaults to **two** frames back
/// (the DeluxePaint double-buffering convention). The reference is
/// clamped to the seed frame for the first delta(s), matching the §1.3
/// bootstrap where both double-buffers hold a copy of frame 0.
pub fn parse_anim(bytes: &[u8]) -> Result<AnimImage> {
    if bytes.len() < 12 {
        return Err(Error::invalid("ANIM: file shorter than FORM header"));
    }
    if &bytes[0..4] != b"FORM" {
        return Err(Error::invalid("ANIM: missing FORM signature"));
    }
    if &bytes[8..12] != b"ANIM" {
        return Err(Error::invalid(format!(
            "ANIM: outer form type is {:?} (expected ANIM)",
            std::str::from_utf8(&bytes[8..12]).unwrap_or("????")
        )));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8 + total).min(bytes.len());

    // Walk the outer FORM. Children are nested FORM ILBM groups.
    let mut frames: Vec<IlbmImage> = Vec::new();
    // Per-frame planar history. `planar_history[i]` is the reconstructed
    // planar bitmap for output frame `i`. A delta frame modifies the
    // buffer `interleave` frames back (§2.1 ANHD `interleave`; `0`
    // defaults to two frames back — the DeluxePaint double-buffering
    // convention) rather than always the immediately-previous frame.
    let mut planar_history: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut bmhd: Option<Bmhd> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut camg = Camg::default();

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
        let body_start = cursor + 8;
        let body_end_inner = body_start + size;
        if body_end_inner > body_end {
            return Err(Error::invalid(format!(
                "ANIM: child chunk {} extends past outer FORM",
                std::str::from_utf8(&id).unwrap_or("????")
            )));
        }

        if &id == b"FORM" && body_end_inner >= body_start + 4 {
            let inner_form = [
                bytes[body_start],
                bytes[body_start + 1],
                bytes[body_start + 2],
                bytes[body_start + 3],
            ];
            if &inner_form == b"ILBM" {
                if frames.is_empty() {
                    // First ILBM: full image. Hand the whole inner
                    // FORM to parse_ilbm.
                    let mut full = Vec::with_capacity(8 + size);
                    full.extend_from_slice(b"FORM");
                    full.extend_from_slice(&(size as u32).to_be_bytes());
                    full.extend_from_slice(&bytes[body_start..body_end_inner]);
                    let img = parse_ilbm(&full)?;
                    bmhd = Some(img.bmhd);
                    palette = img.palette.clone();
                    camg = img.camg;
                    // Recover the planar form so we can apply deltas
                    // against it. Re-encode by walking the BODY.
                    planar_history.push(rgba_to_planar(&img));
                    frames.push(img);
                } else {
                    // Subsequent ILBM: delta. Walk the inner FORM for
                    // ANHD + BODY.
                    let bmhd =
                        bmhd.ok_or_else(|| Error::invalid("ANIM: delta frame before any BMHD"))?;
                    let palette = palette.clone();
                    let camg = camg;
                    let mut anhd: Option<Anhd> = None;
                    let mut delta_body: Option<Vec<u8>> = None;
                    let mut sub = body_start + 4; // skip "ILBM"
                    while sub + 8 <= body_end_inner {
                        let cid = [bytes[sub], bytes[sub + 1], bytes[sub + 2], bytes[sub + 3]];
                        let csize = u32::from_be_bytes([
                            bytes[sub + 4],
                            bytes[sub + 5],
                            bytes[sub + 6],
                            bytes[sub + 7],
                        ]) as usize;
                        let cdata_start = sub + 8;
                        let cdata_end = cdata_start + csize;
                        if cdata_end > body_end_inner {
                            return Err(Error::invalid("ANIM: inner chunk overruns FORM ILBM"));
                        }
                        match &cid {
                            b"ANHD" => anhd = Some(Anhd::parse(&bytes[cdata_start..cdata_end])?),
                            // Op-5 / op-0 emit `BODY`; op-7 emits
                            // `DLTA` (per the Appendix). Both are
                            // delta payloads from the per-frame
                            // operation's perspective so we map both
                            // into `delta_body`.
                            b"BODY" | b"DLTA" => {
                                delta_body = Some(bytes[cdata_start..cdata_end].to_vec())
                            }
                            _ => {} // skip CMAP/DPI/etc on delta frames
                        }
                        sub = cdata_start + csize + (csize & 1);
                    }
                    let anhd =
                        anhd.ok_or_else(|| Error::invalid("ANIM: delta frame missing ANHD chunk"))?;
                    let delta = delta_body.ok_or_else(|| {
                        Error::invalid("ANIM: delta frame missing BODY/DLTA chunk")
                    })?;
                    // §2.1: `interleave` is how many frames back this
                    // delta modifies. `0` defaults to two frames back
                    // (double-buffering). The reference is clamped to the
                    // seed frame for the first delta(s), which is exactly
                    // the §1.3 bootstrap where both double-buffers hold a
                    // copy of frame 0 before the first delta is applied.
                    let back = if anhd.interleave == 0 {
                        2
                    } else {
                        anhd.interleave as usize
                    };
                    let cur_index = frames.len();
                    // Saturating: when `back` would point before frame 0
                    // (the first one or two deltas) the seed buffer is the
                    // referenced double-buffer per §1.3.
                    let src_index = cur_index.saturating_sub(back);
                    let mut planar = planar_history.get(src_index).cloned().ok_or_else(|| {
                        Error::invalid("ANIM: delta frame with no prior planar state")
                    })?;
                    apply_delta(&anhd, &mut planar, &delta, &bmhd)?;
                    let img = planar_to_rgba(&planar, &bmhd, &palette, &camg)?;
                    planar_history.push(planar);
                    frames.push(img);
                }
            }
            // Non-ILBM nested FORMs (LIST etc.) are skipped.
        }

        let padded = size + (size & 1);
        cursor = body_start + padded;
    }

    let bmhd = bmhd.ok_or_else(|| Error::invalid("ANIM: no ILBM frames"))?;
    Ok(AnimImage {
        width: bmhd.width as u32,
        height: bmhd.height as u32,
        frames,
    })
}

/// Re-pack a decoded RGBA frame back into the planar bitplane form
/// the delta decoder operates on. The encoder side keeps decoded
/// indices around for delta application.
///
/// For HAM frames we actually re-pack the raw indices (top-2-bits +
/// channel value) — but the decoder side only kept RGB output, so we
/// reconstruct nearest-palette indices. For round-2 HAM ANIM is best-
/// effort; the per-pixel index isn't a function of RGB alone, so we
/// use a palette nearest-fit and surface that as a known limitation.
fn rgba_to_planar(image: &IlbmImage) -> Vec<Vec<u8>> {
    let bmhd = image.bmhd;
    let n_planes = bmhd.n_planes as usize;
    let row_bytes = bmhd.row_bytes();
    let has_mask = bmhd.masking == Masking::HasMask;
    let pal: Vec<[u8; 3]> = if image.camg.is_ehb() && image.palette.len() <= 32 {
        expand_ehb_palette(&image.palette)
    } else if image.palette.is_empty() {
        Vec::new()
    } else {
        image.palette.clone()
    };
    let mut out: Vec<Vec<u8>> =
        Vec::with_capacity(bmhd.height as usize * (n_planes + has_mask as usize));
    for y in 0..bmhd.height as usize {
        let mut indices = vec![0u8; bmhd.width as usize];
        let mut mask = vec![0u8; row_bytes];
        for x in 0..bmhd.width as usize {
            let src = (y * bmhd.width as usize + x) * 4;
            let r = image.rgba[src];
            let g = image.rgba[src + 1];
            let b = image.rgba[src + 2];
            let a = image.rgba[src + 3];
            // Nearest match.
            let mut best = 0usize;
            let mut best_d = i32::MAX;
            for (i, p) in pal.iter().enumerate() {
                let dr = r as i32 - p[0] as i32;
                let dg = g as i32 - p[1] as i32;
                let db = b as i32 - p[2] as i32;
                let d = dr * dr + dg * dg + db * db;
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            indices[x] = best as u8;
            if a >= 0x80 {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                mask[bi] |= 1 << bit;
            }
        }
        let plane_rows = indices_to_planar_row(&indices, bmhd.n_planes, row_bytes);
        for pr in plane_rows {
            out.push(pr);
        }
        if has_mask {
            out.push(mask);
        }
    }
    out
}

/// Inverse of `rgba_to_planar`: build an RGBA `IlbmImage` from the
/// running planar state.
fn planar_to_rgba(
    planar: &[Vec<u8>],
    bmhd: &Bmhd,
    palette: &[[u8; 3]],
    camg: &Camg,
) -> Result<IlbmImage> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let width = bmhd.width as u32;
    let height = bmhd.height as u32;
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let pal: Vec<[u8; 3]> = if camg.is_ehb() && palette.len() <= 32 {
        expand_ehb_palette(palette)
    } else {
        palette.to_vec()
    };
    for y in 0..bmhd.height as usize {
        let row_base = y * planes_per_row;
        let plane_refs: Vec<&[u8]> = (0..n_planes)
            .map(|p| planar[row_base + p].as_slice())
            .collect();
        let indices = planar_row_to_indices(&plane_refs, bmhd.width);
        let rgb_row: Vec<[u8; 3]> = if camg.is_ham() {
            let bits = match n_planes {
                6 => 4u8,
                8 => 6u8,
                other => {
                    return Err(Error::unsupported(format!(
                        "ANIM HAM: unsupported plane count {other}"
                    )))
                }
            };
            expand_ham_row(&indices, &pal, bits)
        } else {
            indices
                .iter()
                .map(|&i| {
                    let i = i as usize;
                    if i < pal.len() {
                        pal[i]
                    } else {
                        [0, 0, 0]
                    }
                })
                .collect()
        };
        let mask_row: Option<&[u8]> = if has_mask {
            Some(planar[row_base + n_planes].as_slice())
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
            } else {
                0xFF
            };
            rgba[dst + 3] = alpha;
        }
    }
    Ok(IlbmImage {
        width,
        height,
        bmhd: *bmhd,
        palette: palette.to_vec(),
        camg: *camg,
        rgba,
        ..IlbmImage::default()
    })
}

/// Test-only re-export of [`apply_op5`] so integration tests can
/// drive the decoder without rebuilding a full ANIM container.
#[doc(hidden)]
pub fn apply_op5_for_test(
    anhd: &Anhd,
    planar: &mut [Vec<u8>],
    delta: &[u8],
    bmhd: &Bmhd,
) -> Result<()> {
    let _ = anhd;
    apply_op5(planar, delta, bmhd)
}

/// Test-only re-export of [`apply_op1`] so integration tests can drive
/// the XOR decoder without rebuilding a full ANIM container.
#[doc(hidden)]
pub fn apply_op1_for_test(
    anhd: &Anhd,
    planar: &mut [Vec<u8>],
    delta: &[u8],
    bmhd: &Bmhd,
) -> Result<()> {
    apply_op1(anhd, planar, delta, bmhd)
}

/// Apply a single ANHD-tagged delta to the running planar state.
fn apply_delta(anhd: &Anhd, planar: &mut [Vec<u8>], delta: &[u8], bmhd: &Bmhd) -> Result<()> {
    match anhd.operation {
        0 => {
            // Op 0: full literal BODY (uncompressed). Same shape as the
            // seed frame. We re-decode it into `planar` overwriting
            // every row.
            let n_planes = bmhd.n_planes as usize;
            let has_mask = bmhd.masking == Masking::HasMask;
            let planes_per_row = n_planes + has_mask as usize;
            let row_bytes = bmhd.row_bytes();
            let need = bmhd.height as usize * planes_per_row * row_bytes;
            if bmhd.compression == Compression::None {
                if delta.len() < need {
                    return Err(Error::invalid("ANIM op 0: short BODY"));
                }
                for (i, chunk) in delta[..need].chunks_exact(row_bytes).enumerate() {
                    planar[i] = chunk.to_vec();
                }
            } else {
                // ByteRun1 frames: decode row by row.
                let mut input = delta;
                for i in 0..bmhd.height as usize * planes_per_row {
                    let mut row = Vec::with_capacity(row_bytes);
                    let consumed = byterun1_decode_row(input, row_bytes, &mut row)?;
                    input = &input[consumed..];
                    planar[i] = row;
                }
            }
            Ok(())
        }
        // Op 1 = XOR ILBM mode (§1.2.1) — the BODY decodes to a planar
        // XOR-bitmap that is XOR'd into the running state.
        1 => apply_op1(anhd, planar, delta, bmhd),
        // Op 2 = Long Delta (4-byte data words), op 3 = Short Delta
        // (2-byte data words). Same group grammar otherwise (§2.2.1).
        2 => apply_op23(planar, delta, bmhd, true),
        3 => apply_op23(planar, delta, bmhd, false),
        // Op 4 = Generalized short/long Delta (§2.2.2). The §2.2.2
        // `SetDLTAshort` reference routine documents the
        // short-data / vertical / RLC / separate-info / non-XOR
        // configuration; `ANHD.bits` selects the variant.
        4 => apply_op4(anhd, planar, delta, bmhd),
        5 => apply_op5(planar, delta, bmhd),
        7 => {
            // Op 7 honours bit 0 of ANHD.bits: 0 = short data (2 B),
            // 1 = long data (4 B).
            let long_data = (anhd.bits & 1) != 0;
            apply_op7(planar, delta, bmhd, long_data)
        }
        other => Err(Error::unsupported(format!(
            "ANIM: ANHD operation {other} not implemented (only 0, 1, 2, 3, 4, 5 and 7 supported)"
        ))),
    }
}

/// Op 5 — Byte Vertical Delta (the most common ANIM5/DPaint III mode).
///
/// The first `8 * 4` bytes of the delta are eight u32-BE pointers
/// (one per bitplane, slots 0..=7); slot `p` is either `0`
/// (plane unchanged) or a byte offset into the *same* buffer at which
/// plane `p`'s "data list" starts. Each data list walks the plane's
/// columns left-to-right; per column the encoder maintains an
/// implicit row cursor starting at row 0. The op-byte stream is:
///
/// * `op == 0` — column terminator (advance to next column).
/// * `op` with top bit clear — skip `op` rows (advance cursor by `op`).
/// * `op == 0x80` — long: next byte `cnt`, then 1 repeat byte `v`;
///   write `v` to the next `cnt` rows.
/// * other `op` with top bit set — short: low 7 bits = `cnt`; write
///   `cnt` literal bytes one per row.
///
/// All write-ops also advance the row cursor by `cnt`. The column
/// terminator does not consume the cursor — the cursor is reset
/// implicitly when we move to the next column.
fn apply_op5(planar: &mut [Vec<u8>], delta: &[u8], bmhd: &Bmhd) -> Result<()> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;

    // Pointer table: 8 u32 BE pointers per spec (max 8 colour planes).
    if delta.len() < 32 {
        return Err(Error::invalid("ANIM op 5: pointer table truncated"));
    }
    for p in 0..n_planes {
        let off = u32::from_be_bytes([
            delta[p * 4],
            delta[p * 4 + 1],
            delta[p * 4 + 2],
            delta[p * 4 + 3],
        ]) as usize;
        if off == 0 {
            continue; // plane unchanged
        }
        if off >= delta.len() {
            return Err(Error::invalid("ANIM op 5: data pointer out of range"));
        }
        let mut cursor = off;
        for col in 0..row_bytes {
            let mut row: usize = 0;
            // Ops for this column.
            loop {
                if cursor >= delta.len() {
                    return Err(Error::invalid("ANIM op 5: data list truncated mid-column"));
                }
                let op = delta[cursor];
                cursor += 1;
                if op == 0 {
                    // End of this column's ops; advance to next column.
                    break;
                } else if op & 0x80 == 0 {
                    // Skip `op` rows.
                    row += op as usize;
                } else if op == 0x80 {
                    // Long form: next byte = count, next byte = repeat value.
                    if cursor + 1 >= delta.len() {
                        return Err(Error::invalid("ANIM op 5: 0x80 extension truncated"));
                    }
                    let cnt = delta[cursor] as usize;
                    let v = delta[cursor + 1];
                    cursor += 2;
                    for r in 0..cnt {
                        let abs_row = row + r;
                        if abs_row >= height {
                            break;
                        }
                        let row_idx = abs_row * planes_per_row + p;
                        if row_idx < planar.len() && col < planar[row_idx].len() {
                            planar[row_idx][col] = v;
                        }
                    }
                    row += cnt;
                } else {
                    // Short form: low 7 bits = literal count.
                    let cnt = (op & 0x7F) as usize;
                    if cursor + cnt > delta.len() {
                        return Err(Error::invalid("ANIM op 5: literal run extends past delta"));
                    }
                    for r in 0..cnt {
                        let abs_row = row + r;
                        if abs_row >= height {
                            break;
                        }
                        let row_idx = abs_row * planes_per_row + p;
                        if row_idx < planar.len() && col < planar[row_idx].len() {
                            planar[row_idx][col] = delta[cursor + r];
                        }
                    }
                    cursor += cnt;
                    row += cnt;
                }
            }
        }
    }
    Ok(())
}

/// Op 1 — XOR ILBM mode (§1.2.1 / §1.3, the original ANIM mode).
///
/// Per §1.2.1: the encoder XORs every corresponding byte of the new
/// frame against the reference frame, producing a bitmap that is `0`
/// wherever the two frames agreed and the differing bits elsewhere;
/// that XOR-bitmap is then stored "using run-length-encoding" (a
/// ByteRun1 BODY, or an uncompressed BODY per `BMHD.compression`).
/// §1.3: "the usual run-length-decoding routine can be easily modified
/// to do the exclusive-or operation required. Note that runs of zero
/// bytes, which will be very common, can be ignored, as an exclusive or
/// of any byte value to a byte of zero will not alter the original byte
/// value." This decoder expands the BODY into the full planar bitmap
/// and XORs it byte-for-byte into the running planar state.
///
/// **Scope.** This implements the §1.2.1 XOR case for the *full-frame
/// rectangle* (`x == 0`, `y == 0`, `w == width`, `h == height`),
/// covering both the all-planes BODY and the §2.1 `mask` plane-subset
/// BODY. §2.1 defines `mask` byte-exactly — "plane mask where each bit
/// is set =1 if there is data and =0 if not." When the rectangle is the
/// whole bitmap there is no intra-rectangle stride or `x` bit-alignment
/// to resolve: the BODY is simply the stack of full-width
/// (`row_bytes`-wide) scanline rows for *only the planes whose mask bit
/// is set*, in ascending plane order, interleaved scanline-by-scanline
/// exactly as a full ILBM BODY is — so plane `p`'s row for scanline `y`
/// is present iff `mask` bit `p` is set. Run-length (ByteRun1) or
/// uncompressed per `BMHD.compression`; a `0` byte in the XOR bitmap
/// leaves the running byte untouched (§1.3).
///
/// What stays rejected is the genuine *sub-rectangle* (`x != 0 ||
/// y != 0 || w != width || h != height`): the staged spec gives no wire
/// description of the row stride within a narrower rectangle nor of how
/// the rectangle's left edge `x` aligns to a byte/bit boundary inside
/// each plane row, so decoding it would require guessing that layout.
/// Such an ANHD returns `Error::unsupported` and is surfaced as a docs
/// gap.
///
/// The optional `HasMask` scanline plane (ILBM mask plane) is *not* a
/// colour plane and is not addressed by the ANHD `mask` bits; whether
/// it participates in a plane-subset XOR BODY is undocumented, so a
/// sparse plane mask is only accepted when the bitmap carries no
/// `HasMask` plane. A `HasMask` bitmap may still use the all-planes
/// XOR BODY (every row, including the mask scanline, present).
fn apply_op1(anhd: &Anhd, planar: &mut [Vec<u8>], delta: &[u8], bmhd: &Bmhd) -> Result<()> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;

    // The genuine sub-rectangle layout is undocumented (see doc comment).
    let full_rect = anhd.w == bmhd.width && anhd.h == bmhd.height && anhd.x == 0 && anhd.y == 0;
    if !full_rect {
        return Err(Error::unsupported(
            "ANIM op 1: sub-rectangle XOR BODY layout is not described in the staged spec (full-frame rectangle only)",
        ));
    }

    // `mask == 0` is treated as "all colour planes present" (the
    // encoder's all-planes tag and the historical full-frame BODY both
    // map here); a non-zero mask selects the §2.1 plane subset.
    let all_planes_mask = op1_full_plane_mask(n_planes);
    let plane_mask = if anhd.mask == 0 {
        all_planes_mask
    } else {
        anhd.mask
    };

    // Decide which interleaved rows the BODY carries. For the all-planes
    // case every row participates (colour planes + any HasMask scanline).
    // For a sparse plane subset only the selected colour-plane rows are
    // present; the HasMask scanline's participation is undocumented, so a
    // sparse mask on a HasMask bitmap is rejected rather than guessed.
    let sparse = plane_mask != all_planes_mask;
    if sparse && has_mask {
        return Err(Error::unsupported(
            "ANIM op 1: plane-masked XOR BODY on a HasMask bitmap is not described in the staged spec",
        ));
    }

    // `row_present[k]` for the `k`-th interleaved row within a scanline
    // (`0..planes_per_row`): a colour plane is present iff its mask bit
    // is set; the HasMask scanline (only reachable in the all-planes
    // case here) is always present.
    let row_present = |k: usize| -> bool {
        if k < n_planes {
            (plane_mask >> k) & 1 != 0
        } else {
            true // HasMask scanline (all-planes case only)
        }
    };

    // Expand the BODY into the XOR-bitmap rows the mask selects and XOR
    // each into the running state. ByteRun1 rows decode independently;
    // an uncompressed BODY is a flat stack of `row_bytes`-sized rows in
    // the same scanline-interleaved order, skipping unselected planes.
    if bmhd.compression == Compression::None {
        let mut input = delta;
        for y in 0..height {
            for k in 0..planes_per_row {
                if !row_present(k) {
                    continue;
                }
                if input.len() < row_bytes {
                    return Err(Error::invalid("ANIM op 1: uncompressed XOR BODY too short"));
                }
                let idx = y * planes_per_row + k;
                xor_row_into(&mut planar[idx], &input[..row_bytes]);
                input = &input[row_bytes..];
            }
        }
    } else {
        let mut input = delta;
        for y in 0..height {
            for k in 0..planes_per_row {
                if !row_present(k) {
                    continue;
                }
                let mut decoded = Vec::with_capacity(row_bytes);
                let consumed = byterun1_decode_row(input, row_bytes, &mut decoded)?;
                input = &input[consumed..];
                let idx = y * planes_per_row + k;
                xor_row_into(&mut planar[idx], &decoded);
            }
        }
    }
    Ok(())
}

/// XOR `src` into `dst` byte-for-byte over the overlapping prefix.
/// A zero byte in `src` leaves `dst` unchanged (§1.3).
fn xor_row_into(dst: &mut [u8], src: &[u8]) {
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        *d ^= s;
    }
}

/// The all-planes-present `mask` value for `n_planes` colour planes:
/// the low `n_planes` bits set (§2.1 "plane mask where each bit is set
/// =1 if there is data"). `n_planes >= 8` saturates to `0xFF`.
fn op1_full_plane_mask(n_planes: usize) -> u8 {
    if n_planes >= 8 {
        0xFF
    } else {
        ((1u16 << n_planes) - 1) as u8
    }
}

/// Op 7 — Short / Long Vertical Delta.
///
/// The DLTA payload begins with 16 big-endian u32 pointers (8 opcode-
/// list pointers, then 8 data-list pointers). Each pointer is a byte
/// offset into the *same* DLTA buffer; a zero pointer means the plane
/// is unchanged from the previous frame.
///
/// For each colour plane `p` whose opcode-list pointer is non-zero,
/// the bitplane is split into vertical columns of `data_size` bytes
/// (`data_size = 2` for short data, `4` for long data). The column
/// count is therefore `row_bytes / data_size`. Per column the
/// encoder emits an `op_count` byte (zero is legal — column unchanged)
/// followed by `op_count` opcode bytes; the three opcode classes are:
///
/// * **Skip** — hi bit clear, non-zero. The byte value is the number
///   of rows to advance the dest cursor by (no data consumed).
/// * **Uniq** — hi bit set. `byte & 0x7F` is the number of data items
///   to copy *literally* from the data list, one per consecutive row.
/// * **Same** — `0x00` byte followed by a count byte. One data item
///   is fetched from the data list and copied to `count`
///   consecutive rows.
///
/// "Advance the dest cursor by one row" means add `row_bytes` to the
/// byte offset within the bitplane (NOT `data_size`). The column's
/// starting byte offset within each row is `column_index * data_size`.
///
/// `long_data = true` selects the long-data variant (bit 0 of
/// `ANHD.bits` set); `false` selects short data.
fn apply_op7(planar: &mut [Vec<u8>], delta: &[u8], bmhd: &Bmhd, long_data: bool) -> Result<()> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let data_size = if long_data { 4 } else { 2 };

    if row_bytes % data_size != 0 {
        return Err(Error::invalid(format!(
            "ANIM op 7: row_bytes {row_bytes} not a multiple of data size {data_size}"
        )));
    }
    let cols = row_bytes / data_size;

    // 16 big-endian u32 pointers — opcodes then data.
    if delta.len() < 64 {
        return Err(Error::invalid("ANIM op 7: pointer table truncated"));
    }
    let read_ptr = |slot: usize| -> usize {
        u32::from_be_bytes([
            delta[slot * 4],
            delta[slot * 4 + 1],
            delta[slot * 4 + 2],
            delta[slot * 4 + 3],
        ]) as usize
    };

    for p in 0..n_planes.min(8) {
        let op_ptr = read_ptr(p);
        let data_ptr = read_ptr(8 + p);
        if op_ptr == 0 {
            continue; // plane unchanged
        }
        if op_ptr >= delta.len() {
            return Err(Error::invalid("ANIM op 7: opcode pointer out of range"));
        }
        if data_ptr >= delta.len() {
            return Err(Error::invalid("ANIM op 7: data pointer out of range"));
        }
        let mut op_cur = op_ptr;
        let mut data_cur = data_ptr;

        for col in 0..cols {
            if op_cur >= delta.len() {
                return Err(Error::invalid(
                    "ANIM op 7: opcode list truncated at op_count",
                ));
            }
            let op_count = delta[op_cur] as usize;
            op_cur += 1;
            if op_count == 0 {
                continue; // no change in this column
            }
            let mut row: usize = 0;
            let col_byte = col * data_size;
            for _ in 0..op_count {
                if op_cur >= delta.len() {
                    return Err(Error::invalid(
                        "ANIM op 7: opcode list truncated mid-column",
                    ));
                }
                let op = delta[op_cur];
                op_cur += 1;
                if op == 0 {
                    // Same op: next opcode byte is the row count; copy
                    // one data item from the data list `cnt` times.
                    if op_cur >= delta.len() {
                        return Err(Error::invalid("ANIM op 7: Same op missing count byte"));
                    }
                    let cnt = delta[op_cur] as usize;
                    op_cur += 1;
                    if data_cur + data_size > delta.len() {
                        return Err(Error::invalid("ANIM op 7: Same op data item truncated"));
                    }
                    let item_start = data_cur;
                    data_cur += data_size;
                    for r in 0..cnt {
                        let abs_row = row + r;
                        if abs_row >= height {
                            break;
                        }
                        let row_idx = abs_row * planes_per_row + p;
                        if row_idx < planar.len() && col_byte + data_size <= planar[row_idx].len() {
                            planar[row_idx][col_byte..col_byte + data_size]
                                .copy_from_slice(&delta[item_start..item_start + data_size]);
                        }
                    }
                    row += cnt;
                } else if op & 0x80 == 0 {
                    // Skip op: forward dest cursor by `op` rows. No
                    // data consumed.
                    row += op as usize;
                } else {
                    // Uniq op: copy `op & 0x7F` data items, one per
                    // consecutive row, from the data list.
                    let cnt = (op & 0x7F) as usize;
                    if data_cur + cnt * data_size > delta.len() {
                        return Err(Error::invalid("ANIM op 7: Uniq op data items truncated"));
                    }
                    for r in 0..cnt {
                        let abs_row = row + r;
                        let item_start = data_cur + r * data_size;
                        if abs_row < height {
                            let row_idx = abs_row * planes_per_row + p;
                            if row_idx < planar.len()
                                && col_byte + data_size <= planar[row_idx].len()
                            {
                                planar[row_idx][col_byte..col_byte + data_size]
                                    .copy_from_slice(&delta[item_start..item_start + data_size]);
                            }
                        }
                    }
                    data_cur += cnt * data_size;
                    row += cnt;
                }
            }
        }
    }
    Ok(())
}

/// Test-only re-export of [`apply_op7`] so integration tests can
/// drive the op-7 decoder without rebuilding a full ANIM container.
#[doc(hidden)]
pub fn apply_op7_for_test(
    planar: &mut [Vec<u8>],
    delta: &[u8],
    bmhd: &Bmhd,
    long_data: bool,
) -> Result<()> {
    apply_op7(planar, delta, bmhd, long_data)
}

/// The subset of `ANHD.bits` options op-4 (Generalized Delta) accepts.
///
/// The §2.2.2 `SetDLTAshort` reference routine is the only normative
/// description of the op-4 wire format, and it documents one
/// configuration: short-word data, vertical columns, run-length-coded,
/// separate info list per plane, non-XOR. The `bits` field
/// (`docs/image/iff/anim.txt` §2.1) flags every option:
///
/// ```text
/// bit #        set =0                       set =1
/// 0            short data                   long data
/// 1            (non-XOR)                    XOR
/// 2            separate info per plane      one info list for all planes
/// 3            not RLC                      RLC (run length coded)
/// 4            horizontal                   vertical
/// 5            short info offsets           long info offsets
/// ```
///
/// We support the variants the reference routine covers directly:
/// `bit0` chooses the data-word width (short 2 B / long 4 B), `bit2`
/// chooses whether each plane has its own info list or all planes
/// share one, `bit3` must be set (RLC — the only documented op
/// grammar), and `bit4` must be set (vertical — the routine's `nw`
/// column stride; the spec notes horizontal "simply set[s] nw=1" but
/// gives no separate wire description so we don't guess). XOR mode
/// (`bit1`) has no documented data-merge semantics in this spec and is
/// rejected.
#[derive(Clone, Copy, Debug)]
struct Op4Options {
    long_data: bool,
    /// `bit5`: info-list offsets within the op stream are long (4 B)
    /// rather than short (2 B). The §2.2.2 routine reads `*ptr` as a
    /// `WORD` (short offsets); this flag widens that read.
    long_offsets: bool,
}

impl Op4Options {
    /// Decode the `bits` word into the supported-option set, rejecting
    /// the configurations the §2.2.2 reference routine does not
    /// describe.
    fn from_bits(bits: u32) -> Result<Self> {
        let long_data = (bits & 0b0000_0001) != 0;
        let xor = (bits & 0b0000_0010) != 0;
        let shared_info = (bits & 0b0000_0100) != 0;
        let rlc = (bits & 0b0000_1000) != 0;
        let vertical = (bits & 0b0001_0000) != 0;
        let long_offsets = (bits & 0b0010_0000) != 0;
        // Bits 6..=31 are reserved "set =0" per §2.1; a player should
        // "check undefined bits in options 4 and 5 to assure they are
        // zero". Honour that.
        if bits & !0b0011_1111 != 0 {
            return Err(Error::unsupported(format!(
                "ANIM op 4: reserved ANHD.bits set (0x{bits:08X}); spec requires bits 6..=31 = 0"
            )));
        }
        if xor {
            return Err(Error::unsupported(
                "ANIM op 4: XOR mode (ANHD.bits bit 1) has no documented data-merge wire format",
            ));
        }
        if !rlc {
            return Err(Error::unsupported(
                "ANIM op 4: non-RLC mode (ANHD.bits bit 3 clear) has no documented op grammar",
            ));
        }
        if !vertical {
            return Err(Error::unsupported(
                "ANIM op 4: horizontal mode (ANHD.bits bit 4 clear) has no separate documented wire format",
            ));
        }
        let _ = shared_info; // both shared and separate handled below
        Ok(Self {
            long_data,
            long_offsets,
        })
    }
}

/// Op 4 — Generalized short/long Delta mode (§1.2.4; wire format
/// §2.2.2, "Format for method 4").
///
/// The DLTA payload opens with 16 big-endian u32 pointers: 8 *data*
/// list pointers (one per plane) followed by 8 *offset/numbers* (op)
/// list pointers. Per the §2.2.2 reference routine `SetDLTAshort`,
/// these pointers — and the per-op column offsets inside the op
/// lists — are measured in **16-bit words** (the routine does
/// `WORD*`-pointer arithmetic: `data = deltaword + deltadata[i]`,
/// `dest = planeptr + *ptr`), NOT in bytes as ops 5/7 do.
///
/// For each plane `p` whose op-list pointer is non-zero the op list is
/// a flat sequence of `(offset, size)` pairs terminated by an offset
/// word equal to `0xFFFF`:
///
/// * `offset` (UWORD) — the **absolute** word position within the
///   plane where this op's vertical run begins (`dest = planeptr +
///   offset`). Each op restarts from its own absolute offset; offsets
///   are not cumulative.
/// * `size` (WORD, signed):
///   * `size > 0` — copy `size` data words from the data list, one per
///     consecutive row (`*dest = *data++; dest += nw`), advancing the
///     dest pointer by `nw = row_bytes / word_size` words per row.
///   * `size < 0` — copy ONE data word to `|size|` consecutive rows
///     (`*dest = *data` repeated; `data++` once afterwards). This is
///     the run-length "Same" op.
///
/// `nw` is the vertical column stride in words. When `row_bytes` is not
/// a multiple of the word size the plane has trailing bytes the word
/// stride can't address; we reject that as [`Error::invalid`] (the
/// reference routine implicitly assumes `BytesPerRow` is even/quad).
///
/// `opts` carries the `ANHD.bits` decode: `long_data` widens the data
/// word to 4 bytes; `long_offsets` widens the in-op `offset`/`size`
/// fields to 4 bytes (`bit5`). The plane-pointer table is always
/// 16 × u32. When `bit2` (shared info list) is set the same op-list
/// pointer is simply repeated across plane slots, which this routine
/// handles transparently since it dereferences each slot independently.
fn apply_op4(anhd: &Anhd, planar: &mut [Vec<u8>], delta: &[u8], bmhd: &Bmhd) -> Result<()> {
    let opts = Op4Options::from_bits(anhd.bits)?;
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let word_size = if opts.long_data { 4 } else { 2 };
    let off_size = if opts.long_offsets { 4 } else { 2 };

    if row_bytes % word_size != 0 {
        return Err(Error::invalid(format!(
            "ANIM op 4: row_bytes {row_bytes} not a multiple of data-word size {word_size}"
        )));
    }
    // Vertical column stride in words (`nw` in the §2.2.2 routine).
    let nw = row_bytes / word_size;
    let words_per_plane = height * nw;

    // 16 big-endian u32 pointers — data lists then op (offset/number)
    // lists. Per the §2.2.2 routine the pointer value is a *word*
    // offset from the DLTA start.
    if delta.len() < 64 {
        return Err(Error::invalid("ANIM op 4: pointer table truncated"));
    }
    let read_ptr = |slot: usize| -> usize {
        u32::from_be_bytes([
            delta[slot * 4],
            delta[slot * 4 + 1],
            delta[slot * 4 + 2],
            delta[slot * 4 + 3],
        ]) as usize
    };
    // Read an `off_size`-byte big-endian field at byte position `at`.
    let read_off = |at: usize| -> Result<u32> {
        if at + off_size > delta.len() {
            return Err(Error::invalid("ANIM op 4: op list truncated"));
        }
        Ok(match off_size {
            2 => u16::from_be_bytes([delta[at], delta[at + 1]]) as u32,
            _ => u32::from_be_bytes([delta[at], delta[at + 1], delta[at + 2], delta[at + 3]]),
        })
    };
    // The op-list terminator is the all-ones offset word at the active
    // offset width (0xFFFF for short, 0xFFFF_FFFF for long).
    let term: u32 = if opts.long_offsets {
        0xFFFF_FFFF
    } else {
        0xFFFF
    };
    // The §2.2.2 routine reads `size` as a signed WORD; sign-extend the
    // `off_size`-wide field.
    let sign_extend = |v: u32| -> i64 {
        if opts.long_offsets {
            v as i32 as i64
        } else {
            v as u16 as i16 as i64
        }
    };

    for p in 0..n_planes.min(8) {
        // Slots 0..=7 are the data-list pointers; 8..=15 the op-list
        // pointers (§2.2.2 ¶ "The first 8 are as before … The next 8
        // are pointers to the start of the offset/numbers data list").
        let data_word_ptr = read_ptr(p);
        let op_word_ptr = read_ptr(8 + p);
        if op_word_ptr == 0 {
            continue; // plane unchanged
        }
        // Word offsets → byte offsets.
        let data_byte0 = data_word_ptr
            .checked_mul(2)
            .ok_or_else(|| Error::invalid("ANIM op 4: data pointer overflow"))?;
        let op_byte0 = op_word_ptr
            .checked_mul(2)
            .ok_or_else(|| Error::invalid("ANIM op 4: op pointer overflow"))?;
        if data_byte0 > delta.len() || op_byte0 >= delta.len() {
            return Err(Error::invalid("ANIM op 4: list pointer out of range"));
        }

        // Gather the plane into a contiguous word buffer the absolute
        // word offsets index into, mirroring the op-2/3 decoder's
        // gather/scatter.
        let mut buf = Vec::with_capacity(height * row_bytes);
        for y in 0..height {
            buf.extend_from_slice(&planar[y * planes_per_row + p]);
        }

        let mut op_cur = op_byte0;
        let mut data_cur = data_byte0;
        loop {
            let offset = read_off(op_cur)?;
            op_cur += off_size;
            if offset == term {
                break;
            }
            let size_raw = read_off(op_cur)?;
            op_cur += off_size;
            let size = sign_extend(size_raw);

            // `dest = planeptr + offset` — absolute word position.
            let start_word = offset as usize;
            if size < 0 {
                // Same op: one data word → |size| consecutive rows.
                let rows = (-size) as usize;
                if data_cur + word_size > delta.len() {
                    return Err(Error::invalid("ANIM op 4: Same op data word truncated"));
                }
                let item = &delta[data_cur..data_cur + word_size];
                data_cur += word_size;
                for r in 0..rows {
                    let w = start_word + r * nw;
                    if w >= words_per_plane {
                        return Err(Error::invalid("ANIM op 4: Same op writes past plane end"));
                    }
                    buf[w * word_size..(w + 1) * word_size].copy_from_slice(item);
                }
            } else {
                // Uniq op: `size` data words, one per consecutive row.
                let rows = size as usize;
                if data_cur + rows * word_size > delta.len() {
                    return Err(Error::invalid("ANIM op 4: Uniq op data words truncated"));
                }
                for r in 0..rows {
                    let w = start_word + r * nw;
                    if w >= words_per_plane {
                        return Err(Error::invalid("ANIM op 4: Uniq op writes past plane end"));
                    }
                    let item_start = data_cur + r * word_size;
                    buf[w * word_size..(w + 1) * word_size]
                        .copy_from_slice(&delta[item_start..item_start + word_size]);
                }
                data_cur += rows * word_size;
            }
        }

        // Scatter the contiguous buffer back into the planar rows.
        for y in 0..height {
            planar[y * planes_per_row + p]
                .copy_from_slice(&buf[y * row_bytes..(y + 1) * row_bytes]);
        }
    }
    Ok(())
}

/// Test-only re-export of [`apply_op4`] so integration tests can drive
/// the op-4 decoder without rebuilding a full ANIM container. `bits`
/// is the `ANHD.bits` option word the §2.2.2 routine keys off.
#[doc(hidden)]
pub fn apply_op4_for_test(
    bits: u32,
    planar: &mut [Vec<u8>],
    delta: &[u8],
    bmhd: &Bmhd,
) -> Result<()> {
    let anhd = Anhd {
        operation: 4,
        bits,
        ..Anhd::default()
    };
    apply_op4(&anhd, planar, delta, bmhd)
}

/// Op 2 / Op 3 — Long / Short Delta mode (spec §1.2.2 / §1.2.3; wire
/// format §2.2.1, "Format for methods 2 & 3").
///
/// The DLTA payload starts with 8 big-endian u32 pointers, one per
/// bitplane slot; each is a byte offset into the DLTA at which that
/// plane's group list starts ("the pointer for the plane data starting
/// immediately following these 8 pointers will have a value of 32").
/// A zero pointer means "no changed words in this plane".
///
/// Per plane the payload is a list of *groups*. Offsets and counts
/// are big-endian shorts in both modes; the data words are big-endian
/// longs in Long Delta mode (`long_data = true`, op 2) and shorts in
/// Short Delta mode (`long_data = false`, op 3). The bitplane is
/// addressed as the contiguous word array it occupies in Amiga memory
/// (`height * row_bytes` bytes) — for op 2 a long word may straddle a
/// row boundary when `row_bytes` is not a multiple of 4, so we
/// gather each plane into a contiguous buffer, apply the groups, and
/// scatter the rows back.
///
/// Group grammar (§2.2.1): a word cursor starts out pointing at the
/// first word of the bitplane. Each group opens with an offset short:
///
/// * `0xFFFF` — terminates the plane's group list.
/// * positive (sign bit clear) — "it indicates the increment in long
///   or short words through the bitplane": the offset is added to the
///   cursor and the following data word is placed at that position.
/// * negative (sign bit set) — "the absolute value is the offset+2",
///   i.e. the cursor advances by `abs - 2`; the following short gives
///   the number of contiguous data words that follow, which are
///   placed in contiguous word locations starting at the cursor.
///
/// The spec describes the cursor as the pointer the data word "would
/// be placed at"; we keep it pointing at the *last written word* after
/// every group (for a run, the run's final word), and subsequent
/// offsets accumulate from there. The in-tree encoder
/// ([`encode_op23_body`]) emits offsets under the same convention.
fn apply_op23(planar: &mut [Vec<u8>], delta: &[u8], bmhd: &Bmhd, long_data: bool) -> Result<()> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let word_size = if long_data { 4 } else { 2 };

    // Pointer table: 8 u32 BE pointers (max 8 colour planes).
    if delta.len() < 32 {
        return Err(Error::invalid("ANIM op 2/3: pointer table truncated"));
    }
    let read_u16 = |at: usize| -> Result<u16> {
        if at + 2 > delta.len() {
            return Err(Error::invalid("ANIM op 2/3: group list truncated"));
        }
        Ok(u16::from_be_bytes([delta[at], delta[at + 1]]))
    };

    for p in 0..n_planes.min(8) {
        let off = u32::from_be_bytes([
            delta[p * 4],
            delta[p * 4 + 1],
            delta[p * 4 + 2],
            delta[p * 4 + 3],
        ]) as usize;
        if off == 0 {
            continue; // plane unchanged
        }
        if off >= delta.len() {
            return Err(Error::invalid("ANIM op 2/3: plane pointer out of range"));
        }

        // Gather the plane into the contiguous byte buffer the word
        // offsets index into.
        let mut buf = Vec::with_capacity(height * row_bytes);
        for y in 0..height {
            buf.extend_from_slice(&planar[y * planes_per_row + p]);
        }
        let word_count = buf.len() / word_size;

        let mut cur = off; // byte cursor inside the DLTA payload
        let mut wcursor: usize = 0; // word cursor inside the bitplane
        loop {
            let offset_word = read_u16(cur)?;
            cur += 2;
            if offset_word == 0xFFFF {
                break; // plane terminator
            }
            if offset_word & 0x8000 == 0 {
                // Positive offset: advance, then place ONE data word.
                wcursor += offset_word as usize;
                if wcursor >= word_count {
                    return Err(Error::invalid("ANIM op 2/3: word offset past plane end"));
                }
                if cur + word_size > delta.len() {
                    return Err(Error::invalid("ANIM op 2/3: data word truncated"));
                }
                buf[wcursor * word_size..(wcursor + 1) * word_size]
                    .copy_from_slice(&delta[cur..cur + word_size]);
                cur += word_size;
            } else {
                // Negative offset: abs = offset + 2, then a count
                // short, then `count` contiguous data words.
                let abs = 0x1_0000 - offset_word as usize; // 2..=0x8000
                let advance = abs - 2;
                wcursor += advance;
                let count = read_u16(cur)? as usize;
                cur += 2;
                if count == 0 {
                    return Err(Error::invalid("ANIM op 2/3: zero-length data run"));
                }
                if wcursor + count > word_count {
                    return Err(Error::invalid("ANIM op 2/3: data run past plane end"));
                }
                if cur + count * word_size > delta.len() {
                    return Err(Error::invalid("ANIM op 2/3: data run truncated"));
                }
                buf[wcursor * word_size..(wcursor + count) * word_size]
                    .copy_from_slice(&delta[cur..cur + count * word_size]);
                cur += count * word_size;
                // Leave the cursor on the run's last written word so
                // the next group's offset accumulates from there.
                wcursor += count - 1;
            }
        }

        // Scatter the contiguous buffer back into the per-row planar
        // state.
        for y in 0..height {
            planar[y * planes_per_row + p]
                .copy_from_slice(&buf[y * row_bytes..(y + 1) * row_bytes]);
        }
    }
    Ok(())
}

/// Test-only re-export of [`apply_op23`] so integration tests can
/// drive the op-2/3 decoder without rebuilding a full ANIM container.
#[doc(hidden)]
pub fn apply_op23_for_test(
    planar: &mut [Vec<u8>],
    delta: &[u8],
    bmhd: &Bmhd,
    long_data: bool,
) -> Result<()> {
    apply_op23(planar, delta, bmhd, long_data)
}

/// Encode a single op-5 (Byte Vertical Delta) BODY payload from a
/// previous and current planar frame.
///
/// `prev_planar` and `cur_planar` are the row-major flat arrays of
/// bitplane rows in IFF order (`planes_per_row = n_planes + mask_plane`),
/// each row being `row_bytes` long, in the same shape used by the
/// decoder ([`apply_op5`]). They must agree in dimensions and in the
/// number of stored colour planes.
///
/// The output is the bytes of the `BODY` chunk: a 32-byte pointer
/// table (8 × u32 BE) followed by per-plane data lists. Plane slots
/// that aren't dirty get a `0` pointer. Per the in-tree decoder
/// description, each data list walks columns 0..`row_bytes`
/// left-to-right; each column is a sequence of ops walking rows
/// top-to-bottom, where:
///
/// * `op = 0` — column terminator;
/// * `op in 1..=0x7F` — skip `op` rows (cursor += op);
/// * `op = 0x80` — next byte = `cnt`, next byte = `v`; write `v` to
///   the next `cnt` rows (cursor += cnt);
/// * `op in 0x81..=0xFF` — literal: low 7 bits = `cnt`, then `cnt`
///   bytes written one-per-row (cursor += cnt).
///
/// The encoder walks each plane column-by-column, emitting one of
/// (skip, repeat, literal) for each delta run; the chosen op for a
/// run minimises the byte cost (`repeat` = 3 bytes, `literal` = 1 +
/// `cnt` bytes). Run-length is capped at 127 for literals (short op
/// space) and 255 for repeats (`cnt` is a u8); longer runs split.
///
/// Used by [`encode_anim_op5`] but exposed publicly so callers driving
/// the lower-level container can build their own ANIM5 streams.
pub fn encode_op5_body(
    prev_planar: &[Vec<u8>],
    cur_planar: &[Vec<u8>],
    bmhd: &Bmhd,
) -> Result<Vec<u8>> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let expected = height * planes_per_row;
    if prev_planar.len() != expected || cur_planar.len() != expected {
        return Err(Error::invalid(format!(
            "ANIM op 5 encode: planar buffers have {} / {} rows, expected {expected}",
            prev_planar.len(),
            cur_planar.len()
        )));
    }
    if n_planes > 8 {
        // op-5 pointer table is 8 slots (one u32 per colour plane);
        // formats with > 8 planes can't address every plane via op-5
        // and must use op-7/op-8 (short / long vertical delta).
        return Err(Error::unsupported(format!(
            "ANIM op 5 encode: requires ≤ 8 colour planes (got {n_planes})"
        )));
    }

    // Build the per-plane data lists.
    let mut plane_data: Vec<Vec<u8>> = vec![Vec::new(); 8];
    let mut plane_dirty = [false; 8];
    for p in 0..n_planes {
        let mut list = Vec::new();
        let mut any_change = false;
        for col in 0..row_bytes {
            encode_op5_column(
                &mut list,
                prev_planar,
                cur_planar,
                p,
                col,
                planes_per_row,
                row_bytes,
                height,
                &mut any_change,
            );
            // Terminator: cap each column's op-stream with 0x00.
            list.push(0);
        }
        if any_change {
            plane_data[p] = list;
            plane_dirty[p] = true;
        }
    }

    // Assemble: 32-byte pointer table + concatenated data lists.
    // Plane pointers are absolute offsets from the start of the BODY.
    let mut out = vec![0u8; 32];
    for (slot, data) in plane_data.iter_mut().enumerate().take(8) {
        if !plane_dirty[slot] {
            continue;
        }
        let offset = out.len() as u32;
        out[slot * 4..slot * 4 + 4].copy_from_slice(&offset.to_be_bytes());
        out.append(data);
    }
    Ok(out)
}

/// Walk a single column of plane `p`, emitting skip / repeat / literal
/// ops into `out`. Updates `any_change` if at least one delta byte
/// differs in this column.
#[allow(clippy::too_many_arguments)]
fn encode_op5_column(
    out: &mut Vec<u8>,
    prev: &[Vec<u8>],
    cur: &[Vec<u8>],
    plane: usize,
    col: usize,
    planes_per_row: usize,
    _row_bytes: usize,
    height: usize,
    any_change: &mut bool,
) {
    // Build the byte-vertical column values for prev / cur, then walk
    // the column row-by-row. Each row contributes one delta byte.
    let mut row = 0usize;
    while row < height {
        let prev_byte = prev[row * planes_per_row + plane][col];
        let cur_byte = cur[row * planes_per_row + plane][col];
        if prev_byte == cur_byte {
            // Count contiguous unchanged rows starting at `row`.
            let mut skip = 0usize;
            while row + skip < height {
                let pb = prev[(row + skip) * planes_per_row + plane][col];
                let cb = cur[(row + skip) * planes_per_row + plane][col];
                if pb != cb {
                    break;
                }
                skip += 1;
            }
            // Skip runs are u7 (op space 1..=0x7F); split if longer.
            let mut remaining = skip;
            while remaining > 0 {
                let chunk = remaining.min(0x7F);
                out.push(chunk as u8);
                remaining -= chunk;
            }
            row += skip;
        } else {
            // Find the contiguous changed run starting at `row`.
            let mut end = row + 1;
            while end < height {
                let pb = prev[end * planes_per_row + plane][col];
                let cb = cur[end * planes_per_row + plane][col];
                if pb == cb {
                    break;
                }
                end += 1;
            }
            // Emit the run by picking repeat vs literal greedily,
            // splitting at run-length caps.
            let mut i = row;
            while i < end {
                // Look for a maximal repeat of the same byte at i.
                let v = cur[i * planes_per_row + plane][col];
                let mut rep_end = i + 1;
                while rep_end < end
                    && cur[rep_end * planes_per_row + plane][col] == v
                    && rep_end - i < 0xFF
                {
                    rep_end += 1;
                }
                let rep_len = rep_end - i;
                // Repeat costs 3 bytes (0x80, cnt, v); literal of length
                // L costs 1 + L bytes. Use repeat only if it's cheaper
                // *and* legal: rep_len ≥ 3 makes 3 ≤ 1 + L = 1 + rep_len,
                // i.e. rep_len ≥ 2 means literal is 1+2=3 — same cost,
                // prefer literal. rep_len ≥ 3 means repeat is strictly
                // cheaper.
                if rep_len >= 3 {
                    out.push(0x80);
                    out.push(rep_len as u8);
                    out.push(v);
                    i = rep_end;
                } else {
                    // Literal run: extend until we hit a usable repeat
                    // (≥ 3 same bytes ahead) or the end of the changed
                    // run, capped at 0x7F bytes.
                    let lit_start = i;
                    let mut lit_end = i + 1;
                    while lit_end < end && lit_end - lit_start < 0x7F {
                        // Peek ahead for a 3-run starting at lit_end.
                        let lv = cur[lit_end * planes_per_row + plane][col];
                        let l1 = lit_end + 1 < end
                            && cur[(lit_end + 1) * planes_per_row + plane][col] == lv;
                        let l2 = lit_end + 2 < end
                            && cur[(lit_end + 2) * planes_per_row + plane][col] == lv;
                        if l1 && l2 {
                            // Switch to repeat at lit_end; close literal here.
                            break;
                        }
                        lit_end += 1;
                    }
                    let lit_len = lit_end - lit_start;
                    debug_assert!((1..=0x7F).contains(&lit_len));
                    out.push(0x80 | lit_len as u8);
                    for r in lit_start..lit_end {
                        out.push(cur[r * planes_per_row + plane][col]);
                    }
                    i = lit_end;
                }
            }
            *any_change = true;
            row = end;
        }
    }
}

/// Encode a sequence of ILBM frames as a FORM/ANIM file using
/// `operation = 5` (Byte Vertical Delta) for every delta frame.
///
/// The seed frame is the full leading FORM ILBM (same as
/// [`encode_anim_op0`]); subsequent frames carry an `ANHD` (op = 5)
/// plus a `BODY` chunk produced by [`encode_op5_body`] from the diff
/// between the prior and current planar frames. The encoder
/// reconstructs the planar form via [`rgba_to_planar`] which uses a
/// palette nearest-fit; HAM frames therefore round-trip pixel-exactly
/// only when each pixel is already a palette colour. Indexed and EHB
/// modes round-trip losslessly.
///
/// Compatible with the in-tree [`parse_anim`] op-5 decoder; tested via
/// `tests/anim_op5.rs`.
pub fn encode_anim_op5(frames: &[IlbmImage]) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid(
            "ANIM op 5 encode: at least one frame required",
        ));
    }
    if frames[0].bmhd.n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ANIM op 5 encode: requires ≤ 8 colour planes (got {})",
            frames[0].bmhd.n_planes
        )));
    }
    let leading = crate::ilbm::encode_ilbm(&frames[0])?;
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"ANIM");
    out.extend_from_slice(&leading);
    if leading.len() & 1 == 1 {
        out.push(0);
    }

    // Track the running planar state. Reconstruct it from the seed
    // frame's RGBA the same way the decoder does.
    let mut prev_planar = rgba_to_planar(&frames[0]);

    for frame in &frames[1..] {
        let cur_planar = rgba_to_planar(frame);
        let body = encode_op5_body(&prev_planar, &cur_planar, &frame.bmhd)?;
        let anhd = Anhd {
            operation: 5,
            mask: 0,
            w: frame.bmhd.width,
            h: frame.bmhd.height,
            x: frame.bmhd.x_origin,
            y: frame.bmhd.y_origin,
            abs_time: 0,
            rel_time: 1,
            // These encoders compute each frame as a delta against the
            // immediately-previous frame, so the §2.1 `interleave` tag
            // is `1` (modify one frame back), not the `0` double-buffer
            // default that means two frames back.
            interleave: 1,
            pad0: 0,
            bits: 0,
        };
        let anhd_bytes = anhd.write(body.len() as u32);
        let inner_size = (4 + 8 + 40 + 8 + body.len()) as u32;
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&inner_size.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"ANHD");
        out.extend_from_slice(&40u32.to_be_bytes());
        out.extend_from_slice(&anhd_bytes);
        out.extend_from_slice(b"BODY");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
        prev_planar = cur_planar;
    }

    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Encode a single op-1 (XOR ILBM mode) BODY from a previous and
/// current planar frame.
///
/// Per §1.2.1 the BODY is the byte-for-byte XOR of the two frames'
/// planar bitmaps, then run-length-encoded. `prev_planar` and
/// `cur_planar` are the row-major flat arrays of bitplane rows in IFF
/// order (`planes_per_row = n_planes + mask_plane`), each row
/// `row_bytes` long — the same shape the decoder ([`apply_op1`])
/// produces. The output is a ByteRun1 BODY: each XOR row is packed
/// independently, exactly as the seed-frame BODY would be, so the
/// running [`parse_anim`] op-1 path decodes it directly.
pub fn encode_op1_body(
    prev_planar: &[Vec<u8>],
    cur_planar: &[Vec<u8>],
    bmhd: &Bmhd,
) -> Result<Vec<u8>> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let expected = height * planes_per_row;
    if prev_planar.len() != expected || cur_planar.len() != expected {
        return Err(Error::invalid(format!(
            "ANIM op 1 encode: planar buffers have {} / {} rows, expected {expected}",
            prev_planar.len(),
            cur_planar.len()
        )));
    }
    // The XOR BODY honours `BMHD.compression` (the spec says the
    // XOR-bitmap is "saved using run-length-encoding", i.e. ByteRun1,
    // but an uncompressed seed BMHD keeps the BODY uncompressed so the
    // decoder's `compression`-driven expansion stays consistent).
    let compress = bmhd.compression != Compression::None;
    let mut out = Vec::new();
    for (prev_row, cur_row) in prev_planar.iter().zip(cur_planar.iter()) {
        if prev_row.len() != row_bytes || cur_row.len() != row_bytes {
            return Err(Error::invalid("ANIM op 1 encode: row length mismatch"));
        }
        let mut xor_row = vec![0u8; row_bytes];
        for ((d, &a), &b) in xor_row.iter_mut().zip(prev_row.iter()).zip(cur_row.iter()) {
            *d = a ^ b;
        }
        if compress {
            out.extend_from_slice(&byterun1_encode_row(&xor_row));
        } else {
            out.extend_from_slice(&xor_row);
        }
    }
    Ok(out)
}

/// Encode a sequence of frames as a FORM/ANIM using op-1 (XOR ILBM
/// mode, §1.2.1). The first frame is the literal seed ILBM; every
/// subsequent frame is stored as a ByteRun1 BODY of its XOR against
/// the previous frame's planar state, tagged with an `ANHD.operation
/// = 1` header carrying the full-frame `mask` / `w` / `h` / `x` / `y`
/// fields. Compatible with the in-tree [`parse_anim`] op-1 decoder;
/// tested via `tests/anim_op1.rs`.
pub fn encode_anim_op1(frames: &[IlbmImage]) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid(
            "ANIM op 1 encode: at least one frame required",
        ));
    }
    let leading = crate::ilbm::encode_ilbm(&frames[0])?;
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"ANIM");
    out.extend_from_slice(&leading);
    if leading.len() & 1 == 1 {
        out.push(0);
    }

    let mut prev_planar = rgba_to_planar(&frames[0]);

    for frame in &frames[1..] {
        let cur_planar = rgba_to_planar(frame);
        let body = encode_op1_body(&prev_planar, &cur_planar, &frame.bmhd)?;
        let n_planes = frame.bmhd.n_planes as usize;
        let anhd = Anhd {
            operation: 1,
            // Full-plane mask: every colour plane participates in the
            // full-frame XOR BODY (§2.1 "bit set =1 if there is data").
            mask: op1_full_plane_mask(n_planes),
            w: frame.bmhd.width,
            h: frame.bmhd.height,
            x: 0,
            y: 0,
            abs_time: 0,
            rel_time: 1,
            // Delta computed against the immediately-previous frame:
            // tag `interleave = 1` so the decoder applies it one frame
            // back (not the §2.1 `0` double-buffer default of two).
            interleave: 1,
            pad0: 0,
            bits: 0,
        };
        let anhd_bytes = anhd.write(body.len() as u32);
        let inner_size = (4 + 8 + 40 + 8 + body.len()) as u32;
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&inner_size.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"ANHD");
        out.extend_from_slice(&40u32.to_be_bytes());
        out.extend_from_slice(&anhd_bytes);
        out.extend_from_slice(b"BODY");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
        prev_planar = cur_planar;
    }

    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Encode a single op-7 (Short / Long Vertical Delta) DLTA payload
/// from a previous and current planar frame.
///
/// `prev_planar` and `cur_planar` are the row-major flat arrays of
/// bitplane rows in IFF order (`planes_per_row = n_planes +
/// mask_plane`), each row being `row_bytes` long, in the same shape
/// used by the decoder ([`apply_op7`]). They must agree in
/// dimensions and in the number of stored colour planes.
///
/// `long_data` selects the data-item width: `false` = short (2-byte
/// items, the typical case) and `true` = long (4-byte items, set
/// when `ANHD.bits` bit 0 is on). `row_bytes` MUST divide evenly by
/// the resulting data-item width; mismatched widths are rejected as
/// [`Error::invalid`].
///
/// The output is the DLTA chunk body: a 64-byte pointer table (16 ×
/// u32 BE — 8 opcode-list pointers, then 8 data-list pointers)
/// followed by per-plane opcode lists and data lists. Plane slots
/// that aren't dirty get a `0` opcode pointer; the matching data
/// pointer is also `0` for consistency (the decoder reads it only
/// when the opcode pointer is non-zero).
///
/// Per `docs/image/iff/anim.txt` Appendix Anim7 §#.# the per-column
/// op stream is `op_count + ops[]` where each op is one of:
///
/// * **Skip** — hi bit clear, non-zero. Advance the dest cursor by
///   `op` rows. No data consumed.
/// * **Uniq** — hi bit set. Copy `op & 0x7F` data items literally,
///   one per consecutive row.
/// * **Same** — `0x00` followed by a count byte. Copy one data item
///   to the next `count` rows.
///
/// The encoder splits each column into runs of equal-vs-different
/// data items and picks the cheapest opcode per run, matching the
/// op-5 encoder's greedy strategy adapted to the per-column op-count
/// layout (op-7 has an explicit `op_count` byte instead of op-5's
/// `0x00` column terminator).
///
/// Used by [`encode_anim_op7`] but exposed publicly so callers
/// driving the lower-level container can build their own ANIM7
/// streams.
pub fn encode_op7_body(
    prev_planar: &[Vec<u8>],
    cur_planar: &[Vec<u8>],
    bmhd: &Bmhd,
    long_data: bool,
) -> Result<Vec<u8>> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let data_size = if long_data { 4 } else { 2 };
    let expected = height * planes_per_row;
    if prev_planar.len() != expected || cur_planar.len() != expected {
        return Err(Error::invalid(format!(
            "ANIM op 7 encode: planar buffers have {} / {} rows, expected {expected}",
            prev_planar.len(),
            cur_planar.len()
        )));
    }
    if n_planes > 8 {
        // op-7 pointer table is 8 opcode + 8 data slots; > 8 planes
        // can't address every plane via op-7.
        return Err(Error::unsupported(format!(
            "ANIM op 7 encode: requires ≤ 8 colour planes (got {n_planes})"
        )));
    }
    if row_bytes % data_size != 0 {
        return Err(Error::invalid(format!(
            "ANIM op 7 encode: row_bytes {row_bytes} not a multiple of data size {data_size}"
        )));
    }
    let cols = row_bytes / data_size;

    // Per-plane opcode + data lists, plus a dirty flag.
    let mut plane_op: Vec<Vec<u8>> = vec![Vec::new(); 8];
    let mut plane_data: Vec<Vec<u8>> = vec![Vec::new(); 8];
    let mut plane_dirty = [false; 8];

    for p in 0..n_planes {
        let mut op_list: Vec<u8> = Vec::new();
        let mut data_list: Vec<u8> = Vec::new();
        let mut any_change = false;
        for col in 0..cols {
            let col_byte = col * data_size;
            // Collect this column's per-row data items (one slice of
            // `data_size` bytes per row) for prev and cur. Equality
            // between items determines skip-vs-write per row.
            //
            // `col_ops` holds the byte-serialised opcode stream;
            // `col_op_count` counts *logical* ops (the value the
            // op_count prefix byte records — the decoder loops
            // `op_count` times consuming variable-length opcodes).
            let mut col_ops: Vec<u8> = Vec::new();
            let mut col_op_count: usize = 0;
            let mut col_data: Vec<u8> = Vec::new();
            let mut row = 0usize;
            while row < height {
                // Compare data items.
                let prev_row = &prev_planar[row * planes_per_row + p];
                let cur_row = &cur_planar[row * planes_per_row + p];
                let prev_item = if col_byte + data_size <= prev_row.len() {
                    &prev_row[col_byte..col_byte + data_size]
                } else {
                    &[][..]
                };
                let cur_item = if col_byte + data_size <= cur_row.len() {
                    &cur_row[col_byte..col_byte + data_size]
                } else {
                    &[][..]
                };
                if prev_item == cur_item && !prev_item.is_empty() {
                    // Count contiguous unchanged rows from `row`.
                    let mut skip = 0usize;
                    while row + skip < height {
                        let pr = &prev_planar[(row + skip) * planes_per_row + p];
                        let cr = &cur_planar[(row + skip) * planes_per_row + p];
                        let pi = if col_byte + data_size <= pr.len() {
                            &pr[col_byte..col_byte + data_size]
                        } else {
                            &[][..]
                        };
                        let ci = if col_byte + data_size <= cr.len() {
                            &cr[col_byte..col_byte + data_size]
                        } else {
                            &[][..]
                        };
                        if pi != ci || pi.is_empty() {
                            break;
                        }
                        skip += 1;
                    }
                    // Skip ops are a single byte with the hi bit
                    // clear: value 1..=127. Split runs longer than
                    // 127 into multiple skips. Each skip is one
                    // logical op.
                    let mut remaining = skip;
                    while remaining > 0 {
                        let chunk = remaining.min(0x7F);
                        col_ops.push(chunk as u8);
                        col_op_count += 1;
                        remaining -= chunk;
                    }
                    row += skip;
                } else {
                    // Find the contiguous changed run starting at `row`.
                    let mut end = row + 1;
                    while end < height {
                        let pr = &prev_planar[end * planes_per_row + p];
                        let cr = &cur_planar[end * planes_per_row + p];
                        let pi = if col_byte + data_size <= pr.len() {
                            &pr[col_byte..col_byte + data_size]
                        } else {
                            &[][..]
                        };
                        let ci = if col_byte + data_size <= cr.len() {
                            &cr[col_byte..col_byte + data_size]
                        } else {
                            &[][..]
                        };
                        if pi == ci && !pi.is_empty() {
                            break;
                        }
                        end += 1;
                    }
                    // Emit the run by picking Same-vs-Uniq per
                    // sub-run, capping at 127 for Uniq (hi bit
                    // available) and 255 for Same (count byte).
                    let mut i = row;
                    while i < end {
                        // Look for a maximal repeat of the same
                        // data item starting at i.
                        let item_start = i * planes_per_row + p;
                        let v = &cur_planar[item_start][col_byte..col_byte + data_size];
                        let mut rep_end = i + 1;
                        while rep_end < end && rep_end - i < 0xFF {
                            let cand = &cur_planar[rep_end * planes_per_row + p]
                                [col_byte..col_byte + data_size];
                            if cand != v {
                                break;
                            }
                            rep_end += 1;
                        }
                        let rep_len = rep_end - i;
                        // Same op costs 2 bytes (op + cnt) of op
                        // stream + `data_size` of data; Uniq of
                        // length L costs 1 byte of op stream + L *
                        // data_size of data. Same is cheaper when L
                        // ≥ 2; we use it for runs ≥ 2 (matches the
                        // op-5 encoder's threshold scaled down for
                        // the per-column op_count bookkeeping).
                        if rep_len >= 2 {
                            col_ops.push(0x00);
                            col_ops.push(rep_len as u8);
                            col_op_count += 1;
                            col_data.extend_from_slice(v);
                            i = rep_end;
                        } else {
                            // Uniq run: extend until we'd switch to a
                            // Same opcode (2-run ahead) or hit the end
                            // of the changed run, capped at 0x7F items.
                            let lit_start = i;
                            let mut lit_end = i + 1;
                            while lit_end < end && lit_end - lit_start < 0x7F {
                                let lv = &cur_planar[lit_end * planes_per_row + p]
                                    [col_byte..col_byte + data_size];
                                let next = lit_end + 1 < end
                                    && &cur_planar[(lit_end + 1) * planes_per_row + p]
                                        [col_byte..col_byte + data_size]
                                        == lv;
                                if next {
                                    // 2-run ahead — close literal here.
                                    break;
                                }
                                lit_end += 1;
                            }
                            let lit_len = lit_end - lit_start;
                            debug_assert!((1..=0x7F).contains(&lit_len));
                            col_ops.push(0x80 | lit_len as u8);
                            col_op_count += 1;
                            for r in lit_start..lit_end {
                                let item = &cur_planar[r * planes_per_row + p]
                                    [col_byte..col_byte + data_size];
                                col_data.extend_from_slice(item);
                            }
                            i = lit_end;
                        }
                    }
                    any_change = true;
                    row = end;
                }
            }
            // Per-column op stream: op_count byte + serialised ops.
            // op_count of 0 is legal and means "column unchanged" —
            // the decoder skips straight to the next column with no
            // data consumed. op_count records the *logical* op count
            // (not the byte length of the op stream); each iteration
            // of the decoder's `for _ in 0..op_count` reads one
            // variable-length opcode.
            if col_op_count > u8::MAX as usize {
                // Spec uses a u8 op-count; a column whose op stream
                // wouldn't fit can't be emitted in op-7. Surface a
                // diagnostic rather than truncate.
                return Err(Error::unsupported(format!(
                    "ANIM op 7 encode: column {col} of plane {p} produced {col_op_count} logical ops (max 255)"
                )));
            }
            op_list.push(col_op_count as u8);
            op_list.extend_from_slice(&col_ops);
            data_list.extend_from_slice(&col_data);
        }
        if any_change {
            plane_op[p] = op_list;
            plane_data[p] = data_list;
            plane_dirty[p] = true;
        }
    }

    // Assemble: 64-byte pointer table + per-plane opcode lists +
    // per-plane data lists. Pointers are absolute offsets from the
    // start of the DLTA. Convention: opcode lists come first, then
    // data lists — same layout the decoder reads.
    let mut out = vec![0u8; 64];
    // Opcode pointers (slots 0..=7).
    for (slot, list) in plane_op.iter_mut().enumerate().take(8) {
        if !plane_dirty[slot] {
            continue;
        }
        let offset = out.len() as u32;
        out[slot * 4..slot * 4 + 4].copy_from_slice(&offset.to_be_bytes());
        out.append(list);
    }
    // Data pointers (slots 8..=15).
    for (slot, list) in plane_data.iter_mut().enumerate().take(8) {
        if !plane_dirty[slot] {
            continue;
        }
        let offset = out.len() as u32;
        out[(8 + slot) * 4..(8 + slot) * 4 + 4].copy_from_slice(&offset.to_be_bytes());
        out.append(list);
    }
    Ok(out)
}

/// Encode a sequence of ILBM frames as a FORM/ANIM file using
/// `operation = 7` (Short / Long Vertical Delta) for every delta
/// frame.
///
/// The seed frame is the full leading FORM ILBM (same as
/// [`encode_anim_op0`] / [`encode_anim_op5`]); subsequent frames
/// carry an `ANHD` (op = 7) plus a `DLTA` chunk produced by
/// [`encode_op7_body`] from the diff between the prior and current
/// planar frames. `long_data` selects the short (2-byte items;
/// `ANHD.bits` bit 0 cleared) vs long (4-byte items; bit 0 set)
/// variant.
///
/// Compatible with the in-tree [`parse_anim`] op-7 decoder; tested
/// via `tests/anim_op7_encode.rs`.
pub fn encode_anim_op7(frames: &[IlbmImage], long_data: bool) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid(
            "ANIM op 7 encode: at least one frame required",
        ));
    }
    if frames[0].bmhd.n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ANIM op 7 encode: requires ≤ 8 colour planes (got {})",
            frames[0].bmhd.n_planes
        )));
    }
    let leading = crate::ilbm::encode_ilbm(&frames[0])?;
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"ANIM");
    out.extend_from_slice(&leading);
    if leading.len() & 1 == 1 {
        out.push(0);
    }

    let mut prev_planar = rgba_to_planar(&frames[0]);

    for frame in &frames[1..] {
        let cur_planar = rgba_to_planar(frame);
        let dlta = encode_op7_body(&prev_planar, &cur_planar, &frame.bmhd, long_data)?;
        let anhd = Anhd {
            operation: 7,
            mask: 0,
            w: frame.bmhd.width,
            h: frame.bmhd.height,
            x: frame.bmhd.x_origin,
            y: frame.bmhd.y_origin,
            abs_time: 0,
            rel_time: 1,
            // Delta against the immediately-previous frame ⇒ one frame
            // back (§2.1 `interleave`), not the `0` two-back default.
            interleave: 1,
            pad0: 0,
            bits: if long_data { 1 } else { 0 },
        };
        let anhd_bytes = anhd.write(dlta.len() as u32);
        // Inner FORM ILBM size = 4 ("ILBM") + 8+40 (ANHD) + 8 + dlta.
        let inner_size = (4 + 8 + 40 + 8 + dlta.len()) as u32;
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&inner_size.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"ANHD");
        out.extend_from_slice(&40u32.to_be_bytes());
        out.extend_from_slice(&anhd_bytes);
        out.extend_from_slice(b"DLTA");
        out.extend_from_slice(&(dlta.len() as u32).to_be_bytes());
        out.extend_from_slice(&dlta);
        if dlta.len() & 1 == 1 {
            out.push(0);
        }
        prev_planar = cur_planar;
    }

    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Encode a single op-4 (Generalized Delta mode) DLTA payload from a
/// previous and current planar frame, in the short/long-data, vertical,
/// RLC, separate-info-per-plane, non-XOR configuration the §2.2.2
/// `SetDLTAshort` reference routine documents.
///
/// `prev_planar` and `cur_planar` are the row-major flat arrays of
/// bitplane rows in IFF order (`planes_per_row = n_planes +
/// mask_plane`), each row `row_bytes` long — the same shape
/// [`apply_op4`] consumes. `long_data` selects the data-word width
/// (`false` = short 2-byte words → `ANHD.bits` bit 0 clear, `true` =
/// long 4-byte words → bit 0 set).
///
/// The output is the DLTA chunk body per §2.2.2: a 64-byte pointer
/// table (16 × u32 BE — 8 data-list pointers then 8 op-list pointers,
/// each a **word** offset from the DLTA start per the reference
/// routine's `WORD*` arithmetic) followed by per-plane op lists and
/// data lists. An unchanged plane gets `0` in both its op- and
/// data-pointer slots.
///
/// Each plane's op list is a flat run of `(offset, size)` short pairs
/// terminated by `0xFFFF`. `offset` is the absolute word position of
/// the column run's first row; `size > 0` is a Uniq run of `size`
/// per-row data words and `size < 0` is a Same run writing one data
/// word to `|size|` rows. The encoder walks each vertical column
/// (stride `nw = row_bytes / word_size`), splits it into changed runs,
/// and within each run picks Same (one data word) for repeats of ≥ 2
/// equal words and Uniq otherwise — the §2.2.2 routine's two op
/// classes. Runs longer than a signed-short can express (`> 0x7FFF`
/// rows) split into multiple ops at the same column.
///
/// This encoder always emits short (2-byte) op offsets (`ANHD.bits`
/// bit 5 clear) and separate per-plane info lists (bit 2 clear), the
/// configuration the reference routine reads. Compatible with the
/// in-tree [`apply_op4`] decoder.
pub fn encode_op4_body(
    prev_planar: &[Vec<u8>],
    cur_planar: &[Vec<u8>],
    bmhd: &Bmhd,
    long_data: bool,
) -> Result<Vec<u8>> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let word_size = if long_data { 4 } else { 2 };
    let expected = height * planes_per_row;
    if prev_planar.len() != expected || cur_planar.len() != expected {
        return Err(Error::invalid(format!(
            "ANIM op 4 encode: planar buffers have {} / {} rows, expected {expected}",
            prev_planar.len(),
            cur_planar.len()
        )));
    }
    if n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ANIM op 4 encode: requires ≤ 8 colour planes (got {n_planes})"
        )));
    }
    if row_bytes % word_size != 0 {
        return Err(Error::unsupported(format!(
            "ANIM op 4 encode: row_bytes {row_bytes} not a multiple of word size {word_size}"
        )));
    }
    let nw = row_bytes / word_size; // words per row = vertical stride

    fn word_at(row: &[u8], w: usize, word_size: usize) -> &[u8] {
        &row[w * word_size..(w + 1) * word_size]
    }

    let mut plane_op: Vec<Vec<u8>> = vec![Vec::new(); 8];
    let mut plane_data: Vec<Vec<u8>> = vec![Vec::new(); 8];
    let mut plane_dirty = [false; 8];

    for p in 0..n_planes {
        let mut op_list: Vec<u8> = Vec::new();
        let mut data_list: Vec<u8> = Vec::new();
        let mut any_change = false;

        // Walk vertical columns: column `c` is word index `c` within
        // each row; descending the column adds `nw` to the absolute
        // word position.
        for c in 0..nw {
            let mut row = 0usize;
            while row < height {
                let prev_w = word_at(&prev_planar[row * planes_per_row + p], c, word_size);
                let cur_w = word_at(&cur_planar[row * planes_per_row + p], c, word_size);
                if prev_w == cur_w {
                    row += 1;
                    continue;
                }
                // Changed run [row, end) in this column.
                let mut end = row + 1;
                while end < height {
                    let pw = word_at(&prev_planar[end * planes_per_row + p], c, word_size);
                    let cw = word_at(&cur_planar[end * planes_per_row + p], c, word_size);
                    if pw == cw {
                        break;
                    }
                    end += 1;
                }
                // Emit ops for the run, each anchored at its absolute
                // word offset `c + r * nw`.
                let mut r = row;
                while r < end {
                    let abs_word = c + r * nw;
                    let v = word_at(&cur_planar[r * planes_per_row + p], c, word_size);
                    // Maximal repeat of `v` from r.
                    let mut rep_end = r + 1;
                    while rep_end < end {
                        let cand = word_at(&cur_planar[rep_end * planes_per_row + p], c, word_size);
                        if cand != v {
                            break;
                        }
                        rep_end += 1;
                    }
                    let rep_len = rep_end - r;
                    if rep_len >= 2 {
                        // Same op: split into ≤ 0x7FFF-row chunks (signed
                        // short size field).
                        let mut remaining = rep_len;
                        let mut at = r;
                        while remaining > 0 {
                            let chunk = remaining.min(0x7FFF);
                            let start_word = c + at * nw;
                            op_list.extend_from_slice(&(start_word as u16).to_be_bytes());
                            let size = -(chunk as i32) as i16;
                            op_list.extend_from_slice(&size.to_be_bytes());
                            data_list.extend_from_slice(word_at(
                                &cur_planar[at * planes_per_row + p],
                                c,
                                word_size,
                            ));
                            at += chunk;
                            remaining -= chunk;
                        }
                        let _ = abs_word;
                        r = rep_end;
                    } else {
                        // Uniq run: extend until a 2-run appears or the
                        // changed run ends, capped at 0x7FFF rows.
                        let lit_start = r;
                        let mut lit_end = r + 1;
                        while lit_end < end && lit_end - lit_start < 0x7FFF {
                            let lv =
                                word_at(&cur_planar[lit_end * planes_per_row + p], c, word_size);
                            let next = lit_end + 1 < end
                                && word_at(
                                    &cur_planar[(lit_end + 1) * planes_per_row + p],
                                    c,
                                    word_size,
                                ) == lv;
                            if next {
                                break;
                            }
                            lit_end += 1;
                        }
                        let lit_len = lit_end - lit_start;
                        let start_word = c + lit_start * nw;
                        op_list.extend_from_slice(&(start_word as u16).to_be_bytes());
                        op_list.extend_from_slice(&(lit_len as i16).to_be_bytes());
                        for rr in lit_start..lit_end {
                            data_list.extend_from_slice(word_at(
                                &cur_planar[rr * planes_per_row + p],
                                c,
                                word_size,
                            ));
                        }
                        r = lit_end;
                    }
                }
                any_change = true;
                row = end;
            }
        }
        if any_change {
            // Terminate the plane's op list with 0xFFFF.
            op_list.extend_from_slice(&0xFFFFu16.to_be_bytes());
            plane_op[p] = op_list;
            plane_data[p] = data_list;
            plane_dirty[p] = true;
        }
    }

    // Assemble: 64-byte pointer table (8 data ptrs then 8 op ptrs) +
    // per-plane op lists + per-plane data lists. Pointers are **word**
    // offsets from the DLTA start per the §2.2.2 routine; the layout
    // therefore starts on a word boundary (64 is even) and every list
    // is appended at an even byte offset so the word offset is exact.
    let mut out = vec![0u8; 64];

    // Op lists first (so we can compute their offsets), recording the
    // op-pointer slots (8..=15).
    let mut op_word_offsets = [0u32; 8];
    for (slot, list) in plane_op.iter_mut().enumerate().take(8) {
        if !plane_dirty[slot] {
            continue;
        }
        debug_assert!(out.len() % 2 == 0, "op list must start word-aligned");
        op_word_offsets[slot] = (out.len() / 2) as u32;
        out.append(list);
    }
    // Data lists next, recording the data-pointer slots (0..=7).
    let mut data_word_offsets = [0u32; 8];
    for (slot, list) in plane_data.iter_mut().enumerate().take(8) {
        if !plane_dirty[slot] {
            continue;
        }
        debug_assert!(out.len() % 2 == 0, "data list must start word-aligned");
        data_word_offsets[slot] = (out.len() / 2) as u32;
        out.append(list);
    }
    // Fill the pointer table: slots 0..=7 = data, 8..=15 = op.
    for slot in 0..8 {
        out[slot * 4..slot * 4 + 4].copy_from_slice(&data_word_offsets[slot].to_be_bytes());
        out[(8 + slot) * 4..(8 + slot) * 4 + 4]
            .copy_from_slice(&op_word_offsets[slot].to_be_bytes());
    }
    Ok(out)
}

/// Encode a sequence of ILBM frames as a FORM/ANIM file using
/// `operation = 4` (Generalized short/long Delta mode) for every delta
/// frame. See [`encode_op4_body`] for the payload layout. `long_data`
/// selects the short (2-byte words; `ANHD.bits` bit 0 clear) vs long
/// (4-byte words; bit 0 set) data variant.
///
/// Compatible with the in-tree [`parse_anim`] op-4 decoder; tested via
/// `tests/anim_op4.rs`.
pub fn encode_anim_op4(frames: &[IlbmImage], long_data: bool) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid(
            "ANIM op 4 encode: at least one frame required",
        ));
    }
    if frames[0].bmhd.n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ANIM op 4 encode: requires ≤ 8 colour planes (got {})",
            frames[0].bmhd.n_planes
        )));
    }
    let leading = crate::ilbm::encode_ilbm(&frames[0])?;
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"ANIM");
    out.extend_from_slice(&leading);
    if leading.len() & 1 == 1 {
        out.push(0);
    }

    let mut prev_planar = rgba_to_planar(&frames[0]);

    for frame in &frames[1..] {
        let cur_planar = rgba_to_planar(frame);
        let dlta = encode_op4_body(&prev_planar, &cur_planar, &frame.bmhd, long_data)?;
        let anhd = Anhd {
            operation: 4,
            mask: 0,
            w: frame.bmhd.width,
            h: frame.bmhd.height,
            x: frame.bmhd.x_origin,
            y: frame.bmhd.y_origin,
            abs_time: 0,
            rel_time: 1,
            // Delta against the immediately-previous frame ⇒ one frame
            // back (§2.1 `interleave`), not the `0` two-back default.
            interleave: 1,
            pad0: 0,
            // bit 0 = data width, bit 3 = RLC, bit 4 = vertical — the
            // configuration `encode_op4_body` emits and `apply_op4`
            // reads.
            bits: (if long_data { 0b1 } else { 0 }) | 0b0000_1000 | 0b0001_0000,
        };
        let anhd_bytes = anhd.write(dlta.len() as u32);
        let inner_size = (4 + 8 + 40 + 8 + dlta.len()) as u32;
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&inner_size.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"ANHD");
        out.extend_from_slice(&40u32.to_be_bytes());
        out.extend_from_slice(&anhd_bytes);
        out.extend_from_slice(b"DLTA");
        out.extend_from_slice(&(dlta.len() as u32).to_be_bytes());
        out.extend_from_slice(&dlta);
        if dlta.len() & 1 == 1 {
            out.push(0);
        }
        prev_planar = cur_planar;
    }

    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Encode a single op-2 / op-3 (Long / Short Delta mode) DLTA payload
/// from a previous and current planar frame.
///
/// `prev_planar` and `cur_planar` are the row-major flat arrays of
/// bitplane rows in IFF order (`planes_per_row = n_planes +
/// mask_plane`), each row being `row_bytes` long — the same shape the
/// decoder ([`apply_op23_for_test`]) operates on.
///
/// `long_data` selects the data-word width: `true` = Long Delta mode
/// (op 2, 4-byte data words) and `false` = Short Delta mode (op 3,
/// 2-byte data words). Offsets and counts are big-endian shorts in
/// both modes. The bitplane is addressed as one contiguous word
/// array (`height * row_bytes` bytes), so for op 2 data words may
/// straddle row boundaries; the total plane byte count MUST divide
/// evenly by the word width or the trailing bytes would be
/// unaddressable — mismatches are rejected as [`Error::Unsupported`].
///
/// The output is the DLTA chunk body per §2.2.1: a 32-byte pointer
/// table (8 × u32 BE, one per bitplane; `0` = plane unchanged)
/// followed by per-plane group lists. Per changed word run the
/// encoder emits either a positive-offset group (offset short + one
/// data word) for an isolated change, or a negative-offset group
/// (offset short with `abs = offset + 2`, count short, `count`
/// contiguous data words) for runs of ≥ 2 changed words — §1.2.2 ¶
/// "Strings of 2 or more long-words in a row which change can be run
/// together so offsets do not have to be saved for each one". Each
/// plane's list ends with the `0xFFFF` terminator. Gaps wider than a
/// positive short can express are bridged by rewriting an unchanged
/// word (a semantic no-op costing one group).
///
/// Used by [`encode_anim_op2`] / [`encode_anim_op3`] but exposed
/// publicly so callers driving the lower-level container can build
/// their own ANIM2 / ANIM3 streams.
pub fn encode_op23_body(
    prev_planar: &[Vec<u8>],
    cur_planar: &[Vec<u8>],
    bmhd: &Bmhd,
    long_data: bool,
) -> Result<Vec<u8>> {
    let n_planes = bmhd.n_planes as usize;
    let has_mask = bmhd.masking == Masking::HasMask;
    let planes_per_row = n_planes + has_mask as usize;
    let row_bytes = bmhd.row_bytes();
    let height = bmhd.height as usize;
    let word_size = if long_data { 4 } else { 2 };
    let expected = height * planes_per_row;
    if prev_planar.len() != expected || cur_planar.len() != expected {
        return Err(Error::invalid(format!(
            "ANIM op 2/3 encode: planar buffers have {} / {} rows, expected {expected}",
            prev_planar.len(),
            cur_planar.len()
        )));
    }
    if n_planes > 8 {
        // The §2.2.1 pointer table has 8 slots (one u32 per colour
        // plane); deeper formats can't be addressed by ops 2/3.
        return Err(Error::unsupported(format!(
            "ANIM op 2/3 encode: requires ≤ 8 colour planes (got {n_planes})"
        )));
    }
    let plane_bytes = height * row_bytes;
    if plane_bytes % word_size != 0 {
        return Err(Error::unsupported(format!(
            "ANIM op 2/3 encode: plane size {plane_bytes} not a multiple of word size {word_size}"
        )));
    }
    let word_count = plane_bytes / word_size;

    let mut out = vec![0u8; 32];
    for p in 0..n_planes {
        // Gather both frames' plane `p` into contiguous buffers so
        // word offsets address the same flat array the decoder uses.
        let mut prev_buf = Vec::with_capacity(plane_bytes);
        let mut cur_buf = Vec::with_capacity(plane_bytes);
        for y in 0..height {
            prev_buf.extend_from_slice(&prev_planar[y * planes_per_row + p]);
            cur_buf.extend_from_slice(&cur_planar[y * planes_per_row + p]);
        }
        fn word_at(buf: &[u8], w: usize, word_size: usize) -> &[u8] {
            &buf[w * word_size..(w + 1) * word_size]
        }

        let mut list: Vec<u8> = Vec::new();
        let mut wcursor: usize = 0; // last written word (starts at word 0)
        let mut any_change = false;
        let mut i = 0usize;
        while i < word_count {
            if word_at(&prev_buf, i, word_size) == word_at(&cur_buf, i, word_size) {
                i += 1;
                continue;
            }
            // Contiguous run of changed words [i, end).
            let mut end = i + 1;
            while end < word_count
                && word_at(&prev_buf, end, word_size) != word_at(&cur_buf, end, word_size)
            {
                end += 1;
            }
            // Bridge gaps a positive offset short can't express by
            // rewriting an unchanged word in place (cur == prev
            // there, so the write is a no-op on the plane state).
            while i - wcursor > 0x7FFF {
                wcursor += 0x7FFF;
                list.extend_from_slice(&0x7FFFu16.to_be_bytes());
                list.extend_from_slice(word_at(&cur_buf, wcursor, word_size));
            }
            while i < end {
                let run = end - i;
                // Offset is ≤ 0x7FFF after bridging. A run group's
                // offset must satisfy `abs = offset + 2 ≤ 0x8000`
                // (0xFFFF is the terminator); offset 0x7FFF would
                // overflow that, so fall back to a single-word group
                // for the run's first word.
                let offset = i - wcursor;
                if run >= 2 && offset <= 0x7FFE {
                    let count = run.min(0xFFFF);
                    let encoded = (0x1_0000 - (offset + 2)) as u16;
                    list.extend_from_slice(&encoded.to_be_bytes());
                    list.extend_from_slice(&(count as u16).to_be_bytes());
                    for w in i..i + count {
                        list.extend_from_slice(word_at(&cur_buf, w, word_size));
                    }
                    wcursor = i + count - 1;
                    i += count;
                } else {
                    list.extend_from_slice(&(offset as u16).to_be_bytes());
                    list.extend_from_slice(word_at(&cur_buf, i, word_size));
                    wcursor = i;
                    i += 1;
                }
            }
            any_change = true;
        }
        if any_change {
            list.extend_from_slice(&0xFFFFu16.to_be_bytes());
            let offset = out.len() as u32;
            out[p * 4..p * 4 + 4].copy_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&list);
        }
        // Unchanged plane: pointer slot stays 0 per §2.2.1 ¶ "If there
        // are no changed words in a given plane, then the pointer in
        // the first 32 bytes of the chunk is =0".
    }
    Ok(out)
}

/// Encode a sequence of ILBM frames as a FORM/ANIM file using
/// `operation = 2` (Long Delta mode — 4-byte data words) for every
/// delta frame. See [`encode_op23_body`] for the payload layout.
///
/// Compatible with the in-tree [`parse_anim`] op-2 decoder; tested
/// via `tests/anim_op23.rs`.
pub fn encode_anim_op2(frames: &[IlbmImage]) -> Result<Vec<u8>> {
    encode_anim_op23(frames, true)
}

/// Encode a sequence of ILBM frames as a FORM/ANIM file using
/// `operation = 3` (Short Delta mode — 2-byte data words) for every
/// delta frame. See [`encode_op23_body`] for the payload layout.
///
/// Compatible with the in-tree [`parse_anim`] op-3 decoder; tested
/// via `tests/anim_op23.rs`.
pub fn encode_anim_op3(frames: &[IlbmImage]) -> Result<Vec<u8>> {
    encode_anim_op23(frames, false)
}

/// Shared op-2 / op-3 container assembly: a leading full FORM ILBM
/// seed frame followed by ANHD + DLTA delta frames, mirroring the
/// op-5 / op-7 encoders' shape. `ANHD.bits` stays 0 — the spec
/// defines option bits for methods 4 and 5 only and recommends zero
/// elsewhere.
fn encode_anim_op23(frames: &[IlbmImage], long_data: bool) -> Result<Vec<u8>> {
    let operation = if long_data { 2 } else { 3 };
    if frames.is_empty() {
        return Err(Error::invalid(format!(
            "ANIM op {operation} encode: at least one frame required"
        )));
    }
    if frames[0].bmhd.n_planes > 8 {
        return Err(Error::unsupported(format!(
            "ANIM op {operation} encode: requires ≤ 8 colour planes (got {})",
            frames[0].bmhd.n_planes
        )));
    }
    let leading = crate::ilbm::encode_ilbm(&frames[0])?;
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"ANIM");
    out.extend_from_slice(&leading);
    if leading.len() & 1 == 1 {
        out.push(0);
    }

    let mut prev_planar = rgba_to_planar(&frames[0]);

    for frame in &frames[1..] {
        let cur_planar = rgba_to_planar(frame);
        let dlta = encode_op23_body(&prev_planar, &cur_planar, &frame.bmhd, long_data)?;
        let anhd = Anhd {
            operation,
            mask: 0,
            w: frame.bmhd.width,
            h: frame.bmhd.height,
            x: frame.bmhd.x_origin,
            y: frame.bmhd.y_origin,
            abs_time: 0,
            rel_time: 1,
            // Delta against the immediately-previous frame ⇒ one frame
            // back (§2.1 `interleave`), not the `0` two-back default.
            interleave: 1,
            pad0: 0,
            bits: 0,
        };
        let anhd_bytes = anhd.write(dlta.len() as u32);
        let inner_size = (4 + 8 + 40 + 8 + dlta.len()) as u32;
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&inner_size.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"ANHD");
        out.extend_from_slice(&40u32.to_be_bytes());
        out.extend_from_slice(&anhd_bytes);
        out.extend_from_slice(b"DLTA");
        out.extend_from_slice(&(dlta.len() as u32).to_be_bytes());
        out.extend_from_slice(&dlta);
        if dlta.len() & 1 == 1 {
            out.push(0);
        }
        prev_planar = cur_planar;
    }

    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Encode a sequence of ILBM frames as a FORM/ANIM file using
/// `operation = 0` (literal full BODY) for every delta frame. This is
/// the simplest legal ANIM the spec allows; players (DPaint III etc.)
/// must handle op 0 since it's the fallback.
///
/// Use this for round-tripping in tests; production-quality ANIM5
/// encode (op-5 byte-vertical delta) lives in [`encode_anim_op5`].
pub fn encode_anim_op0(frames: &[IlbmImage]) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid("ANIM encode: at least one frame required"));
    }
    let leading = crate::ilbm::encode_ilbm(&frames[0])?;
    // Strip the outer "FORM <size> ILBM" header — we'll wrap it into
    // the outer ANIM container ourselves. Actually, we keep the inner
    // FORM ILBM as a child chunk of the outer FORM ANIM.
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"ANIM");
    // Child 0: the leading FORM ILBM (verbatim).
    out.extend_from_slice(&leading);
    if leading.len() & 1 == 1 {
        out.push(0);
    }

    // Subsequent frames: FORM ILBM with ANHD (op 0) + BODY.
    for frame in &frames[1..] {
        let body = encode_full_body(frame)?;
        let anhd = Anhd {
            operation: 0,
            mask: 0,
            w: frame.bmhd.width,
            h: frame.bmhd.height,
            x: frame.bmhd.x_origin,
            y: frame.bmhd.y_origin,
            abs_time: 0,
            rel_time: 1,
            // Op-0 frames are full literal overwrites that ignore prior
            // state, but the pipeline still produces one frame per step;
            // tag `interleave = 1` for a uniform single-step ANIM.
            interleave: 1,
            pad0: 0,
            bits: 0,
        };
        let anhd_bytes = anhd.write(body.len() as u32);

        // Inner FORM ILBM size = 4 ("ILBM") + 8+40 (ANHD) + 8 + body.
        let inner_size = (4 + 8 + 40 + 8 + body.len()) as u32;
        let inner_size_padded = inner_size + (inner_size & 1);

        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&inner_size.to_be_bytes());
        out.extend_from_slice(b"ILBM");
        out.extend_from_slice(b"ANHD");
        out.extend_from_slice(&40u32.to_be_bytes());
        out.extend_from_slice(&anhd_bytes);
        out.extend_from_slice(b"BODY");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
        // Pad outer chunk if odd.
        let _ = inner_size_padded;
    }

    let form_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    Ok(out)
}

/// Build a single uncompressed planar BODY for `image` (no ByteRun1).
/// Used only by the op-0 encoder.
fn encode_full_body(image: &IlbmImage) -> Result<Vec<u8>> {
    let bmhd = image.bmhd;
    let n_planes = bmhd.n_planes as usize;
    let row_bytes = bmhd.row_bytes();
    let has_mask = bmhd.masking == Masking::HasMask;
    let pal: Vec<[u8; 3]> = if image.camg.is_ehb() && image.palette.len() <= 32 {
        expand_ehb_palette(&image.palette)
    } else {
        image.palette.clone()
    };
    let mut out =
        Vec::with_capacity(bmhd.height as usize * (n_planes + has_mask as usize) * row_bytes);
    for y in 0..bmhd.height as usize {
        let mut indices = vec![0u8; bmhd.width as usize];
        let mut mask = vec![0u8; row_bytes];
        for x in 0..bmhd.width as usize {
            let src = (y * bmhd.width as usize + x) * 4;
            let r = image.rgba[src];
            let g = image.rgba[src + 1];
            let b = image.rgba[src + 2];
            let a = image.rgba[src + 3];
            let mut best = 0usize;
            let mut best_d = i32::MAX;
            for (i, p) in pal.iter().enumerate() {
                let dr = r as i32 - p[0] as i32;
                let dg = g as i32 - p[1] as i32;
                let db = b as i32 - p[2] as i32;
                let d = dr * dr + dg * dg + db * db;
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            indices[x] = best as u8;
            if a >= 0x80 {
                let bi = x / 8;
                let bit = 7 - (x % 8);
                mask[bi] |= 1 << bit;
            }
        }
        let plane_rows = indices_to_planar_row(&indices, bmhd.n_planes, row_bytes);
        for pr in plane_rows {
            out.extend_from_slice(&pr);
        }
        if has_mask {
            out.extend_from_slice(&mask);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_palette() -> Vec<[u8; 3]> {
        vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]]
    }

    fn frame_solid(color: [u8; 4], w: u16, h: u16, palette: Vec<[u8; 3]>) -> IlbmImage {
        let bmhd = Bmhd {
            width: w,
            height: h,
            x_origin: 0,
            y_origin: 0,
            n_planes: 2,
            masking: Masking::None,
            compression: Compression::None,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: w as i16,
            page_height: h as i16,
        };
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for _ in 0..(w as usize) * (h as usize) {
            rgba.extend_from_slice(&color);
        }
        IlbmImage {
            width: w as u32,
            height: h as u32,
            bmhd,
            palette,
            camg: Camg::default(),
            rgba,
            ..IlbmImage::default()
        }
    }

    #[test]
    fn op0_roundtrip_three_solid_frames() {
        let pal = solid_palette();
        let frames = vec![
            frame_solid([255, 0, 0, 0xFF], 8, 4, pal.clone()),
            frame_solid([0, 255, 0, 0xFF], 8, 4, pal.clone()),
            frame_solid([0, 0, 255, 0xFF], 8, 4, pal.clone()),
        ];
        let bytes = encode_anim_op0(&frames).unwrap();
        // Quick magic check.
        assert_eq!(&bytes[0..4], b"FORM");
        assert_eq!(&bytes[8..12], b"ANIM");
        let dec = parse_anim(&bytes).unwrap();
        assert_eq!(dec.frames.len(), 3, "round-trips 3 frames");
        assert_eq!(dec.width, 8);
        assert_eq!(dec.height, 4);
        for (i, f) in dec.frames.iter().enumerate() {
            // First pixel of each frame should match the source colour.
            let src_color = match i {
                0 => [255, 0, 0],
                1 => [0, 255, 0],
                _ => [0, 0, 255],
            };
            assert_eq!(
                &f.rgba[0..3],
                &src_color,
                "frame {i} solid colour roundtripped"
            );
        }
    }

    #[test]
    fn anhd_roundtrip() {
        let a = Anhd {
            operation: 5,
            mask: 0,
            w: 320,
            h: 200,
            x: 0,
            y: 0,
            abs_time: 0,
            rel_time: 2,
            interleave: 0,
            pad0: 0,
            bits: 0,
        };
        let bytes = a.write(1234);
        let parsed = Anhd::parse(&bytes).unwrap();
        assert_eq!(parsed.operation, 5);
        assert_eq!(parsed.w, 320);
        assert_eq!(parsed.h, 200);
        assert_eq!(parsed.rel_time, 2);
    }

    #[test]
    fn probe_recognises_form_anim() {
        let mut bytes = vec![0u8; 12];
        bytes[0..4].copy_from_slice(b"FORM");
        bytes[8..12].copy_from_slice(b"ANIM");
        assert_eq!(probe(&bytes), 100);
    }

    /// Wrap a chunk body in `ckID + ckSize + body + odd-pad`.
    fn frame_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + body.len() + 1);
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
        out
    }

    /// Assemble a `FORM ANIM` from a seed ILBM plus a list of delta
    /// frames, each `(Anhd, op5-DLTA/BODY payload)`. The seed is encoded
    /// with `encode_ilbm`; every delta frame is an inner `FORM ILBM`
    /// carrying `ANHD` + `BODY`.
    fn assemble_anim(seed: &IlbmImage, deltas: &[(Anhd, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"ANIM");
        let leading = crate::ilbm::encode_ilbm(seed).unwrap();
        out.extend_from_slice(&leading);
        if leading.len() & 1 == 1 {
            out.push(0);
        }
        for (anhd, body) in deltas {
            let anhd_bytes = anhd.write(body.len() as u32);
            let mut inner = Vec::new();
            inner.extend_from_slice(b"ILBM");
            inner.extend_from_slice(&frame_chunk(b"ANHD", &anhd_bytes));
            inner.extend_from_slice(&frame_chunk(b"BODY", body));
            out.extend_from_slice(&frame_chunk(b"FORM", &inner));
        }
        let form_size = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&form_size.to_be_bytes());
        out
    }

    /// An `interleave = 0` delta references the frame **two** back
    /// (DeluxePaint double-buffering), not the immediately-previous
    /// frame. Build seed = red, frame 1 = green (op-0 literal), frame 2 =
    /// op-5 delta from the *seed* (red) tagged `interleave = 0`, so the
    /// decoded frame 2 must be the seed-plus-delta result — proving the
    /// decoder selected the 2-back buffer, not frame 1.
    #[test]
    fn interleave_zero_references_two_frames_back() {
        let pal = solid_palette();
        let seed = frame_solid([255, 0, 0, 0xFF], 8, 4, pal.clone()); // index 1
        let green = frame_solid([0, 255, 0, 0xFF], 8, 4, pal.clone()); // index 2
        let blue = frame_solid([0, 0, 255, 0xFF], 8, 4, pal.clone()); // index 3

        let seed_planar = rgba_to_planar(&seed);
        let blue_planar = rgba_to_planar(&blue);

        // Frame 1: full op-0 literal of green.
        let green_body = encode_full_body(&green).unwrap();
        let anhd0 = Anhd {
            operation: 0,
            w: 8,
            h: 4,
            rel_time: 1,
            interleave: 1,
            ..Anhd::default()
        };

        // Frame 2: op-5 delta from the SEED (red) → blue, tagged
        // interleave = 0 (two frames back ⇒ references the seed).
        let blue_delta = encode_op5_body(&seed_planar, &blue_planar, &blue.bmhd).unwrap();
        let anhd_blue = Anhd {
            operation: 5,
            w: 8,
            h: 4,
            rel_time: 1,
            interleave: 0,
            ..Anhd::default()
        };

        let bytes = assemble_anim(&seed, &[(anhd0, green_body), (anhd_blue, blue_delta)]);
        let dec = parse_anim(&bytes).unwrap();
        assert_eq!(dec.frames.len(), 3);
        // Frame 0 = red, frame 1 = green, frame 2 must be blue because
        // the delta was applied two frames back (to the red seed), not
        // to the green frame 1.
        assert_eq!(&dec.frames[0].rgba[0..3], &[255, 0, 0]);
        assert_eq!(&dec.frames[1].rgba[0..3], &[0, 255, 0]);
        assert_eq!(
            &dec.frames[2].rgba[0..3],
            &[0, 0, 255],
            "interleave=0 delta resolved against the 2-back seed buffer"
        );
    }

    /// An `interleave = 1` delta references the immediately-previous
    /// frame. Same construction but the frame-2 delta is computed from
    /// frame 1 (green) and tagged `interleave = 1`.
    #[test]
    fn interleave_one_references_previous_frame() {
        let pal = solid_palette();
        let seed = frame_solid([255, 0, 0, 0xFF], 8, 4, pal.clone());
        let green = frame_solid([0, 255, 0, 0xFF], 8, 4, pal.clone());
        let blue = frame_solid([0, 0, 255, 0xFF], 8, 4, pal.clone());

        let green_planar = rgba_to_planar(&green);
        let blue_planar = rgba_to_planar(&blue);

        let green_body = encode_full_body(&green).unwrap();
        let anhd0 = Anhd {
            operation: 0,
            w: 8,
            h: 4,
            rel_time: 1,
            interleave: 1,
            ..Anhd::default()
        };

        // delta green → blue, applied one frame back (to green).
        let blue_delta = encode_op5_body(&green_planar, &blue_planar, &blue.bmhd).unwrap();
        let anhd_blue = Anhd {
            operation: 5,
            w: 8,
            h: 4,
            rel_time: 1,
            interleave: 1,
            ..Anhd::default()
        };

        let bytes = assemble_anim(&seed, &[(anhd0, green_body), (anhd_blue, blue_delta)]);
        let dec = parse_anim(&bytes).unwrap();
        assert_eq!(dec.frames.len(), 3);
        assert_eq!(&dec.frames[2].rgba[0..3], &[0, 0, 255]);
    }

    /// All existing multi-frame encoders now tag `interleave = 1` (they
    /// compute each frame as a delta against the immediately-previous
    /// one), so a full encode → decode round-trip is still pixel-exact.
    #[test]
    fn encoders_tag_interleave_one_and_roundtrip() {
        let pal = solid_palette();
        let frames = vec![
            frame_solid([255, 0, 0, 0xFF], 8, 4, pal.clone()),
            frame_solid([0, 255, 0, 0xFF], 8, 4, pal.clone()),
            frame_solid([0, 0, 255, 0xFF], 8, 4, pal.clone()),
        ];
        let bytes = encode_anim_op5(&frames).unwrap();
        let dec = parse_anim(&bytes).unwrap();
        assert_eq!(dec.frames.len(), 3);
        assert_eq!(&dec.frames[0].rgba[0..3], &[255, 0, 0]);
        assert_eq!(&dec.frames[1].rgba[0..3], &[0, 255, 0]);
        assert_eq!(&dec.frames[2].rgba[0..3], &[0, 0, 255]);
    }
}
