//! Round-82 coverage: ILBM `CRNG` (DeluxePaint colour-range cycling)
//! and `CCRT` (Graphicraft colour-cycling timing) chunks.
//!
//! Each test builds an in-memory ILBM with one or more CRNG / CCRT
//! chunks attached, round-trips it through [`encode_ilbm`] +
//! [`parse_ilbm`], and verifies that:
//!
//! * the resulting file contains the expected number of `CRNG` /
//!   `CCRT` chunks,
//! * each parsed entry equals the original byte-for-byte, and
//! * the convenience accessors ([`Crng::cycles_per_second`],
//!   [`Crng::is_active`], [`Ccrt::delay_seconds`], etc.) report the
//!   spec-documented values.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Ccrt, Compression, Crng, IlbmImage, Masking,
};

fn make_indexed_image(palette: Vec<[u8; 3]>) -> IlbmImage {
    let w: u16 = 8;
    let h: u16 = 2;
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        for x in 0..w {
            let idx = ((x as usize) + (y as usize)) % palette.len();
            let p = palette[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes: 4, // 16-entry palette
        masking: Masking::None,
        compression: Compression::None,
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
        camg: Camg::default(),
        rgba,
        ..IlbmImage::default()
    }
}

fn make_16_color_image() -> IlbmImage {
    let pal: Vec<[u8; 3]> = (0..16u8)
        .map(|i| [i * 16, 255 - i * 16, i.wrapping_mul(17)])
        .collect();
    make_indexed_image(pal)
}

// ───────────────────── CRNG ─────────────────────

#[test]
fn crng_single_active_roundtrip() {
    let mut img = make_16_color_image();
    img.crngs.push(Crng {
        pad1: 0,
        rate: 16384, // one step per VBL tick ≈ 60 Hz
        flags: Crng::FLAG_ACTIVE,
        low: 4,
        high: 9,
    });
    let bytes = encode_ilbm(&img).unwrap();
    // CRNG FourCC must appear in the file (and its 8-byte payload).
    assert!(
        bytes.windows(4).any(|w| w == b"CRNG"),
        "encoded file should contain a CRNG chunk"
    );
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.crngs.len(), 1, "exactly one CRNG must round-trip");
    let c = dec.crngs[0];
    assert_eq!(c, img.crngs[0], "CRNG bytes must round-trip identical");
    assert!(c.is_active());
    assert!(!c.is_reverse());
    assert_eq!(c.range_len(), 6);
    let hz = c.cycles_per_second();
    assert!((hz - 60.0).abs() < 1e-3, "rate=16384 → ~60 Hz, got {hz}");
}

#[test]
fn crng_multiple_preserve_order() {
    let mut img = make_16_color_image();
    img.crngs.push(Crng {
        pad1: 0,
        rate: 8192,
        flags: Crng::FLAG_ACTIVE,
        low: 0,
        high: 3,
    });
    img.crngs.push(Crng {
        pad1: 0,
        rate: 4096,
        flags: Crng::FLAG_ACTIVE | Crng::FLAG_REVERSE,
        low: 4,
        high: 7,
    });
    img.crngs.push(Crng {
        pad1: 0,
        rate: 16384,
        flags: 0, // inactive
        low: 8,
        high: 15,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.crngs.len(), 3);
    assert_eq!(dec.crngs, img.crngs, "order and bytes must preserve");
    assert!(dec.crngs[0].is_active());
    assert!(!dec.crngs[0].is_reverse());
    assert!(dec.crngs[1].is_active());
    assert!(dec.crngs[1].is_reverse());
    assert!(!dec.crngs[2].is_active());
    // Re-encode and check the byte stream is identical.
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2, "re-encode must be byte-stable");
}

#[test]
fn crng_inactive_zero_rate_zero_hz() {
    let c = Crng {
        pad1: 0,
        rate: 0,
        flags: 0,
        low: 0,
        high: 15,
    };
    assert!(!c.is_active());
    assert_eq!(c.cycles_per_second(), 0.0);
    assert_eq!(c.range_len(), 16);
}

#[test]
fn crng_inverted_range_zero_len() {
    // low > high: malformed but tolerated by the parser. Helper
    // returns 0 rather than wrapping.
    let c = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 10,
        high: 4,
    };
    assert_eq!(c.range_len(), 0);
}

#[test]
fn crng_rejects_short_chunk() {
    // 7-byte payload (one short) must fail parse.
    let short = [0u8; 7];
    assert!(Crng::parse(&short).is_err());
}

// ───────────────────── CCRT ─────────────────────

#[test]
fn ccrt_forward_roundtrip() {
    let mut img = make_16_color_image();
    img.ccrts.push(Ccrt {
        direction: 1,
        start: 2,
        end: 11,
        seconds: 0,
        micros: 250_000, // 0.25 s
        pad: 0,
    });
    let bytes = encode_ilbm(&img).unwrap();
    assert!(
        bytes.windows(4).any(|w| w == b"CCRT"),
        "encoded file should contain a CCRT chunk"
    );
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.ccrts.len(), 1);
    let c = dec.ccrts[0];
    assert_eq!(c, img.ccrts[0]);
    assert!(c.is_active());
    assert!(!c.is_reverse());
    assert_eq!(c.range_len(), 10);
    let secs = c.delay_seconds();
    assert!((secs - 0.25).abs() < 1e-9, "0.25 s delay, got {secs}");
}

#[test]
fn ccrt_backward_roundtrip() {
    let mut img = make_16_color_image();
    img.ccrts.push(Ccrt {
        direction: -1,
        start: 0,
        end: 5,
        seconds: 1,
        micros: 0,
        pad: 0,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.ccrts.len(), 1);
    let c = dec.ccrts[0];
    assert_eq!(c, img.ccrts[0]);
    assert!(c.is_active());
    assert!(c.is_reverse());
    assert!((c.delay_seconds() - 1.0).abs() < 1e-9);
}

#[test]
fn ccrt_inactive_direction_zero() {
    let c = Ccrt {
        direction: 0,
        start: 0,
        end: 15,
        seconds: 0,
        micros: 100_000,
        pad: 0,
    };
    assert!(!c.is_active());
    assert!(!c.is_reverse());
}

#[test]
fn ccrt_multiple_preserve_order() {
    let mut img = make_16_color_image();
    img.ccrts.push(Ccrt {
        direction: 1,
        start: 0,
        end: 3,
        seconds: 0,
        micros: 500_000,
        pad: 0,
    });
    img.ccrts.push(Ccrt {
        direction: -1,
        start: 4,
        end: 7,
        seconds: 2,
        micros: 0,
        pad: 0,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.ccrts.len(), 2);
    assert_eq!(dec.ccrts, img.ccrts);
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2, "re-encode must be byte-stable");
}

#[test]
fn ccrt_rejects_short_chunk() {
    let short = [0u8; 13];
    assert!(Ccrt::parse(&short).is_err());
}

#[test]
fn ccrt_negative_components_zero_delay() {
    // Defensive: negative seconds/micros are out-of-spec; the helper
    // clamps the reported delay to 0 rather than returning negative.
    let c = Ccrt {
        direction: 1,
        start: 0,
        end: 15,
        seconds: -1,
        micros: 0,
        pad: 0,
    };
    assert_eq!(c.delay_seconds(), 0.0);
}

// ───────────────────── Mixed ─────────────────────

#[test]
fn crng_and_ccrt_together_roundtrip() {
    let mut img = make_16_color_image();
    img.crngs.push(Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 0,
        high: 5,
    });
    img.ccrts.push(Ccrt {
        direction: 1,
        start: 6,
        end: 11,
        seconds: 0,
        micros: 100_000,
        pad: 0,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.crngs.len(), 1);
    assert_eq!(dec.ccrts.len(), 1);
    assert_eq!(dec.crngs[0], img.crngs[0]);
    assert_eq!(dec.ccrts[0], img.ccrts[0]);
    assert_eq!(dec.rgba, img.rgba, "pixel data still round-trips");
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}

#[test]
fn unknown_chunks_remain_skipped() {
    // Construct a hand-rolled FORM ILBM whose chunk list includes an
    // unknown 4-byte chunk 'XXXX' between BMHD and BODY. The parser
    // must still succeed and not surface the unknown chunk.
    let mut img = make_16_color_image();
    img.bmhd.compression = Compression::None;
    let bytes = encode_ilbm(&img).unwrap();
    // Sanity: original parses without CRNG/CCRT.
    let plain = parse_ilbm(&bytes).unwrap();
    assert!(plain.crngs.is_empty());
    assert!(plain.ccrts.is_empty());
    // Inject "XXXX" chunk (4 bytes payload) just before BODY.
    let body_pos = bytes
        .windows(4)
        .position(|w| w == b"BODY")
        .expect("BODY must be present");
    let mut spliced = Vec::with_capacity(bytes.len() + 12);
    spliced.extend_from_slice(&bytes[..body_pos]);
    spliced.extend_from_slice(b"XXXX");
    spliced.extend_from_slice(&4u32.to_be_bytes());
    spliced.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    spliced.extend_from_slice(&bytes[body_pos..]);
    // Patch FORM size.
    let new_form_size = (spliced.len() - 8) as u32;
    spliced[4..8].copy_from_slice(&new_form_size.to_be_bytes());
    let dec = parse_ilbm(&spliced).expect("unknown chunk should still parse");
    assert_eq!(dec.rgba, img.rgba);
    assert!(dec.crngs.is_empty());
    assert!(dec.ccrts.is_empty());
}
