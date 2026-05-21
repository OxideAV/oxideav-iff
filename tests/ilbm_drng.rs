//! Round-88 coverage: ILBM `DRNG` (DeluxePaint IV extended range
//! cycling) chunk.
//!
//! DRNG is a super-set of `CRNG`: it lets the cycle window
//! `[min, max]` carry true-colour RGB cells *and* palette-register
//! cells, so the range can step through colours that have no permanent
//! `CMAP` entry. Each test builds an in-memory ILBM with one or more
//! `DRNG` chunks attached, round-trips it through [`encode_ilbm`] +
//! [`parse_ilbm`], and verifies that:
//!
//! * the resulting file contains the expected number of `DRNG` chunks,
//! * every cell list (true-colour + palette-register) round-trips
//!   byte-identical including order, and
//! * the convenience accessors ([`Drng::cycles_per_second`],
//!   [`Drng::is_active`], [`Drng::has_true_cells`], [`Drng::has_reg_cells`],
//!   [`Drng::range_len`]) report the spec-documented values.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, Drng, DrngRegCell, DrngTrueCell, IlbmImage,
    Masking,
};

fn make_16_color_image() -> IlbmImage {
    let palette: Vec<[u8; 3]> = (0..16u8)
        .map(|i| [i * 16, 255 - i * 16, i.wrapping_mul(17)])
        .collect();
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
        n_planes: 4,
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

// ───────────────────── basic round-trip ─────────────────────

#[test]
fn drng_empty_cell_lists_roundtrip() {
    // Smallest valid DRNG: just the 8-byte header, no cells.
    let mut img = make_16_color_image();
    img.drngs.push(Drng {
        min: 0,
        max: 7,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE,
        trues: Vec::new(),
        regs: Vec::new(),
    });
    let bytes = encode_ilbm(&img).unwrap();
    assert!(
        bytes.windows(4).any(|w| w == b"DRNG"),
        "encoded file should contain a DRNG chunk"
    );
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.drngs.len(), 1);
    let d = &dec.drngs[0];
    assert_eq!(d, &img.drngs[0]);
    assert!(d.is_active());
    assert_eq!(d.range_len(), 8);
    assert_eq!(d.cycles_per_second(), 60.0);
    assert!(!d.has_true_cells());
    assert!(!d.has_reg_cells());
}

#[test]
fn drng_with_true_cells_roundtrip() {
    let mut img = make_16_color_image();
    img.drngs.push(Drng {
        min: 4,
        max: 11,
        rate: 8192,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB,
        trues: vec![
            DrngTrueCell {
                cell: 4,
                r: 0x10,
                g: 0x20,
                b: 0x30,
            },
            DrngTrueCell {
                cell: 7,
                r: 0xC0,
                g: 0xC0,
                b: 0x00,
            },
            DrngTrueCell {
                cell: 11,
                r: 0xFF,
                g: 0xFF,
                b: 0xFF,
            },
        ],
        regs: Vec::new(),
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.drngs.len(), 1);
    let d = &dec.drngs[0];
    assert_eq!(d, &img.drngs[0]);
    assert_eq!(d.trues.len(), 3);
    assert!(d.has_true_cells());
    assert!(!d.has_reg_cells());
    // Cell order must be preserved verbatim.
    assert_eq!(d.trues[0].cell, 4);
    assert_eq!(d.trues[2].r, 0xFF);
    // Re-encode is byte-stable.
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}

#[test]
fn drng_with_reg_cells_roundtrip() {
    let mut img = make_16_color_image();
    img.drngs.push(Drng {
        min: 0,
        max: 5,
        rate: 4096,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_REGS,
        trues: Vec::new(),
        regs: vec![
            DrngRegCell { cell: 0, index: 12 },
            DrngRegCell { cell: 3, index: 7 },
            DrngRegCell { cell: 5, index: 1 },
        ],
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.drngs.len(), 1);
    let d = &dec.drngs[0];
    assert_eq!(d, &img.drngs[0]);
    assert_eq!(d.regs.len(), 3);
    assert!(!d.has_true_cells());
    assert!(d.has_reg_cells());
    // Cell-list order survives the parse cycle.
    assert_eq!(d.regs[0].index, 12);
    assert_eq!(d.regs[2].cell, 5);
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}

#[test]
fn drng_with_both_cell_lists_roundtrip() {
    let mut img = make_16_color_image();
    img.drngs.push(Drng {
        min: 2,
        max: 9,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB | Drng::FLAG_DP_REGS,
        trues: vec![
            DrngTrueCell {
                cell: 2,
                r: 0x80,
                g: 0x40,
                b: 0x20,
            },
            DrngTrueCell {
                cell: 6,
                r: 0x10,
                g: 0xF0,
                b: 0x10,
            },
        ],
        regs: vec![
            DrngRegCell { cell: 4, index: 15 },
            DrngRegCell { cell: 9, index: 0 },
        ],
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.drngs.len(), 1);
    assert_eq!(dec.drngs[0], img.drngs[0]);
    // Re-encode is byte-stable even with both lists populated.
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}

#[test]
fn drng_multiple_preserve_order() {
    let mut img = make_16_color_image();
    img.drngs.push(Drng {
        min: 0,
        max: 3,
        rate: 8192,
        flags: Drng::FLAG_ACTIVE,
        trues: Vec::new(),
        regs: Vec::new(),
    });
    img.drngs.push(Drng {
        min: 4,
        max: 7,
        rate: 4096,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB,
        trues: vec![DrngTrueCell {
            cell: 5,
            r: 0xAA,
            g: 0xBB,
            b: 0xCC,
        }],
        regs: Vec::new(),
    });
    img.drngs.push(Drng {
        min: 8,
        max: 11,
        rate: 16384,
        flags: 0, // inactive
        trues: Vec::new(),
        regs: vec![DrngRegCell { cell: 9, index: 2 }],
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.drngs.len(), 3);
    assert_eq!(dec.drngs, img.drngs);
    assert!(dec.drngs[0].is_active());
    assert!(dec.drngs[1].is_active());
    assert!(!dec.drngs[2].is_active());
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}

// ───────────────────── accessors / corner cases ─────────────────────

#[test]
fn drng_inactive_zero_rate_zero_hz() {
    let d = Drng {
        min: 0,
        max: 15,
        rate: 0,
        flags: 0,
        trues: Vec::new(),
        regs: Vec::new(),
    };
    assert!(!d.is_active());
    assert_eq!(d.cycles_per_second(), 0.0);
    assert_eq!(d.range_len(), 16);
}

#[test]
fn drng_inverted_range_zero_len() {
    let d = Drng {
        min: 12,
        max: 4,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE,
        trues: Vec::new(),
        regs: Vec::new(),
    };
    assert_eq!(d.range_len(), 0);
}

#[test]
fn drng_flag_without_cells_still_reports_capability() {
    // Some encoders set DP_RGB / DP_REGS even when the cell list is
    // empty. The capability helpers should honour the flag bit.
    let d = Drng {
        min: 0,
        max: 7,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB | Drng::FLAG_DP_REGS,
        trues: Vec::new(),
        regs: Vec::new(),
    };
    assert!(d.has_true_cells());
    assert!(d.has_reg_cells());
}

// ───────────────────── error paths ─────────────────────

#[test]
fn drng_rejects_short_header() {
    // 7-byte payload — header itself is 8 bytes.
    let short = [0u8; 7];
    assert!(Drng::parse(&short).is_err());
}

#[test]
fn drng_rejects_truncated_true_cells() {
    // Header advertises ntrue=2 (8 bytes of trues) but only 4 are
    // provided. The parser must refuse rather than silently truncate.
    let mut body = vec![0u8, 7, 0x40, 0, 0, 1, 2, 0]; // min=0 max=7 rate=16384 flags=1 ntrue=2 nregs=0
    body.extend_from_slice(&[0x00, 0x10, 0x20, 0x30]); // only one cell
    assert!(Drng::parse(&body).is_err());
}

#[test]
fn drng_rejects_truncated_reg_cells() {
    // Header advertises nregs=3 (6 bytes of regs) but only 2 are
    // provided.
    let mut body = vec![0u8, 7, 0x40, 0, 0, 1, 0, 3]; // ntrue=0 nregs=3
    body.extend_from_slice(&[0x01, 0x05]); // only one reg cell
    assert!(Drng::parse(&body).is_err());
}

// ───────────────────── mixed with CRNG/CCRT ─────────────────────

#[test]
fn drng_alongside_crng_ccrt_roundtrip() {
    use oxideav_iff::ilbm::{Ccrt, Crng};
    let mut img = make_16_color_image();
    img.crngs.push(Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 0,
        high: 3,
    });
    img.ccrts.push(Ccrt {
        direction: 1,
        start: 4,
        end: 7,
        seconds: 0,
        micros: 250_000,
        pad: 0,
    });
    img.drngs.push(Drng {
        min: 8,
        max: 11,
        rate: 8192,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB,
        trues: vec![DrngTrueCell {
            cell: 9,
            r: 0x11,
            g: 0x22,
            b: 0x33,
        }],
        regs: Vec::new(),
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.crngs.len(), 1);
    assert_eq!(dec.ccrts.len(), 1);
    assert_eq!(dec.drngs.len(), 1);
    assert_eq!(dec.crngs[0], img.crngs[0]);
    assert_eq!(dec.ccrts[0], img.ccrts[0]);
    assert_eq!(dec.drngs[0], img.drngs[0]);
    assert_eq!(dec.rgba, img.rgba);
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}

// ───────────────────── odd-size pad-byte verification ─────────────────────

#[test]
fn drng_odd_size_payload_gets_pad_byte() {
    // 8-byte header + 1 reg cell (2 bytes) = 10 bytes — even. Add a
    // single odd cell list to make payload length odd. Payload =
    // 8 + 4*0 + 2*nregs. That's always even. Try ntrue=0, nregs=1:
    // 8 + 0 + 2 = 10 (even). Use ntrue=1 + nregs=0 → 8 + 4 = 12 (even).
    // For odd we need ntrue or nregs such that 4*ntrue + 2*nregs is odd —
    // which is impossible (both terms are even). The DRNG payload is
    // *always* even-sized. Validate this invariant: encode and confirm
    // no pad byte was appended.
    let mut img = make_16_color_image();
    img.drngs.push(Drng {
        min: 0,
        max: 3,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB,
        trues: vec![DrngTrueCell {
            cell: 1,
            r: 1,
            g: 2,
            b: 3,
        }],
        regs: vec![DrngRegCell { cell: 2, index: 5 }],
    });
    let bytes = encode_ilbm(&img).unwrap();
    // Locate DRNG chunk and confirm its declared length is even (so no
    // pad byte was inserted by the encoder).
    let drng_pos = bytes
        .windows(4)
        .position(|w| w == b"DRNG")
        .expect("DRNG must be present");
    let sz = u32::from_be_bytes([
        bytes[drng_pos + 4],
        bytes[drng_pos + 5],
        bytes[drng_pos + 6],
        bytes[drng_pos + 7],
    ]);
    assert_eq!(sz & 1, 0, "DRNG payload size must be even (always)");
    // Confirm the round-trip is byte-stable regardless.
    let dec = parse_ilbm(&bytes).unwrap();
    let bytes2 = encode_ilbm(&dec).unwrap();
    assert_eq!(bytes, bytes2);
}
