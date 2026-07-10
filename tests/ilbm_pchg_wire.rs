//! PCHG wire-layout conformance — LineMask bitmap + record shapes.
//!
//! Exercises the on-disk LineData layout: the MSB-first LineMask
//! bitmap sized `((LineCount + 31) / 32) * 4` bytes, the Small
//! (12-bit) records with their split ChangeCount16 / ChangeCount32
//! register groups and packed `(reg << 12) | (R4 << 8) | (G4 << 4) |
//! B4` words, and the Big (32-bit) 6-byte records with their on-disk
//! A, R, B, G component order plus the opt-in alpha flag.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, Pchg, PchgChange,
    PchgKind, PchgLine, Sham, PCHGF_32BIT, PCHGF_USE_ALPHA,
};

fn line(l: u32, changes: &[(u16, [u8; 3])]) -> PchgLine {
    PchgLine {
        line: l,
        changes: changes
            .iter()
            .map(|&(index, rgb)| PchgChange::new(index, rgb))
            .collect(),
    }
}

#[test]
fn linemask_spans_multiple_longwords() {
    // Changes on lines 0 and 39 → LineCount 40 → two mask longwords.
    let lines = vec![
        line(0, &[(1, [0x10, 0x20, 0x30])]),
        line(39, &[(2, [0x40, 0x50, 0x60])]),
    ];
    let bytes = Pchg::from_lines(lines.clone(), PchgKind::Big).encode(PchgKind::Big);
    // Header 20 bytes, then an 8-byte mask.
    assert_eq!(bytes[20], 0x80, "line 0 = bit 31 of the first longword");
    assert_eq!(bytes[21..24], [0, 0, 0]);
    // Line 39: byte 39/8 = 4 of the mask, bit 7 - (39 % 8) = 0.
    assert_eq!(bytes[24], 0x01);
    assert_eq!(bytes[25..28], [0, 0, 0]);
    // Two 8-byte Big records (u16 count + one 6-byte change) follow.
    assert_eq!(bytes.len(), 20 + 8 + 2 * (2 + 6));
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines, lines);
}

#[test]
fn small_second_count_group_addresses_registers_16_to_31() {
    // One line touching register 3 (group 1) and register 20 (group 2).
    let lines = vec![line(
        0,
        &[(3, [0x11, 0x22, 0x33]), (20, [0xAA, 0xBB, 0xCC])],
    )];
    let bytes = Pchg::from_lines(lines.clone(), PchgKind::Small).encode(PchgKind::Small);
    // Header (20) + mask (4) + record.
    assert_eq!(bytes[20..24], [0x80, 0, 0, 0]);
    assert_eq!(bytes[24], 1, "ChangeCount16");
    assert_eq!(bytes[25], 1, "ChangeCount32");
    // Group-1 word: reg 3, RGB444 0x123.
    assert_eq!(u16::from_be_bytes([bytes[26], bytes[27]]), 0x3123);
    // Group-2 word: reg 20 → packed as 20 - 16 = 4, RGB444 0xABC.
    assert_eq!(u16::from_be_bytes([bytes[28], bytes[29]]), 0x4ABC);
    assert_eq!(bytes.len(), 30);
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines, lines);
}

#[test]
fn small_register_saturates_at_31() {
    // The 12-bit layout cannot address past the 32-register OCS
    // palette; higher registers saturate to 31.
    let src = Pchg::from_lines(vec![line(0, &[(200, [0x11, 0x11, 0x11])])], PchgKind::Small);
    assert_eq!(src.lines[0].changes[0].index, 31);
}

#[test]
fn big_record_component_order_is_a_r_b_g() {
    let bytes = Pchg::from_lines(vec![line(0, &[(5, [0x01, 0x02, 0x03])])], PchgKind::Big)
        .encode(PchgKind::Big);
    // Header (20) + mask (4) + u16 count + record.
    assert_eq!(bytes[24..26], [0, 1], "ChangeCount");
    assert_eq!(bytes[26..28], [0, 5], "Register");
    // A, R, B, G — Blue precedes Green on disk.
    assert_eq!(bytes[28..32], [0x00, 0x01, 0x03, 0x02]);
}

#[test]
fn big_alpha_flag_roundtrip() {
    let mut with_alpha = PchgChange::new(2, [0x10, 0x20, 0x30]);
    with_alpha.alpha = Some(0x55);
    // A second change without an explicit alpha in the same chunk
    // defaults to opaque 0xFF once the flag is on.
    let without = PchgChange::new(3, [0x40, 0x50, 0x60]);
    let src = Pchg::from_lines(
        vec![PchgLine {
            line: 0,
            changes: vec![with_alpha, without],
        }],
        PchgKind::Big,
    );
    let h = src.header().unwrap();
    assert_eq!(h.flags & PCHGF_32BIT, PCHGF_32BIT);
    assert_eq!(h.flags & PCHGF_USE_ALPHA, PCHGF_USE_ALPHA);
    assert_eq!(src.lines[0].changes[0].alpha, Some(0x55));
    assert_eq!(src.lines[0].changes[1].alpha, Some(0xFF));
}

#[test]
fn big_alpha_byte_ignored_without_flag() {
    // Hand-build a Big chunk with a junk Alpha byte but no
    // PCHGF_USE_ALPHA: the byte must not surface.
    let mut raw = Vec::new();
    raw.extend_from_slice(&0u16.to_be_bytes()); // Compression
    raw.extend_from_slice(&PCHGF_32BIT.to_be_bytes()); // Flags
    raw.extend_from_slice(&0i16.to_be_bytes()); // StartLine
    raw.extend_from_slice(&1u16.to_be_bytes()); // LineCount
    raw.extend_from_slice(&1u16.to_be_bytes()); // ChangedLines
    raw.extend_from_slice(&0u16.to_be_bytes()); // MinReg
    raw.extend_from_slice(&0u16.to_be_bytes()); // MaxReg
    raw.extend_from_slice(&1u16.to_be_bytes()); // MaxChanges
    raw.extend_from_slice(&1u32.to_be_bytes()); // TotalChanges
    raw.extend_from_slice(&[0x80, 0, 0, 0]); // LineMask
    raw.extend_from_slice(&1u16.to_be_bytes()); // ChangeCount
    raw.extend_from_slice(&0u16.to_be_bytes()); // Register
    raw.extend_from_slice(&[0xDE, 0x11, 0x33, 0x22]); // A(junk), R, B, G
    let pchg = Pchg::parse(&raw).unwrap();
    assert_eq!(pchg.lines[0].changes[0].alpha, None);
    assert_eq!(pchg.lines[0].changes[0].rgb, [0x11, 0x22, 0x33]);
}

#[test]
fn negative_start_line_changes_apply_before_first_row() {
    // StartLine = -2, LineCount = 3: two above-top lines plus row 0.
    let mut raw = Vec::new();
    raw.extend_from_slice(&0u16.to_be_bytes()); // Compression
    raw.extend_from_slice(&1u16.to_be_bytes()); // Flags = 12-bit
    raw.extend_from_slice(&(-2i16).to_be_bytes()); // StartLine
    raw.extend_from_slice(&3u16.to_be_bytes()); // LineCount
    raw.extend_from_slice(&2u16.to_be_bytes()); // ChangedLines
    raw.extend_from_slice(&1u16.to_be_bytes()); // MinReg
    raw.extend_from_slice(&2u16.to_be_bytes()); // MaxReg
    raw.extend_from_slice(&1u16.to_be_bytes()); // MaxChanges
    raw.extend_from_slice(&2u32.to_be_bytes()); // TotalChanges
    raw.extend_from_slice(&[0xA0, 0, 0, 0]); // lines -2 and 0 set
                                             // Line -2: reg 1 → 0xF00 (red).
    raw.extend_from_slice(&[1, 0, 0x1F, 0x00]);
    // Line 0: reg 2 → 0x0F0 (green).
    raw.extend_from_slice(&[1, 0, 0x20, 0xF0]);
    let pchg = Pchg::parse(&raw).unwrap();
    // Both records clamp to (or land on) line 0 and apply in order.
    assert_eq!(pchg.lines.len(), 2);
    assert_eq!(pchg.lines[0].line, 0);
    assert_eq!(pchg.lines[1].line, 0);
    let pal = pchg.palette_at_line(&[[0; 3]; 4], 0);
    assert_eq!(pal[1], [0xFF, 0, 0]);
    assert_eq!(pal[2], [0, 0xFF, 0]);
}

#[test]
fn pchg_takes_precedence_over_sham() {
    // Both a SHAM and a PCHG in one ILBM: PCHG is the designated
    // successor and must drive the per-line palette.
    let w = 4u16;
    let h = 2u16;
    let palette = vec![[0u8, 0, 0], [0xFF, 0xFF, 0xFF]];

    // SHAM says line 1 register 1 is blue; PCHG says it is red.
    let mut sham_line0: Vec<[u8; 3]> = vec![[0, 0, 0]; 16];
    sham_line0[1] = [0xFF, 0xFF, 0xFF];
    let mut sham_line1 = sham_line0.clone();
    sham_line1[1] = [0x00, 0x00, 0xFF];
    let sham = Sham {
        version: 0,
        palettes: vec![sham_line0, sham_line1],
    };
    let pchg = Pchg::from_lines(vec![line(1, &[(1, [0xFF, 0x00, 0x00])])], PchgKind::Big);

    // Row 0 all white (index 1); row 1 all red (index 1 after PCHG).
    let mut rgba = Vec::new();
    for _ in 0..w {
        rgba.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    }
    for _ in 0..w {
        rgba.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
    }

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
        sham: Some(sham),
        pchg: Some(pchg),
        ..IlbmImage::default()
    };

    let decoded = parse_ilbm(&encode_ilbm(&img).unwrap()).unwrap();
    assert!(decoded.sham.is_some(), "SHAM survives the round-trip");
    assert!(decoded.pchg.is_some(), "PCHG survives the round-trip");
    // Row 1 pixel: red per PCHG, not SHAM's blue.
    let px = &decoded.rgba[(4 * 4)..(4 * 4 + 3)];
    assert_eq!(px, &[0xFF, 0x00, 0x00]);
}

#[test]
fn truncated_records_keep_decoded_prefix() {
    // Drop the last record byte: the parser keeps everything whole.
    let lines = vec![
        line(0, &[(1, [0x11, 0x22, 0x33])]),
        line(1, &[(2, [0x44, 0x55, 0x66])]),
    ];
    let mut bytes = Pchg::from_lines(lines, PchgKind::Big).encode(PchgKind::Big);
    bytes.truncate(bytes.len() - 1);
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines.len(), 1, "only the intact first record decodes");
    assert_eq!(back.lines[0].line, 0);
}

#[test]
fn truncated_linemask_is_rejected() {
    let lines = vec![line(0, &[(1, [0x11, 0x22, 0x33])])];
    let mut bytes = Pchg::from_lines(lines, PchgKind::Small).encode(PchgKind::Small);
    // Slice into the 4-byte mask.
    bytes.truncate(22);
    assert!(Pchg::parse(&bytes).is_err());
}
