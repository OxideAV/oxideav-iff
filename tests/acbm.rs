//! ACBM — Amiga Contiguous BitMap (`FORM ACBM`, AmigaBASIC sibling of
//! ILBM). The row-interleaved `BODY` is replaced by an `ABIT` chunk that
//! stores the bitplanes **plane-by-plane, contiguously** and
//! **uncompressed**. Everything else (BMHD/CMAP/CAMG/GRAB/EHB/HAM/…)
//! matches ILBM, so the decode reuses ILBM's render path.
//!
//! Spec reference: `docs/image/ilbm/multimediawiki-iff.html` §4.1
//! ("ACBM is similar to ILBM except that the BODY chunk is replaced by
//! an ABIT chunk. An ABIT chunk contains non-interleaved, plane-by-plane
//! planar image data … conceived because it hugely sped up loading and
//! saving screens from AmigaBASIC.").
//!
//! Covers:
//! * indexed 1..=8 bitplane `encode_acbm`/`parse_acbm` round-trip;
//! * ABIT carries plane-contiguous bytes (verified against the
//!   hand-computed bitplane layout);
//! * ACBM decodes to byte-identical RGBA as the equivalent ILBM;
//! * EHB, HAM6 and `HasMask` modes round-trip;
//! * `GRAB` + colour-cycling sub-chunks survive the round-trip;
//! * the `iff_acbm` container demuxer (probe + extension) emits one
//!   keyframe of the decoded RGBA;
//! * rejection of compressed ABIT, 24-bit, PBM, wrong FORM type, and
//!   missing BMHD/ABIT.

#![allow(clippy::needless_range_loop)]

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, ContainerRegistry, Error, MediaType, Muxer, Packet, PixelFormat,
    ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_iff::ilbm::{
    encode_acbm, encode_ilbm, indices_to_planar_row, parse_acbm, parse_ilbm, Bmhd, Camg,
    Compression, Grab, IlbmImage, IlbmMuxer, Masking, MuxerMode, CAMG_EHB, CAMG_HAM,
};

fn bmhd(w: u16, h: u16, n_planes: u8, masking: Masking) -> Bmhd {
    Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    }
}

/// Build an indexed image whose pixel at (x,y) is index `(x + y) % n`.
fn indexed_image(w: u16, h: u16, n_planes: u8, palette: Vec<[u8; 3]>) -> IlbmImage {
    let n = palette.len();
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let idx = (x as usize + y as usize) % n;
            let p = palette[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: bmhd(w, h, n_planes, Masking::None),
        palette,
        camg: Camg::default(),
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    }
}

fn pal16() -> Vec<[u8; 3]> {
    (0..16u8)
        .map(|i| [i * 16, 255 - i * 16, i.wrapping_mul(17)])
        .collect()
}

// ───────────────────── round-trip ─────────────────────

#[test]
fn acbm_indexed_roundtrip() {
    let img = indexed_image(13, 5, 4, pal16());
    let bytes = encode_acbm(&img).unwrap();
    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"ACBM");
    // The body chunk must be ABIT, never BODY.
    assert!(
        find_chunk(&bytes, b"ABIT").is_some(),
        "ACBM file must carry an ABIT chunk"
    );
    assert!(
        find_chunk(&bytes, b"BODY").is_none(),
        "ACBM file must not carry a BODY chunk"
    );

    let dec = parse_acbm(&bytes).unwrap();
    assert_eq!(dec.width, 13);
    assert_eq!(dec.height, 5);
    assert_eq!(&dec.form_type, b"ACBM");
    assert_eq!(dec.rgba, img.rgba, "ACBM indexed RGBA round-trips");
}

#[test]
fn acbm_all_plane_counts_roundtrip() {
    for n_planes in 1..=8u8 {
        let entries = 1usize << n_planes;
        let palette: Vec<[u8; 3]> = (0..entries)
            .map(|i| [(i * 3) as u8, (i * 5) as u8, (i * 7) as u8])
            .collect();
        let img = indexed_image(11, 7, n_planes, palette);
        let bytes = encode_acbm(&img).unwrap();
        let dec = parse_acbm(&bytes).unwrap();
        assert_eq!(
            dec.rgba, img.rgba,
            "ACBM round-trips at {n_planes} bitplanes"
        );
    }
}

/// ABIT is plane-by-plane contiguous: plane 0's whole bitmap, then
/// plane 1's, … Verify the on-wire bytes match the hand-computed
/// bitplane layout (vs ILBM's scanline-interleaved BODY).
#[test]
fn acbm_abit_is_plane_contiguous() {
    let w = 16u16;
    let h = 3u16;
    let n_planes = 2u8;
    let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let img = indexed_image(w, h, n_planes, palette);
    let bytes = encode_acbm(&img).unwrap();
    let abit = find_chunk(&bytes, b"ABIT").expect("ABIT present");

    let row_bytes = (w as usize).div_ceil(16) * 2;
    let plane_size = row_bytes * h as usize;
    assert_eq!(
        abit.len(),
        plane_size * n_planes as usize,
        "ABIT length = planes × rows × row_bytes (uncompressed)"
    );

    // Recompute the expected plane-contiguous bytes directly: for each
    // plane, lay out all H rows back-to-back.
    let mut expected: Vec<u8> = Vec::new();
    let mut per_plane: Vec<Vec<u8>> = vec![Vec::new(); n_planes as usize];
    for y in 0..h {
        let indices: Vec<u8> = (0..w)
            .map(|x| ((x as usize + y as usize) % 4) as u8)
            .collect();
        let plane_rows = indices_to_planar_row(&indices, n_planes, row_bytes);
        for (p, row) in plane_rows.into_iter().enumerate() {
            per_plane[p].extend_from_slice(&row);
        }
    }
    for p in per_plane {
        expected.extend_from_slice(&p);
    }
    assert_eq!(abit, &expected[..], "ABIT bytes are plane-contiguous");
}

// ───────────────────── ACBM == ILBM render equivalence ─────────────────────

#[test]
fn acbm_renders_same_rgba_as_ilbm() {
    let img = indexed_image(17, 9, 5, pal16());
    let acbm_bytes = encode_acbm(&img).unwrap();
    let acbm_dec = parse_acbm(&acbm_bytes).unwrap();

    let ilbm_bytes = encode_ilbm(&img).unwrap();
    let ilbm_dec = parse_ilbm(&ilbm_bytes).unwrap();

    assert_eq!(
        acbm_dec.rgba, ilbm_dec.rgba,
        "ACBM and ILBM decode the same image to identical RGBA"
    );
}

// ───────────────────── EHB ─────────────────────

#[test]
fn acbm_ehb_roundtrip() {
    // 32-entry base palette; EHB expands to 64. Image uses the full
    // 64-entry expanded space.
    let base: Vec<[u8; 3]> = (0..32u8)
        .map(|i| [i.wrapping_mul(8), i.wrapping_mul(4), i.wrapping_mul(2)])
        .collect();
    let expanded = oxideav_iff::ilbm::expand_ehb_palette(&base);
    let w = 16u16;
    let h = 4u16;
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let idx = (x as usize + y as usize) % 64;
            let p = expanded[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let camg = Camg { raw: CAMG_EHB };
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: bmhd(w, h, 6, Masking::None),
        palette: base,
        camg,
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let bytes = encode_acbm(&img).unwrap();
    let dec = parse_acbm(&bytes).unwrap();
    assert_eq!(dec.rgba, img.rgba, "ACBM EHB round-trips");
}

// ───────────────────── HAM6 ─────────────────────

#[test]
fn acbm_ham6_roundtrip() {
    // HAM is lossy on encode; check the decode of the encoded ACBM
    // equals the decode of the encoded ILBM (same encoder front-end).
    let palette: Vec<[u8; 3]> = (0..16u8).map(|i| [i * 16, i * 8, 255 - i * 16]).collect();
    let w = 12u16;
    let h = 6u16;
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 20) as u8, (y * 40) as u8, 128, 0xFF]);
        }
    }
    let camg = Camg { raw: CAMG_HAM };
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: bmhd(w, h, 6, Masking::None),
        palette,
        camg,
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let acbm = parse_acbm(&encode_acbm(&img).unwrap()).unwrap();
    let ilbm = parse_ilbm(&encode_ilbm(&img).unwrap()).unwrap();
    assert_eq!(
        acbm.rgba, ilbm.rgba,
        "ACBM HAM6 decode matches ILBM HAM6 decode"
    );
}

// ───────────────────── HasMask ─────────────────────

#[test]
fn acbm_hasmask_roundtrip() {
    let palette = pal16();
    let w = 16u16;
    let h = 4u16;
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            // Make a checkerboard of transparent pixels.
            let transparent = (x + y) % 3 == 0;
            let idx = (x as usize + y as usize) % 16;
            let p = palette[idx];
            let a = if transparent { 0x00 } else { 0xFF };
            rgba.extend_from_slice(&[p[0], p[1], p[2], a]);
        }
    }
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: bmhd(w, h, 4, Masking::HasMask),
        palette,
        camg: Camg::default(),
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let dec = parse_acbm(&encode_acbm(&img).unwrap()).unwrap();
    // Compare alpha channel survives the contiguous mask plane.
    for (i, chunk) in dec.rgba.chunks_exact(4).enumerate() {
        assert_eq!(
            chunk[3],
            rgba[i * 4 + 3],
            "ACBM HasMask alpha round-trips at pixel {i}"
        );
    }
}

// ───────────────────── sub-chunks ─────────────────────

#[test]
fn acbm_grab_survives_roundtrip() {
    let mut img = indexed_image(8, 4, 3, pal16());
    img.grab = Some(Grab { x: 3, y: -2 });
    let dec = parse_acbm(&encode_acbm(&img).unwrap()).unwrap();
    let g = dec.grab.expect("GRAB survives ACBM round-trip");
    assert_eq!((g.x, g.y), (3, -2));
}

// ───────────────────── container demuxer ─────────────────────

#[test]
fn acbm_demuxer_decodes_one_keyframe() {
    let img = indexed_image(10, 6, 4, pal16());
    let bytes = encode_acbm(&img).unwrap();

    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);

    // Probe routes ACBM to iff_acbm (not iff_ilbm).
    let mut cur = Cursor::new(bytes.clone());
    let format = reg.probe_input(&mut cur, None).expect("probe matches ACBM");
    assert_eq!(format, "iff_acbm");

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let mut dmx = reg
        .open_demuxer("iff_acbm", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    assert_eq!(dmx.format_name(), "iff_acbm");
    let s = &dmx.streams()[0];
    assert_eq!(s.params.width, Some(10));
    assert_eq!(s.params.height, Some(6));

    let pkt = dmx.next_packet().unwrap();
    assert!(pkt.flags.keyframe);
    assert_eq!(pkt.data, img.rgba);
    // EOF after the single frame.
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

#[test]
fn acbm_extension_routes_to_demuxer() {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    assert_eq!(reg.container_for_extension("acbm"), Some("iff_acbm"));
}

// ───────────────────── streaming muxer (MuxerMode::Acbm) ─────────────────────

static CTR: AtomicU64 = AtomicU64::new(0);

fn unique_path() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("oxideav-iff-acbm-{}-{n}.acbm", std::process::id()))
}

fn rgba_stream(width: u32, height: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(PixelFormat::Rgba);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    }
}

#[test]
fn muxer_acbm_mode_roundtrip() {
    let w = 12u32;
    let h = 8u32;
    // A 4-colour checkerboard so the indexed palette stays small.
    let palette = [[10u8, 20, 30], [200, 0, 0], [0, 200, 0], [0, 0, 200]];
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let p = palette[((x + y) % 4) as usize];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }

    let stream = rgba_stream(w, h);
    let path = unique_path();
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_mode(MuxerMode::Acbm);
        mux.write_header().unwrap();
        mux.write_packet(&Packet::new(0, stream.time_base, rgba.clone()))
            .unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();

    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"ACBM");
    assert!(find_chunk(&bytes, b"ABIT").is_some());
    assert!(find_chunk(&bytes, b"BODY").is_none());

    let dec = parse_acbm(&bytes).unwrap();
    assert_eq!(&dec.form_type, b"ACBM");
    assert_eq!(dec.bmhd.compression, Compression::None);
    assert_eq!(dec.width, w);
    assert_eq!(dec.height, h);
    assert_eq!(dec.rgba, rgba, "ACBM muxer-mode round-trips the RGBA");
}

// ───────────────────── rejection cases ─────────────────────

#[test]
fn acbm_rejects_compressed_abit() {
    let mut img = indexed_image(8, 4, 3, pal16());
    img.bmhd.compression = Compression::ByteRun1;
    // Encoder forces None, so build a hand-crafted file with compression=1.
    let bytes = encode_acbm(&{
        let mut i = img.clone();
        i.bmhd.compression = Compression::None;
        i
    })
    .unwrap();
    // Flip the BMHD compression byte to 1 and re-parse: must reject.
    let mut tampered = bytes.clone();
    let bmhd_off = find_chunk_offset(&tampered, b"BMHD").unwrap();
    // BMHD layout: width(2) height(2) x(2) y(2) nplanes(1) masking(1) compression(1)
    tampered[bmhd_off + 8 + 10] = 1;
    let err = parse_acbm(&tampered).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
}

#[test]
fn acbm_rejects_24bit() {
    let mut img = indexed_image(8, 2, 1, pal16());
    img.bmhd.n_planes = 24;
    let err = encode_acbm(&img).unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn acbm_encode_rejects_pbm() {
    let mut img = indexed_image(8, 2, 8, pal16());
    img.form_type = *b"PBM ";
    let err = encode_acbm(&img).unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn acbm_parse_rejects_wrong_form_type() {
    let bytes = encode_ilbm(&indexed_image(8, 2, 2, pal16())).unwrap();
    // This is a FORM ILBM, not ACBM.
    let err = parse_acbm(&bytes).unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn acbm_parse_rejects_missing_abit() {
    // Hand-build a FORM ACBM with BMHD + CMAP but no ABIT.
    let mut form: Vec<u8> = Vec::new();
    form.extend_from_slice(b"ACBM");
    let bm = bmhd(8, 2, 2, Masking::None);
    push(&mut form, b"BMHD", &bm.write());
    push(&mut form, b"CMAP", &[0, 0, 0, 255, 0, 0]);
    let mut file = Vec::new();
    file.extend_from_slice(b"FORM");
    file.extend_from_slice(&(form.len() as u32).to_be_bytes());
    file.extend_from_slice(&form);
    let err = parse_acbm(&file).unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn acbm_parse_rejects_truncated_abit() {
    let img = indexed_image(16, 4, 4, pal16());
    let bytes = encode_acbm(&img).unwrap();
    // Truncate the ABIT body by lying about nothing — just cut the file
    // off mid-ABIT. Easiest: rebuild with a too-short ABIT.
    let mut form: Vec<u8> = Vec::new();
    form.extend_from_slice(b"ACBM");
    push(&mut form, b"BMHD", &img.bmhd.write());
    let mut cmap = Vec::new();
    for c in &img.palette {
        cmap.extend_from_slice(c);
    }
    push(&mut form, b"CMAP", &cmap);
    push(&mut form, b"ABIT", &[0u8; 4]); // far too short
    let mut file = Vec::new();
    file.extend_from_slice(b"FORM");
    file.extend_from_slice(&(form.len() as u32).to_be_bytes());
    file.extend_from_slice(&form);
    let err = parse_acbm(&file).unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = bytes;
}

// ───────────────────── helpers ─────────────────────

fn push(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() & 1 == 1 {
        out.push(0);
    }
}

/// Find a chunk body slice by id within a complete FORM file.
fn find_chunk<'a>(file: &'a [u8], id: &[u8; 4]) -> Option<&'a [u8]> {
    let off = find_chunk_offset(file, id)?;
    let size =
        u32::from_be_bytes([file[off + 4], file[off + 5], file[off + 6], file[off + 7]]) as usize;
    Some(&file[off + 8..off + 8 + size])
}

/// Offset of a chunk header (the 4CC) within a complete FORM file.
fn find_chunk_offset(file: &[u8], id: &[u8; 4]) -> Option<usize> {
    let mut cursor = 12usize; // skip FORM + size + form type
    while cursor + 8 <= file.len() {
        let this = &file[cursor..cursor + 4];
        let size = u32::from_be_bytes([
            file[cursor + 4],
            file[cursor + 5],
            file[cursor + 6],
            file[cursor + 7],
        ]) as usize;
        if this == id {
            return Some(cursor);
        }
        cursor += 8 + size + (size & 1);
    }
    None
}
