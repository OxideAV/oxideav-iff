//! 24-bit true-colour ILBM (`BMHD.n_planes == 24`, no `CMAP`, literal RGB
//! bitplanes laid out R LSB → R MSB, G LSB → G MSB, B LSB → B MSB per
//! scanline). Encoder + decoder + muxer-mode coverage.
//!
//! Spec reference: `docs/image/ilbm/fileformatinfo-iff.html` §3.3.4
//! ("If there is no CMAP and if BMHD.BitPlanes is 24, the ILBM contains
//! a 24-bit image, and the BODY encodes pixels as literal RGB values.")
//!
//! Covers:
//! * `Compression::None` + `Compression::ByteRun1` + `Compression::Auto`
//!   round-trip for full-colour images;
//! * decode rejects 24-bit BODY with a `HasMask` plane (spec gap, not
//!   a supported mode);
//! * encode rejects `HAM` / `EHB` CAMG flags combined with 24 bitplanes
//!   (mutually exclusive with literal-RGB);
//! * `IlbmMuxer::with_mode(MuxerMode::TrueColor24)` end-to-end through
//!   the streaming muxer API (emits no CMAP, n_planes=24, ILBM form);
//! * pixel-exact round-trip across the full 24-bit value space using a
//!   gradient sweep that touches every channel boundary;
//! * Auto compression picks ByteRun1 on a flat fill (savings expected
//!   vs. the literal 24-row-per-scanline raw layout).

#![allow(clippy::needless_range_loop)]

use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, MediaType, Packet, PixelFormat, StreamInfo, TimeBase,
};
use oxideav_core::{Muxer, WriteSeek};
use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, IlbmMuxer, Masking, MuxerMode,
    CAMG_EHB, CAMG_HAM,
};

static CTR: AtomicU64 = AtomicU64::new(0);

fn unique_path(suffix: &str) -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-iff-tc24-{}-{n}.{suffix}",
        std::process::id()
    ))
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

/// Build a true-colour `IlbmImage` for direct `encode_ilbm` / `parse_ilbm`
/// round-trip testing. No CMAP, no HAM, no EHB.
fn tc24_image(width: u16, height: u16, compression: Compression, rgba: Vec<u8>) -> IlbmImage {
    let bmhd = Bmhd {
        width,
        height,
        x_origin: 0,
        y_origin: 0,
        n_planes: 24,
        masking: Masking::None,
        compression,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: width as i16,
        page_height: height as i16,
    };
    IlbmImage {
        width: width as u32,
        height: height as u32,
        bmhd,
        palette: Vec::new(),
        camg: Camg::default(),
        form_type: *b"ILBM",
        rgba,
        ..IlbmImage::default()
    }
}

/// A 16x4 RGB gradient that exercises low, mid and high bits of every
/// channel: each row sweeps red 0..16 while green/blue rotate through
/// representative byte values.
fn gradient_rgba(width: u16, height: u16) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height {
        for x in 0..width {
            let r = (x * 17 + 3) as u8; // hits 3, 20, 37, ... 54
            let g = (y * 51 + (x & 0x7) * 7) as u8;
            let b = ((x ^ y).wrapping_mul(29)) as u8;
            rgba.extend_from_slice(&[r, g, b, 0xFF]);
        }
    }
    rgba
}

#[test]
fn encode_decode_truecolor24_uncompressed_round_trip() {
    let rgba = gradient_rgba(16, 4);
    let img = tc24_image(16, 4, Compression::None, rgba.clone());
    let bytes = encode_ilbm(&img).expect("encode_ilbm tc24");
    let parsed = parse_ilbm(&bytes).expect("parse_ilbm tc24");
    assert_eq!(parsed.bmhd.n_planes, 24);
    assert!(parsed.palette.is_empty());
    assert_eq!(parsed.bmhd.compression, Compression::None);
    assert_eq!(parsed.rgba, rgba);
}

#[test]
fn encode_decode_truecolor24_byterun1_round_trip() {
    let rgba = gradient_rgba(32, 8);
    let img = tc24_image(32, 8, Compression::ByteRun1, rgba.clone());
    let bytes = encode_ilbm(&img).expect("encode_ilbm tc24 rle");
    let parsed = parse_ilbm(&bytes).expect("parse_ilbm tc24 rle");
    assert_eq!(parsed.bmhd.compression, Compression::ByteRun1);
    assert_eq!(parsed.rgba, rgba);
}

#[test]
fn encode_decode_truecolor24_auto_picks_rle_for_solid_fill() {
    // A solid colour: every row is the same byte, so each plane row is
    // also constant and ByteRun1 dramatically beats raw.
    let w: u16 = 64;
    let h: u16 = 16;
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for _ in 0..(w as usize) * (h as usize) {
        rgba.extend_from_slice(&[0x12, 0x34, 0x56, 0xFF]);
    }
    let img = tc24_image(w, h, Compression::Auto, rgba.clone());
    let bytes = encode_ilbm(&img).expect("encode_ilbm tc24 auto");
    let parsed = parse_ilbm(&bytes).expect("parse_ilbm tc24 auto");
    assert_eq!(parsed.bmhd.compression, Compression::ByteRun1);
    assert_eq!(parsed.rgba, rgba);

    // Cross-check: encoding the same image with explicit raw produces a
    // bigger file.
    let raw_img = tc24_image(w, h, Compression::None, rgba.clone());
    let raw_bytes = encode_ilbm(&raw_img).expect("encode_ilbm tc24 raw");
    assert!(
        bytes.len() < raw_bytes.len(),
        "Auto should beat raw for a solid fill: {} >= {}",
        bytes.len(),
        raw_bytes.len()
    );
}

#[test]
fn encode_truecolor24_omits_cmap_chunk() {
    // The literal-RGB BODY needs no palette; the serialised file must
    // not contain a `CMAP` chunk. (We grep for the four bytes literally
    // since BMHD / BODY are positioned by chunk walker and the file is
    // small enough to inspect.)
    let rgba = gradient_rgba(8, 2);
    let img = tc24_image(8, 2, Compression::None, rgba);
    let bytes = encode_ilbm(&img).expect("encode_ilbm tc24 no cmap");
    // FORM…ILBM at the start.
    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"ILBM");
    // No CMAP anywhere.
    assert!(
        !bytes.windows(4).any(|w| w == b"CMAP"),
        "encoded true-colour file unexpectedly contains a CMAP chunk"
    );
}

#[test]
fn decode_truecolor24_rejects_hasmask_plane() {
    // Hand-craft a minimal 24-plane file with HasMask flagged. The
    // decoder must refuse rather than silently treat the 25th plane as
    // a mask (the EGFF spec doesn't define `Masking::HasMask` semantics
    // for literal-RGB BODY).
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"FORM");
    bytes.extend_from_slice(&0u32.to_be_bytes()); // size patched
    bytes.extend_from_slice(b"ILBM");
    bytes.extend_from_slice(b"BMHD");
    bytes.extend_from_slice(&20u32.to_be_bytes());
    let bmhd = Bmhd {
        width: 8,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 24,
        masking: Masking::HasMask,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 8,
        page_height: 2,
    };
    bytes.extend_from_slice(&bmhd.write());
    // BODY: 24 planes * 2 rows + 1 mask plane * 2 rows; each row=2 bytes.
    bytes.extend_from_slice(b"BODY");
    let body_len = 25usize * 2 * 2;
    bytes.extend_from_slice(&(body_len as u32).to_be_bytes());
    bytes.resize(bytes.len() + body_len, 0);
    let form_size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&form_size.to_be_bytes());
    let err = parse_ilbm(&bytes).expect_err("HasMask + n_planes=24 must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("HasMask") || msg.contains("24"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn encode_truecolor24_rejects_ham_or_ehb_camg() {
    // HAM viewport with 24 bitplanes is illegal — HAM is a 6/8-plane
    // viewport. We expect a clear rejection at encode time.
    let rgba = gradient_rgba(8, 2);
    let mut img = tc24_image(8, 2, Compression::None, rgba.clone());
    img.camg = Camg { raw: CAMG_HAM };
    assert!(encode_ilbm(&img).is_err(), "tc24 + HAM should be rejected");
    let mut img = tc24_image(8, 2, Compression::None, rgba);
    img.camg = Camg { raw: CAMG_EHB };
    assert!(encode_ilbm(&img).is_err(), "tc24 + EHB should be rejected");
}

#[test]
fn muxer_truecolor24_roundtrip_through_temp_file() {
    let w: u32 = 16;
    let h: u32 = 8;
    let rgba = gradient_rgba(w as u16, h as u16);
    let path = unique_path("ilbm");
    let stream = rgba_stream(w, h);
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_mode(MuxerMode::TrueColor24)
            .with_compression(Compression::Auto);
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba.clone());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let parsed = parse_ilbm(&bytes).expect("parse_ilbm tc24 muxer");
    assert_eq!(parsed.bmhd.n_planes, 24);
    assert!(parsed.palette.is_empty());
    assert_eq!(&parsed.form_type, b"ILBM");
    assert_eq!(parsed.rgba, rgba);
    std::fs::remove_file(&path).ok();
}

#[test]
fn muxer_truecolor24_drops_alpha_to_opaque() {
    // The encoder discards the alpha channel because 24-bit ILBM has no
    // mask-plane or transparent-colour key. We feed a half-alpha source
    // and verify the decoder returns 0xFF for every pixel.
    let w: u16 = 8;
    let h: u16 = 2;
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for x in 0..(w as usize) * (h as usize) {
        rgba.extend_from_slice(&[(x * 13) as u8, (x * 23) as u8, (x * 41) as u8, 0x40]);
    }
    let img = tc24_image(w, h, Compression::None, rgba.clone());
    let bytes = encode_ilbm(&img).expect("encode tc24 drop alpha");
    let parsed = parse_ilbm(&bytes).expect("parse tc24 drop alpha");
    for px in parsed.rgba.chunks_exact(4) {
        assert_eq!(px[3], 0xFF);
    }
    // RGB bytes survive.
    for (i, px) in parsed.rgba.chunks_exact(4).enumerate() {
        assert_eq!(&px[..3], &rgba[i * 4..i * 4 + 3]);
    }
}

#[test]
fn decode_truecolor24_full_byte_range_per_channel() {
    // Build a 256x3 RGBA frame: row 0 = R sweep 0..256, row 1 = G sweep,
    // row 2 = B sweep. Confirms every plane bit position (0..=7) on every
    // channel survives encode → decode.
    let w: u16 = 256;
    let h: u16 = 3;
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for x in 0..256 {
        rgba.extend_from_slice(&[x as u8, 0, 0, 0xFF]);
    }
    for x in 0..256 {
        rgba.extend_from_slice(&[0, x as u8, 0, 0xFF]);
    }
    for x in 0..256 {
        rgba.extend_from_slice(&[0, 0, x as u8, 0xFF]);
    }
    let img = tc24_image(w, h, Compression::ByteRun1, rgba.clone());
    let bytes = encode_ilbm(&img).expect("encode tc24 sweep");
    let parsed = parse_ilbm(&bytes).expect("parse tc24 sweep");
    assert_eq!(parsed.rgba, rgba);
}

#[test]
fn encode_truecolor24_empty_palette_with_indexed_planes_still_rejected() {
    // Sanity guard: the relaxation of the "palette required" check is
    // scoped to n_planes==24. An n_planes=4 image with no palette must
    // still error.
    let bmhd = Bmhd {
        width: 8,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 4,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 8,
        page_height: 2,
    };
    let img = IlbmImage {
        width: 8,
        height: 2,
        bmhd,
        palette: Vec::new(),
        camg: Camg::default(),
        form_type: *b"ILBM",
        rgba: vec![0u8; 8 * 2 * 4],
        ..IlbmImage::default()
    };
    assert!(
        encode_ilbm(&img).is_err(),
        "indexed encode with empty palette should still fail"
    );
}

#[test]
fn decode_truecolor24_with_unexpected_cmap_ignored() {
    // Files in the wild sometimes carry a redundant CMAP even on
    // n_planes=24 (some authoring tools include a thumbnail palette).
    // The decoder should ignore the palette and still produce literal
    // RGB pixels.
    let rgba = gradient_rgba(8, 2);
    let img = tc24_image(8, 2, Compression::None, rgba.clone());
    let mut bytes = encode_ilbm(&img).expect("encode tc24 base");
    // Re-assemble manually with an injected CMAP after BMHD. Find BMHD
    // chunk end (12 byte FORM header + 8 byte BMHD header + 20 bytes).
    let bmhd_end = 12 + 8 + 20;
    let palette: &[u8] = &[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF];
    let mut cmap_chunk = Vec::new();
    cmap_chunk.extend_from_slice(b"CMAP");
    cmap_chunk.extend_from_slice(&(palette.len() as u32).to_be_bytes());
    cmap_chunk.extend_from_slice(palette);
    let mut spliced = Vec::with_capacity(bytes.len() + cmap_chunk.len());
    spliced.extend_from_slice(&bytes[..bmhd_end]);
    spliced.extend_from_slice(&cmap_chunk);
    spliced.extend_from_slice(&bytes[bmhd_end..]);
    // Patch FORM size.
    let new_form_size = (spliced.len() - 8) as u32;
    spliced[4..8].copy_from_slice(&new_form_size.to_be_bytes());
    std::mem::swap(&mut bytes, &mut spliced);

    let parsed = parse_ilbm(&bytes).expect("parse tc24 + redundant CMAP");
    assert_eq!(parsed.bmhd.n_planes, 24);
    assert_eq!(parsed.rgba, rgba);
    // The palette field still records what was on disk (so a parse →
    // encode round-trip can re-emit it). The pixel pipeline ignores it.
    assert_eq!(parsed.palette.len(), 2);
}

#[test]
fn muxer_truecolor24_emits_no_cmap() {
    let w: u32 = 8;
    let h: u32 = 2;
    let rgba: Vec<u8> = (0..(w * h))
        .flat_map(|i| {
            let v = (i % 256) as u8;
            [v, v.wrapping_add(0x40), v.wrapping_add(0x80), 0xFF]
        })
        .collect();
    let path = unique_path("ilbm");
    let stream = rgba_stream(w, h);
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_mode(MuxerMode::TrueColor24);
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba.clone());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes.windows(4).any(|w| w == b"CMAP"),
        "muxer TrueColor24 emitted a CMAP chunk"
    );
    std::fs::remove_file(&path).ok();
}
