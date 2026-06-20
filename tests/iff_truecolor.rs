//! Integration coverage for the IFF deep / true-colour FORM-level decoders
//! exposed by `oxideav_iff::ilbm`: `parse_rgb8`, `parse_rgbn`, `parse_deep`,
//! and the caller-supplies-table `assemble_deep_tvdc`.
//!
//! These exercise the **public** entry points end-to-end on hand-built
//! `FORM RGB8` / `FORM RGBN` / `FORM DEEP` files — multi-row images, runs
//! that spill across scanline boundaries, and each `GenlockPolicy` — rather
//! than the per-body unit helpers in `src/ilbm.rs`.
//!
//! Spec reference: `docs/image/iff/iff-truecolor-chunks.md`
//!   §1 (DEEP: DGBL/DPEL/DLOC/DBOD + §1.5 TVDC),
//!   §3 (RGB8 §3.2 / RGBN §3.1 Turbo-Silver genlock-RLE bodies, §3.3 genlock).

use oxideav_iff::ilbm::{
    assemble_deep_tvdc, encode_deep, encode_rgb8, encode_rgbn, parse_deep, parse_rgb8, parse_rgbn,
    DeepCompression, Dpel, GenlockPolicy,
};

/// Wrap a list of `(id, payload)` chunks in an even-padded IFF FORM.
fn iff_form(form_type: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(form_type);
    for (id, payload) in chunks {
        body.extend_from_slice(*id);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(payload);
        if payload.len() & 1 == 1 {
            body.push(0);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn bmhd(w: u16, h: u16, n_planes: u8, compression: u8) -> Vec<u8> {
    let mut b = vec![0u8; 20];
    b[0..2].copy_from_slice(&w.to_be_bytes());
    b[2..4].copy_from_slice(&h.to_be_bytes());
    b[8] = n_planes;
    b[10] = compression;
    b[14] = 1;
    b[15] = 1;
    b
}

/// One RGB8 coded LONG: 24-bit RGB (red MS byte) + genlock + 7-bit count.
fn rgb8_long(r: u8, g: u8, b: u8, lock: bool, count: u8) -> [u8; 4] {
    assert!((1..=127).contains(&count));
    let rgb = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    let w = (rgb << 8) | (u32::from(lock) << 7) | (u32::from(count) & 0x7F);
    w.to_be_bytes()
}

/// One RGBN coded WORD with a 3-bit inline count (1..=7).
fn rgbn_word(r: u16, g: u16, b: u16, lock: bool, count: u16) -> [u8; 2] {
    assert!((1..=7).contains(&count));
    let rgb12 = (r & 0xF) << 8 | (g & 0xF) << 4 | (b & 0xF);
    let w = rgb12 << 4 | (u16::from(lock) << 3) | count;
    w.to_be_bytes()
}

fn dpel(elems: &[(u16, u16)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(elems.len() as u32).to_be_bytes());
    for (ct, depth) in elems {
        b.extend_from_slice(&ct.to_be_bytes());
        b.extend_from_slice(&depth.to_be_bytes());
    }
    b
}

fn dgbl(dw: u16, dh: u16, compression: u16) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b[0..2].copy_from_slice(&dw.to_be_bytes());
    b[2..4].copy_from_slice(&dh.to_be_bytes());
    b[4..6].copy_from_slice(&compression.to_be_bytes());
    b[6] = 1;
    b[7] = 1;
    b
}

// ───────────────────────────── RGB8 §3.2 ─────────────────────────────

#[test]
fn rgb8_two_row_image_with_run_spill() {
    // 2x2: a single run of 3 magenta then 1 green. The magenta run spills
    // from the end of row 0 into the start of row 1 (the body is a flat
    // width*height pixel stream).
    let mut bdy = Vec::new();
    bdy.extend_from_slice(&rgb8_long(0xC0, 0x10, 0xC0, false, 3));
    bdy.extend_from_slice(&rgb8_long(0x00, 0xFF, 0x00, false, 1));
    let file = iff_form(
        b"RGB8",
        &[
            (b"BMHD", bmhd(2, 2, 25, 4)),
            (b"CAMG", vec![0, 0, 0, 0]),
            (b"BODY", bdy),
        ],
    );
    let img = parse_rgb8(&file, GenlockPolicy::default()).unwrap();
    assert_eq!((img.width, img.height), (2, 2));
    assert!(img.is_rgb8);
    // pixels 0,1,2 magenta; pixel 3 green.
    for px in 0..3 {
        assert_eq!(&img.rgba[px * 4..px * 4 + 4], &[0xC0, 0x10, 0xC0, 0xFF]);
    }
    assert_eq!(&img.rgba[12..16], &[0x00, 0xFF, 0x00, 0xFF]);
}

#[test]
fn rgb8_genlock_brush_policy_marks_transparency() {
    let mut bdy = Vec::new();
    bdy.extend_from_slice(&rgb8_long(0x80, 0x80, 0x80, true, 1)); // genlock set
    bdy.extend_from_slice(&rgb8_long(0x40, 0x40, 0x40, false, 1));
    let file = iff_form(
        b"RGB8",
        &[
            (b"BMHD", bmhd(2, 1, 25, 4)),
            (b"CAMG", vec![0, 0, 0, 0]),
            (b"BODY", bdy),
        ],
    );
    let img = parse_rgb8(&file, GenlockPolicy::BrushTransparency).unwrap();
    assert_eq!(img.rgba[3], 0x00); // genlocked → transparent
    assert_eq!(img.rgba[7], 0xFF); // ungenlocked → opaque
}

// ───────────────────────────── RGBN §3.1 ─────────────────────────────

#[test]
fn rgbn_widens_4bit_guns_and_fills_row() {
    // 4x1: red run of 2 then white run of 2.
    let mut bdy = Vec::new();
    bdy.extend_from_slice(&rgbn_word(0xF, 0x0, 0x0, false, 2));
    bdy.extend_from_slice(&rgbn_word(0xF, 0xF, 0xF, false, 2));
    let file = iff_form(
        b"RGBN",
        &[
            (b"BMHD", bmhd(4, 1, 13, 4)),
            (b"CAMG", vec![0, 0, 0, 0]),
            (b"BODY", bdy),
        ],
    );
    let img = parse_rgbn(&file, GenlockPolicy::default()).unwrap();
    assert!(!img.is_rgb8);
    assert_eq!(&img.rgba[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
    assert_eq!(&img.rgba[4..8], &[0xFF, 0x00, 0x00, 0xFF]);
    assert_eq!(&img.rgba[8..12], &[0xFF, 0xFF, 0xFF, 0xFF]);
    assert_eq!(&img.rgba[12..16], &[0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn rgbn_byte_count_cascade_through_form() {
    // 10-pixel run: 3-bit inline count 0 escalates to a BYTE count of 10.
    let rgb12 = (0x1 << 8 | 0x2 << 4 | 0x3) as u16;
    let mut bdy = Vec::new();
    bdy.extend_from_slice(&(rgb12 << 4).to_be_bytes()); // inline count 0
    bdy.push(10); // BYTE count
    let file = iff_form(
        b"RGBN",
        &[
            (b"BMHD", bmhd(10, 1, 13, 4)),
            (b"CAMG", vec![0, 0, 0, 0]),
            (b"BODY", bdy),
        ],
    );
    let img = parse_rgbn(&file, GenlockPolicy::default()).unwrap();
    assert_eq!(img.width, 10);
    for px in 0..10 {
        assert_eq!(&img.rgba[px * 4..px * 4 + 4], &[0x11, 0x22, 0x33, 0xFF]);
    }
}

// ───────────────────────────── DEEP §1 ─────────────────────────────

#[test]
fn deep_nocompression_two_row_rgb888() {
    // 2x2 RGB888 chunky body; dimensions from DGBL display size.
    let body: Vec<u8> = vec![
        10, 11, 12, 20, 21, 22, // row 0
        30, 31, 32, 40, 41, 42, // row 1
    ];
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl(2, 2, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", body),
        ],
    );
    let img = parse_deep(&file).unwrap();
    assert_eq!((img.width, img.height), (2, 2));
    assert_eq!(img.dgbl.compression, DeepCompression::None);
    assert_eq!(&img.rgba[0..4], &[10, 11, 12, 0xFF]);
    assert_eq!(&img.rgba[12..16], &[40, 41, 42, 0xFF]);
}

#[test]
fn deep_rgba_with_alpha_component() {
    // 1x1 RGBA 8:8:8:8 → alpha reaches the output.
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl(1, 1, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8), (4, 8)])),
            (b"DBOD", vec![1, 2, 3, 0x77]),
        ],
    );
    let img = parse_deep(&file).unwrap();
    assert_eq!(&img.rgba[0..4], &[1, 2, 3, 0x77]);
}

#[test]
fn deep_in_form_tvdc_is_rejected_documented_gap() {
    // The §1.5 16-word delta table is not carried in-FORM.
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl(2, 1, 5)), // TVDC
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![0, 0]),
        ],
    );
    assert!(parse_deep(&file).is_err());
}

#[test]
fn deep_tvdc_caller_supplied_table_decodes_two_rows() {
    // 2x2 RGB888 TVDC body: one Red line, one Green line, one Blue line
    // per row (§1.5). Table: nibble 1 → +1; nibble 0 = run sentinel.
    let mut table = [0i16; 16];
    table[1] = 1;
    let dpel = Dpel::parse(&dpel(&[(1, 8), (2, 8), (3, 8)])).unwrap();

    // Each component line of width 2 with nibbles "1 1" → v = 1, 2.
    // bytes 0x11 for the two high/low nibbles → emits [1, 2].
    let line = [0x11u8];
    let mut body = Vec::new();
    for _row in 0..2 {
        body.extend_from_slice(&line); // red
        body.extend_from_slice(&line); // green
        body.extend_from_slice(&line); // blue
    }
    let rgba = assemble_deep_tvdc(&dpel, 2, 2, &table, &body).unwrap();
    // Every pixel: R == G == B, value 1 at x=0, 2 at x=1, identical per row.
    assert_eq!(&rgba[0..4], &[1, 1, 1, 0xFF]);
    assert_eq!(&rgba[4..8], &[2, 2, 2, 0xFF]);
    assert_eq!(&rgba[8..12], &[1, 1, 1, 0xFF]);
    assert_eq!(&rgba[12..16], &[2, 2, 2, 0xFF]);
}

// ───────────── encode → decode round-trip through the public API ─────────────

/// A multi-row RGB8 image survives `encode_rgb8` → `parse_rgb8` byte-exact,
/// including a run that spans a scanline boundary.
#[test]
fn rgb8_encode_decode_round_trip_multi_row() {
    // 3x2: the first colour forms a run of 5 (spilling across the row break),
    // the last pixel is distinct.
    let mut rgba = Vec::new();
    for _ in 0..5 {
        rgba.extend_from_slice(&[0x12, 0x34, 0x56, 0xFF]);
    }
    rgba.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xFF]);
    let file = encode_rgb8(3, 2, &rgba).unwrap();
    let back = parse_rgb8(&file, GenlockPolicy::BrushTransparency).unwrap();
    assert!(back.is_rgb8);
    assert_eq!((back.width, back.height), (3, 2));
    assert_eq!(back.rgba, rgba);
}

/// An RGB8 image whose first run exceeds the 7-bit inline count survives the
/// public encode → decode round-trip (the encoder splits, the decoder rejoins).
#[test]
fn rgb8_encode_decode_round_trip_long_run() {
    let rgba: Vec<u8> = std::iter::repeat([0x7F, 0x00, 0x7F, 0xFF])
        .take(200)
        .flatten()
        .collect();
    let file = encode_rgb8(200, 1, &rgba).unwrap();
    let back = parse_rgb8(&file, GenlockPolicy::BrushTransparency).unwrap();
    assert_eq!(back.rgba, rgba);
}

/// A nibble-replicated RGBN image survives `encode_rgbn` → `parse_rgbn`.
#[test]
fn rgbn_encode_decode_round_trip_replicated() {
    let mut rgba = Vec::new();
    rgba.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
    rgba.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    rgba.extend_from_slice(&[0x44, 0x88, 0xCC, 0xFF]);
    rgba.extend_from_slice(&[0x44, 0x88, 0xCC, 0xFF]);
    let file = encode_rgbn(4, 1, &rgba).unwrap();
    let back = parse_rgbn(&file, GenlockPolicy::BrushTransparency).unwrap();
    assert!(!back.is_rgb8);
    assert_eq!(back.rgba, rgba);
}

/// A NOCOMPRESSION DEEP image survives `encode_deep` → `parse_deep`, alpha
/// included.
#[test]
fn deep_nocompression_encode_decode_round_trip() {
    let dpel = Dpel::parse(&dpel(&[(1, 8), (2, 8), (3, 8), (4, 8)])).unwrap();
    let rgba = vec![
        10, 20, 30, 0x80, 40, 50, 60, 0xC0, // row 0
        70, 80, 90, 0xFF, 11, 22, 33, 0x44, // row 1
    ];
    let file = encode_deep(&dpel, 2, 2, DeepCompression::None, None, &rgba).unwrap();
    let back = parse_deep(&file).unwrap();
    assert_eq!((back.width, back.height), (2, 2));
    assert_eq!(back.rgba, rgba);
}

/// A TVDC DEEP image encoded by `encode_deep` round-trips through the
/// caller-supplied-table `assemble_deep_tvdc` (the table travels out of band).
#[test]
fn deep_tvdc_encode_decode_round_trip() {
    let mut table = [0i16; 16];
    table[1] = 1; // +1
    table[2] = -1; // -1
    table[3] = 7; // +7 (seeds each component's first byte)
    let dpel = Dpel::parse(&dpel(&[(1, 8), (2, 8), (3, 8)])).unwrap();
    // R/G/B all start at 7 (delta +7 from 0) then oscillate 8,7,8 (+1,-1,+1).
    let rgba = vec![7, 7, 7, 0xFF, 8, 8, 8, 0xFF, 7, 7, 7, 0xFF, 8, 8, 8, 0xFF];
    let file = encode_deep(&dpel, 4, 1, DeepCompression::Tvdc, Some(&table), &rgba).unwrap();
    // Pull the DBOD back out and decode with the same table.
    let body = {
        let mut cur = 12usize;
        let mut found = Vec::new();
        while cur + 8 <= file.len() {
            let id = &file[cur..cur + 4];
            let size =
                u32::from_be_bytes([file[cur + 4], file[cur + 5], file[cur + 6], file[cur + 7]])
                    as usize;
            if id == b"DBOD" {
                found = file[cur + 8..cur + 8 + size].to_vec();
                break;
            }
            cur = cur + 8 + size + (size & 1);
        }
        found
    };
    let back = assemble_deep_tvdc(&dpel, 4, 1, &table, &body).unwrap();
    assert_eq!(back, rgba);
}
