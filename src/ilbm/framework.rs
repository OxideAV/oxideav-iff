//! `oxideav-core` container adapters for the IFF raster FORMs.
//!
//! Everything in this file needs the framework: the `iff_ilbm` /
//! `iff_acbm` / `iff_rgb8` / `iff_rgbn` / `iff_deep` / `iff_tvpp`
//! demuxers (one `rawvideo` / `Rgba` keyframe per decoded picture, or
//! `mjpeg` passthrough packets for a §1.5b JPEG `FORM DEEP`), the
//! `IlbmMuxer` / `DeepMuxer` / `RgbTrueColorMuxer` container muxers and
//! [`register`]. The parsers and encoders they wrap live in the parent
//! module and are framework-free; this module is compiled only with the
//! default-on `registry` feature.

use std::io::{Read, SeekFrom, Write};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType, Packet,
    PixelFormat, Result, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_core::{Muxer, ReadSeek};

use super::*;
use crate::chunk::{read_chunk_header, read_form_type, GROUP_FORM};

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("iff_ilbm", open);
    reg.register_muxer("iff_ilbm", open_muxer);
    reg.register_extension("ilbm", "iff_ilbm");
    reg.register_extension("lbm", "iff_ilbm");
    reg.register_probe("iff_ilbm", probe);

    // ACBM — Amiga Contiguous BitMap (AmigaBASIC sibling of ILBM). The
    // BODY is replaced by a plane-contiguous, uncompressed ABIT chunk;
    // everything else (BMHD/CMAP/CAMG/…) matches ILBM. Decode-only via the
    // container path; `parse_acbm`/`encode_acbm` cover the round-trip.
    reg.register_demuxer("iff_acbm", open_acbm);
    reg.register_extension("acbm", "iff_acbm");
    reg.register_probe("iff_acbm", probe_acbm);

    // Turbo-Silver / Imagine true-colour FORMs (decode-only). Both share the
    // ILBM-like outer container but carry a Turbo-Silver genlock-RLE BODY
    // (`BMHD.compression == 4`). See `iff-truecolor-chunks.md` §3.
    reg.register_demuxer("iff_rgb8", open_rgb8);
    reg.register_muxer("iff_rgb8", open_rgb8_muxer);
    reg.register_extension("rgb8", "iff_rgb8");
    reg.register_probe("iff_rgb8", probe_rgb8);
    reg.register_demuxer("iff_rgbn", open_rgbn);
    reg.register_muxer("iff_rgbn", open_rgbn_muxer);
    reg.register_extension("rgbn", "iff_rgbn");
    reg.register_probe("iff_rgbn", probe_rgbn);

    // Amiga Centre Scotland / TVPaint chunky deep-raster FORM. Decode covers
    // NOCOMPRESSION / RUNLENGTH pixel frames plus the §1.5b JPEG passthrough;
    // the muxer emits NOCOMPRESSION / RUNLENGTH multi-frame FORMs.
    reg.register_demuxer("iff_deep", open_deep);
    reg.register_muxer("iff_deep", open_deep_muxer);
    reg.register_extension("deep", "iff_deep");
    reg.register_probe("iff_deep", probe_deep);

    // TVPaint project FORM (best-effort, non-canonical; §2). Reuses the DEEP
    // raster vocabulary; each DBOD layer is surfaced as one keyframe and the
    // TVPP-specific MIXR/BGP1/BGP2 chunks are decoded raw (not via the demuxer).
    reg.register_demuxer("iff_tvpp", open_tvpp);
    reg.register_extension("tvpp", "iff_tvpp");
    reg.register_probe("iff_tvpp", probe_tvpp);
}

pub(super) fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" {
        match &p.buf[8..12] {
            b"ILBM" | b"PBM " => 100,
            _ => 0,
        }
    } else {
        0
    }
}

fn probe_acbm(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" && &p.buf[8..12] == b"ACBM" {
        100
    } else {
        0
    }
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
    // Grow-on-read (take + read_to_end) instead of pre-allocating the
    // declared FORM size, so a forged 32-bit size over a tiny stream
    // can't demand an attacker-sized buffer; a short read is rejected.
    let body_size = hdr.size as u64 - 4;
    let mut form_body = Vec::new();
    (&mut *input).take(body_size).read_to_end(&mut form_body)?;
    if (form_body.len() as u64) < body_size {
        return Err(Error::invalid(format!(
            "ILBM: FORM declares {} bytes but the stream ends after {}",
            body_size,
            form_body.len()
        )));
    }

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
        format: "iff_ilbm",
    }))
}

fn open_acbm(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let hdr = read_chunk_header(&mut *input)?.ok_or_else(|| Error::invalid("ACBM: empty file"))?;
    if hdr.id != GROUP_FORM {
        return Err(Error::invalid(format!(
            "ACBM: expected FORM chunk, got {}",
            hdr.id_str()
        )));
    }
    let form_type = read_form_type(&mut *input)?;
    if &form_type != b"ACBM" {
        return Err(Error::invalid(format!(
            "IFF: not an ACBM file (form type {:?})",
            std::str::from_utf8(&form_type).unwrap_or("????")
        )));
    }
    // Same grow-on-read allocation guard as the ILBM demuxer above.
    let body_size = hdr.size as u64 - 4;
    let mut form_body = Vec::new();
    (&mut *input).take(body_size).read_to_end(&mut form_body)?;
    if (form_body.len() as u64) < body_size {
        return Err(Error::invalid(format!(
            "ACBM: FORM declares {} bytes but the stream ends after {}",
            body_size,
            form_body.len()
        )));
    }

    let mut full = Vec::with_capacity(8 + 4 + form_body.len());
    full.extend_from_slice(b"FORM");
    full.extend_from_slice(&hdr.size.to_be_bytes());
    full.extend_from_slice(&form_type);
    full.extend_from_slice(&form_body);

    let image = parse_acbm(&full)?;
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
        format: "iff_acbm",
    }))
}

struct IlbmDemuxer {
    streams: Vec<StreamInfo>,
    image: Option<IlbmImage>,
    format: &'static str,
}

impl Demuxer for IlbmDemuxer {
    fn format_name(&self) -> &str {
        self.format
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
    /// Contiguous-bitplane `FORM/ACBM` (AmigaBASIC sibling of ILBM).
    /// Same indexed palette + plane-count derivation as
    /// [`MuxerMode::IndexedAuto`], but the body is the plane-contiguous,
    /// uncompressed `ABIT` chunk instead of the row-interleaved `BODY`
    /// (multimediawiki IFF §4.1). Compression is forced off (ABIT is a
    /// verbatim memory image).
    Acbm,
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
            MuxerMode::IndexedAuto | MuxerMode::Acbm => {
                let (pal, _) = build_palette(&self.pending);
                let np = if pal.len() <= 1 {
                    1
                } else {
                    let bits = (pal.len() as u32 - 1).next_power_of_two().trailing_zeros();
                    bits.max(1) as u8
                };
                let ft = if self.mode == MuxerMode::Acbm {
                    *b"ACBM"
                } else {
                    *b"ILBM"
                };
                (pal, np, Camg::default(), ft)
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

        // ABIT is always uncompressed; everything else honours the
        // muxer's compression choice.
        let compression = if self.mode == MuxerMode::Acbm {
            Compression::None
        } else {
            self.compression
        };
        let bmhd = Bmhd {
            width: self.width as u16,
            height: self.height as u16,
            x_origin: 0,
            y_origin: 0,
            n_planes,
            masking,
            compression,
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
        let bytes = if self.mode == MuxerMode::Acbm {
            encode_acbm(&img)?
        } else {
            encode_ilbm(&img)?
        };
        self.output.write_all(&bytes)?;
        self.output.flush()?;
        self.written = true;
        Ok(())
    }
}

fn probe_rgb8(p: &oxideav_core::ProbeData) -> u8 {
    probe_rgb_form(p.buf, b"RGB8")
}

fn probe_rgbn(p: &oxideav_core::ProbeData) -> u8 {
    probe_rgb_form(p.buf, b"RGBN")
}

fn probe_rgb_form(buf: &[u8], form_type: &[u8; 4]) -> u8 {
    if buf.len() >= 12 && &buf[0..4] == b"FORM" && &buf[8..12] == form_type {
        100
    } else {
        0
    }
}

/// Single-frame true-colour demuxer shared by `iff_rgb8` / `iff_rgbn`.
struct RgbTrueColorDemuxer {
    format_name: &'static str,
    streams: Vec<StreamInfo>,
    rgba: Option<Vec<u8>>,
}

impl Demuxer for RgbTrueColorDemuxer {
    fn format_name(&self) -> &str {
        self.format_name
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        let rgba = self.rgba.take().ok_or(Error::Eof)?;
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, rgba);
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

/// Read the whole outer `FORM` from `input` into a contiguous buffer that
/// `parse_rgb8` / `parse_rgbn` / `parse_deep` can walk verbatim.
fn read_true_color_form(
    input: &mut dyn ReadSeek,
    label: &str,
    expect: &[u8; 4],
) -> Result<Vec<u8>> {
    let hdr = read_chunk_header(&mut *input)?
        .ok_or_else(|| Error::invalid(format!("{label}: empty file")))?;
    if hdr.id != GROUP_FORM {
        return Err(Error::invalid(format!(
            "{label}: expected FORM chunk, got {}",
            hdr.id_str()
        )));
    }
    let form_type = read_form_type(&mut *input)?;
    if &form_type != expect {
        return Err(Error::invalid(format!(
            "{label}: not a {} file (form type {:?})",
            std::str::from_utf8(expect).unwrap_or("????"),
            std::str::from_utf8(&form_type).unwrap_or("????"),
        )));
    }
    let body_size = (hdr.size as u64)
        .checked_sub(4)
        .ok_or_else(|| Error::invalid(format!("{label}: FORM size shorter than form type")))?;
    // Allocation guard: bound the declared FORM size by what the stream can
    // actually supply before reserving the buffer, so a forged multi-gigabyte
    // size field fails as a truncation error instead of an attacker-sized
    // allocation.
    let here = input.stream_position()?;
    let end = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(here))?;
    if body_size > end.saturating_sub(here) {
        return Err(Error::invalid(format!(
            "{label}: FORM declares {body_size} body bytes but only {} remain",
            end.saturating_sub(here)
        )));
    }
    let mut form_body = vec![0u8; body_size as usize];
    input.read_exact(&mut form_body)?;
    let mut full = Vec::with_capacity(12 + form_body.len());
    full.extend_from_slice(b"FORM");
    full.extend_from_slice(&hdr.size.to_be_bytes());
    full.extend_from_slice(&form_type);
    full.extend_from_slice(&form_body);
    Ok(full)
}

fn true_color_stream(width: u16, height: u16) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(u32::from(width));
    params.height = Some(u32::from(height));
    params.pixel_format = Some(PixelFormat::Rgba);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    }
}

fn open_rgb8(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let full = read_true_color_form(&mut *input, "RGB8", b"RGB8")?;
    let image = parse_rgb8(&full, GenlockPolicy::default())?;
    Ok(Box::new(RgbTrueColorDemuxer {
        format_name: "iff_rgb8",
        streams: vec![true_color_stream(image.width, image.height)],
        rgba: Some(image.rgba),
    }))
}

fn open_rgbn(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let full = read_true_color_form(&mut *input, "RGBN", b"RGBN")?;
    let image = parse_rgbn(&full, GenlockPolicy::default())?;
    Ok(Box::new(RgbTrueColorDemuxer {
        format_name: "iff_rgbn",
        streams: vec![true_color_stream(image.width, image.height)],
        rgba: Some(image.rgba),
    }))
}

// ───────────────── true-colour FORMs — container-level muxers ─────────────────
//
// Muxer parity for the three true-colour FORMs whose function-level encoders
// already existed: `iff_deep` (multi-frame `FORM DEEP`, NOCOMPRESSION /
// RUNLENGTH with an auto picker, DCHG timing from the packet durations),
// and `iff_rgb8` / `iff_rgbn` (single-frame Turbo-Silver genlock-RLE FORMs).
// All accept a single `rawvideo` / `Rgba` video stream, mirroring the
// `IlbmMuxer` contract, and assemble the FORM at `write_trailer` time.

/// Body-compression choice for [`DeepMuxer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeepMuxerCompression {
    /// Try both NOCOMPRESSION and RUNLENGTH across all frames and emit
    /// whichever FORM is smaller (the DGBL compression method is global to
    /// the FORM, so the choice is made over the total body size).
    #[default]
    Auto,
    /// Raw chunky DBOD bodies (`DGBL.Compression = 0`).
    None,
    /// §1.5b whole-DBOD ByteRun1 bodies (`DGBL.Compression = 1`).
    RunLength,
}

/// Container-level `FORM DEEP` muxer (`iff_deep`). Accepts a single
/// `rawvideo` / `Rgba` video stream and one packet per DBOD frame (§1.4 —
/// several images in one FORM are successive cels).
///
/// * **DPEL** is derived from the pixels at `write_trailer`: RGB 8:8:8 when
///   every frame is fully opaque, RGBA 8:8:8:8 otherwise (§1.2 — the plain
///   24-bit layout is DEEP's equivalent of "RGB8").
/// * **Compression** follows [`DeepMuxerCompression`] (default `Auto`).
/// * **DCHG** (§1.6): with more than one frame, the first packet's
///   `duration` is converted through the stream `time_base` to the
///   millisecond FrameRate; packets without a duration produce no DCHG.
///
/// The emitted FORM round-trips through [`parse_deep_frames`] and the
/// `iff_deep` demuxer pixel-exactly.
pub struct DeepMuxer {
    output: Box<dyn WriteSeek>,
    width: u16,
    height: u16,
    time_base: TimeBase,
    compression: DeepMuxerCompression,
    frames: Vec<Vec<u8>>,
    first_duration: Option<i64>,
    written: bool,
}

impl DeepMuxer {
    pub fn new(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        let (width, height, time_base) = true_color_muxer_stream_shape("DEEP", streams)?;
        Ok(Self {
            output,
            width,
            height,
            time_base,
            compression: DeepMuxerCompression::default(),
            frames: Vec::new(),
            first_duration: None,
            written: false,
        })
    }

    /// Choose the DBOD body coding (default: [`DeepMuxerCompression::Auto`]).
    pub fn with_compression(mut self, c: DeepMuxerCompression) -> Self {
        self.compression = c;
        self
    }
}

/// Shared stream-shape validation for the true-colour muxers: exactly one
/// `rawvideo`-style video stream, `PixelFormat::Rgba`, with dimensions that
/// fit the 16-bit fields every IFF raster header uses.
pub(crate) fn true_color_muxer_stream_shape(
    label: &str,
    streams: &[StreamInfo],
) -> Result<(u16, u16, TimeBase)> {
    if streams.len() != 1 {
        return Err(Error::unsupported(format!(
            "{label} muxer supports exactly one video stream"
        )));
    }
    let s = &streams[0];
    if s.params.media_type != MediaType::Video {
        return Err(Error::invalid(format!("{label} stream must be video")));
    }
    if s.params.pixel_format != Some(PixelFormat::Rgba) {
        return Err(Error::unsupported(format!(
            "{label} muxer requires PixelFormat::Rgba"
        )));
    }
    let width = s
        .params
        .width
        .ok_or_else(|| Error::invalid(format!("{label} muxer: missing width")))?;
    let height = s
        .params
        .height
        .ok_or_else(|| Error::invalid(format!("{label} muxer: missing height")))?;
    let width = u16::try_from(width)
        .map_err(|_| Error::invalid(format!("{label} muxer: width {width} exceeds 65535")))?;
    let height = u16::try_from(height)
        .map_err(|_| Error::invalid(format!("{label} muxer: height {height} exceeds 65535")))?;
    Ok((width, height, s.time_base))
}

impl Muxer for DeepMuxer {
    fn format_name(&self) -> &str {
        "iff_deep"
    }
    fn write_header(&mut self) -> Result<()> {
        Ok(()) // the FORM is assembled at write_trailer time
    }
    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let expected = usize::from(self.width) * usize::from(self.height) * 4;
        if packet.data.len() != expected {
            return Err(Error::invalid(format!(
                "DEEP muxer: packet size {} does not match width*height*4 = {expected}",
                packet.data.len()
            )));
        }
        if self.frames.is_empty() {
            self.first_duration = packet.duration;
        }
        self.frames.push(packet.data.clone());
        Ok(())
    }
    fn write_trailer(&mut self) -> Result<()> {
        if self.written {
            return Ok(());
        }
        if self.frames.is_empty() {
            return Err(Error::invalid("DEEP muxer: no frames written"));
        }

        // §1.2: emit the minimal layout — plain 24-bit RGB when every pixel
        // is fully opaque, RGBA 8:8:8:8 when any frame carries alpha.
        let opaque = self
            .frames
            .iter()
            .all(|f| f.chunks_exact(4).all(|px| px[3] == 0xFF));
        let mut elements = vec![
            DpelElement {
                c_type: DeepCType::Red,
                c_bit_depth: 8,
            },
            DpelElement {
                c_type: DeepCType::Green,
                c_bit_depth: 8,
            },
            DpelElement {
                c_type: DeepCType::Blue,
                c_bit_depth: 8,
            },
        ];
        if !opaque {
            elements.push(DpelElement {
                c_type: DeepCType::Alpha,
                c_bit_depth: 8,
            });
        }
        let dpel = Dpel { elements };

        // §1.6: DCHG only means something for a multi-frame FORM; derive the
        // millisecond FrameRate from the first packet's duration.
        let dchg = if self.frames.len() > 1 {
            self.first_duration.and_then(|dur| {
                let num = self.time_base.num().max(0);
                let den = self.time_base.den().max(1);
                let millis = dur.saturating_mul(1000).saturating_mul(num) / den;
                i32::try_from(millis)
                    .ok()
                    .filter(|&ms| ms > 0)
                    .map(|ms| Dchg { frame_rate: ms })
            })
        } else {
            None
        };

        let frame_refs: Vec<&[u8]> = self.frames.iter().map(|f| f.as_slice()).collect();
        let encode = |compression: DeepCompression| {
            encode_deep_frames(
                &dpel,
                self.width,
                self.height,
                compression,
                dchg,
                &frame_refs,
            )
        };
        let form = match self.compression {
            DeepMuxerCompression::None => encode(DeepCompression::None)?,
            DeepMuxerCompression::RunLength => encode(DeepCompression::RunLength)?,
            DeepMuxerCompression::Auto => {
                let raw = encode(DeepCompression::None)?;
                let rle = encode(DeepCompression::RunLength)?;
                if rle.len() < raw.len() {
                    rle
                } else {
                    raw
                }
            }
        };
        self.output.write_all(&form)?;
        self.output.flush()?;
        self.written = true;
        Ok(())
    }
}

/// Container-level single-frame muxer for the Turbo-Silver `FORM RGB8` /
/// `FORM RGBN` genlock-RLE FORMs (`iff_rgb8` / `iff_rgbn`). Accepts one
/// `rawvideo` / `Rgba` packet and assembles the FORM via [`encode_rgb8`] /
/// [`encode_rgbn`] at `write_trailer` — alpha 0 drives the §3.3 genlock bit
/// (brush-transparency semantics), and RGBN quantises each gun to its top
/// nibble (§3.1, 4 bits per gun).
pub struct RgbTrueColorMuxer {
    format_name: &'static str,
    is_rgb8: bool,
    output: Box<dyn WriteSeek>,
    width: u16,
    height: u16,
    pending: Vec<u8>,
    written: bool,
}

impl RgbTrueColorMuxer {
    fn new(
        format_name: &'static str,
        is_rgb8: bool,
        output: Box<dyn WriteSeek>,
        streams: &[StreamInfo],
    ) -> Result<Self> {
        let label = if is_rgb8 { "RGB8" } else { "RGBN" };
        let (width, height, _tb) = true_color_muxer_stream_shape(label, streams)?;
        Ok(Self {
            format_name,
            is_rgb8,
            output,
            width,
            height,
            pending: Vec::new(),
            written: false,
        })
    }
}

impl Muxer for RgbTrueColorMuxer {
    fn format_name(&self) -> &str {
        self.format_name
    }
    fn write_header(&mut self) -> Result<()> {
        Ok(()) // the FORM is assembled at write_trailer time
    }
    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.pending.is_empty() {
            return Err(Error::unsupported(
                "RGB8/RGBN muxer: the FORM stores one image (single packet)",
            ));
        }
        let expected = usize::from(self.width) * usize::from(self.height) * 4;
        if packet.data.len() != expected {
            return Err(Error::invalid(format!(
                "RGB8/RGBN muxer: packet size {} does not match width*height*4 = {expected}",
                packet.data.len()
            )));
        }
        self.pending.extend_from_slice(&packet.data);
        Ok(())
    }
    fn write_trailer(&mut self) -> Result<()> {
        if self.written {
            return Ok(());
        }
        if self.pending.is_empty() {
            return Err(Error::invalid("RGB8/RGBN muxer: no frame written"));
        }
        let form = if self.is_rgb8 {
            encode_rgb8(self.width, self.height, &self.pending)?
        } else {
            encode_rgbn(self.width, self.height, &self.pending)?
        };
        self.output.write_all(&form)?;
        self.output.flush()?;
        self.written = true;
        Ok(())
    }
}

fn open_deep_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(DeepMuxer::new(output, streams)?))
}

fn open_rgb8_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(RgbTrueColorMuxer::new(
        "iff_rgb8", true, output, streams,
    )?))
}

fn open_rgbn_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(RgbTrueColorMuxer::new(
        "iff_rgbn", false, output, streams,
    )?))
}

// ───────────────── FORM DEEP — container registry wiring ─────────────────
//
// `iff_deep` demuxer: a `FORM DEEP` chunky deep-raster file decodes through
// the standard `ContainerRegistry::open_*` path, surfacing **one** keyframe per
// DBOD frame (§1.4) — a still DEEP is one packet, a cel-anim DEEP plays every
// DBOD with per-frame PTS from the DCHG timing (§1.6). NOCOMPRESSION + §1.5b
// RUNLENGTH bodies are pixel-decoded to `rawvideo` / `Rgba` packets; a §1.5b
// JPEG FORM is passed through as `"mjpeg"` packets (one validated JFIF stream
// per DBOD) for a downstream JPEG decoder. TVDC (no in-FORM delta table — the
// §1.5 gap) and HUFFMAN / DYNAMICHUFF return the same `Error::invalid`
// `parse_deep_frames` raises. Source: iff-truecolor-chunks.md §1.

fn probe_deep(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" && &p.buf[8..12] == b"DEEP" {
        100
    } else {
        0
    }
}

/// `FORM DEEP` demuxer. Emits one keyframe per DBOD frame (§1.4): a still
/// DEEP is a one-packet stream, a cel-anim DEEP plays every frame in document
/// order with per-frame PTS derived from the DCHG timing (§1.6). Packets are
/// decoded `rawvideo` / `Rgba` frames for the pixel-decodable codings, or
/// verbatim JFIF streams under codec id `"mjpeg"` for a §1.5b JPEG FORM.
struct DeepDemuxer {
    streams: Vec<StreamInfo>,
    /// Per-frame packet payloads in document order, drained front-to-back
    /// (decoded RGBA, or verbatim JFIF for the JPEG passthrough path).
    frames: std::collections::VecDeque<Vec<u8>>,
    /// Next frame's presentation timestamp, in `time_base` units.
    next_pts: i64,
    /// Per-frame duration in `time_base` units (1 if no DCHG delay applies).
    frame_duration: i64,
    /// Total stream duration in microseconds, if a DCHG delay is known.
    duration_us: Option<i64>,
}

impl Demuxer for DeepDemuxer {
    fn format_name(&self) -> &str {
        "iff_deep"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        let rgba = self.frames.pop_front().ok_or(Error::Eof)?;
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, rgba);
        pkt.pts = Some(self.next_pts);
        pkt.dts = Some(self.next_pts);
        pkt.duration = Some(self.frame_duration);
        pkt.flags.keyframe = true;
        self.next_pts += self.frame_duration;
        Ok(pkt)
    }
    fn metadata(&self) -> &[(String, String)] {
        &[]
    }
    fn duration_micros(&self) -> Option<i64> {
        self.duration_us
    }
}

fn probe_tvpp(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" && &p.buf[8..12] == b"TVPP" {
        100
    } else {
        0
    }
}

/// `FORM TVPP` demuxer (best-effort, §2). Surfaces every decoded DBOD layer as
/// a `rawvideo` / `Rgba` keyframe in document order. The TVPP-specific
/// MIXR/BGP1/BGP2 chunks are not exposed through the packet stream; a caller
/// that wants them uses [`parse_tvpp`] directly.
fn open_tvpp(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let full = read_true_color_form(&mut *input, "TVPP", b"TVPP")?;
    let img = parse_tvpp(&full)?;
    let width = img.layers[0].width;
    let height = img.layers[0].height;
    let frame_count = img.layers.len() as i64;

    let (time_base, frame_duration, duration_us) =
        deep_stream_timing(img.dchg.and_then(|d| d.delay_millis()), frame_count);

    let mut stream = true_color_stream(width, height);
    stream.time_base = time_base;
    stream.duration = Some(frame_duration.saturating_mul(frame_count));

    let frames: std::collections::VecDeque<Vec<u8>> =
        img.layers.into_iter().map(|f| f.rgba).collect();

    Ok(Box::new(DeepDemuxer {
        streams: vec![stream],
        frames,
        next_pts: 0,
        frame_duration,
        duration_us,
    }))
}

/// §1.6 DCHG FrameRate is a millisecond delay; build a 1/1000-s time base so
/// each frame's duration is exactly the DCHG value. With no usable delay
/// (still image, or a `0`/`-1` sentinel) fall back to the unit time base.
/// Returns `(time_base, frame_duration, duration_us)`.
fn deep_stream_timing(delay_millis: Option<u32>, frame_count: i64) -> (TimeBase, i64, Option<i64>) {
    match delay_millis {
        Some(ms) => {
            let dur = i64::from(ms);
            let total_us = dur.saturating_mul(frame_count).saturating_mul(1_000);
            (TimeBase::new(1, 1000), dur, Some(total_us))
        }
        None => (TimeBase::new(1, 1), 1, None),
    }
}

/// Locate the first DGBL chunk in an in-memory `FORM DEEP` and return its
/// declared compression method, so the demuxer can pick the raw-RGBA or the
/// JPEG-passthrough packet path before committing to a full decode. Errors
/// (missing/short DGBL, unknown code) are left for the full parse to report.
fn deep_form_compression(bytes: &[u8]) -> Option<DeepCompression> {
    if bytes.len() < 12 {
        return None;
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8 + total).min(bytes.len());
    let mut cursor = 12usize;
    while cursor + 8 <= body_end {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_be_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let payload_start = cursor + 8;
        let payload_end = payload_start.checked_add(size)?;
        if payload_end > body_end {
            return None;
        }
        if id == b"DGBL" {
            return Dgbl::parse(&bytes[payload_start..payload_end])
                .ok()
                .map(|d| d.compression);
        }
        cursor = payload_start + size + (size & 1);
    }
    None
}

/// Stream shape for the JPEG-in-DEEP passthrough path (§1.5b): the packets
/// carry complete JFIF streams, so the codec id is `"mjpeg"` (the
/// self-contained-JFIF-per-frame codec) and no raw pixel format is declared —
/// the JPEG header is authoritative for the decoded geometry; the DGBL/DLOC
/// dimensions are advertised as the container's declared size.
fn deep_jpeg_stream(width: u16, height: u16) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("mjpeg"));
    params.media_type = MediaType::Video;
    params.width = Some(u32::from(width));
    params.height = Some(u32::from(height));
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    }
}

fn open_deep(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let full = read_true_color_form(&mut *input, "DEEP", b"DEEP")?;

    // §1.5b JPEG bodies are surfaced whole as `"mjpeg"` packets for a
    // downstream JPEG decoder instead of being pixel-decoded here.
    if deep_form_compression(&full) == Some(DeepCompression::Jpeg) {
        let movie = extract_deep_jpeg_frames(&full)?;
        let width = movie.frames[0].width;
        let height = movie.frames[0].height;
        let frame_count = movie.frames.len() as i64;
        let delay = movie.dchg.and_then(|d| d.delay_millis());
        let (time_base, frame_duration, duration_us) = deep_stream_timing(delay, frame_count);

        let mut stream = deep_jpeg_stream(width, height);
        stream.time_base = time_base;
        stream.duration = Some(frame_duration.saturating_mul(frame_count));

        let frames: std::collections::VecDeque<Vec<u8>> =
            movie.frames.into_iter().map(|f| f.jfif).collect();

        return Ok(Box::new(DeepDemuxer {
            streams: vec![stream],
            frames,
            next_pts: 0,
            frame_duration,
            duration_us,
        }));
    }

    let movie = parse_deep_frames(&full)?;
    let width = movie.frames[0].width;
    let height = movie.frames[0].height;
    let frame_count = movie.frames.len() as i64;

    let (time_base, frame_duration, duration_us) =
        deep_stream_timing(movie.frame_delay_millis(), frame_count);

    let mut stream = true_color_stream(width, height);
    stream.time_base = time_base;
    stream.duration = Some(frame_duration.saturating_mul(frame_count));

    let frames: std::collections::VecDeque<Vec<u8>> =
        movie.frames.into_iter().map(|f| f.rgba).collect();

    Ok(Box::new(DeepDemuxer {
        streams: vec![stream],
        frames,
        next_pts: 0,
        frame_duration,
        duration_us,
    }))
}
