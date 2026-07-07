//! PCHG (Palette CHanGe) — encoder / re-encode-from-change-list surface.
//!
//! Covers [`Pchg::encode`] and [`Pchg::from_lines`], the inverse of
//! [`Pchg::parse`] for the uncompressed change-record encodings. Two
//! properties are exercised:
//!
//! * **Structural round-trip** — `parse(encode(kind)).lines` reproduces
//!   the change list (exactly for `Big`, 4-bit-quantised for `Small`),
//!   and the re-derived 20-byte header hints agree with the payload
//!   (`header_matches_payload`).
//! * **End-to-end** — a `Pchg` built with `from_lines` serialises through
//!   `encode_ilbm` into a real `FORM ILBM` whose `PCHG` chunk decodes back
//!   to the same per-line palette overrides via `parse_ilbm`.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, Pchg, PchgChange,
    PchgHeader, PchgKind, PchgLine,
};

fn line(l: u32, changes: &[(u16, [u8; 3])]) -> PchgLine {
    PchgLine {
        line: l,
        changes: changes
            .iter()
            .map(|&(index, rgb)| PchgChange { index, rgb })
            .collect(),
    }
}

#[test]
fn big_encode_is_exact_roundtrip() {
    // 24-bit channels round-trip losslessly.
    let lines = vec![
        line(2, &[(0, [0x12, 0x34, 0x56]), (5, [0xAB, 0xCD, 0xEF])]),
        line(7, &[(257, [0x00, 0x80, 0xFF])]),
    ];
    let src = Pchg::from_lines(lines.clone(), PchgKind::Big);
    // from_lines re-parses, so its own lines equal the input verbatim.
    assert_eq!(src.lines, lines);

    let bytes = src.encode(PchgKind::Big);
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines, lines);
    assert_eq!(back.kind(), Some(PchgKind::Big));
    assert!(back.header_matches_payload());
}

#[test]
fn small_encode_quantises_to_4bit() {
    // Channels that are multiples of 0x11 survive the 12-bit encoding
    // exactly; the register index must fit a byte.
    let lines = vec![
        line(0, &[(1, [0x00, 0xFF, 0x00]), (3, [0x11, 0x22, 0x33])]),
        line(4, &[(2, [0xAA, 0xBB, 0xCC])]),
    ];
    let src = Pchg::from_lines(lines.clone(), PchgKind::Small);
    assert_eq!(src.lines, lines);
    assert_eq!(src.kind(), Some(PchgKind::Small));

    let back = Pchg::parse(&src.encode(PchgKind::Small)).unwrap();
    assert_eq!(back.lines, lines);
    assert!(back.header_matches_payload());
}

#[test]
fn small_encode_is_lossy_for_non_multiples() {
    // 0x1F truncates to nibble 0x1 → widened back to 0x11.
    let lines = vec![line(0, &[(0, [0x1F, 0x2E, 0x3D])])];
    let src = Pchg::from_lines(lines, PchgKind::Small);
    assert_eq!(src.lines[0].changes[0].rgb, [0x11, 0x22, 0x33]);
}

#[test]
fn header_window_and_hints_are_derived() {
    // Changes on lines 3 and 9 → StartLine=3, LineCount=7, with the gap
    // lines 4..=8 emitted as zero-change records.
    let lines = vec![
        line(3, &[(0, [0x10, 0x20, 0x30])]),
        line(9, &[(1, [0x40, 0x50, 0x60]), (2, [0x70, 0x80, 0x90])]),
    ];
    let bytes = Pchg::from_lines(lines, PchgKind::Big).encode(PchgKind::Big);
    let h: PchgHeader = Pchg::parse(&bytes).unwrap().header().unwrap();
    assert_eq!(h.compression, 0);
    assert_eq!(h.start_line, 3);
    assert_eq!(h.line_count, 7);
    assert_eq!(h.changed_lines, 2);
    assert_eq!(h.min_reg, 0);
    assert_eq!(h.max_reg, 2);
    assert_eq!(h.max_changes, 2);
    assert_eq!(h.total_changes, 3);
}

#[test]
fn empty_change_list_is_header_only() {
    let empty = Pchg::from_lines(Vec::new(), PchgKind::Small);
    let bytes = empty.encode(PchgKind::Small);
    assert_eq!(bytes.len(), 20);
    let back = Pchg::parse(&bytes).unwrap();
    assert!(back.lines.is_empty());
    let h = back.header().unwrap();
    assert_eq!(h.start_line, 0);
    assert_eq!(h.line_count, 0);
    assert_eq!(h.total_changes, 0);
}

#[test]
fn unsorted_change_list_encodes_in_scanline_order() {
    // Input out of order; the encoder must place records by absolute line.
    let lines = vec![
        line(6, &[(9, [0x11, 0x11, 0x11])]),
        line(1, &[(4, [0x22, 0x22, 0x22])]),
    ];
    let back = Pchg::parse(&Pchg::from_lines(lines, PchgKind::Big).encode(PchgKind::Big)).unwrap();
    assert_eq!(back.lines.len(), 2);
    assert_eq!(back.lines[0].line, 1);
    assert_eq!(back.lines[1].line, 6);
}

#[test]
fn from_lines_pchg_serialises_through_encode_ilbm() {
    // Build a tiny 4×2 indexed ILBM carrying a hand-authored PCHG, encode
    // the whole FORM, and confirm the PCHG survives a parse_ilbm round-trip.
    let w = 4u16;
    let h = 2u16;
    let palette = vec![[0u8, 0, 0], [0xFF, 0xFF, 0xFF]];
    // 2 bitplanes worth of indices; keep everything index 0/1.
    let indices = [0u8, 1, 0, 1, 1, 0, 1, 0];
    let rgba: Vec<u8> = indices
        .iter()
        .flat_map(|&i| {
            let c = palette[i as usize];
            [c[0], c[1], c[2], 0xFF]
        })
        .collect();

    let pchg = Pchg::from_lines(vec![line(1, &[(0, [0x33, 0x66, 0x99])])], PchgKind::Big);

    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: Bmhd {
            width: w,
            height: h,
            x_origin: 0,
            y_origin: 0,
            n_planes: 1,
            masking: Masking::None,
            compression: Compression::None,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: w as i16,
            page_height: h as i16,
        },
        camg: Camg::default(),
        palette,
        rgba,
        pchg: Some(pchg),
        ..IlbmImage::default()
    };

    let bytes = encode_ilbm(&img).unwrap();
    let decoded = parse_ilbm(&bytes).unwrap();
    let dp = decoded.pchg.expect("PCHG chunk survived the round-trip");
    assert_eq!(dp.lines.len(), 1);
    assert_eq!(dp.lines[0].line, 1);
    assert_eq!(dp.lines[0].changes[0].index, 0);
    assert_eq!(dp.lines[0].changes[0].rgb, [0x33, 0x66, 0x99]);
}
