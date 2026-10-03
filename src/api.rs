//! The `IMAGE_CRATE_API` root functions for the IFF raster FORMs:
//! [`probe`], [`info`], [`decode`] / [`decode_with`] / [`decode_rgb8`] /
//! [`decode_rgba8`] / [`decode_all`] / [`decode_from`], [`encode`] /
//! [`encode_rgb8`] / [`encode_rgba8`] / [`encode_to`] / [`encode_all`].
//!
//! Every function is framework-free and wraps the document-model
//! parsers and encoders in [`crate::ilbm`] and [`crate::anim`]
//! (`parse_ilbm` / `parse_acbm` / `parse_deep` / `parse_rgb8` /
//! `parse_rgbn` / `parse_anim`, `encode_ilbm` / `encode_acbm` /
//! `encode_deep` / `encode_rgb8` / `encode_rgbn` /
//! `encode_anim_op*_timed`); the `registry` adapters call these same
//! functions, so there is one implementation.
//!
//! Formats covered: `FORM ILBM` (planar indexed 1–8 planes, EHB, HAM6 /
//! HAM8, 24-bit literal RGB, `SHAM` / `PCHG` per-line palettes, every
//! `BMHD` masking mode), `FORM PBM ` (chunky), `FORM ACBM` (contiguous
//! bitplanes), `FORM DEEP` (chunky deep raster, NOCOMPRESSION /
//! RUNLENGTH / caller-tabled TVDC), `FORM RGB8` / `FORM RGBN` (Turbo
//! Silver genlock RLE), `FORM ANIM` (delta animation, ops 0–8) and
//! `CAT ` / `LIST` groups of those.

use std::io::{Read, Write};
use std::time::Duration;

use crate::anim::{
    encode_anim_op0_timed, encode_anim_op1_timed, encode_anim_op2_timed, encode_anim_op3_timed,
    encode_anim_op4_timed, encode_anim_op5_timed, encode_anim_op7_timed, encode_anim_op8_timed,
    parse_anim_indexed, FrameTiming,
};
use crate::chunk::{parse_group_children, probe_top_level_group, GroupChild, GroupKind};
use crate::error::{IffError, Result};
use crate::ilbm::{
    assemble_acbm_form, assemble_ilbm_form, build_palette, build_palette_capped, encode_acbm,
    encode_deep_frames, encode_ilbm, encode_rgb8 as encode_rgb8_form,
    encode_rgbn as encode_rgbn_form, expand_ehb_palette, pack_body_resolving, parse_acbm_indexed,
    parse_deep_frames, parse_deep_frames_with_tvdc_table, parse_ilbm_indexed, parse_rgb8,
    parse_rgbn, push_planar_row, Bmhd, Camg, Compression, Dchg, DeepCType, DeepCompression, Dpel,
    DpelElement, GenlockPolicy, IlbmImage, IndexedView, InferredFormat, Masking, Pchg,
};
use crate::image::{
    Frame, IffForm, IffImage, ImageInfo, Palette, PixelFormat, Plane, RgbImage, RgbaImage,
};
use crate::options::{AnimOp, DecodeOptions, EncodeOptions};

// ───────────────────────── probe ─────────────────────────

/// `true` when `bytes` start like an IFF raster picture: a `FORM` whose
/// type is `ILBM`, `PBM `, `ACBM`, `DEEP`, `RGB8`, `RGBN` or `ANIM`,
/// or a `CAT ` / `LIST` whose contents type is one of those raster
/// FORMs. Twelve bytes are inspected; nothing is allocated and short
/// input is `false`. Audio FORMs (`8SVX`, `AIFF`) are not pictures and
/// probe `false` here (their container probes live in the registry).
pub fn probe(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    let group = &bytes[0..4];
    let ft: [u8; 4] = [bytes[8], bytes[9], bytes[10], bytes[11]];
    match group {
        b"FORM" => IffForm::from_form_type(&ft).is_some() || &ft == b"ANIM",
        b"CAT " | b"LIST" => IffForm::from_form_type(&ft).is_some() || &ft == b"ANIM",
        _ => false,
    }
}

// ───────────────────────── header walk ─────────────────────────

/// The property chunks of a `FORM ILBM` / `PBM ` / `ACBM`, read without
/// touching the pixel body.
struct RasterHeader {
    form: IffForm,
    bmhd: Bmhd,
    cmap_len: usize,
    camg: Option<Camg>,
    has_sham: bool,
    has_pchg: bool,
}

/// The resolved reading of a planar header: the layout [`decode`]
/// returns and the flags [`info`] reports.
struct Layout {
    format: PixelFormat,
    has_alpha: bool,
    ham: bool,
    ehb: bool,
}

/// The `(id, payload)` children of a `FORM`, bounded by its declared
/// size (clamped to the buffer, as the lenient parsers do). A child that
/// runs past the FORM is an error, exactly as in the pixel parsers.
fn chunk_walk<'a>(bytes: &'a [u8], label: &str) -> Result<Vec<([u8; 4], &'a [u8])>> {
    if bytes.len() < 12 {
        return Err(IffError::invalid(format!(
            "{label}: file shorter than FORM header"
        )));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = total.saturating_add(8).min(bytes.len());
    let mut cursor = 12usize;
    let mut out = Vec::new();
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
        let start = cursor + 8;
        let end = start.saturating_add(size);
        if end > body_end {
            return Err(IffError::invalid(format!(
                "{label}: chunk {:?} extends past FORM",
                std::str::from_utf8(&id).unwrap_or("????")
            )));
        }
        out.push((id, &bytes[start..end]));
        cursor = start + size + (size & 1);
    }
    Ok(out)
}

fn raster_header(bytes: &[u8], form: IffForm) -> Result<RasterHeader> {
    let label = match form {
        IffForm::Pbm => "PBM",
        IffForm::Acbm => "ACBM",
        _ => "ILBM",
    };
    let mut bmhd = None;
    let mut cmap_len = 0usize;
    let mut camg = None;
    let mut has_sham = false;
    let mut has_pchg = false;
    for (id, payload) in chunk_walk(bytes, label)? {
        match &id {
            b"BMHD" => bmhd = Some(Bmhd::parse(payload)?),
            b"CMAP" => cmap_len = payload.len() / 3,
            b"CAMG" => camg = Some(Camg::parse(payload)?),
            b"SHAM" => has_sham = true,
            // The reader policy ignores an unvalidatable PCHG; mirror it.
            b"PCHG" => has_pchg = Pchg::parse_reader(payload)?.is_some(),
            _ => {}
        }
    }
    let bmhd = bmhd.ok_or_else(|| IffError::invalid(format!("{label}: missing BMHD chunk")))?;
    Ok(RasterHeader {
        form,
        bmhd,
        cmap_len,
        camg,
        has_sham,
        has_pchg,
    })
}

/// The same decision the renderers make, from the header alone.
fn planar_layout(h: &RasterHeader) -> Layout {
    let bmhd = &h.bmhd;
    let camg = h.camg.unwrap_or_default();
    if h.form == IffForm::Pbm {
        // Chunky: single palette, EHB by the raw flag, HAM rejected at
        // decode; the only per-pixel alpha is the lasso fill.
        let lasso = bmhd.masking == Masking::Lasso;
        let key = bmhd.masking == Masking::HasTransparentColor;
        return Layout {
            format: if lasso {
                PixelFormat::Rgba
            } else {
                PixelFormat::Pal8
            },
            has_alpha: lasso || key,
            ham: camg.is_ham(),
            ehb: camg.is_ehb(),
        };
    }
    if bmhd.n_planes == 24 {
        return Layout {
            format: PixelFormat::Rgb24,
            has_alpha: false,
            ham: false,
            ehb: false,
        };
    }
    let resolution = camg.resolve_planar_format(bmhd.n_planes, h.cmap_len);
    let ham = resolution.format == InferredFormat::Ham;
    let ehb = resolution.format == InferredFormat::Ehb;
    let has_mask = bmhd.masking == Masking::HasMask;
    let lasso = bmhd.masking == Masking::Lasso && !ham && !has_mask;
    let key = bmhd.masking == Masking::HasTransparentColor && !ham;
    let indexed = !ham && !h.has_sham && !h.has_pchg && !has_mask && !lasso;
    let has_alpha = has_mask || lasso || key;
    let format = if indexed {
        PixelFormat::Pal8
    } else if has_alpha {
        PixelFormat::Rgba
    } else {
        PixelFormat::Rgb24
    };
    Layout {
        format,
        has_alpha,
        ham,
        ehb,
    }
}

/// Header-only facts about a `FORM DEEP`.
struct DeepHeader {
    width: u16,
    height: u16,
    compression: u8,
    dpel: Dpel,
    frames: u32,
}

fn deep_header(bytes: &[u8]) -> Result<DeepHeader> {
    let mut dgbl = None;
    let mut dpel = None;
    let mut frames = 0u32;
    let mut first_dloc = None;
    let mut pending_dloc = None;
    for (id, payload) in chunk_walk(bytes, "DEEP")? {
        match &id {
            b"DGBL" => dgbl = Some(crate::ilbm::Dgbl::parse(payload)?),
            b"DPEL" => dpel = Some(Dpel::parse(payload)?),
            b"DLOC" => pending_dloc = Some(crate::ilbm::Dloc::parse(payload)?),
            b"DBOD" => {
                if frames == 0 {
                    first_dloc = pending_dloc.take();
                }
                frames = frames.saturating_add(1);
            }
            _ => {}
        }
    }
    let dgbl = dgbl.ok_or_else(|| IffError::invalid("DEEP: missing DGBL chunk"))?;
    let dpel = dpel.ok_or_else(|| IffError::invalid("DEEP: missing DPEL chunk"))?;
    let (width, height) = match first_dloc {
        Some(dl) => (dl.w, dl.h),
        None => (dgbl.display_width, dgbl.display_height),
    };
    Ok(DeepHeader {
        width,
        height,
        compression: dgbl.compression.to_u16() as u8,
        dpel,
        frames,
    })
}

/// The raw `BMHD` fields of a `FORM RGB8` / `FORM RGBN` (their
/// compression byte, 4, is outside the ILBM [`Compression`] set, so the
/// typed parser does not apply): `(width, height, n_planes, masking,
/// compression)`.
struct RawBmhd {
    width: u16,
    height: u16,
    n_planes: u8,
    masking: u8,
    compression: u8,
}

/// Header-only facts about a `FORM RGB8` / `FORM RGBN`.
fn truecolor_header(bytes: &[u8], form: IffForm) -> Result<(RawBmhd, Option<Camg>)> {
    let label = if form == IffForm::Rgb8 {
        "RGB8"
    } else {
        "RGBN"
    };
    let mut bmhd = None;
    let mut camg = None;
    for (id, payload) in chunk_walk(bytes, label)? {
        match &id {
            b"BMHD" => {
                if payload.len() < 20 {
                    return Err(IffError::invalid(format!(
                        "{label} BMHD: need 20 bytes, got {}",
                        payload.len()
                    )));
                }
                bmhd = Some(RawBmhd {
                    width: u16::from_be_bytes([payload[0], payload[1]]),
                    height: u16::from_be_bytes([payload[2], payload[3]]),
                    n_planes: payload[8],
                    masking: payload[9],
                    compression: payload[10],
                });
            }
            b"CAMG" => camg = Some(Camg::parse(payload)?),
            _ => {}
        }
    }
    let bmhd = bmhd.ok_or_else(|| IffError::invalid(format!("{label}: missing BMHD chunk")))?;
    Ok((bmhd, camg))
}

/// The nested `FORM`s of a `FORM ANIM`, re-wrapped as self-contained
/// `FORM ILBM` byte strings (header included) so the ILBM walkers can
/// read them.
fn anim_children(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for (id, payload) in chunk_walk(bytes, "ANIM")? {
        if &id == b"FORM" && payload.len() >= 4 && &payload[0..4] == b"ILBM" {
            out.push(rewrap_form(payload));
        }
    }
    Ok(out)
}

/// `FORM` + size + `payload` (payload starts with the 4-byte form type).
fn rewrap_form(payload: &[u8]) -> Vec<u8> {
    let mut full = Vec::with_capacity(8 + payload.len());
    full.extend_from_slice(b"FORM");
    full.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    full.extend_from_slice(payload);
    full
}

/// The raster `FORM` children of a top-level `CAT ` / `LIST`, re-wrapped
/// with their headers. `PROP` sets and non-raster children are skipped.
fn group_children(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let group = probe_top_level_group(bytes)?
        .ok_or_else(|| IffError::invalid("IFF: not a FORM / CAT / LIST"))?;
    let end = (group.size as usize).saturating_add(8).min(bytes.len());
    if end < 12 {
        return Err(IffError::invalid("IFF: group shorter than its header"));
    }
    let children = parse_group_children(group.kind, &bytes[12..end])?;
    let mut out = Vec::new();
    for child in children {
        if let GroupChild::Group {
            kind: GroupKind::Form,
            inner_type,
            body,
        } = child
        {
            if IffForm::from_form_type(&inner_type).is_some() || &inner_type == b"ANIM" {
                let mut full = Vec::with_capacity(12 + body.len());
                full.extend_from_slice(b"FORM");
                full.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
                full.extend_from_slice(&inner_type);
                full.extend_from_slice(body);
                out.push(full);
            }
        }
    }
    Ok(out)
}

enum TopLevel {
    Raster(IffForm),
    Anim,
    Group,
}

fn classify(bytes: &[u8]) -> Result<TopLevel> {
    if bytes.len() < 12 {
        return Err(IffError::invalid("IFF: file shorter than a FORM header"));
    }
    let ft: [u8; 4] = [bytes[8], bytes[9], bytes[10], bytes[11]];
    match &bytes[0..4] {
        b"FORM" => {
            if &ft == b"ANIM" {
                return Ok(TopLevel::Anim);
            }
            IffForm::from_form_type(&ft)
                .map(TopLevel::Raster)
                .ok_or_else(|| {
                    IffError::unsupported(format!(
                        "IFF: FORM {:?} is not a raster picture",
                        std::str::from_utf8(&ft).unwrap_or("????")
                    ))
                })
        }
        b"CAT " | b"LIST" => Ok(TopLevel::Group),
        _ => Err(IffError::invalid(
            "IFF: missing FORM / CAT / LIST signature",
        )),
    }
}

/// `strict`: the declared `FORM` size must fit the buffer and nothing
/// may follow it.
fn check_strict(bytes: &[u8], opts: &DecodeOptions) -> Result<()> {
    if !opts.strict || bytes.len() < 8 {
        return Ok(());
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as u64;
    let declared = total + 8 + (total & 1);
    let len = bytes.len() as u64;
    if total + 8 > len {
        return Err(IffError::invalid(format!(
            "IFF: FORM declares {} bytes but the file holds {len} (strict)",
            total + 8
        )));
    }
    if len > declared {
        return Err(IffError::invalid(format!(
            "IFF: {} trailing bytes after the FORM (strict)",
            len - declared
        )));
    }
    Ok(())
}

// ───────────────────────── info ─────────────────────────

/// Read the property chunks (`BMHD` / `CMAP` / `CAMG` / `SHAM` / `PCHG`
/// presence, or `DGBL` / `DPEL`) without decoding a pixel. For a `FORM
/// ANIM` the seed frame is described and `frames` counts the nested
/// `FORM ILBM`s; for a `CAT ` / `LIST` the first raster child is
/// described and `frames` counts the raster children.
pub fn info(bytes: &[u8]) -> Result<ImageInfo> {
    match classify(bytes)? {
        TopLevel::Raster(form) => raster_info(bytes, form, 1),
        TopLevel::Anim => {
            let children = anim_children(bytes)?;
            let seed = children
                .first()
                .ok_or_else(|| IffError::invalid("ANIM: no ILBM frames"))?;
            raster_info(seed, IffForm::Ilbm, children.len() as u32)
        }
        TopLevel::Group => {
            let children = group_children(bytes)?;
            let first = children
                .first()
                .ok_or_else(|| IffError::unsupported("IFF: group holds no raster FORM"))?;
            let mut i = info(first)?;
            i.frames = children.len() as u32;
            Ok(i)
        }
    }
}

fn raster_info(bytes: &[u8], form: IffForm, frames: u32) -> Result<ImageInfo> {
    match form {
        IffForm::Ilbm | IffForm::Pbm | IffForm::Acbm => {
            let h = raster_header(bytes, form)?;
            let layout = planar_layout(&h);
            let mut i = ImageInfo::new(
                u32::from(h.bmhd.width),
                u32::from(h.bmhd.height),
                layout.format,
                frames,
                layout.has_alpha,
                form,
            );
            i.n_planes = h.bmhd.n_planes;
            i.compression = h.bmhd.compression.to_byte();
            i.masking = h.bmhd.masking.to_byte();
            i.viewmode = h.camg.map(|c| c.raw);
            i.palette_len = h.cmap_len as u32;
            i.ham = layout.ham;
            i.ehb = layout.ehb;
            Ok(i)
        }
        IffForm::Deep => {
            let h = deep_header(bytes)?;
            let alpha = h.dpel.has_alpha();
            let mut i = ImageInfo::new(
                u32::from(h.width),
                u32::from(h.height),
                if alpha {
                    PixelFormat::Rgba
                } else {
                    PixelFormat::Rgb24
                },
                h.frames.max(1),
                alpha,
                form,
            );
            i.n_planes = h.dpel.total_bits().min(255) as u8;
            i.compression = h.compression;
            Ok(i)
        }
        IffForm::Rgb8 | IffForm::Rgbn => {
            let (bmhd, camg) = truecolor_header(bytes, form)?;
            // The default genlock policy keeps every coded colour, so the
            // picture is opaque RGB; `BrushTransparency` on
            // `DecodeOptions` turns the genlock bit into alpha 0 and the
            // decode returns `Rgba`.
            let mut i = ImageInfo::new(
                u32::from(bmhd.width),
                u32::from(bmhd.height),
                PixelFormat::Rgb24,
                frames,
                false,
                form,
            );
            i.n_planes = bmhd.n_planes;
            i.compression = bmhd.compression;
            i.masking = bmhd.masking;
            i.viewmode = camg.map(|c| c.raw);
            Ok(i)
        }
    }
}

// ───────────────────────── decode ─────────────────────────

/// Decode the primary picture (the seed frame of an `ANIM`, the first
/// `DBOD` of a `DEEP`, the first raster child of a group) in its native
/// layout with [`DecodeOptions::default`].
pub fn decode(bytes: &[u8]) -> Result<IffImage> {
    decode_with(bytes, &DecodeOptions::default())
}

/// [`decode`] with explicit limits, strictness, genlock policy and TVDC
/// table. Limits are checked against the header before the pixel
/// buffers are allocated.
pub fn decode_with(bytes: &[u8], opts: &DecodeOptions) -> Result<IffImage> {
    check_strict(bytes, opts)?;
    match classify(bytes)? {
        TopLevel::Raster(form) => decode_raster(bytes, form, opts),
        TopLevel::Anim => {
            let children = anim_children(bytes)?;
            let seed = children
                .first()
                .ok_or_else(|| IffError::invalid("ANIM: no ILBM frames"))?;
            decode_raster(seed, IffForm::Ilbm, opts)
        }
        TopLevel::Group => {
            let children = group_children(bytes)?;
            let first = children
                .first()
                .ok_or_else(|| IffError::unsupported("IFF: group holds no raster FORM"))?;
            decode_with(first, opts)
        }
    }
}

/// Reject before allocating: the renderers build a `width × height × 4`
/// RGBA working buffer, which must fit `usize` and the configured limits.
fn check_limits(width: u32, height: u32, opts: &DecodeOptions) -> Result<()> {
    let bytes = u64::from(width) * u64::from(height) * 4;
    opts.check(width, height, bytes)?;
    if usize::try_from(bytes).is_err() {
        return Err(IffError::unsupported(format!(
            "IFF: {width}x{height} RGBA does not fit this platform's address space"
        )));
    }
    Ok(())
}

fn decode_raster(bytes: &[u8], form: IffForm, opts: &DecodeOptions) -> Result<IffImage> {
    match form {
        IffForm::Ilbm | IffForm::Pbm | IffForm::Acbm => {
            let h = raster_header(bytes, form)?;
            check_limits(u32::from(h.bmhd.width), u32::from(h.bmhd.height), opts)?;
            let layout = planar_layout(&h);
            let (img, indexed) = if form == IffForm::Acbm {
                parse_acbm_indexed(bytes)?
            } else {
                parse_ilbm_indexed(bytes)?
            };
            Ok(ilbm_to_image(img, indexed, form, &layout))
        }
        IffForm::Deep => {
            let h = deep_header(bytes)?;
            check_limits(u32::from(h.width), u32::from(h.height), opts)?;
            let movie = match &opts.tvdc_table {
                Some(t) => parse_deep_frames_with_tvdc_table(bytes, t)?,
                None => parse_deep_frames(bytes)?,
            };
            let first = movie
                .frames
                .into_iter()
                .next()
                .ok_or_else(|| IffError::invalid("DEEP: missing DBOD chunk"))?;
            Ok(deep_frame_to_image(
                u32::from(first.width),
                u32::from(first.height),
                first.rgba,
                &movie.dpel,
            ))
        }
        IffForm::Rgb8 | IffForm::Rgbn => {
            let (bmhd, camg) = truecolor_header(bytes, form)?;
            check_limits(u32::from(bmhd.width), u32::from(bmhd.height), opts)?;
            let tc = if form == IffForm::Rgb8 {
                parse_rgb8(bytes, opts.genlock)?
            } else {
                parse_rgbn(bytes, opts.genlock)?
            };
            let (w, h) = (u32::from(tc.width), u32::from(tc.height));
            let alpha = opts.genlock == GenlockPolicy::BrushTransparency;
            let mut img = rgba_to_image(w, h, tc.rgba, alpha);
            img.form = form;
            img.n_planes = bmhd.n_planes;
            img.viewmode = camg.map(|c| c.raw);
            Ok(img)
        }
    }
}

/// Pack an RGBA buffer as `Rgba` (keep alpha) or `Rgb24` (strip it).
fn rgba_to_image(width: u32, height: u32, rgba: Vec<u8>, keep_alpha: bool) -> IffImage {
    if keep_alpha {
        IffImage::unchecked(
            width,
            height,
            PixelFormat::Rgba,
            vec![Plane::new(width as usize * 4, rgba)],
        )
    } else {
        let rgb: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        IffImage::unchecked(
            width,
            height,
            PixelFormat::Rgb24,
            vec![Plane::new(width as usize * 3, rgb)],
        )
    }
}

fn deep_frame_to_image(width: u32, height: u32, rgba: Vec<u8>, dpel: &Dpel) -> IffImage {
    let mut img = rgba_to_image(width, height, rgba, dpel.has_alpha());
    img.form = IffForm::Deep;
    img.n_planes = dpel.total_bits().min(255) as u8;
    img
}

/// Build the contract image from a decoded ILBM document and its
/// indexed view: `Pal8` when the view exists, otherwise `Rgba` / `Rgb24`
/// by the header's alpha rule.
fn ilbm_to_image(img: IlbmImage, indexed: IndexedView, form: IffForm, layout: &Layout) -> IffImage {
    let (w, h) = (img.width, img.height);
    let mut out = match indexed {
        Some((indices, palette)) => {
            let mut entries: Vec<[u8; 4]> =
                palette.iter().map(|c| [c[0], c[1], c[2], 255]).collect();
            // Cover every index the picture uses (and the transparent
            // key) so `to_rgba8` reproduces the renderer's opaque-black
            // out-of-range rule and `validate` holds.
            let max_used = indices.iter().copied().max().map(usize::from).unwrap_or(0);
            let mut need = max_used + 1;
            if img.bmhd.masking == Masking::HasTransparentColor {
                need = need.max(usize::from(img.bmhd.transparent_color) + 1);
            }
            if entries.len() < need.min(256) {
                entries.resize(need.min(256), [0, 0, 0, 255]);
            }
            if img.bmhd.masking == Masking::HasTransparentColor {
                if let Some(e) = entries.get_mut(usize::from(img.bmhd.transparent_color)) {
                    e[3] = 0;
                }
            }
            IffImage::unchecked(
                w,
                h,
                PixelFormat::Pal8,
                vec![Plane::new(w as usize, indices)],
            )
            .with_palette(Palette::new(entries))
        }
        None => rgba_to_image(w, h, img.rgba, layout.has_alpha),
    };
    out.form = form;
    out.n_planes = img.bmhd.n_planes;
    out.viewmode = (img.camg.raw != 0).then_some(img.camg.raw);
    out.aspect = (img.bmhd.x_aspect != 0 || img.bmhd.y_aspect != 0)
        .then_some((img.bmhd.x_aspect, img.bmhd.y_aspect));
    out
}

/// [`decode`] then [`IffImage::to_rgb8`].
pub fn decode_rgb8(bytes: &[u8]) -> Result<RgbImage> {
    let img = decode(bytes)?;
    Ok(RgbImage::new(img.width, img.height, img.to_rgb8()))
}

/// [`decode`] then [`IffImage::to_rgba8`].
pub fn decode_rgba8(bytes: &[u8]) -> Result<RgbaImage> {
    let img = decode(bytes)?;
    Ok(RgbaImage::new(img.width, img.height, img.to_rgba8()))
}

/// Read `r` to its end and [`decode`] the bytes.
pub fn decode_from<R: Read>(mut r: R) -> Result<IffImage> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    decode(&buf)
}

/// Every picture in the file, with [`DecodeOptions::default`]:
///
/// * `FORM ANIM` — one frame per delta-decoded frame (`Pal8` when the
///   animation is a plain indexed one, else `Rgba` / `Rgb24` like
///   [`decode`]); `delay` is how long the frame stays up (the next
///   frame's `ANHD.reltime` in 1/60 s jiffies, the last frame holding its
///   own `reltime`, at least one jiffy), as
///   [`crate::anim::AnimImage::playback`] computes it.
/// * `FORM DEEP` — one frame per `DBOD`, each at its own `DLOC` size,
///   `delay` from the `DCHG` frame rate when it is a literal delay.
/// * `CAT ` / `LIST` — one frame per raster `FORM` child; children that
///   fail to decode are skipped (`strict` makes that an error).
/// * Any other FORM — the single picture.
pub fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>> {
    decode_all_with(bytes, &DecodeOptions::default())
}

/// [`decode_all`] with explicit options.
pub fn decode_all_with(bytes: &[u8], opts: &DecodeOptions) -> Result<Vec<Frame>> {
    check_strict(bytes, opts)?;
    match classify(bytes)? {
        TopLevel::Raster(IffForm::Deep) => {
            let h = deep_header(bytes)?;
            check_limits(u32::from(h.width), u32::from(h.height), opts)?;
            let movie = match &opts.tvdc_table {
                Some(t) => parse_deep_frames_with_tvdc_table(bytes, t)?,
                None => parse_deep_frames(bytes)?,
            };
            let delay = movie
                .dchg
                .as_ref()
                .and_then(Dchg::delay_millis)
                .map(|ms| Duration::from_millis(u64::from(ms)));
            let dpel = movie.dpel;
            Ok(movie
                .frames
                .into_iter()
                .enumerate()
                .map(|(i, f)| {
                    Frame::new(
                        deep_frame_to_image(u32::from(f.width), u32::from(f.height), f.rgba, &dpel),
                        delay,
                        i as u32,
                    )
                })
                .collect())
        }
        TopLevel::Raster(form) => Ok(vec![Frame::new(decode_raster(bytes, form, opts)?, None, 0)]),
        TopLevel::Anim => {
            let children = anim_children(bytes)?;
            let seed = children
                .first()
                .ok_or_else(|| IffError::invalid("ANIM: no ILBM frames"))?;
            let h = raster_header(seed, IffForm::Ilbm)?;
            check_limits(u32::from(h.bmhd.width), u32::from(h.bmhd.height), opts)?;
            let layout = planar_layout(&h);
            let (anim, indices) = parse_anim_indexed(bytes)?;
            let playback = anim.playback();
            let effective: Vec<[u8; 3]> = match anim.frames.first() {
                Some(seed) if layout.ehb && seed.palette.len() <= 32 => {
                    expand_ehb_palette(&seed.palette)
                }
                Some(seed) => seed.palette.clone(),
                None => Vec::new(),
            };
            let mut out = Vec::with_capacity(anim.frames.len());
            for (i, (img, idx)) in anim.frames.into_iter().zip(indices).enumerate() {
                let view = idx.map(|v| (v, effective.clone()));
                let image = ilbm_to_image(img, view, IffForm::Ilbm, &layout);
                let delay = playback
                    .frames
                    .get(i)
                    .map(|p| Duration::from_micros(p.duration_micros()));
                out.push(Frame::new(image, delay, i as u32));
            }
            Ok(out)
        }
        TopLevel::Group => {
            let children = group_children(bytes)?;
            let mut out = Vec::with_capacity(children.len());
            for (i, child) in children.iter().enumerate() {
                match decode_with(child, opts) {
                    Ok(image) => out.push(Frame::new(image, None, i as u32)),
                    Err(e) if opts.strict => return Err(e),
                    Err(IffError::LimitExceeded(s)) => return Err(IffError::LimitExceeded(s)),
                    Err(_) => {}
                }
            }
            if out.is_empty() {
                return Err(IffError::unsupported(
                    "IFF: group holds no decodable raster FORM",
                ));
            }
            Ok(out)
        }
    }
}

// ───────────────────────── encode ─────────────────────────

/// Write `image` as the FORM [`EncodeOptions::form`] names (default: the
/// image's own [`IffImage::form`], `ILBM` for caller-built images).
///
/// * `Pal8` into `ILBM` / `ACBM` / `PBM `: index-for-index, the palette
///   as `CMAP` (EHB viewmode: the first 32 entries), a palette entry
///   with alpha 0 as the `HasTransparentColor` key. Lossless.
/// * `Rgb24` / `Rgba` into `ILBM`: the 24-bit literal-RGB form by
///   default; [`EncodeOptions::indexed`] quantises to ≤ 256 colours and
///   writes bitplanes (alpha becomes a `HasMask` plane); a HAM / EHB
///   viewmode selects those encoders. `PBM ` / `ACBM` have no 24-bit
///   form and need `indexed`.
/// * Into `DEEP`: RGB 8:8:8 or RGBA 8:8:8:8 `DPEL` by the input's alpha.
/// * Into `RGB8` / `RGBN`: alpha 0 drives the genlock bit (`RGBN`
///   keeps the top 4 bits of each gun).
///
/// An `Rgba` input whose alpha the target cannot carry is
/// `Error::Unsupported` unless [`EncodeOptions::drop_alpha`] is set.
pub fn encode(image: &IffImage, opts: &EncodeOptions) -> Result<Vec<u8>> {
    image.validate()?;
    let form = opts.form.unwrap_or(image.form);
    let (w, h) = dims16(image)?;
    match form {
        IffForm::Ilbm | IffForm::Pbm | IffForm::Acbm => encode_planar_family(image, form, opts),
        IffForm::Deep => {
            let rgba = image_rgba_for_encode(image, opts, opts.deep_rgb_only, "DEEP")?;
            let with_alpha = image.format == PixelFormat::Rgba && !opts.deep_rgb_only;
            let dpel = deep_dpel(with_alpha);
            let compression = match opts.compression {
                Compression::None => DeepCompression::None,
                Compression::ByteRun1 => DeepCompression::RunLength,
                Compression::Auto => {
                    let raw =
                        encode_deep_frames(&dpel, w, h, DeepCompression::None, None, &[&rgba])?;
                    let rle = encode_deep_frames(
                        &dpel,
                        w,
                        h,
                        DeepCompression::RunLength,
                        None,
                        &[&rgba],
                    )?;
                    return Ok(if rle.len() < raw.len() { rle } else { raw });
                }
            };
            encode_deep_frames(&dpel, w, h, compression, None, &[&rgba])
        }
        IffForm::Rgb8 => {
            let rgba = image_rgba_for_encode(image, opts, false, "RGB8")?;
            encode_rgb8_form(w, h, &rgba)
        }
        IffForm::Rgbn => {
            let rgba = image_rgba_for_encode(image, opts, false, "RGBN")?;
            encode_rgbn_form(w, h, &rgba)
        }
    }
}

fn dims16(image: &IffImage) -> Result<(u16, u16)> {
    if image.width == 0 || image.height == 0 {
        return Err(IffError::invalid("IFF encode: zero-dimension image"));
    }
    let w = u16::try_from(image.width).map_err(|_| {
        IffError::unsupported(format!(
            "IFF encode: width {} exceeds the 16-bit BMHD field",
            image.width
        ))
    })?;
    let h = u16::try_from(image.height).map_err(|_| {
        IffError::unsupported(format!(
            "IFF encode: height {} exceeds the 16-bit BMHD field",
            image.height
        ))
    })?;
    Ok((w, h))
}

/// Tightly packed RGBA for the true-colour encoders. `Rgba` input keeps
/// its alpha when the target carries it (`alpha_ok`), otherwise it must
/// be opaque or `drop_alpha` must be set.
fn image_rgba_for_encode(
    image: &IffImage,
    opts: &EncodeOptions,
    force_opaque: bool,
    label: &str,
) -> Result<Vec<u8>> {
    let mut rgba = image.to_rgba8();
    let has_alpha =
        image.format == PixelFormat::Rgba && rgba.iter().skip(3).step_by(4).any(|&a| a != 255);
    if has_alpha && force_opaque {
        if !opts.drop_alpha {
            return Err(IffError::unsupported(format!(
                "{label} encode: the chosen layout carries no alpha and the input is not opaque \
                 (set EncodeOptions::drop_alpha to discard it)"
            )));
        }
        rgba.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
    }
    Ok(rgba)
}

fn deep_dpel(with_alpha: bool) -> Dpel {
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
    if with_alpha {
        elements.push(DpelElement {
            c_type: DeepCType::Alpha,
            c_bit_depth: 8,
        });
    }
    Dpel { elements }
}

fn planes_for(palette_len: usize) -> u8 {
    let mut n = 1u8;
    while (1usize << n) < palette_len && n < 8 {
        n += 1;
    }
    n
}

fn bmhd_for(
    image: &IffImage,
    opts: &EncodeOptions,
    n_planes: u8,
    masking: Masking,
    key: u16,
) -> Bmhd {
    let (x_aspect, y_aspect) = opts.aspect.or(image.aspect).unwrap_or((1, 1));
    Bmhd {
        width: image.width as u16,
        height: image.height as u16,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking,
        compression: opts.compression,
        pad: 0,
        transparent_color: key,
        x_aspect,
        y_aspect,
        page_width: image.width as i16,
        page_height: image.height as i16,
    }
}

/// `ILBM` / `PBM ` / `ACBM` from any layout.
fn encode_planar_family(image: &IffImage, form: IffForm, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let viewmode = opts.viewmode.or(image.viewmode).unwrap_or(0);
    let camg = Camg { raw: viewmode };
    let is_ham = camg.is_ham();
    let is_ehb = camg.is_ehb();
    let label = match form {
        IffForm::Pbm => "PBM",
        IffForm::Acbm => "ACBM",
        _ => "ILBM",
    };

    // ── Pal8: index-for-index ──
    if image.format == PixelFormat::Pal8 {
        let palette = image
            .palette
            .as_ref()
            .ok_or_else(|| IffError::invalid("IFF encode: Pal8 image without a palette"))?;
        if is_ham {
            return Err(IffError::unsupported(format!(
                "{label} encode: a HAM viewmode needs RGB input (indices are not HAM codes)"
            )));
        }
        if matches!(opts.masking, Some(Masking::HasMask) | Some(Masking::Lasso)) {
            return Err(IffError::unsupported(format!(
                "{label} encode: Pal8 input has no per-pixel alpha for a mask plane / lasso"
            )));
        }
        let (masking, key) = match opts.masking {
            Some(m) => (m, palette.transparent_index().unwrap_or(0)),
            None => match palette.transparent_index() {
                Some(i) => (Masking::HasTransparentColor, i),
                None => (Masking::None, 0),
            },
        };
        let mut cmap = palette.to_rgb_triples();
        let n_planes = if form == IffForm::Pbm {
            8
        } else if is_ehb {
            if cmap.len() > 64 {
                return Err(IffError::unsupported(format!(
                    "{label} encode: EHB addresses 64 colours, palette has {}",
                    cmap.len()
                )));
            }
            cmap.truncate(32);
            6
        } else {
            let need = planes_for(cmap.len());
            let n = opts
                .n_planes
                .or((1..=8).contains(&image.n_planes).then_some(image.n_planes))
                .unwrap_or(need);
            if !(1..=8).contains(&n) {
                return Err(IffError::unsupported(format!(
                    "{label} encode: {n} bitplanes requested, indexed pictures carry 1..=8"
                )));
            }
            if n < need {
                return Err(IffError::invalid(format!(
                    "{label} encode: {n} bitplanes cannot address a {}-entry palette",
                    cmap.len()
                )));
            }
            n
        };
        if cmap.len() > 256 {
            return Err(IffError::unsupported(format!(
                "{label} encode: palette has {} entries, CMAP indices are 8-bit",
                cmap.len()
            )));
        }
        let bmhd = bmhd_for(image, opts, n_planes, masking, key);
        let shell = IlbmImage {
            width: image.width,
            height: image.height,
            bmhd,
            palette: cmap,
            camg,
            form_type: form.form_type(),
            ..IlbmImage::default()
        };
        let w = image.width as usize;
        let stride = image.stride();
        let data = image.data();
        return Ok(match form {
            IffForm::Pbm => {
                let row_stride = (w + 1) & !1;
                let rows: Vec<Vec<u8>> = (0..image.height as usize)
                    .map(|y| {
                        let mut row = vec![0u8; row_stride];
                        row[..w].copy_from_slice(&data[y * stride..y * stride + w]);
                        row
                    })
                    .collect();
                let (body, resolved) = pack_body_resolving(rows, opts.compression);
                assemble_ilbm_form(&shell, &body, resolved)
            }
            IffForm::Acbm => {
                let rows = indices_rows(data, w, stride, image.height as usize, &bmhd, None);
                assemble_acbm_form(&shell, &rows)
            }
            _ => {
                let rows = indices_rows(data, w, stride, image.height as usize, &bmhd, None);
                let (body, resolved) = pack_body_resolving(rows, opts.compression);
                assemble_ilbm_form(&shell, &body, resolved)
            }
        });
    }

    // ── RGB input ──
    let rgba = image.to_rgba8();
    let any_alpha =
        image.format == PixelFormat::Rgba && rgba.iter().skip(3).step_by(4).any(|&a| a != 255);

    if is_ham || is_ehb {
        // The HAM / EHB encoders quantise from RGBA against a capped
        // palette (16 / 64 for HAM6 / HAM8, 32 for EHB) exactly as the
        // container muxer does.
        let n_planes = if is_ehb {
            6
        } else if opts.n_planes == Some(8) || image.n_planes == 8 {
            8
        } else {
            6
        };
        let cap = if is_ehb {
            32
        } else if n_planes == 8 {
            64
        } else {
            16
        };
        if form == IffForm::Pbm {
            return Err(IffError::unsupported(
                "PBM encode: the chunky form has no HAM / EHB encoder",
            ));
        }
        let masking = match opts.masking {
            Some(m) => m,
            None if any_alpha => Masking::HasMask,
            None => Masking::None,
        };
        let shell = IlbmImage {
            width: image.width,
            height: image.height,
            bmhd: bmhd_for(image, opts, n_planes, masking, 0),
            palette: build_palette_capped(&rgba, cap),
            camg,
            form_type: form.form_type(),
            rgba,
            ..IlbmImage::default()
        };
        return if form == IffForm::Acbm {
            encode_acbm(&shell)
        } else {
            encode_ilbm(&shell)
        };
    }

    if opts.indexed || form == IffForm::Pbm || form == IffForm::Acbm {
        if !opts.indexed {
            return Err(IffError::unsupported(format!(
                "{label} encode: no 24-bit form exists; set EncodeOptions::indexed to quantise \
                 RGB input to a palette"
            )));
        }
        let (palette, indices) = build_palette(&rgba);
        let masking = match opts.masking {
            Some(m) => m,
            None if any_alpha => Masking::HasMask,
            None => Masking::None,
        };
        if form == IffForm::Pbm && masking == Masking::HasMask {
            return Err(IffError::unsupported(
                "PBM encode: the chunky form has no mask plane (set drop_alpha or a transparent key)",
            ));
        }
        let n_planes = if form == IffForm::Pbm {
            8
        } else {
            let need = planes_for(palette.len());
            let n = opts.n_planes.unwrap_or(need);
            if !(1..=8).contains(&n) || n < need {
                return Err(IffError::invalid(format!(
                    "{label} encode: {n} bitplanes cannot address a {}-entry palette",
                    palette.len()
                )));
            }
            n
        };
        let bmhd = bmhd_for(image, opts, n_planes, masking, 0);
        let shell = IlbmImage {
            width: image.width,
            height: image.height,
            bmhd,
            palette,
            camg,
            form_type: form.form_type(),
            ..IlbmImage::default()
        };
        let w = image.width as usize;
        let mask: Option<Vec<u8>> = (masking == Masking::HasMask).then(|| {
            rgba.iter()
                .skip(3)
                .step_by(4)
                .map(|&a| u8::from(a >= 0x80))
                .collect()
        });
        return Ok(match form {
            IffForm::Pbm => {
                let row_stride = (w + 1) & !1;
                let rows: Vec<Vec<u8>> = indices
                    .chunks_exact(w)
                    .map(|r| {
                        let mut row = vec![0u8; row_stride];
                        row[..w].copy_from_slice(r);
                        row
                    })
                    .collect();
                let (body, resolved) = pack_body_resolving(rows, opts.compression);
                assemble_ilbm_form(&shell, &body, resolved)
            }
            IffForm::Acbm => {
                let rows = indices_rows(
                    &indices,
                    w,
                    w,
                    image.height as usize,
                    &bmhd,
                    mask.as_deref(),
                );
                assemble_acbm_form(&shell, &rows)
            }
            _ => {
                let rows = indices_rows(
                    &indices,
                    w,
                    w,
                    image.height as usize,
                    &bmhd,
                    mask.as_deref(),
                );
                let (body, resolved) = pack_body_resolving(rows, opts.compression);
                assemble_ilbm_form(&shell, &body, resolved)
            }
        });
    }

    // 24-bit literal-RGB ILBM: no alpha mechanism.
    if any_alpha && !opts.drop_alpha {
        return Err(IffError::unsupported(
            "ILBM encode: the 24-bit literal-RGB form carries no alpha and the input is not \
             opaque (set EncodeOptions::indexed for a mask plane, drop_alpha to discard it, \
             or form = Deep)",
        ));
    }
    if opts.masking.is_some_and(|m| m != Masking::None) {
        return Err(IffError::unsupported(
            "ILBM encode: the 24-bit literal-RGB form has no masking mode",
        ));
    }
    let shell = IlbmImage {
        width: image.width,
        height: image.height,
        bmhd: bmhd_for(image, opts, 24, Masking::None, 0),
        palette: Vec::new(),
        camg,
        form_type: *b"ILBM",
        rgba,
        ..IlbmImage::default()
    };
    encode_ilbm(&shell)
}

/// Planar rows (`n_planes` colour rows then the optional mask row per
/// scanline) straight from palette indices. `mask` is one byte per
/// pixel (non-zero = opaque) when `bmhd.masking == HasMask`.
fn indices_rows(
    indices: &[u8],
    width: usize,
    stride: usize,
    height: usize,
    bmhd: &Bmhd,
    mask: Option<&[u8]>,
) -> Vec<Vec<u8>> {
    let row_bytes = bmhd.row_bytes();
    let has_mask = bmhd.masking == Masking::HasMask;
    let mut rows = Vec::with_capacity(height * (bmhd.n_planes as usize + has_mask as usize));
    for y in 0..height {
        let row = &indices[y * stride..y * stride + width];
        let mask_row: Option<Vec<u8>> = match (has_mask, mask) {
            (true, Some(m)) => {
                let mut bits = vec![0u8; row_bytes];
                for (x, &a) in m[y * width..y * width + width].iter().enumerate() {
                    if a != 0 {
                        bits[x / 8] |= 1 << (7 - (x % 8));
                    }
                }
                Some(bits)
            }
            (true, None) => Some(vec![0xFF; row_bytes]),
            _ => None,
        };
        push_planar_row(
            &mut rows,
            row,
            bmhd.n_planes,
            row_bytes,
            mask_row.as_deref(),
        );
    }
    rows
}

/// [`encode`] of a packed RGB8 buffer (`3 × width × height` bytes).
pub fn encode_rgb8(width: u32, height: u32, rgb: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode(&IffImage::from_rgb8(width, height, rgb.to_vec())?, opts)
}

/// [`encode`] of a packed RGBA8 buffer (`4 × width × height` bytes).
/// Alpha goes where the target can carry it (a `HasMask` plane with
/// [`EncodeOptions::indexed`], the `DEEP` alpha component, the `RGB8` /
/// `RGBN` genlock bit); into the default 24-bit `ILBM` a non-opaque
/// input is `Error::Unsupported` unless [`EncodeOptions::drop_alpha`].
pub fn encode_rgba8(width: u32, height: u32, rgba: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode(&IffImage::from_rgba8(width, height, rgba.to_vec())?, opts)
}

/// [`encode`] into a writer.
pub fn encode_to<W: Write>(image: &IffImage, opts: &EncodeOptions, mut w: W) -> Result<()> {
    let bytes = encode(image, opts)?;
    w.write_all(&bytes)?;
    Ok(())
}

/// Write several pictures as one file, the mirror of [`decode_all`]:
///
/// * `form` `Ilbm` (the default) — a `FORM ANIM` with the
///   [`EncodeOptions::anim_op`] delta coding (default op-5). Every frame
///   must share the first frame's size; frames are quantised against
///   one `CMAP` (a `Pal8` first frame's palette, else the first 256
///   distinct colours of the first frame). `Frame::delay` becomes the
///   following frame's `ANHD.reltime` in jiffies (1/60 s, rounded to
///   nearest, at least 1).
/// * `form` `Deep` — a multi-`DBOD` `FORM DEEP`; the first frame's
///   `delay` becomes the `DCHG` frame rate in milliseconds.
///
/// Other forms hold one picture and are `Error::Unsupported`.
pub fn encode_all(frames: &[Frame], opts: &EncodeOptions) -> Result<Vec<u8>> {
    let first = frames
        .first()
        .ok_or_else(|| IffError::invalid("IFF encode_all: no frames"))?;
    let form = opts.form.unwrap_or(IffForm::Ilbm);
    let (w, h) = dims16(&first.image)?;
    for f in frames {
        f.image.validate()?;
        if f.image.width != first.image.width || f.image.height != first.image.height {
            return Err(IffError::unsupported(format!(
                "IFF encode_all: frame geometry {}x{} differs from the first frame's {}x{}",
                f.image.width, f.image.height, first.image.width, first.image.height
            )));
        }
    }
    match form {
        IffForm::Ilbm => encode_anim(frames, w, h, opts),
        IffForm::Deep => {
            let with_alpha =
                !opts.deep_rgb_only && frames.iter().any(|f| f.image.format == PixelFormat::Rgba);
            let mut bodies = Vec::with_capacity(frames.len());
            for f in frames {
                bodies.push(image_rgba_for_encode(&f.image, opts, !with_alpha, "DEEP")?);
            }
            let refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
            let dpel = deep_dpel(with_alpha);
            let dchg = (frames.len() > 1)
                .then_some(first.delay)
                .flatten()
                .map(|d| Dchg {
                    frame_rate: i32::try_from(d.as_millis()).unwrap_or(i32::MAX).max(1),
                });
            let compression = match opts.compression {
                Compression::None => DeepCompression::None,
                Compression::ByteRun1 => DeepCompression::RunLength,
                Compression::Auto => {
                    let raw = encode_deep_frames(&dpel, w, h, DeepCompression::None, dchg, &refs)?;
                    let rle =
                        encode_deep_frames(&dpel, w, h, DeepCompression::RunLength, dchg, &refs)?;
                    return Ok(if rle.len() < raw.len() { rle } else { raw });
                }
            };
            encode_deep_frames(&dpel, w, h, compression, dchg, &refs)
        }
        other => Err(IffError::unsupported(format!(
            "IFF encode_all: FORM {:?} holds a single picture",
            std::str::from_utf8(&other.form_type()).unwrap_or("????")
        ))),
    }
}

fn encode_anim(frames: &[Frame], w: u16, h: u16, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let first = &frames[0].image;
    let viewmode = opts.viewmode.or(first.viewmode).unwrap_or(0);
    let camg = Camg { raw: viewmode };
    if camg.is_ham() {
        return Err(IffError::unsupported(
            "ANIM encode: HAM animations are not supported by the delta encoders",
        ));
    }
    // One CMAP for the whole animation.
    let first_rgba = first.to_rgba8();
    let (palette, key) = match (&first.format, &first.palette) {
        (PixelFormat::Pal8, Some(p)) => (p.to_rgb_triples(), p.transparent_index()),
        _ => (build_palette(&first_rgba).0, None),
    };
    if palette.len() > 256 {
        return Err(IffError::unsupported(format!(
            "ANIM encode: palette has {} entries, CMAP indices are 8-bit",
            palette.len()
        )));
    }
    let (palette, n_planes) = if camg.is_ehb() {
        let mut p = palette;
        p.truncate(32);
        (p, 6)
    } else {
        let need = planes_for(palette.len());
        let n = opts.n_planes.unwrap_or(need);
        if !(1..=8).contains(&n) || n < need {
            return Err(IffError::invalid(format!(
                "ANIM encode: {n} bitplanes cannot address a {}-entry palette",
                palette.len()
            )));
        }
        (palette, n)
    };
    let any_alpha = frames.iter().any(|f| {
        f.image.format == PixelFormat::Rgba
            && f.image.data().iter().skip(3).step_by(4).any(|&a| a != 255)
    });
    let (masking, key) = match opts.masking {
        Some(m) => (m, key.unwrap_or(0)),
        None => match key {
            Some(k) => (Masking::HasTransparentColor, k),
            None if any_alpha => (Masking::HasMask, 0),
            None => (Masking::None, 0),
        },
    };
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking,
        compression: opts.compression,
        pad: 0,
        transparent_color: key,
        x_aspect: opts.aspect.or(first.aspect).map(|a| a.0).unwrap_or(1),
        y_aspect: opts.aspect.or(first.aspect).map(|a| a.1).unwrap_or(1),
        page_width: w as i16,
        page_height: h as i16,
    };
    let images: Vec<IlbmImage> = frames
        .iter()
        .map(|f| IlbmImage {
            width: u32::from(w),
            height: u32::from(h),
            bmhd,
            palette: palette.clone(),
            camg,
            form_type: *b"ILBM",
            rgba: f.image.to_rgba8(),
            ..IlbmImage::default()
        })
        .collect();
    // `Frame::delay` is how long frame i stays up, i.e. frame i+1's
    // reltime; the seed frame's own reltime is 0.
    let mut timing = Vec::with_capacity(frames.len());
    timing.push(FrameTiming::default());
    for f in &frames[..frames.len() - 1] {
        let jiffies = f
            .delay
            .map(|d| ((d.as_micros() * 60 + 500_000) / 1_000_000).max(1))
            .unwrap_or(1);
        timing.push(FrameTiming {
            rel_time: u32::try_from(jiffies).unwrap_or(u32::MAX),
            abs_time: 0,
        });
    }
    match opts.anim_op {
        AnimOp::Op0 => encode_anim_op0_timed(&images, &timing),
        AnimOp::Op1 => encode_anim_op1_timed(&images, &timing),
        AnimOp::Op2 => encode_anim_op2_timed(&images, &timing),
        AnimOp::Op3 => encode_anim_op3_timed(&images, &timing),
        AnimOp::Op4 { long_data } => encode_anim_op4_timed(&images, long_data, &timing),
        AnimOp::Op5 => encode_anim_op5_timed(&images, &timing),
        AnimOp::Op7 { long_data } => encode_anim_op7_timed(&images, long_data, &timing),
        AnimOp::Op8 { long_data } => encode_anim_op8_timed(&images, long_data, &timing),
    }
}
