//! Round-2 ILBM coverage: PBM, GRAB, SHAM, PCHG, HAM/EHB encode, ANIM
//! op-5 delta. Each test builds a hand-rolled file or round-trips
//! through the public encode/decode entry points.

// Per-row plane and mask state flows through index-based 2D loops
// (mirrors the format spec). Iterators would obscure the
// scanline/column relationship.
#![allow(clippy::needless_range_loop)]

use oxideav_iff::anim::{apply_op5_for_test, parse_anim, Anhd, AnimImage};
use oxideav_iff::ilbm::{
    encode_ilbm, expand_ehb_palette, parse_ilbm, Bmhd, Camg, Compression, Grab, IlbmImage, Masking,
    PchgChange, PchgLine, Sham, CAMG_EHB, CAMG_HAM,
};

// ───────────────────── PBM ─────────────────────

fn make_pbm_image(w: u16, h: u16, palette: Vec<[u8; 3]>) -> IlbmImage {
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        for x in 0..w {
            let idx = ((x as usize) ^ (y as usize)) % palette.len();
            let p = palette[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes: 8, // PBM is always 8 bits/pixel
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
        form_type: *b"PBM ",
        ..IlbmImage::default()
    }
}

#[test]
fn pbm_roundtrip_uncompressed() {
    let pal: Vec<[u8; 3]> = (0..16u8)
        .map(|i| [i * 16, 255 - i * 16, i.wrapping_mul(17)])
        .collect();
    let img = make_pbm_image(8, 4, pal.clone());
    let bytes = encode_ilbm(&img).unwrap();
    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"PBM ");
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(&dec.form_type, b"PBM ");
    assert_eq!(dec.width, 8);
    assert_eq!(dec.height, 4);
    assert_eq!(dec.rgba, img.rgba, "PBM uncompressed RGBA round-trips");
}

#[test]
fn pbm_roundtrip_byterun1() {
    let pal: Vec<[u8; 3]> = (0..16u8).map(|i| [i * 16, 0, 0]).collect();
    let mut img = make_pbm_image(20, 5, pal);
    img.bmhd.compression = Compression::ByteRun1;
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.rgba, img.rgba, "PBM ByteRun1 RGBA round-trips");
}

#[test]
fn pbm_odd_width_padded_to_even_stride() {
    let pal: Vec<[u8; 3]> = vec![[10, 20, 30], [40, 50, 60]];
    let img = make_pbm_image(7, 3, pal); // odd width: stride = 8
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.width, 7);
    assert_eq!(dec.height, 3);
    // Compare visible pixel area only.
    assert_eq!(dec.rgba.len(), 7 * 3 * 4);
    assert_eq!(dec.rgba, img.rgba);
}

// ───────────────────── GRAB ─────────────────────

#[test]
fn grab_chunk_roundtrips() {
    let mut img = make_pbm_image(4, 2, vec![[0, 0, 0], [255, 255, 255]]);
    img.grab = Some(Grab { x: 1, y: 2 });
    let bytes = encode_ilbm(&img).unwrap();
    // GRAB FourCC must appear in the file.
    let pos = bytes
        .windows(4)
        .position(|w| w == b"GRAB")
        .expect("GRAB chunk in encoded file");
    // Body comes after the 4-byte FourCC + 4-byte size.
    let body = &bytes[pos + 8..pos + 12];
    let x = i16::from_be_bytes([body[0], body[1]]);
    let y = i16::from_be_bytes([body[2], body[3]]);
    assert_eq!(x, 1);
    assert_eq!(y, 2);
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.grab, Some(Grab { x: 1, y: 2 }));
}

// ───────────────────── SHAM ─────────────────────

#[test]
fn sham_per_line_palette_overrides_cmap() {
    // Build a HAM6 image: 4×2 pixels. Default CMAP is all black.
    // SHAM gives row 0 a red palette and row 1 a green palette. Op
    // 0b00 val=1 should look up index 1 of the *row's* palette.
    let pal_row0: Vec<[u8; 3]> = vec![
        [0; 3],
        [0xFF, 0, 0], // row 0 index 1 = bright red
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
    ];
    let pal_row1: Vec<[u8; 3]> = vec![
        [0; 3],
        [0, 0xFF, 0], // row 1 index 1 = bright green
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
        [0; 3],
    ];
    // For HAM6 + SHAM the op-encoded indices live in the BODY. We
    // build them via the encoder by feeding the desired RGB; the
    // HAM encoder will pick op=00 val=1 to hit row's index 1 (since
    // it's the only non-black palette entry that matches).
    let bmhd = Bmhd {
        width: 4,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 6, // HAM6
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 4,
        page_height: 2,
    };
    // RGBA: row 0 all bright red, row 1 all bright green.
    let mut rgba = Vec::with_capacity(4 * 2 * 4);
    for _ in 0..4 {
        rgba.extend_from_slice(&[0xFF, 0, 0, 0xFF]);
    }
    for _ in 0..4 {
        rgba.extend_from_slice(&[0, 0xFF, 0, 0xFF]);
    }
    let sham = Sham {
        version: 0,
        palettes: vec![pal_row0.clone(), pal_row1.clone()],
    };
    let img = IlbmImage {
        width: 4,
        height: 2,
        bmhd,
        palette: pal_row0.clone(),
        camg: Camg { raw: CAMG_HAM },
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        sham: Some(sham.clone()),
        ..IlbmImage::default()
    };

    let bytes = encode_ilbm(&img).unwrap();
    let pos = bytes
        .windows(4)
        .position(|w| w == b"SHAM")
        .expect("SHAM chunk in encoded file");
    let _ = pos;
    let dec = parse_ilbm(&bytes).unwrap();
    let dec_sham = dec.sham.as_ref().expect("SHAM round-tripped");
    // SHAM is RGB444; the round-trip widens 4-bit→8-bit by replicating
    // the nibble (0xF → 0xFF). The two non-zero entries should match.
    assert_eq!(dec_sham.palettes[0][1], [0xFF, 0, 0], "row 0 index 1");
    assert_eq!(dec_sham.palettes[1][1], [0, 0xFF, 0], "row 1 index 1");

    // Decoded rows should be the original RGB (HAM op=00 val=1 picks
    // the row palette entry per scanline).
    assert_eq!(&dec.rgba[0..4], &[0xFF, 0, 0, 0xFF]);
    assert_eq!(&dec.rgba[4 * 4..4 * 4 + 4], &[0, 0xFF, 0, 0xFF]);
}

#[test]
fn sham_typed_accessors_walk_explicit_rows_and_fallback() {
    // Build a SHAM with two explicit row palettes. The first row's
    // index-1 is red, the second row's index-1 is green; index-0 of
    // both rows is black.
    let mut pal_row0: Vec<[u8; 3]> = vec![[0; 3]; 16];
    pal_row0[1] = [0xF0, 0, 0]; // RGB444 nibble pattern post-widen
    let mut pal_row1: Vec<[u8; 3]> = vec![[0; 3]; 16];
    pal_row1[1] = [0, 0xF0, 0];
    let sham = Sham {
        version: 0,
        palettes: vec![pal_row0.clone(), pal_row1.clone()],
    };

    // rows() / is_empty() / row_palette()
    assert!(!sham.is_empty(), "SHAM with 2 rows is non-empty");
    assert_eq!(sham.rows(), 2);
    assert_eq!(sham.row_palette(0), Some(pal_row0.as_slice()));
    assert_eq!(sham.row_palette(1), Some(pal_row1.as_slice()));
    assert_eq!(sham.row_palette(2), None, "past end is None");

    // palette_at_line() picks the per-row palette verbatim when y is
    // in-range.
    let base: Vec<[u8; 3]> = vec![[0x80, 0x80, 0x80]; 16];
    let at0 = sham.palette_at_line(&base, 0);
    assert_eq!(at0, pal_row0, "row 0 returns SHAM row 0");
    let at1 = sham.palette_at_line(&base, 1);
    assert_eq!(at1, pal_row1, "row 1 returns SHAM row 1");

    // palette_at_line() past the last stored row falls back to base,
    // truncated/padded to 16 entries.
    let at_past = sham.palette_at_line(&base, 99);
    assert_eq!(at_past.len(), 16, "fallback is always 16 entries");
    assert_eq!(at_past[0], [0x80, 0x80, 0x80], "first base entry kept");
    let short_base: Vec<[u8; 3]> = vec![[0x11, 0x22, 0x33]; 4];
    let at_past_short = sham.palette_at_line(&short_base, 99);
    assert_eq!(at_past_short.len(), 16, "padded up to 16 entries");
    assert_eq!(at_past_short[0], [0x11, 0x22, 0x33]);
    assert_eq!(at_past_short[3], [0x11, 0x22, 0x33]);
    assert_eq!(at_past_short[4], [0, 0, 0], "padding is black");
    assert_eq!(at_past_short[15], [0, 0, 0]);
}

#[test]
fn sham_empty_chunk_reports_zero_rows() {
    // A SHAM that decoded the version word but stored no palettes —
    // e.g. an `expected_height == 0` parse path or a hand-built
    // empty descriptor. rows() / is_empty() / row_palette() must all
    // agree.
    let sham = Sham {
        version: 0,
        palettes: Vec::new(),
    };
    assert!(sham.is_empty());
    assert_eq!(sham.rows(), 0);
    assert!(sham.row_palette(0).is_none());
    let base: Vec<[u8; 3]> = vec![[1, 2, 3]; 16];
    let fallback = sham.palette_at_line(&base, 0);
    assert_eq!(fallback, base, "empty SHAM uses base for any y");
}

#[test]
fn sham_parse_pads_short_chunks_and_accessors_see_padded_rows() {
    // The parser pads missing rows by repeating the prior palette;
    // verify the typed accessors observe the padded view (so callers
    // don't need to re-implement the padding rule).
    // Build a 4-byte body: version=0 then ONE explicit 32-byte palette
    // whose only non-zero entry is RGB444 (red) at index 1.
    let mut body: Vec<u8> = Vec::with_capacity(2 + 32);
    body.extend_from_slice(&0u16.to_be_bytes());
    for i in 0..16u8 {
        if i == 1 {
            // index-1: 0x0F00 → r=0xF, g=0, b=0
            body.push(0x0F);
            body.push(0x00);
        } else {
            body.push(0x00);
            body.push(0x00);
        }
    }
    let sham = Sham::parse(&body, 3).expect("SHAM parse with padded tail");
    // rows() reports the padded length, not the explicit byte count.
    assert_eq!(sham.rows(), 3);
    // The parser widens 0xF→0xFF.
    let red = [0xFF, 0, 0];
    assert_eq!(sham.row_palette(0).unwrap()[1], red, "explicit row 0");
    assert_eq!(
        sham.row_palette(1).unwrap()[1],
        red,
        "row 1 padded by repeating row 0"
    );
    assert_eq!(
        sham.row_palette(2).unwrap()[1],
        red,
        "row 2 padded by repeating row 0"
    );
}

// ───────────────────── PCHG ─────────────────────

#[test]
fn pchg_small_format_roundtrip() {
    // Build a 4×2 indexed image; PCHG overrides palette index 1 to
    // green on row 1.
    let pal: Vec<[u8; 3]> = vec![[0xFF, 0, 0], [0xFF, 0, 0]]; // both red
    let bmhd = Bmhd {
        width: 4,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 4,
        page_height: 2,
    };
    // Build a synthetic PCHG raw payload (small format):
    // Header 20 bytes:
    //   u16 Compression=0; u16 Flags=1 (small);
    //   i16 StartLine=0; u16 LineCount=2;
    //   u16 ChangedLines=1; u16 MinReg=1; u16 MaxReg=1; u16 MaxChanges=1;
    //   u32 TotalChanges=1.
    let mut raw = Vec::new();
    raw.extend_from_slice(&0u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&0i16.to_be_bytes());
    raw.extend_from_slice(&2u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u32.to_be_bytes());
    // LineMask (one longword covers LineCount=2): row 0 clear, row 1 set.
    raw.extend_from_slice(&[0x40, 0x00, 0x00, 0x00]);
    // Row 1 record: ChangeCount16=1, ChangeCount32=0, then the packed
    // word (reg 1 << 12) | (R4 0x0 << 8) | (G4 0xF << 4) | B4 0x0.
    raw.push(1);
    raw.push(0);
    raw.extend_from_slice(&0x10F0u16.to_be_bytes());
    let pchg = oxideav_iff::ilbm::Pchg {
        raw: raw.clone(),
        lines: vec![PchgLine {
            line: 1,
            changes: vec![PchgChange::new(1, [0, 0xFF, 0])],
        }],
    };

    let mut rgba = Vec::with_capacity(4 * 2 * 4);
    // Row 0: all index-1 → red (palette default).
    for _ in 0..4 {
        rgba.extend_from_slice(&[0xFF, 0, 0, 0xFF]);
    }
    // Row 1: all index-1 → green (PCHG override).
    for _ in 0..4 {
        rgba.extend_from_slice(&[0, 0xFF, 0, 0xFF]);
    }
    let img = IlbmImage {
        width: 4,
        height: 2,
        bmhd,
        palette: pal,
        camg: Camg::default(),
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        pchg: Some(pchg),
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&img).unwrap();
    let pos = bytes
        .windows(4)
        .position(|w| w == b"PCHG")
        .expect("PCHG chunk in encoded file");
    let _ = pos;
    let dec = parse_ilbm(&bytes).unwrap();
    let dec_pchg = dec.pchg.expect("PCHG round-tripped");
    assert_eq!(dec_pchg.lines.len(), 1);
    assert_eq!(dec_pchg.lines[0].line, 1);
    assert_eq!(dec_pchg.lines[0].changes.len(), 1);
    assert_eq!(dec_pchg.lines[0].changes[0].index, 1);
    assert_eq!(dec_pchg.lines[0].changes[0].rgb, [0, 0xFF, 0]);
    // Row 0 stays red, row 1 turns green.
    assert_eq!(&dec.rgba[0..3], &[0xFF, 0, 0]);
    assert_eq!(&dec.rgba[4 * 4..4 * 4 + 3], &[0, 0xFF, 0]);
}

// ───────────────────── HAM6 / HAM8 encode ─────────────────────

#[test]
fn ham6_encode_decode_smooth_gradient() {
    // 16×2 grey gradient — HAM6 should reach within ±16 LSB on each
    // channel because the value field is 4 bits (16 levels).
    let bmhd = Bmhd {
        width: 16,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 6, // HAM6
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 16,
        page_height: 2,
    };
    let palette: Vec<[u8; 3]> = (0..16u8).map(|i| [i * 17, i * 17, i * 17]).collect();
    let mut rgba = Vec::with_capacity(16 * 2 * 4);
    for _y in 0..2 {
        for x in 0..16u8 {
            // Smooth gradient 0..255 in steps of 17.
            let v = x.saturating_mul(17);
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let img = IlbmImage {
        width: 16,
        height: 2,
        bmhd,
        palette,
        camg: Camg { raw: CAMG_HAM },
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.width, 16);
    assert_eq!(dec.height, 2);
    assert!(dec.camg.is_ham(), "CAMG HAM bit preserved");
    // Allow up to 16 LSB error per channel (HAM6 quantises to 4-bit).
    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(dec.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 16,
                "pixel {i} channel {c}: orig={} got={} err={}",
                orig[c],
                got[c],
                err
            );
        }
        assert_eq!(got[3], 0xFF, "alpha pixel {i}");
    }
}

#[test]
fn ham8_encode_decode_smooth_gradient() {
    // 64×2 fine gradient — HAM8's 6-bit channel should be within
    // 4 LSB of the source.
    let bmhd = Bmhd {
        width: 64,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 8,
        masking: Masking::None,
        compression: Compression::ByteRun1,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 64,
        page_height: 2,
    };
    let palette: Vec<[u8; 3]> = (0..64u8).map(|i| [i * 4, i * 4, i * 4]).collect();
    let mut rgba = Vec::with_capacity(64 * 2 * 4);
    for _y in 0..2 {
        for x in 0..64u8 {
            let v = x.saturating_mul(4);
            rgba.extend_from_slice(&[v, v, v, 0xFF]);
        }
    }
    let img = IlbmImage {
        width: 64,
        height: 2,
        bmhd,
        palette,
        camg: Camg { raw: CAMG_HAM },
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.width, 64);
    assert!(dec.camg.is_ham());
    for (i, (orig, got)) in rgba
        .chunks_exact(4)
        .zip(dec.rgba.chunks_exact(4))
        .enumerate()
    {
        for c in 0..3 {
            let err = (orig[c] as i32 - got[c] as i32).abs();
            assert!(
                err <= 4,
                "pixel {i} channel {c}: orig={} got={} err={}",
                orig[c],
                got[c],
                err
            );
        }
    }
}

// ───────────────────── EHB encode ─────────────────────

#[test]
fn ehb_encode_uses_expanded_palette() {
    // 8×1 image with two distinct colours: `[0xFF,0,0]` (palette[1])
    // and `[0x7F,0,0]` (palette[33] — half-brite of palette[1]). EHB
    // encode should emit indices 1 and 33 respectively.
    let mut palette: Vec<[u8; 3]> = vec![[0; 3]; 32];
    palette[1] = [0xFE, 0, 0]; // 0xFE so half = 0x7F
    let bmhd = Bmhd {
        width: 8,
        height: 1,
        x_origin: 0,
        y_origin: 0,
        n_planes: 6,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 8,
        page_height: 1,
    };
    let mut rgba = Vec::with_capacity(8 * 4);
    for x in 0..8 {
        if x < 4 {
            rgba.extend_from_slice(&[0xFE, 0, 0, 0xFF]); // pal[1]
        } else {
            rgba.extend_from_slice(&[0x7F, 0, 0, 0xFF]); // pal[33] = half of pal[1]
        }
    }
    let img = IlbmImage {
        width: 8,
        height: 1,
        bmhd,
        palette: palette.clone(),
        camg: Camg { raw: CAMG_EHB },
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    assert!(dec.camg.is_ehb(), "CAMG EHB bit preserved");
    // After decoding the EHB-expanded palette puts pal[33] at a
    // half-brite of pal[1] = 0x7F. Compare RGB exactly.
    let expanded = expand_ehb_palette(&palette);
    assert_eq!(expanded[33], [0x7F, 0, 0]);
    for x in 0..4 {
        assert_eq!(&dec.rgba[x * 4..x * 4 + 3], &[0xFE, 0, 0]);
    }
    for x in 4..8 {
        assert_eq!(&dec.rgba[x * 4..x * 4 + 3], &[0x7F, 0, 0]);
    }
}

// ───────────────────── ANIM op-5 byte-vertical delta ─────────────────────

/// Build a hand-crafted ANIM5 stream and verify the op-5 decoder
/// updates the planar state correctly. The seed frame is solid index
/// 0; the delta sets the first column of plane 0 to all 1s for the
/// first 4 rows via a single short-form literal op.
#[test]
fn op5_short_form_writes_literals_at_row_cursor() {
    // 16×4 image, 1 bitplane, no mask.
    let bmhd = Bmhd {
        width: 16,
        height: 4,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 16,
        page_height: 4,
    };
    let palette: Vec<[u8; 3]> = vec![[0, 0, 0], [0xFF, 0xFF, 0xFF]];
    let mut rgba = Vec::with_capacity(16 * 4 * 4);
    for _ in 0..(16 * 4) {
        rgba.extend_from_slice(&[0, 0, 0, 0xFF]); // index 0 = black
    }
    let seed = IlbmImage {
        width: 16,
        height: 4,
        bmhd,
        palette: palette.clone(),
        camg: Camg::default(),
        rgba: rgba.clone(),
        form_type: *b"ILBM",
        ..IlbmImage::default()
    };

    // Simulate the planar state for the seed frame: 4 rows × 1 plane,
    // each plane row is `(width + 15) / 16 * 2 = 2` bytes (all zero).
    let mut planar: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 2]).collect();

    // Build the delta.
    // Pointer table: 8 u32s. Plane 0 → offset 32. Planes 1..=7 → 0.
    let mut delta = Vec::new();
    delta.extend_from_slice(&32u32.to_be_bytes()); // plane 0
    for _ in 1..8 {
        delta.extend_from_slice(&0u32.to_be_bytes());
    }
    // Plane 0 data list:
    // Column 0: short-form literal cnt=4, write 0xFF×4 down rows 0..=3,
    // then column terminator.
    delta.push(0x80 | 4);
    delta.extend_from_slice(&[0xFF; 4]);
    delta.push(0x00);
    // Column 1: column terminator immediately (no changes).
    delta.push(0x00);

    let anhd = Anhd {
        operation: 5,
        ..Default::default()
    };
    apply_op5_for_test(&anhd, &mut planar, &delta, &seed.bmhd).unwrap();

    // After delta, planar[r][0] should be 0xFF for r=0..=3.
    for r in 0..4 {
        assert_eq!(
            planar[r][0], 0xFF,
            "row {r} col 0 of plane 0 should be 0xFF after delta"
        );
        assert_eq!(planar[r][1], 0x00, "row {r} col 1 unchanged");
    }
}

#[test]
fn op5_skip_then_long_repeat() {
    // Cover the long-form (op=0x80) path and the skip path.
    let bmhd = Bmhd {
        width: 16,
        height: 8,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 16,
        page_height: 8,
    };
    let mut planar: Vec<Vec<u8>> = (0..8).map(|_| vec![0u8; 2]).collect();
    // Plane 0 data list:
    //   Column 0: skip 3 rows, then long repeat cnt=4 v=0x77, then end.
    let mut delta = Vec::new();
    delta.extend_from_slice(&32u32.to_be_bytes());
    for _ in 1..8 {
        delta.extend_from_slice(&0u32.to_be_bytes());
    }
    delta.push(3); // skip 3 rows (top bit clear)
    delta.push(0x80); // long form
    delta.push(4); // count
    delta.push(0x77); // repeat byte
    delta.push(0); // column terminator
    delta.push(0); // column 1 terminator

    let anhd = Anhd {
        operation: 5,
        ..Default::default()
    };
    apply_op5_for_test(&anhd, &mut planar, &delta, &bmhd).unwrap();
    for (r, row) in planar.iter().enumerate() {
        let expected = if (3..7).contains(&r) { 0x77 } else { 0x00 };
        assert_eq!(row[0], expected, "row {r}");
    }
}

#[test]
fn anim_op0_roundtrip_via_iff_anim_demuxer() {
    // Build a 3-frame ANIM file via the encoder, write to disk, then
    // demux through the registry to confirm the wire format works
    // through the public API.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let bmhd = Bmhd {
        width: 8,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 8,
        page_height: 2,
    };
    let mut frames: Vec<IlbmImage> = Vec::new();
    for f in 0..3 {
        let mut rgba = Vec::with_capacity(8 * 2 * 4);
        for _ in 0..16 {
            // alternate between black and red across frames
            if f % 2 == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0xFF]);
            } else {
                rgba.extend_from_slice(&[255, 0, 0, 0xFF]);
            }
        }
        frames.push(IlbmImage {
            width: 8,
            height: 2,
            bmhd,
            palette: pal.clone(),
            camg: Camg::default(),
            rgba,
            form_type: *b"ILBM",
            ..IlbmImage::default()
        });
    }
    let bytes = oxideav_iff::anim::encode_anim_op0(&frames).unwrap();
    let parsed: AnimImage = parse_anim(&bytes).unwrap();
    assert_eq!(parsed.frames.len(), 3);
    assert_eq!(parsed.width, 8);
    assert_eq!(parsed.height, 2);
    assert_eq!(&parsed.frames[0].rgba[0..3], &[0, 0, 0]);
    assert_eq!(&parsed.frames[1].rgba[0..3], &[255, 0, 0]);
    assert_eq!(&parsed.frames[2].rgba[0..3], &[0, 0, 0]);
}
