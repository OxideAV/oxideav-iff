//! PCHG with neither `PCHGF_12BIT` nor `PCHGF_32BIT` set — the
//! spec-undefined case, resolved by validation instead of a guess.
//!
//! The PCHG spec defines exactly two record layouts and no default
//! when both format flags are clear. Reader policy: attempt the
//! Small parse strictly (records fully present, registers within the
//! header's `[MinReg, MaxReg]`, LineData consumed exactly), then the
//! Big parse under the same tests, and ignore the chunk when neither
//! validates — a static CMAP palette beats garbage colours. Also pins
//! the interlace rule: PCHG line indices live in *frame space* (rows
//! as stored in BODY), applied with no halving or field-parity
//! adjustment.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, Pchg, PchgChange,
    PchgKind, PchgLine, CAMG_LACE,
};

fn bmhd(width: u16, height: u16) -> Bmhd {
    Bmhd {
        width,
        height,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: width as i16,
        page_height: height as i16,
    }
}

fn line(l: u32, changes: &[(u16, [u8; 3])]) -> PchgLine {
    PchgLine {
        line: l,
        changes: changes
            .iter()
            .map(|&(index, rgb)| PchgChange::new(index, rgb))
            .collect(),
    }
}

/// Clear the 16-bit Flags word of an encoded PCHG chunk body.
fn strip_flags(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes[2] = 0;
    bytes[3] = 0;
    bytes
}

#[test]
fn flagless_small_layout_resolves_by_validation() {
    // 4-bit-exact channels so the Small quantisation is lossless.
    let lines = vec![
        line(0, &[(3, [0x11, 0x22, 0x33]), (20, [0xAA, 0xBB, 0xCC])]),
        line(5, &[(1, [0xFF, 0x00, 0x44])]),
    ];
    let bytes =
        strip_flags(Pchg::from_lines(lines.clone(), PchgKind::Small).encode(PchgKind::Small));
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines, lines, "strict Small parse should win");
}

#[test]
fn flagless_big_layout_resolves_by_validation() {
    // Registers 5..=9: the Small attempt decodes different register
    // numbers (and a different record span), so the strict
    // `[MinReg, MaxReg]` + exact-consumption tests reject it and the
    // Big attempt wins.
    let lines = vec![
        line(2, &[(5, [0x01, 0x02, 0x03]), (9, [0x99, 0x88, 0x77])]),
        line(7, &[(6, [0x10, 0x20, 0x30])]),
    ];
    let bytes = strip_flags(Pchg::from_lines(lines.clone(), PchgKind::Big).encode(PchgKind::Big));
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines, lines, "strict Big parse should win");
}

#[test]
fn flagless_huffman_body_resolves_after_expansion() {
    let lines = vec![line(1, &[(5, [0x40, 0x50, 0x60])])];
    let bytes =
        strip_flags(Pchg::from_lines(lines.clone(), PchgKind::Big).encode_huffman(PchgKind::Big));
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines, lines);
}

/// A flagless chunk whose LineData fits neither layout.
fn unvalidatable_flagless_chunk() -> Vec<u8> {
    let mut raw = Vec::new();
    raw.extend_from_slice(&0u16.to_be_bytes()); // Compression = none
    raw.extend_from_slice(&0u16.to_be_bytes()); // Flags = 0 (neither)
    raw.extend_from_slice(&0i16.to_be_bytes()); // StartLine
    raw.extend_from_slice(&2u16.to_be_bytes()); // LineCount = 2
    raw.extend_from_slice(&1u16.to_be_bytes()); // ChangedLines
    raw.extend_from_slice(&0u16.to_be_bytes()); // MinReg
    raw.extend_from_slice(&0u16.to_be_bytes()); // MaxReg = 0
    raw.extend_from_slice(&1u16.to_be_bytes()); // MaxChanges
    raw.extend_from_slice(&1u32.to_be_bytes()); // TotalChanges
    raw.extend_from_slice(&[0x80, 0, 0, 0]); // LineMask: line 0 set
                                             // 3 trailing bytes: too short for a Small record's word list and
                                             // for a Big record; registers would also overrun MaxReg = 0.
    raw.extend_from_slice(&[0x01, 0x02, 0x03]);
    raw
}

#[test]
fn flagless_unvalidatable_chunk_is_rejected_by_parse() {
    let err = Pchg::parse(&unvalidatable_flagless_chunk()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("PCHGF_12BIT"),
        "diagnostic names the flags: {msg}"
    );
}

#[test]
fn flagless_unvalidatable_chunk_is_ignored_by_the_ilbm_reader() {
    // Reader policy: ignore the unusable PCHG, render from CMAP alone.
    let palette = vec![[0x00, 0x00, 0x00], [0xE0, 0x30, 0x30]];
    let base = IlbmImage {
        width: 4,
        height: 4,
        bmhd: bmhd(4, 4),
        palette: palette.clone(),
        rgba: [0xE0, 0x30, 0x30, 0xFF].repeat(16),
        ..IlbmImage::default()
    };
    let mut form = encode_ilbm(&base).unwrap();
    // Splice the bad PCHG chunk in front of BODY.
    let body_at = form
        .windows(4)
        .position(|w| w == b"BODY")
        .expect("encoded ILBM has a BODY");
    let chunk_body = unvalidatable_flagless_chunk();
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"PCHG");
    chunk.extend_from_slice(&(chunk_body.len() as u32).to_be_bytes());
    chunk.extend_from_slice(&chunk_body);
    if chunk_body.len() % 2 == 1 {
        chunk.push(0);
    }
    form.splice(body_at..body_at, chunk);
    let new_size = (form.len() - 8) as u32;
    form[4..8].copy_from_slice(&new_size.to_be_bytes());

    let img = parse_ilbm(&form).unwrap();
    assert!(img.pchg.is_none(), "unusable PCHG must be ignored");
    assert_eq!(
        &img.rgba[0..4],
        &[0xE0, 0x30, 0x30, 0xFF],
        "pixels come from CMAP alone"
    );
}

#[test]
fn explicit_flag_keeps_the_historical_decode_tolerance() {
    // A *flagged* chunk with a truncated record list still parses
    // leniently (historical PCHG writers were sloppy); the strict
    // gauntlet applies only to the flag-less case.
    let mut bytes = Pchg::from_lines(
        vec![line(0, &[(1, [0x11, 0x11, 0x11]), (2, [0x22, 0x22, 0x22])])],
        PchgKind::Big,
    )
    .encode(PchgKind::Big);
    bytes.truncate(bytes.len() - 3); // cut into the last record
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.lines.len(), 1);
    assert_eq!(back.lines[0].changes.len(), 1, "truncated tail dropped");
}

// ───────────── interlace: PCHG lines are frame-space ─────────────

#[test]
fn lace_changes_apply_at_stored_frame_lines_without_halving() {
    // 400-line-laced-picture arithmetic, miniaturised: an 8-row LACE
    // image with a change at frame line 2. The change must land on
    // stored row 2 exactly — no halving, doubling, or field parity —
    // and odd rows inherit the running palette. The PCHG chunk is
    // spliced into a pre-encoded BODY so every stored pixel keeps
    // palette register 1 (the encoder itself quantises PCHG-aware).
    let palette = vec![[0x00, 0x00, 0x00], [0x10, 0x10, 0x10]];
    let base = IlbmImage {
        width: 4,
        height: 8,
        bmhd: bmhd(4, 8),
        palette: palette.clone(),
        camg: Camg { raw: CAMG_LACE },
        // Every pixel uses register 1.
        rgba: [0x10, 0x10, 0x10, 0xFF].repeat(32),
        ..IlbmImage::default()
    };
    let mut form = encode_ilbm(&base).unwrap();
    let pchg_body = Pchg::from_lines(vec![line(2, &[(1, [0xAA, 0xBB, 0xCC])])], PchgKind::Big)
        .encode(PchgKind::Big);
    let body_at = form
        .windows(4)
        .position(|w| w == b"BODY")
        .expect("encoded ILBM has a BODY");
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"PCHG");
    chunk.extend_from_slice(&(pchg_body.len() as u32).to_be_bytes());
    chunk.extend_from_slice(&pchg_body);
    if pchg_body.len() % 2 == 1 {
        chunk.push(0);
    }
    form.splice(body_at..body_at, chunk);
    let new_size = (form.len() - 8) as u32;
    form[4..8].copy_from_slice(&new_size.to_be_bytes());

    let decoded = parse_ilbm(&form).unwrap();
    let row = |y: usize| &decoded.rgba[y * 4 * 4..y * 4 * 4 + 4];
    assert_eq!(row(0), &[0x10, 0x10, 0x10, 0xFF], "before the change");
    assert_eq!(row(1), &[0x10, 0x10, 0x10, 0xFF], "line 1 inherits");
    assert_eq!(
        row(2),
        &[0xAA, 0xBB, 0xCC, 0xFF],
        "change lands on frame line 2"
    );
    assert_eq!(
        row(3),
        &[0xAA, 0xBB, 0xCC, 0xFF],
        "line 3 inherits the new state"
    );
    assert_eq!(row(7), &[0xAA, 0xBB, 0xCC, 0xFF]);
}

#[test]
fn even_lines_only_predicate_tracks_the_lace_convention() {
    let even = Pchg::from_lines(
        vec![line(0, &[(1, [0x11; 3])]), line(4, &[(1, [0x22; 3])])],
        PchgKind::Big,
    );
    assert!(even.changes_on_even_lines_only());
    let odd = Pchg::from_lines(vec![line(3, &[(1, [0x11; 3])])], PchgKind::Big);
    assert!(!odd.changes_on_even_lines_only());
}
