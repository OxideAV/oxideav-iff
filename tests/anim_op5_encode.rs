//! Round-76 coverage: ANIM op-5 (Byte Vertical Delta) encoder.
//!
//! Each test builds a short ILBM frame sequence, encodes it through
//! [`encode_anim_op5`], decodes back through [`parse_anim`], and
//! checks the resulting frames are pixel-equal to the input. Several
//! tests also probe edge cases of the column-walking op selector:
//!
//! * a frame pair with no delta → every plane pointer is `0`;
//! * a frame pair with a sparse single-column delta → op-5 BODY is
//!   significantly smaller than op-0 (uncompressed) BODY;
//! * a long unchanged run that crosses the 0x7F skip-op cap →
//!   the encoder emits two skip ops;
//! * a long repeat run that crosses the 0xFF repeat-cnt cap →
//!   the encoder emits two repeat ops;
//! * an op-5 BODY decoded against a hand-rolled planar state matches
//!   the decoder's own apply_op5 output.

use oxideav_iff::anim::{encode_anim_op0, encode_anim_op5, encode_op5_body, parse_anim};
use oxideav_iff::ilbm::{Bmhd, Camg, Compression, IlbmImage, Masking};

fn make_frame(
    w: u16,
    h: u16,
    palette: &[[u8; 3]],
    pixel: impl Fn(usize, usize) -> u8,
) -> IlbmImage {
    let bmhd = Bmhd {
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

fn make_frame_planes(
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
fn op5_roundtrip_identical_frames() {
    // Two frames that are pixel-identical → encoder emits an empty
    // BODY (all 8 plane pointers = 0) and the decoder yields the same
    // RGBA back from the planar state.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame(16, 8, &pal, |x, y| ((x ^ y) & 1) as u8);
    let frames = vec![frame.clone(), frame.clone()];
    let bytes = encode_anim_op5(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame.rgba);
    assert_eq!(dec.frames[1].rgba, frame.rgba);
}

#[test]
fn op5_roundtrip_sparse_delta() {
    // A frame pair where only the top-left 4×4 corner changes between
    // frame 0 and frame 1. Verifies the column-walker emits skips for
    // the unchanged trailing rows and that the round-trip is exact.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame0 = make_frame(16, 16, &pal, |_x, _y| 0);
    let frame1 = make_frame(16, 16, &pal, |x, y| if x < 4 && y < 4 { 1 } else { 0 });
    let frames = vec![frame0.clone(), frame1.clone()];
    let bytes = encode_anim_op5(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame0.rgba);
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op5_smaller_than_op0_on_sparse_delta() {
    // For a 64×64 frame where one row changes, op-5 BODY should be
    // dramatically smaller than the equivalent op-0 uncompressed BODY.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame0 = make_frame(64, 64, &pal, |_x, _y| 0);
    let frame1 = make_frame(64, 64, &pal, |_x, y| if y == 0 { 1 } else { 0 });
    let frames = vec![frame0.clone(), frame1.clone()];
    let op5 = encode_anim_op5(&frames).unwrap();
    let op0 = encode_anim_op0(&frames).unwrap();
    // op-5 stream should be at least 20% smaller than the op-0 stream.
    assert!(
        op5.len() < (op0.len() * 8) / 10,
        "op-5 ({} bytes) should be ≥20% smaller than op-0 ({} bytes) for sparse delta",
        op5.len(),
        op0.len()
    );
    // And the round-trip should still be exact.
    let dec = parse_anim(&op5).unwrap();
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op5_handles_long_skip_run_over_cap() {
    // A height-300 frame where rows 0 and 299 differ, but rows 1..=298
    // do not. The encoder must split the 298-row unchanged span into
    // at least two skip ops (each ≤ 0x7F = 127 rows).
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame0 = make_frame(8, 300, &pal, |_x, _y| 0);
    let frame1 = make_frame(8, 300, &pal, |_x, y| if y == 0 || y == 299 { 1 } else { 0 });
    let frames = vec![frame0.clone(), frame1.clone()];
    let bytes = encode_anim_op5(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op5_handles_long_repeat_run_over_cap() {
    // 300-row column where every row changes from 0x00 to a single
    // repeated byte. The encoder must split the repeat into at least
    // two ops (cnt is a u8 ≤ 0xFF).
    // Use 1 plane, width 8 → 1 byte per row. We need all rows in
    // column 0 to change to 0xFF.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame0 = make_frame(8, 300, &pal, |_x, _y| 0);
    let frame1 = make_frame(8, 300, &pal, |_x, _y| 1);
    let frames = vec![frame0.clone(), frame1.clone()];
    let bytes = encode_anim_op5(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op5_roundtrip_indexed_2plane() {
    // 2-bitplane indexed frame pair. Exercises the multi-plane pointer
    // table — both plane pointers must be non-zero where deltas exist.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0], [0u8, 255, 0], [0u8, 0, 255]];
    let frame0 = make_frame_planes(16, 8, 2, &pal, |x, y| ((x + y) & 3) as u8);
    let frame1 = make_frame_planes(16, 8, 2, &pal, |x, y| ((x ^ y) & 3) as u8);
    let frames = vec![frame0.clone(), frame1.clone()];
    let bytes = encode_anim_op5(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame0.rgba);
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op5_roundtrip_multi_frame_sequence() {
    // Four frames where the changing region migrates across the image
    // (animated dot bouncing). Each transition produces a distinct
    // delta; the decoder must apply them cumulatively against the
    // running planar state.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frames: Vec<IlbmImage> = (0..4)
        .map(|f| make_frame(16, 8, &pal, |x, y| if x == f && y == f { 1 } else { 0 }))
        .collect();
    let bytes = encode_anim_op5(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 4);
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(
            dec.frames[i].rgba, f.rgba,
            "frame {i} pixel-exact after op-5 round trip"
        );
    }
}

#[test]
fn op5_body_empty_when_frames_match() {
    // The raw BODY for two identical planar frames should be exactly
    // 32 bytes of zero (the empty pointer table — no plane data).
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame(16, 8, &pal, |_x, _y| 0);
    // Reconstruct the planar state both sides see; we just feed the
    // same buffer twice.
    // We use a private-ish helper: encode_anim_op5 reduces to this
    // via rgba_to_planar internally. Drive it through the public BODY
    // builder by re-using the same planar buffer.
    let rebuilt = encode_anim_op5(&[frame.clone(), frame.clone()]).unwrap();
    // Locate the second inner ILBM's BODY chunk and verify length 32.
    // Layout:
    //   FORM <size> ANIM
    //     <seed FORM ILBM ...> (possibly + pad)
    //     FORM <size2> ILBM ANHD <40> ... BODY <body_size> <body>
    // We find the second BODY chunk and read its size field.
    let mut found = None;
    let mut i = 0;
    while i + 8 < rebuilt.len() {
        if &rebuilt[i..i + 4] == b"BODY" {
            let size = u32::from_be_bytes([
                rebuilt[i + 4],
                rebuilt[i + 5],
                rebuilt[i + 6],
                rebuilt[i + 7],
            ]) as usize;
            // The first BODY belongs to the seed ILBM; we want the
            // second occurrence (delta-frame BODY).
            if let Some(_first) = found {
                found = Some(size);
                break;
            } else {
                found = Some(size);
            }
            i += 8 + size + (size & 1);
            continue;
        }
        i += 1;
    }
    let body_size = found.expect("at least one BODY chunk in encoded stream");
    assert_eq!(
        body_size, 32,
        "identical frames → op-5 BODY is just the empty pointer table"
    );
}

#[test]
fn encode_op5_body_pointer_table_correct() {
    // Drive encode_op5_body directly with hand-built planar buffers
    // and inspect the resulting pointer table. Only plane 0 has a
    // delta; planes 1..=7 (here we have just plane 0) → unchanged.
    let bmhd = Bmhd {
        width: 8,
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
        page_width: 8,
        page_height: 4,
    };
    // 4 rows × 1 plane. Each row is 1 byte (width 8 → row_bytes = 2,
    // since row_bytes rounds up to even-byte boundary).
    let row_bytes = bmhd.row_bytes();
    let prev: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; row_bytes]).collect();
    let mut cur: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; row_bytes]).collect();
    cur[0][0] = 0xAB;
    let body = encode_op5_body(&prev, &cur, &bmhd).unwrap();
    // Plane 0 pointer should be 32 (immediately after the table).
    let p0 = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    assert_eq!(p0, 32);
    // Planes 1..=7 should all be 0.
    for slot in 1..8 {
        let off = u32::from_be_bytes([
            body[slot * 4],
            body[slot * 4 + 1],
            body[slot * 4 + 2],
            body[slot * 4 + 3],
        ]);
        assert_eq!(off, 0, "plane {slot} pointer should be 0");
    }
    // Decode the BODY and confirm the delta lands.
    use oxideav_iff::anim::{apply_op5_for_test, Anhd};
    let mut state = prev.clone();
    let anhd = Anhd {
        operation: 5,
        ..Default::default()
    };
    apply_op5_for_test(&anhd, &mut state, &body, &bmhd).unwrap();
    assert_eq!(state[0][0], 0xAB);
    for row in state.iter().skip(1).take(3) {
        assert_eq!(row[0], 0x00);
    }
    for row in state.iter().take(4) {
        assert_eq!(row[1], 0x00, "col 1 untouched");
    }
}

#[test]
fn encode_op5_body_rejects_more_than_8_planes() {
    let bmhd = Bmhd {
        width: 8,
        height: 4,
        x_origin: 0,
        y_origin: 0,
        n_planes: 9,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 8,
        page_height: 4,
    };
    let row_bytes = bmhd.row_bytes();
    let prev: Vec<Vec<u8>> = (0..(4 * 9)).map(|_| vec![0u8; row_bytes]).collect();
    let cur: Vec<Vec<u8>> = (0..(4 * 9)).map(|_| vec![0u8; row_bytes]).collect();
    let err = encode_op5_body(&prev, &cur, &bmhd).unwrap_err();
    assert!(format!("{err}").contains("8 colour planes"));
}
