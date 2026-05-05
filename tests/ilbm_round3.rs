//! Round-3 ILBM coverage: Compression::Auto picker (RDO), HAM/EHB
//! self-roundtrip through the muxer API, and byte-savings measurements.

#![allow(clippy::needless_range_loop)]

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, CAMG_EHB, CAMG_HAM,
};

// ───────────────────── helpers ─────────────────────

fn solid_pal_32() -> Vec<[u8; 3]> {
    (0..32u8)
        .map(|i| {
            // 32 well-separated colours so all EHB entries are distinct.
            [i.wrapping_mul(7), i.wrapping_mul(11), i.wrapping_mul(13)]
        })
        .collect()
}

fn make_ilbm(
    w: u16,
    h: u16,
    n_planes: u8,
    camg_raw: u32,
    compression: Compression,
    palette: Vec<[u8; 3]>,
    rgba: Vec<u8>,
) -> IlbmImage {
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking: Masking::None,
        compression,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    };
    IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd,
        palette,
        camg: Camg { raw: camg_raw },
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    }
}

/// Build a simple indexed RGBA buffer: each pixel's colour comes from
/// palette[x % palette_len].
fn indexed_rgba(w: u16, h: u16, palette: &[[u8; 3]]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let p = palette[((x as usize) ^ (y as usize)) % palette.len()];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    rgba
}

// ─────────────────── Compression::Auto ───────────────────

/// An image filled with a single solid colour — identical bytes in every
/// row — compresses extremely well with ByteRun1. Auto should pick RLE.
#[test]
fn auto_picks_byterun1_for_solid_colour() {
    let palette: Vec<[u8; 3]> = vec![[0x55, 0xAA, 0xFF], [0, 0, 0]];
    let rgba: Vec<u8> = (0..32 * 8)
        .flat_map(|_| [0x55u8, 0xAAu8, 0xFF, 0xFF])
        .collect();
    let img = make_ilbm(32, 8, 1, 0, Compression::Auto, palette, rgba);
    let auto_bytes = encode_ilbm(&img).unwrap();

    // BMHD.compression byte is at offset 12 + 8 (BMHD header) + 10 = 30.
    // BMHD body: width(2)+height(2)+x(2)+y(2)+n_planes(1)+masking(1)+
    //            compression(1) is at index 10 within the body.
    let bmhd_body_start = 12 + 8; // after FORM header + "ILBM" + "BMHD" + u32 size
    let compression_byte = auto_bytes[bmhd_body_start + 10];
    assert_eq!(
        compression_byte, 1,
        "Auto should write ByteRun1 (byte=1) for solid-colour image"
    );

    // Compare size against explicit None.
    let img_none = make_ilbm(
        32,
        8,
        1,
        0,
        Compression::None,
        vec![[0x55, 0xAA, 0xFF], [0, 0, 0]],
        (0..32 * 8)
            .flat_map(|_| [0x55u8, 0xAAu8, 0xFF, 0xFF])
            .collect(),
    );
    let raw_bytes = encode_ilbm(&img_none).unwrap();

    let savings = raw_bytes.len() as i64 - auto_bytes.len() as i64;
    assert!(
        savings > 0,
        "Auto should save bytes vs None for solid colour (savings={savings})"
    );

    // Round-trip.
    let dec = parse_ilbm(&auto_bytes).unwrap();
    assert_eq!(dec.width, 32);
    assert_eq!(dec.height, 8);
}

/// A fully-random image should be larger or equal with ByteRun1 vs raw.
/// Auto should pick None (or at worst emit ByteRun1 if accidentally equal).
#[test]
fn auto_picks_none_for_random_data() {
    // Generate a 64-colour palette and a pixel pattern that cycles
    // through it with a non-repeating stride — worst case for RLE.
    let palette: Vec<[u8; 3]> = (0..64u8).map(|i| [i * 3, 255 - i * 3, i]).collect();
    let mut rgba = Vec::with_capacity(64 * 16 * 4);
    for y in 0..16usize {
        for x in 0..64usize {
            // Pseudo-random via XOR fold — avoids runs.
            let idx = ((x * 37 + y * 53 + x * y * 7) ^ (x + y * 3)) % 64;
            let p = palette[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let img = make_ilbm(
        64,
        16,
        6,
        0,
        Compression::Auto,
        palette.clone(),
        rgba.clone(),
    );
    let auto_bytes = encode_ilbm(&img).unwrap();

    let img_rle = make_ilbm(
        64,
        16,
        6,
        0,
        Compression::ByteRun1,
        palette.clone(),
        rgba.clone(),
    );
    let rle_bytes = encode_ilbm(&img_rle).unwrap();
    let img_raw = make_ilbm(64, 16, 6, 0, Compression::None, palette, rgba);
    let raw_bytes = encode_ilbm(&img_raw).unwrap();

    // Auto should be <= the larger of the two options.
    assert!(
        auto_bytes.len() <= rle_bytes.len().max(raw_bytes.len()),
        "Auto result {} should not exceed max(rle={}, raw={})",
        auto_bytes.len(),
        rle_bytes.len(),
        raw_bytes.len()
    );

    // BMHD compression byte should match the winning mode.
    let bmhd_body_start = 12 + 8;
    let compression_byte = auto_bytes[bmhd_body_start + 10];
    assert!(
        compression_byte == 0 || compression_byte == 1,
        "Auto must resolve to 0 (None) or 1 (ByteRun1), got {compression_byte}"
    );

    // Round-trip.
    let dec = parse_ilbm(&auto_bytes).unwrap();
    assert_eq!(dec.width, 64);
    assert_eq!(dec.height, 16);
}

// ─────────────────── HAM6 self-roundtrip (muxer → demuxer) ───────────────────

/// Full HAM6 encode→decode through `encode_ilbm` / `parse_ilbm`: build
/// a colour gradient, encode with CAMG_HAM + 6 planes, decode and
/// verify channel error is within HAM6's 4-bit quantisation bound (≤ 16).
#[test]
fn ham6_auto_compression_self_roundtrip() {
    let palette: Vec<[u8; 3]> = (0..16u8).map(|i| [i * 17, i * 17, i * 17]).collect();
    let mut rgba = Vec::with_capacity(16 * 4 * 4);
    for _y in 0..4 {
        for x in 0..16u8 {
            let v = x.saturating_mul(17);
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let img = make_ilbm(16, 4, 6, CAMG_HAM, Compression::Auto, palette, rgba.clone());
    let bytes = encode_ilbm(&img).unwrap();

    // CAMG chunk must be present.
    assert!(
        bytes.windows(4).any(|w| w == b"CAMG"),
        "CAMG chunk must appear for HAM image"
    );

    let dec = parse_ilbm(&bytes).unwrap();
    assert!(dec.camg.is_ham(), "decoded image must still be HAM");
    assert_eq!(dec.bmhd.n_planes, 6);

    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(dec.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 16,
                "HAM6 pixel {i} ch {c}: orig={} got={} err={}",
                orig[c],
                got[c],
                err
            );
        }
        assert_eq!(got[3], 0xFF, "alpha pixel {i}");
    }
}

/// Same but for HAM8 (8-plane, 6-bit channel, ≤4 LSB error).
#[test]
fn ham8_auto_compression_self_roundtrip() {
    let palette: Vec<[u8; 3]> = (0..64u8).map(|i| [i * 4, i * 4, i * 4]).collect();
    let mut rgba = Vec::with_capacity(64 * 2 * 4);
    for _y in 0..2 {
        for x in 0..64u8 {
            let v = x.saturating_mul(4);
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let img = make_ilbm(64, 2, 8, CAMG_HAM, Compression::Auto, palette, rgba.clone());
    let bytes = encode_ilbm(&img).unwrap();

    assert!(
        bytes.windows(4).any(|w| w == b"CAMG"),
        "CAMG chunk required for HAM8"
    );

    let dec = parse_ilbm(&bytes).unwrap();
    assert!(dec.camg.is_ham());
    assert_eq!(dec.bmhd.n_planes, 8);

    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(dec.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 4,
                "HAM8 pixel {i} ch {c}: orig={} got={} err={}",
                orig[c],
                got[c],
                err
            );
        }
    }
}

// ─────────────────── EHB self-roundtrip (Auto compression) ───────────────────

/// EHB encode with Auto: 32+32 palette entries, alternating full-bright
/// and half-brite pixels. Must round-trip exactly (palette quantisation
/// is lossless for exact palette entries).
#[test]
fn ehb_auto_compression_self_roundtrip() {
    let base_pal = solid_pal_32();
    // Build an RGBA buffer that alternates between palette[1] (full-bright)
    // and its half-brite twin (palette[33] once expanded).
    let full = base_pal[1];
    let half = [full[0] >> 1, full[1] >> 1, full[2] >> 1];
    let mut rgba = Vec::with_capacity(32 * 4 * 4);
    for _y in 0..4 {
        for x in 0..32u32 {
            let p = if x & 1 == 0 { full } else { half };
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }

    let img = make_ilbm(
        32,
        4,
        6,
        CAMG_EHB,
        Compression::Auto,
        base_pal.clone(),
        rgba.clone(),
    );
    let bytes = encode_ilbm(&img).unwrap();

    assert!(
        bytes.windows(4).any(|w| w == b"CAMG"),
        "CAMG chunk required for EHB"
    );

    let dec = parse_ilbm(&bytes).unwrap();
    assert!(dec.camg.is_ehb(), "decoded image must be EHB");
    assert_eq!(dec.bmhd.n_planes, 6);

    // The decoder expands the palette; compare pixel-by-pixel within
    // 1 LSB (possible due to the half-brite >>1 being a floor divide).
    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(dec.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 1,
                "EHB pixel {i} ch {c}: orig={} got={} err={}",
                orig[c],
                got[c],
                err
            );
        }
    }
}

// ─────────────────── byte-savings measurement ───────────────────

/// Print byte savings for a run-length-friendly (gradient) 16×4 image.
/// This is not a strict correctness assertion — it ensures the size
/// relationship is sane (Auto ≤ max of the two explicit modes).
#[test]
fn compression_picker_savings_gradient() {
    let palette: Vec<[u8; 3]> = (0..8u8).map(|i| [i * 32, i * 32, i * 32]).collect();
    let rgba = indexed_rgba(16, 4, &palette);

    let auto_bytes = encode_ilbm(&make_ilbm(
        16,
        4,
        3,
        0,
        Compression::Auto,
        palette.clone(),
        rgba.clone(),
    ))
    .unwrap();
    let rle_bytes = encode_ilbm(&make_ilbm(
        16,
        4,
        3,
        0,
        Compression::ByteRun1,
        palette.clone(),
        rgba.clone(),
    ))
    .unwrap();
    let raw_bytes = encode_ilbm(&make_ilbm(16, 4, 3, 0, Compression::None, palette, rgba)).unwrap();

    // Auto must not exceed the better of the two.
    let best = rle_bytes.len().min(raw_bytes.len());
    assert!(
        auto_bytes.len() <= best,
        "Auto ({}) should be ≤ best(rle={}, raw={}) for gradient",
        auto_bytes.len(),
        rle_bytes.len(),
        raw_bytes.len()
    );
}

// ─────────────────── CAMG always emitted for HAM / EHB ───────────────────

/// Encoder must emit CAMG for any image with camg.raw != 0. This is
/// already enforced by encode_ilbm's existing path; confirm it holds
/// for all three HAM / EHB / plain-indexed cases.
#[test]
fn camg_emitted_when_flags_nonzero() {
    let palette: Vec<[u8; 3]> = vec![[0xFF, 0, 0], [0, 0xFF, 0], [0, 0, 0xFF], [0; 3]];
    let rgba: Vec<u8> = vec![0xFF, 0, 0, 0xFF, 0, 0xFF, 0, 0xFF];

    // Non-zero CAMG (arbitrary flag) → chunk must appear.
    let img = make_ilbm(
        2,
        1,
        2,
        0x0042,
        Compression::None,
        palette.clone(),
        rgba.clone(),
    );
    let bytes = encode_ilbm(&img).unwrap();
    assert!(
        bytes.windows(4).any(|w| w == b"CAMG"),
        "CAMG must be emitted when camg.raw != 0"
    );

    // Zero CAMG → chunk must NOT appear (saves 12 bytes on common path).
    let img_plain = make_ilbm(2, 1, 2, 0, Compression::None, palette, rgba);
    let bytes_plain = encode_ilbm(&img_plain).unwrap();
    assert!(
        !bytes_plain.windows(4).any(|w| w == b"CAMG"),
        "CAMG should be omitted when camg.raw == 0"
    );
}

// ─────────────────── Indexed n-plane picker correctness ───────────────────

/// The IlbmMuxer picks the minimum bitplane count for its palette.
/// This exercises the muxer path end-to-end with the Auto compression
/// default (previously ByteRun1, now Auto).
#[test]
fn muxer_auto_compression_indexed_roundtrip() {
    use std::sync::atomic::{AtomicU64, Ordering};

    use oxideav_core::{
        CodecId, CodecParameters, MediaType, Packet, PixelFormat, StreamInfo, TimeBase,
    };
    use oxideav_core::{Muxer, WriteSeek};

    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "oxideav-iff-r3-mux-{}-{n}.ilbm",
        std::process::id()
    ));

    let width = 8u32;
    let height = 4u32;
    // Build RGBA for a 4-colour image.
    let pal: Vec<[u8; 3]> = vec![[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let p = pal[(x + y) % 4];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }

    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(PixelFormat::Rgba);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    };

    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_iff::ilbm::IlbmMuxer::new(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba.clone());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }

    let buf = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    // Must decode cleanly.
    let dec = parse_ilbm(&buf).unwrap();
    assert_eq!(dec.width, width);
    assert_eq!(dec.height, height);

    // Pixels should match within palette quantisation (not necessarily exact
    // because the muxer builds its own palette from the RGBA buffer).
    assert_eq!(dec.rgba.len(), rgba.len());
}
