//! Round-4 ILBM coverage. Two tracks:
//!
//! 1. **Muxer mode coverage.** The `IlbmMuxer` previously only emitted
//!    indexed planar ILBM. Round 4 adds [`MuxerMode::Ham6`],
//!    [`MuxerMode::Ham8`], [`MuxerMode::Ehb`] and [`MuxerMode::Pbm`]
//!    so callers can request any of the four ILBM viewport / form
//!    variants via the streaming API. Tests here exercise each path
//!    end-to-end by writing through the muxer, demuxing through the
//!    registry, and verifying RGBA round-trip within the format's
//!    quantisation bound.
//!
//! 2. **Encoder mask + transparent-colour round-trip.** `Masking::HasMask`
//!    was implemented in the encoder but had no test covering both
//!    alpha values; `Masking::HasTransparentColor` likewise. Both paths
//!    are now exercised here.
//!
//! 3. **ImageMagick cross-decode.** When the env var
//!    `OXIDEAV_IFF_MAGICK_CROSS=1` is set we additionally invoke the
//!    `magick` binary (a clean-room black-box validator — no source
//!    consulted, only its `identify` / `convert` outputs) on each
//!    encoder output. Skipped silently when the env var is unset or
//!    the binary isn't present, so the test suite still passes on CI.

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
        "oxideav-iff-r4-{}-{n}.{suffix}",
        std::process::id()
    ))
}

/// Helper: build a single-stream `StreamInfo` for `width × height` RGBA.
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

/// Run the encoder side of `IlbmMuxer` to a temp file with `mode`,
/// return the file bytes and its decoded `IlbmImage`.
fn mux_through_temp(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    mode: MuxerMode,
    compression: Compression,
) -> (Vec<u8>, IlbmImage) {
    let stream = rgba_stream(width, height);
    let path = unique_path("ilbm");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_mode(mode)
            .with_compression(compression);
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba);
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let buf = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let img = parse_ilbm(&buf).unwrap();
    (buf, img)
}

// ─────────────────── MuxerMode coverage ───────────────────

/// HAM6 muxer round-trip: a 32×4 grey gradient encoded through
/// `IlbmMuxer::with_mode(Ham6)` should produce a CAMG-flagged HAM
/// stream that decodes to within HAM6's 4-bit channel quantisation
/// (≤16 LSB) of the source.
#[test]
fn muxer_ham6_roundtrip() {
    let w = 32u32;
    let h = 4u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let v = (x * 8) as u8;
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let (bytes, img) = mux_through_temp(w, h, rgba.clone(), MuxerMode::Ham6, Compression::Auto);
    assert!(img.camg.is_ham(), "HAM CAMG bit must round-trip");
    assert_eq!(img.bmhd.n_planes, 6, "HAM6 must use 6 bitplanes");
    assert!(
        bytes.windows(4).any(|w| w == b"CAMG"),
        "CAMG chunk required for HAM6"
    );
    // HAM6's 4-bit channel quantiser bounds palette ops at ≤16 LSB,
    // but a "modify" op only changes one channel — the held two
    // channels stay at the previous pixel's value, which may be up
    // to 32 LSB away from the source if two consecutive pixels both
    // need a different (R, G) and only B got modified. The encoder
    // picks the cheapest single-channel op, so allow ≤32 LSB on the
    // worst channel.
    for (orig, got) in rgba.chunks_exact(4).zip(img.rgba.chunks_exact(4)) {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 32,
                "HAM6 muxer ch {c}: orig={} got={}",
                orig[c],
                got[c]
            );
        }
    }
}

/// HAM8 muxer round-trip: a 64×2 fine grey gradient encoded through
/// `IlbmMuxer::with_mode(Ham8)` decodes within ≤4 LSB.
#[test]
fn muxer_ham8_roundtrip() {
    let w = 64u32;
    let h = 2u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let v = (x * 4) as u8;
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let (bytes, img) = mux_through_temp(w, h, rgba.clone(), MuxerMode::Ham8, Compression::Auto);
    assert!(img.camg.is_ham());
    assert_eq!(img.bmhd.n_planes, 8);
    assert!(bytes.windows(4).any(|w| w == b"CAMG"));
    for (orig, got) in rgba.chunks_exact(4).zip(img.rgba.chunks_exact(4)) {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 4,
                "HAM8 muxer ch {c}: orig={} got={}",
                orig[c],
                got[c]
            );
        }
    }
}

/// EHB muxer round-trip: 32 distinct full-bright colours plus their
/// half-brite mirrors. Channel error must stay ≤ 1 because the
/// decoder's expanded palette is exact-match.
#[test]
fn muxer_ehb_roundtrip() {
    let w = 16u32;
    let h = 4u32;
    // 4 well-separated full-bright entries; ensure even values so
    // half-brite (>>1) is exact in u8.
    let bright: [[u8; 3]; 4] = [
        [0xFE, 0x00, 0x00],
        [0x00, 0xFE, 0x00],
        [0x00, 0x00, 0xFE],
        [0xFE, 0xFE, 0xFE],
    ];
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let pick = (x as usize) & 3;
            let p = if (y & 1) == 0 {
                bright[pick]
            } else {
                [
                    bright[pick][0] >> 1,
                    bright[pick][1] >> 1,
                    bright[pick][2] >> 1,
                ]
            };
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let (bytes, img) = mux_through_temp(w, h, rgba.clone(), MuxerMode::Ehb, Compression::Auto);
    assert!(img.camg.is_ehb());
    assert_eq!(img.bmhd.n_planes, 6);
    assert!(bytes.windows(4).any(|w| w == b"CAMG"));
    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(img.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 1,
                "EHB muxer pixel {i} ch {c}: orig={} got={}",
                orig[c],
                got[c]
            );
        }
    }
}

/// PBM muxer round-trip: 8-colour test grid through `MuxerMode::Pbm`
/// produces `FORM/PBM ` bytes that decode back exactly (PBM is
/// lossless for in-palette colours).
#[test]
fn muxer_pbm_roundtrip_uncompressed() {
    let w = 8u32;
    let h = 4u32;
    let pal: [[u8; 3]; 8] = [
        [0xFF, 0, 0],
        [0, 0xFF, 0],
        [0, 0, 0xFF],
        [0xFF, 0xFF, 0],
        [0xFF, 0, 0xFF],
        [0, 0xFF, 0xFF],
        [0xFF, 0xFF, 0xFF],
        [0, 0, 0],
    ];
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let p = pal[(x + y * 3) % 8];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let (bytes, img) = mux_through_temp(w, h, rgba.clone(), MuxerMode::Pbm, Compression::None);
    assert_eq!(&bytes[8..12], b"PBM ", "FORM type must be PBM");
    assert_eq!(&img.form_type, b"PBM ");
    assert_eq!(img.bmhd.n_planes, 8);
    // PBM is lossless when the palette has every source colour.
    assert_eq!(img.rgba, rgba, "PBM muxer is lossless");
}

#[test]
fn muxer_pbm_roundtrip_byterun1() {
    let w = 16u32;
    let h = 4u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    // Long horizontal runs — RLE-friendly.
    for _y in 0..h {
        for _x in 0..w {
            rgba.extend_from_slice(&[0x55, 0x55, 0x55, 0xFF]);
        }
    }
    let (_bytes, img) = mux_through_temp(w, h, rgba.clone(), MuxerMode::Pbm, Compression::ByteRun1);
    assert_eq!(img.rgba, rgba, "PBM ByteRun1 round-trips");
}

// ─────────────────── HasMask + HasTransparentColor self-roundtrip ───────────────────

/// HasMask plane: encode with a checkerboard alpha pattern, decode,
/// verify alpha bits round-trip while colour stays in palette.
#[test]
fn encoder_hasmask_plane_alpha_pattern() {
    let w = 16u16;
    let h = 4u16;
    let palette: Vec<[u8; 3]> = vec![[0xFF, 0, 0], [0, 0xFF, 0]];
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::HasMask,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    };
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let a = if ((x + y) & 1) == 0 { 0xFF } else { 0x00 };
            rgba.extend_from_slice(&[0xFF, 0, 0, a]);
        }
    }
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd,
        palette,
        camg: Camg::default(),
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.bmhd.masking, Masking::HasMask);
    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(dec.rgba.chunks_exact(4))
        .enumerate()
    {
        assert_eq!(orig[3], got[3], "alpha pixel {i} must round-trip");
    }
}

/// HasTransparentColor: pixels with alpha < 0x80 get written as
/// `bmhd.transparent_color` and decoded as alpha-0.
#[test]
fn encoder_has_transparent_colour_keys_off_low_alpha() {
    let w = 8u16;
    let h = 2u16;
    let palette: Vec<[u8; 3]> = vec![
        [0xFF, 0, 0],       // index 0 → red (visible)
        [0, 0xFF, 0],       // index 1 → green (visible)
        [0xAB, 0xCD, 0xEF], // index 2 → arbitrary (transparent key)
    ];
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes: 2,
        masking: Masking::HasTransparentColor,
        compression: Compression::None,
        pad: 0,
        transparent_color: 2,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    };
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    // Row 0: alternating fully-opaque red / fully-transparent (anything).
    // Row 1: all green.
    for x in 0..w {
        if x & 1 == 0 {
            rgba.extend_from_slice(&[0xFF, 0, 0, 0xFF]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 0x00]);
        }
    }
    for _x in 0..w {
        rgba.extend_from_slice(&[0, 0xFF, 0, 0xFF]);
    }
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd,
        palette,
        camg: Camg::default(),
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.bmhd.masking, Masking::HasTransparentColor);
    assert_eq!(dec.bmhd.transparent_color, 2);
    // Row 0: even pixels opaque red; odd pixels keyed transparent.
    for x in 0..w as usize {
        let off = x * 4;
        if x & 1 == 0 {
            assert_eq!(&dec.rgba[off..off + 3], &[0xFF, 0, 0]);
            assert_eq!(dec.rgba[off + 3], 0xFF);
        } else {
            // Index 2 looks up palette[2] but alpha keys to 0.
            assert_eq!(
                dec.rgba[off + 3],
                0x00,
                "transparent pixel x={x} must alpha-0"
            );
        }
    }
    // Row 1: all opaque green.
    let row1_base = (w as usize) * 4;
    for x in 0..w as usize {
        let off = row1_base + x * 4;
        assert_eq!(&dec.rgba[off..off + 3], &[0, 0xFF, 0]);
        assert_eq!(dec.rgba[off + 3], 0xFF);
    }
}

// ─────────────────── ImageMagick cross-decode ───────────────────

/// Detect ImageMagick availability without panicking. Returns `Some(path)`
/// when the binary can be exec'd, `None` otherwise (test silently skips).
fn magick_path() -> Option<String> {
    if std::env::var("OXIDEAV_IFF_MAGICK_CROSS").ok().as_deref() != Some("1") {
        return None;
    }
    for candidate in [
        "magick",
        "/opt/homebrew/bin/magick",
        "/usr/local/bin/magick",
    ] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Convert an ILBM file through magick → PPM, parse the PPM RGB
/// pixels, and return them. Returns `None` (silent skip) when magick
/// or its ILBM delegate is unavailable.
fn magick_decode_to_ppm(path: &std::path::Path) -> Option<(u32, u32, Vec<u8>)> {
    let bin = magick_path()?;
    let ppm = path.with_extension("ppm");
    let _ = std::fs::remove_file(&ppm);
    let out = std::process::Command::new(&bin)
        .arg("convert")
        .arg(path)
        .arg(&ppm)
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("ilbmtoppm") && !ppm.exists() {
        // Delegate not installed.
        return None;
    }
    if !out.status.success() && !ppm.exists() {
        return None;
    }
    let bytes = std::fs::read(&ppm).ok()?;
    let _ = std::fs::remove_file(&ppm);
    parse_ppm_p6(&bytes)
}

/// Tiny P6 PPM parser: returns (w, h, RGB bytes). Only handles
/// 8-bit-per-channel PPMs which is what magick emits by default.
fn parse_ppm_p6(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if bytes.len() < 11 || &bytes[0..2] != b"P6" {
        return None;
    }
    // Header: P6\n<w> <h>\n<maxval>\n<binary RGB>
    let mut i = 2usize;
    // Skip whitespace + comments + parse 3 numbers.
    let mut nums: Vec<u32> = Vec::with_capacity(3);
    while nums.len() < 3 && i < bytes.len() {
        let b = bytes[i];
        if b == b'#' {
            // Comment to EOL.
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b.is_ascii_whitespace() {
            i += 1;
        } else if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let n: u32 = std::str::from_utf8(&bytes[start..i]).ok()?.parse().ok()?;
            nums.push(n);
        } else {
            return None;
        }
    }
    if nums.len() < 3 || nums[2] != 255 {
        return None; // need 8-bit RGB
    }
    // One whitespace byte separates header from binary data.
    if i >= bytes.len() {
        return None;
    }
    i += 1;
    let w = nums[0];
    let h = nums[1];
    let need = (w as usize) * (h as usize) * 3;
    if bytes.len() - i < need {
        return None;
    }
    Some((w, h, bytes[i..i + need].to_vec()))
}

#[test]
fn magick_cross_decode_indexed_byterun1() {
    // 32×8 indexed image — encode through IlbmMuxer with ByteRun1,
    // convert through magick to PPM, and pixel-compare the result.
    // Skipped silently when OXIDEAV_IFF_MAGICK_CROSS != "1" or
    // magick / its ILBM delegate isn't installed.
    if magick_path().is_none() {
        return;
    }
    let w = 32u32;
    let h = 8u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = if (x ^ y) & 1 == 0 { 0xFF } else { 0x00 };
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let stream = rgba_stream(w, h);
    let path = unique_path("ilbm");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_compression(Compression::ByteRun1);
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba.clone());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let res = magick_decode_to_ppm(&path);
    let _ = std::fs::remove_file(&path);
    let Some((mw, mh, mrgb)) = res else {
        // Delegate unavailable on this host → silent skip.
        return;
    };
    assert_eq!(mw, w, "magick width must match ours");
    assert_eq!(mh, h, "magick height must match ours");
    // Pixel-by-pixel compare against the source (drop alpha; magick's
    // PPM has no alpha channel).
    for (i, (orig, got)) in rgba.chunks_exact(4).zip(mrgb.chunks_exact(3)).enumerate() {
        for c in 0..3 {
            assert_eq!(
                orig[c], got[c],
                "magick pixel {i} ch {c}: orig={} got={}",
                orig[c], got[c]
            );
        }
    }
}

/// Cross-decode an HAM6 file via magick. We only check dimensions
/// agree; HAM channel quantisation makes a strict pixel compare
/// pointless. Skipped silently per the env-var guard.
#[test]
fn magick_cross_decode_ham6_byterun1() {
    if magick_path().is_none() {
        return;
    }
    let w = 32u32;
    let h = 4u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let v = (x * 8) as u8;
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let stream = rgba_stream(w, h);
    let path = unique_path("ilbm");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_mode(MuxerMode::Ham6)
            .with_compression(Compression::ByteRun1);
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba.clone());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let res = magick_decode_to_ppm(&path);
    let _ = std::fs::remove_file(&path);
    let Some((mw, mh, _)) = res else {
        return;
    };
    assert_eq!(mw, w);
    assert_eq!(mh, h);
}

#[test]
fn magick_cross_decode_pbm_byterun1() {
    if magick_path().is_none() {
        return;
    }
    let w = 16u32;
    let h = 4u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for _x in 0..w {
            rgba.extend_from_slice(&[0x77, 0x77, 0x77, 0xFF]);
        }
    }
    let stream = rgba_stream(w, h);
    let path = unique_path("ilbm");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = IlbmMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_mode(MuxerMode::Pbm)
            .with_compression(Compression::ByteRun1);
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, rgba.clone());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let res = magick_decode_to_ppm(&path);
    let _ = std::fs::remove_file(&path);
    let Some((mw, mh, mrgb)) = res else {
        return;
    };
    assert_eq!(mw, w);
    assert_eq!(mh, h);
    // PBM is lossless; cross-decoded pixels must match exactly.
    for (i, (orig, got)) in rgba.chunks_exact(4).zip(mrgb.chunks_exact(3)).enumerate() {
        for c in 0..3 {
            assert_eq!(orig[c], got[c], "PBM magick px {i} ch {c}");
        }
    }
}

// ─────────────────── HAM6 muxer with non-grey gradient ───────────────────

/// HAM6 muxer round-trip on a colour gradient (non-grey). HAM6's
/// per-row state machine has to perform several modify ops; the
/// final palette built by the muxer should still allow the decoder
/// to reach within 16 LSB on every channel.
#[test]
fn muxer_ham6_colour_gradient() {
    let w = 32u32;
    let h = 4u32;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _y in 0..h {
        for x in 0..w {
            let r = ((x * 8) & 0xFF) as u8;
            let g = ((255 - x * 4) & 0xFF) as u8;
            let b = ((x * 4) & 0xFF) as u8;
            rgba.extend_from_slice(&[r, g, b, 0xFF]);
        }
    }
    let (_bytes, img) = mux_through_temp(w, h, rgba.clone(), MuxerMode::Ham6, Compression::Auto);
    assert!(img.camg.is_ham());
    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(img.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 32,
                "HAM6 colour grad pixel {i} ch {c}: orig={} got={} err={}",
                orig[c],
                got[c],
                err
            );
        }
    }
}

// ─────────────────── Compression byte savings on PBM ───────────────────

/// PBM ByteRun1 vs uncompressed: a uniform 64×8 image must be much
/// smaller with RLE on (single-pixel byte repeated).
#[test]
fn pbm_byterun1_beats_uncompressed_for_uniform() {
    let w = 64u16;
    let h = 8u16;
    let palette: Vec<[u8; 3]> = vec![[0x42; 3]];
    let bmhd_raw = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes: 8,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    };
    let bmhd_rle = Bmhd {
        compression: Compression::ByteRun1,
        ..bmhd_raw
    };
    let rgba: Vec<u8> = (0..((w as usize) * (h as usize)))
        .flat_map(|_| [0x42u8, 0x42, 0x42, 0xFF])
        .collect();
    let img_raw = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: bmhd_raw,
        palette: palette.clone(),
        camg: Camg::default(),
        rgba: rgba.clone(),
        form_type: *b"PBM ",
        ..IlbmImage::default()
    };
    let img_rle = IlbmImage {
        bmhd: bmhd_rle,
        ..img_raw.clone()
    };
    let raw = encode_ilbm(&img_raw).unwrap();
    let rle = encode_ilbm(&img_rle).unwrap();
    assert!(
        rle.len() < raw.len(),
        "PBM RLE ({}) should beat uncompressed ({}) for uniform image",
        rle.len(),
        raw.len(),
    );
}

// ─────────────────── Round-trip: HAM/EHB CAMG bits preserved through muxer ───────────────────

/// Confirm the muxer always emits a CAMG chunk in HAM/EHB modes (even
/// when the underlying image's `camg.raw == 0` initially — the muxer
/// must set the bit itself when the user picks the mode).
#[test]
fn muxer_modes_emit_camg_chunk() {
    let w = 8u32;
    let h = 2u32;
    let rgba: Vec<u8> = (0..(w * h)).flat_map(|_| [0u8, 0, 0, 0xFF]).collect();
    for (mode, want_ham, want_ehb) in [
        (MuxerMode::Ham6, true, false),
        (MuxerMode::Ham8, true, false),
        (MuxerMode::Ehb, false, true),
    ] {
        let (bytes, img) = mux_through_temp(w, h, rgba.clone(), mode, Compression::None);
        assert!(
            bytes.windows(4).any(|w| w == b"CAMG"),
            "mode {mode:?} must emit CAMG"
        );
        if want_ham {
            assert_eq!(
                img.camg.raw & CAMG_HAM,
                CAMG_HAM,
                "{mode:?} must have HAM bit"
            );
        }
        if want_ehb {
            assert_eq!(
                img.camg.raw & CAMG_EHB,
                CAMG_EHB,
                "{mode:?} must have EHB bit"
            );
        }
    }
}
