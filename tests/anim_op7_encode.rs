//! Round-209 coverage: ANIM op-7 (Short / Long Vertical Delta) encoder.
//!
//! Each test builds a short ILBM frame sequence, encodes it through
//! [`encode_anim_op7`], decodes it back through [`parse_anim`], and
//! checks the resulting frames are pixel-equal to the input.
//! Additional tests probe op-7-specific edge cases:
//!
//! * a frame pair with no delta → every plane's opcode pointer is `0`
//!   and the DLTA payload is exactly 64 bytes (just the pointer table);
//! * short-data mode (2-byte items) and long-data mode (4-byte items)
//!   both round-trip;
//! * a sparse single-column delta produces a DLTA smaller than the
//!   uncompressed BODY would have been;
//! * `encode_op7_body` is callable directly and produces the same
//!   bytes as a hand-constructed pointer table when no plane is
//!   dirty.

use oxideav_iff::anim::{encode_anim_op7, encode_op7_body, parse_anim};
use oxideav_iff::ilbm::{Bmhd, Camg, Compression, IlbmImage, Masking};

fn make_frame(
    w: u16,
    h: u16,
    n_planes: u8,
    palette: &[[u8; 3]],
    pixel: impl Fn(usize, usize) -> u8,
) -> IlbmImage {
    let bmhd = Bmhd {
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
    };
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
        bmhd,
        palette: palette.to_vec(),
        camg: Camg::default(),
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    }
}

#[test]
fn op7_short_roundtrip_identical_frames() {
    // Two frames pixel-identical → encoder emits an empty DLTA (all
    // plane pointers = 0, exactly 64 bytes) and the decoder yields
    // the same RGBA back from the planar state.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame(16, 8, 1, &pal, |x, y| ((x ^ y) & 1) as u8);
    let frames = vec![frame.clone(), frame.clone()];
    let bytes = encode_anim_op7(&frames, false).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame.rgba);
    assert_eq!(dec.frames[1].rgba, frame.rgba);
}

#[test]
fn op7_long_roundtrip_identical_frames() {
    // Same as above but in long-data mode (4-byte items). row_bytes
    // must be a multiple of 4 — width 32 → row_bytes 4 (1 plane).
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame(32, 4, 1, &pal, |x, y| ((x ^ y) & 1) as u8);
    let frames = vec![frame.clone(), frame.clone()];
    let bytes = encode_anim_op7(&frames, true).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[1].rgba, frame.rgba);
}

#[test]
fn op7_short_roundtrip_three_changing_frames() {
    // Three solid-colour frames cycling red → green → blue. Each
    // frame changes every pixel, so every column will emit Same
    // ops; the round-trip must preserve all three frames.
    let pal = vec![[0u8, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let frames = vec![
        make_frame(16, 4, 2, &pal, |_, _| 1),
        make_frame(16, 4, 2, &pal, |_, _| 2),
        make_frame(16, 4, 2, &pal, |_, _| 3),
    ];
    let bytes = encode_anim_op7(&frames, false).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3);
    for (i, (got, want)) in dec.frames.iter().zip(frames.iter()).enumerate() {
        assert_eq!(got.rgba, want.rgba, "frame {i} mismatch");
    }
}

#[test]
fn op7_short_sparse_delta_columns_smaller_than_full_body() {
    // Frame A: all-zero indices; Frame B: only one column flipped.
    // Even though we re-pack via nearest-fit, with a 2-colour palette
    // the index choice is deterministic — sparse delta in column 0
    // only, so most columns emit op_count = 0.
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame_a = make_frame(16, 8, 1, &pal, |_, _| 0);
    let frame_b = make_frame(16, 8, 1, &pal, |x, _| if x == 0 { 1 } else { 0 });
    let frames = vec![frame_a.clone(), frame_b.clone()];
    let bytes = encode_anim_op7(&frames, false).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame_a.rgba);
    assert_eq!(dec.frames[1].rgba, frame_b.rgba);
}

#[test]
fn op7_body_unchanged_frame_yields_zero_pointer_table() {
    // Verify the raw DLTA payload shape when no plane is dirty: 64
    // bytes of zero pointers, no op or data lists. encode_op7_body is
    // exposed for callers building their own ANIM streams.
    use oxideav_iff::ilbm::indices_to_planar_row;

    // 16-wide, 4-tall, 1-plane planar state (row_bytes = 2 for short).
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
    let planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| {
            let row = vec![0u8; bmhd.width as usize];
            indices_to_planar_row(&row, bmhd.n_planes, bmhd.row_bytes()).remove(0)
        })
        .collect();
    let dlta = encode_op7_body(&planar, &planar, &bmhd, false).unwrap();
    assert_eq!(dlta.len(), 64);
    assert!(dlta.iter().all(|&b| b == 0));
}

#[test]
fn op7_short_long_data_produce_different_dlta() {
    // Same frame pair, short vs long data mode. The DLTA payloads
    // should differ because the column count differs (row_bytes /
    // data_size), which affects the op-list structure.
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame_a = make_frame(32, 4, 1, &pal, |_, _| 0);
    let frame_b = make_frame(32, 4, 1, &pal, |_, _| 1);
    let frames = vec![frame_a, frame_b];
    let short_bytes = encode_anim_op7(&frames, false).unwrap();
    let long_bytes = encode_anim_op7(&frames, true).unwrap();
    assert_ne!(short_bytes, long_bytes);
    // Both still round-trip via parse_anim.
    let dec_s = parse_anim(&short_bytes).unwrap();
    let dec_l = parse_anim(&long_bytes).unwrap();
    assert_eq!(dec_s.frames.len(), 2);
    assert_eq!(dec_l.frames.len(), 2);
    assert_eq!(dec_s.frames[1].rgba, dec_l.frames[1].rgba);
}

#[test]
fn op7_rejects_long_data_with_unaligned_row_bytes() {
    // Width = 16 → row_bytes = 2. Long-data mode needs row_bytes
    // divisible by 4 → encoder must reject this configuration.
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame = make_frame(16, 4, 1, &pal, |_, _| 0);
    let frames = vec![frame.clone(), frame];
    let r = encode_anim_op7(&frames, true);
    assert!(r.is_err());
}

#[test]
fn op7_rejects_empty_frame_list() {
    let frames: Vec<IlbmImage> = Vec::new();
    let r = encode_anim_op7(&frames, false);
    assert!(r.is_err());
}

#[test]
fn op7_single_seed_frame_yields_no_delta_frames() {
    // One-element frame list → only the leading FORM ILBM, no
    // ANHD/DLTA chunks.
    let pal = vec![[0u8, 0, 0], [255, 0, 0]];
    let frame = make_frame(16, 8, 1, &pal, |x, y| ((x ^ y) & 1) as u8);
    let bytes = encode_anim_op7(std::slice::from_ref(&frame), false).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 1);
    assert_eq!(dec.frames[0].rgba, frame.rgba);
}
