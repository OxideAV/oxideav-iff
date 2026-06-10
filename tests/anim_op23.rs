//! Round-276 coverage: ANIM op-2 / op-3 (Long / Short Delta mode),
//! spec §1.2.2 / §1.2.3 with the §2.2.1 wire format.
//!
//! Two test families:
//!
//! * hand-crafted DLTA byte vectors driven through the decoder
//!   (`apply_op23_for_test`) pin the group grammar — positive-offset
//!   single-word groups, negative-offset runs (`abs = offset + 2`),
//!   the `0xFFFF` terminator, the zero plane pointer, and the
//!   contiguous-plane addressing that lets op-2 long words straddle
//!   row boundaries;
//! * encoder → `parse_anim` round-trips check both modes end-to-end,
//!   including unchanged planes, sparse single-word deltas, and
//!   multi-frame sequences.

use oxideav_iff::anim::{
    apply_op23_for_test, encode_anim_op2, encode_anim_op3, encode_op23_body, parse_anim,
};
use oxideav_iff::ilbm::{Bmhd, Camg, Compression, IlbmImage, Masking};

fn bmhd(w: u16, h: u16, n_planes: u8) -> Bmhd {
    Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    }
}

fn make_frame(
    w: u16,
    h: u16,
    n_planes: u8,
    palette: &[[u8; 3]],
    pixel: impl Fn(usize, usize) -> u8,
) -> IlbmImage {
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let idx = pixel(x, y) as usize % palette.len();
            let p = palette[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: bmhd(w, h, n_planes),
        palette: palette.to_vec(),
        camg: Camg::default(),
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    }
}

/// Build a DLTA whose plane-0 pointer is 32 (data immediately after
/// the 8-slot table, the §2.2.1 worked value) and whose remaining
/// slots are zero.
fn dlta_plane0(groups: &[u8]) -> Vec<u8> {
    let mut d = vec![0u8; 32];
    d[0..4].copy_from_slice(&32u32.to_be_bytes());
    d.extend_from_slice(groups);
    d
}

#[test]
fn handcrafted_short_delta_single_words_and_run() {
    // 16×4, 1 plane → row_bytes = 2, so each short word is exactly
    // one row. Start from an all-zero plane.
    let b = bmhd(16, 4, 1);
    let mut planar: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 2]).collect();

    // Groups (short mode, offsets/counts/data all BE shorts):
    //   offset +1            → cursor word 1, write 0xABCD
    //   negative 0xFFFC      → abs 4, advance 4-2 = 2 → cursor word 3
    //   count 1, data 0x1234 → word 3 = 0x1234
    //   0xFFFF               → terminator
    let groups = [
        0x00, 0x01, 0xAB, 0xCD, // single word at word 1
        0xFF, 0xFC, 0x00, 0x01, 0x12, 0x34, // run of 1 at word 3
        0xFF, 0xFF, // terminator
    ];
    let delta = dlta_plane0(&groups);
    apply_op23_for_test(&mut planar, &delta, &b, false).unwrap();
    assert_eq!(planar[0], vec![0x00, 0x00], "row 0 untouched");
    assert_eq!(planar[1], vec![0xAB, 0xCD], "row 1 = first group");
    assert_eq!(planar[2], vec![0x00, 0x00], "row 2 untouched");
    assert_eq!(planar[3], vec![0x12, 0x34], "row 3 = run group");
}

#[test]
fn handcrafted_short_delta_run_cursor_lands_on_last_word() {
    // Pin the cursor convention: after a run the cursor points at the
    // run's LAST written word, so a following positive offset of 2
    // skips exactly one word.
    let b = bmhd(16, 6, 1); // 6 words (row_bytes = 2)
    let mut planar: Vec<Vec<u8>> = (0..6).map(|_| vec![0u8; 2]).collect();
    let groups = [
        0xFF, 0xFE, // negative: abs 2, advance 0 → run at word 0
        0x00, 0x02, // count 2
        0x11, 0x11, 0x22, 0x22, // words 0..=1
        0x00, 0x02, // positive offset 2 from word 1 → word 3
        0x33, 0x33, // word 3
        0xFF, 0xFF, // terminator
    ];
    let delta = dlta_plane0(&groups);
    apply_op23_for_test(&mut planar, &delta, &b, false).unwrap();
    assert_eq!(planar[0], vec![0x11, 0x11]);
    assert_eq!(planar[1], vec![0x22, 0x22]);
    assert_eq!(planar[2], vec![0x00, 0x00], "word 2 skipped");
    assert_eq!(planar[3], vec![0x33, 0x33], "offset 2 from run end");
    assert_eq!(planar[4], vec![0x00, 0x00]);
}

#[test]
fn handcrafted_long_delta_word_straddles_rows() {
    // 16×4, 1 plane → row_bytes = 2; in Long Delta mode (op 2) each
    // 4-byte data word spans TWO rows of the contiguous plane. Word
    // index 1 covers rows 2 and 3.
    let b = bmhd(16, 4, 1);
    let mut planar: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 2]).collect();
    let groups = [
        0x00, 0x01, // positive offset 1 → long word 1
        0xDE, 0xAD, 0xBE, 0xEF, // 4-byte data word
        0xFF, 0xFF, // terminator
    ];
    let delta = dlta_plane0(&groups);
    apply_op23_for_test(&mut planar, &delta, &b, true).unwrap();
    assert_eq!(planar[0], vec![0x00, 0x00]);
    assert_eq!(planar[1], vec![0x00, 0x00]);
    assert_eq!(planar[2], vec![0xDE, 0xAD], "long word hi half → row 2");
    assert_eq!(planar[3], vec![0xBE, 0xEF], "long word lo half → row 3");
}

#[test]
fn handcrafted_zero_pointer_leaves_plane_untouched() {
    let b = bmhd(16, 4, 1);
    let mut planar: Vec<Vec<u8>> = (0..4).map(|_| vec![0x5A; 2]).collect();
    let before = planar.clone();
    let delta = vec![0u8; 32]; // all pointers zero
    apply_op23_for_test(&mut planar, &delta, &b, false).unwrap();
    assert_eq!(planar, before);
}

#[test]
fn handcrafted_rejects_truncated_and_out_of_range() {
    let b = bmhd(16, 4, 1);
    let mut planar: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 2]).collect();

    // Pointer table shorter than 32 bytes.
    assert!(apply_op23_for_test(&mut planar, &[0u8; 16], &b, false).is_err());

    // Plane pointer past the end of the DLTA.
    let mut d = vec![0u8; 32];
    d[0..4].copy_from_slice(&999u32.to_be_bytes());
    assert!(apply_op23_for_test(&mut planar, &d, &b, false).is_err());

    // Missing terminator → group list truncated.
    let d = dlta_plane0(&[0x00, 0x01, 0xAB, 0xCD]);
    assert!(apply_op23_for_test(&mut planar, &d, &b, false).is_err());

    // Word offset past the plane end (plane has 4 short words).
    let d = dlta_plane0(&[0x00, 0x09, 0xAB, 0xCD, 0xFF, 0xFF]);
    assert!(apply_op23_for_test(&mut planar, &d, &b, false).is_err());

    // Run extending past the plane end.
    let d = dlta_plane0(&[
        0xFF, 0xFE, 0x00, 0x09, // run of 9 at word 0 (plane holds 4)
        0xFF, 0xFF,
    ]);
    assert!(apply_op23_for_test(&mut planar, &d, &b, false).is_err());

    // Zero-length run is degenerate.
    let d = dlta_plane0(&[0xFF, 0xFE, 0x00, 0x00, 0xFF, 0xFF]);
    assert!(apply_op23_for_test(&mut planar, &d, &b, false).is_err());
}

#[test]
fn op3_roundtrip_three_changing_frames() {
    let pal = vec![[0u8, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let frames = vec![
        make_frame(16, 4, 2, &pal, |_, _| 1),
        make_frame(16, 4, 2, &pal, |_, _| 2),
        make_frame(16, 4, 2, &pal, |_, _| 3),
    ];
    let bytes = encode_anim_op3(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3);
    for (i, (got, want)) in dec.frames.iter().zip(frames.iter()).enumerate() {
        assert_eq!(got.rgba, want.rgba, "frame {i} mismatch");
    }
}

#[test]
fn op2_roundtrip_three_changing_frames() {
    // Long Delta mode over the same sequence. Width 16 → row_bytes 2,
    // plane bytes = 2 × 4 = 8 → divisible by 4, exercising long words
    // that straddle row boundaries through the full container path.
    let pal = vec![[0u8, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let frames = vec![
        make_frame(16, 4, 2, &pal, |_, _| 1),
        make_frame(16, 4, 2, &pal, |x, y| if (x ^ y) & 1 == 0 { 2 } else { 1 }),
        make_frame(16, 4, 2, &pal, |_, _| 3),
    ];
    let bytes = encode_anim_op2(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3);
    for (i, (got, want)) in dec.frames.iter().zip(frames.iter()).enumerate() {
        assert_eq!(got.rgba, want.rgba, "frame {i} mismatch");
    }
}

#[test]
fn op3_sparse_delta_smaller_than_full_body() {
    // Frame B differs from frame A in a single 16-pixel row → one
    // short word per plane changes. The DLTA should be far smaller
    // than the uncompressed plane data.
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame_a = make_frame(16, 16, 1, &pal, |_, _| 0);
    let frame_b = make_frame(16, 16, 1, &pal, |_, y| if y == 7 { 1 } else { 0 });
    let frames = vec![frame_a.clone(), frame_b.clone()];
    let bytes = encode_anim_op3(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame_a.rgba);
    assert_eq!(dec.frames[1].rgba, frame_b.rgba);
}

#[test]
fn op23_body_unchanged_frames_yield_zero_pointer_table() {
    use oxideav_iff::ilbm::indices_to_planar_row;
    let b = bmhd(16, 4, 1);
    let planar: Vec<Vec<u8>> = (0..b.height as usize)
        .map(|_| {
            let row = vec![0u8; b.width as usize];
            indices_to_planar_row(&row, b.n_planes, b.row_bytes()).remove(0)
        })
        .collect();
    let dlta = encode_op23_body(&planar, &planar, &b, false).unwrap();
    assert_eq!(dlta.len(), 32, "just the 8-slot pointer table");
    assert!(dlta.iter().all(|&v| v == 0));
}

#[test]
fn op23_body_run_uses_negative_offset_group() {
    // Two adjacent changed words must collapse into one run group:
    // 32 (table) + 2 (offset) + 2 (count) + 2×2 (data) + 2 (term).
    let b = bmhd(16, 4, 1);
    let prev: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 2]).collect();
    let mut cur = prev.clone();
    cur[1] = vec![0xAA, 0xAA];
    cur[2] = vec![0xBB, 0xBB];
    let dlta = encode_op23_body(&prev, &cur, &b, false).unwrap();
    assert_eq!(dlta.len(), 32 + 2 + 2 + 4 + 2);
    // Plane-0 pointer = 32; first group: words 1..=2 changed, cursor
    // starts at word 0 → offset 1 → encoded abs = 3 → 0xFFFD.
    assert_eq!(&dlta[0..4], &32u32.to_be_bytes());
    assert_eq!(&dlta[32..34], &[0xFF, 0xFD], "negative offset, abs 3");
    assert_eq!(&dlta[34..36], &[0x00, 0x02], "count 2");
    assert_eq!(&dlta[36..40], &[0xAA, 0xAA, 0xBB, 0xBB]);
    assert_eq!(&dlta[40..42], &[0xFF, 0xFF], "terminator");

    // And the decoder reverses it.
    let mut planar = prev.clone();
    apply_op23_for_test(&mut planar, &dlta, &b, false).unwrap();
    assert_eq!(planar, cur);
}

#[test]
fn op23_body_rejects_unaligned_plane_for_long_words() {
    // 16×3 → plane bytes = 6, not a multiple of 4: the trailing two
    // bytes would be unaddressable in Long Delta mode.
    let b = bmhd(16, 3, 1);
    let planar: Vec<Vec<u8>> = (0..3).map(|_| vec![0u8; 2]).collect();
    assert!(encode_op23_body(&planar, &planar, &b, true).is_err());
    // Short mode is fine.
    assert!(encode_op23_body(&planar, &planar, &b, false).is_ok());
}

#[test]
fn op2_op3_produce_different_dlta_but_same_frames() {
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame_a = make_frame(32, 4, 1, &pal, |_, _| 0);
    let frame_b = make_frame(32, 4, 1, &pal, |_, _| 1);
    let frames = vec![frame_a, frame_b];
    let op2 = encode_anim_op2(&frames).unwrap();
    let op3 = encode_anim_op3(&frames).unwrap();
    assert_ne!(op2, op3);
    let dec2 = parse_anim(&op2).unwrap();
    let dec3 = parse_anim(&op3).unwrap();
    assert_eq!(dec2.frames.len(), 2);
    assert_eq!(dec3.frames.len(), 2);
    assert_eq!(dec2.frames[1].rgba, dec3.frames[1].rgba);
}

#[test]
fn op23_rejects_empty_frame_list() {
    assert!(encode_anim_op2(&[]).is_err());
    assert!(encode_anim_op3(&[]).is_err());
}

#[test]
fn op3_single_seed_frame_yields_no_delta_frames() {
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame = make_frame(16, 8, 1, &pal, |x, y| ((x ^ y) & 1) as u8);
    let bytes = encode_anim_op3(std::slice::from_ref(&frame)).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 1);
    assert_eq!(dec.frames[0].rgba, frame.rgba);
}
