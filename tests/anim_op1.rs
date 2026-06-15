//! Round-302 coverage: ANIM op-1 (XOR ILBM mode, §1.2.1 / §1.3).
//!
//! Each test builds a short ILBM frame sequence, encodes it through
//! [`encode_anim_op1`], decodes back through [`parse_anim`], and checks
//! the resulting frames are pixel-equal to the input. op-1 stores each
//! delta frame as the byte-for-byte XOR of the new frame against the
//! previous frame's planar bitmap, run-length-encoded (or uncompressed
//! per `BMHD.compression`); the decoder XORs the expanded bitmap into
//! the running planar state. The full-frame case (whole bitmap, all
//! planes) is the one the staged spec describes byte-exactly; the
//! partial-rectangle / plane-masked variant is rejected (its BODY
//! layout is undocumented).

use oxideav_iff::anim::{apply_op1_for_test, encode_anim_op1, encode_op1_body, parse_anim, Anhd};
use oxideav_iff::ilbm::{Bmhd, Camg, Compression, IlbmImage, Masking};

fn make_frame_planes(
    w: u16,
    h: u16,
    n_planes: u8,
    compression: Compression,
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
        compression,
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
fn op1_roundtrip_identical_frames_byterun1() {
    // Pixel-identical frames → XOR BODY is all zeros, so the decoded
    // second frame equals the first untouched.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame_planes(16, 8, 1, Compression::ByteRun1, &pal, |x, y| {
        ((x ^ y) & 1) as u8
    });
    let frames = vec![frame.clone(), frame.clone()];
    let bytes = encode_anim_op1(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame.rgba);
    assert_eq!(dec.frames[1].rgba, frame.rgba);
}

#[test]
fn op1_roundtrip_identical_frames_uncompressed() {
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame_planes(16, 8, 1, Compression::None, &pal, |x, y| {
        ((x ^ y) & 1) as u8
    });
    let frames = vec![frame.clone(), frame.clone()];
    let bytes = encode_anim_op1(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames[1].rgba, frame.rgba);
}

#[test]
fn op1_roundtrip_sparse_delta() {
    // Only the top-left 4×4 corner changes between frames.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame0 = make_frame_planes(16, 16, 1, Compression::ByteRun1, &pal, |_x, _y| 0);
    let frame1 = make_frame_planes(16, 16, 1, Compression::ByteRun1, &pal, |x, y| {
        if x < 4 && y < 4 {
            1
        } else {
            0
        }
    });
    let frames = vec![frame0.clone(), frame1.clone()];
    let bytes = encode_anim_op1(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 2);
    assert_eq!(dec.frames[0].rgba, frame0.rgba);
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op1_roundtrip_indexed_2plane() {
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0], [0u8, 255, 0], [0u8, 0, 255]];
    let frame0 = make_frame_planes(16, 8, 2, Compression::ByteRun1, &pal, |x, y| {
        ((x + y) & 3) as u8
    });
    let frame1 = make_frame_planes(16, 8, 2, Compression::ByteRun1, &pal, |x, y| {
        ((x ^ y) & 3) as u8
    });
    let frames = vec![frame0.clone(), frame1.clone()];
    let bytes = encode_anim_op1(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames[0].rgba, frame0.rgba);
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}

#[test]
fn op1_roundtrip_multi_frame_sequence() {
    // A dot migrating across the image; each transition is a distinct
    // XOR delta applied cumulatively against the running planar state.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frames: Vec<IlbmImage> = (0..4)
        .map(|f| {
            make_frame_planes(16, 8, 1, Compression::ByteRun1, &pal, move |x, y| {
                if x == f && y == f % 8 {
                    1
                } else {
                    0
                }
            })
        })
        .collect();
    let bytes = encode_anim_op1(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 4);
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(
            dec.frames[i].rgba, f.rgba,
            "frame {i} pixel-exact after op-1 XOR round trip"
        );
    }
}

#[test]
fn op1_body_xor_is_zero_for_identical_planar() {
    // encode_op1_body of two identical planar buffers XOR-decodes to a
    // no-op: every byte is `prev ^ prev == 0`, and a zero byte in the
    // XOR BODY leaves the running state unchanged (§1.3).
    let bmhd = Bmhd {
        width: 16,
        height: 4,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::ByteRun1,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 16,
        page_height: 4,
    };
    let row_bytes = bmhd.row_bytes();
    let prev: Vec<Vec<u8>> = (0..4).map(|_| vec![0xAAu8; row_bytes]).collect();
    let body = encode_op1_body(&prev, &prev, &bmhd).unwrap();
    // Decode the BODY XOR-wise into a copy of `prev`: the all-zero XOR
    // bitmap must leave every byte untouched.
    let mut state = prev.clone();
    let anhd = Anhd {
        operation: 1,
        mask: 1,
        w: 16,
        h: 4,
        x: 0,
        y: 0,
        ..Default::default()
    };
    apply_op1_for_test(&anhd, &mut state, &body, &bmhd).unwrap();
    assert_eq!(state, prev);
}

#[test]
fn op1_decoder_rejects_partial_rectangle() {
    // A hand-built ANIM where the delta ANHD declares a sub-rectangle
    // (w < width) must be rejected — the partial-BODY layout is not in
    // the staged spec.
    let pal = vec![[0u8, 0, 0], [255u8, 0, 0]];
    let frame = make_frame_planes(16, 8, 1, Compression::ByteRun1, &pal, |_x, _y| 0);
    let mut bytes = encode_anim_op1(&[frame.clone(), frame.clone()]).unwrap();
    // Patch the second ANHD's `w` field (offset = first byte of the
    // 40-byte ANHD body, bytes 2..4) down to half-width. Locate the
    // second "ANHD" tag.
    let mut anhd_positions = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ANHD" {
            anhd_positions.push(i);
        }
        i += 1;
    }
    // The op-1 stream has exactly one ANHD (on the single delta frame).
    assert_eq!(anhd_positions.len(), 1);
    let body = anhd_positions[0] + 8; // skip "ANHD" + ckSize
    bytes[body + 2..body + 4].copy_from_slice(&8u16.to_be_bytes()); // w = 8
    let err = parse_anim(&bytes).unwrap_err();
    assert!(
        format!("{err}").contains("partial-rectangle") || format!("{err}").contains("full-frame"),
        "got: {err}"
    );
}

/// Build the planar (interleaved scanline) bitmap of a frame so a test
/// can hand-construct an XOR BODY against it. Returns `height *
/// planes_per_row` rows of `row_bytes` bytes each. No `HasMask`.
fn frame_planar(frame: &IlbmImage) -> Vec<Vec<u8>> {
    let bmhd = &frame.bmhd;
    let n_planes = bmhd.n_planes as usize;
    let row_bytes = bmhd.row_bytes();
    let w = bmhd.width as usize;
    let h = bmhd.height as usize;
    // Recover a palette index per pixel by matching the RGBA against the
    // frame palette (test palettes are distinct so this is exact).
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(h * n_planes);
    for y in 0..h {
        let mut rows: Vec<Vec<u8>> = (0..n_planes).map(|_| vec![0u8; row_bytes]).collect();
        for x in 0..w {
            let off = (y * w + x) * 4;
            let rgb = [frame.rgba[off], frame.rgba[off + 1], frame.rgba[off + 2]];
            let idx = frame.palette.iter().position(|p| *p == rgb).unwrap_or(0) as u32;
            for (p, row) in rows.iter_mut().enumerate() {
                if (idx >> p) & 1 != 0 {
                    row[x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        out.extend(rows);
    }
    out
}

#[test]
fn op1_plane_masked_full_rectangle_decodes() {
    // §2.1 `mask` plane-subset XOR BODY, full-frame rectangle. Two
    // 3-plane frames differ only in plane 1's bits, so a valid XOR BODY
    // can carry just that plane (mask = 0b010) and the decoder must
    // reconstruct the new frame exactly.
    let pal: Vec<[u8; 3]> = (0..8)
        .map(|i| [i as u8 * 32, i as u8, 255 - i as u8])
        .collect();
    // frame0: index bit0 = (x&1), bit2 = (y&1); plane 1 always 0.
    let frame0 = make_frame_planes(16, 4, 3, Compression::None, &pal, |x, y| {
        ((x & 1) | ((y & 1) << 2)) as u8
    });
    // frame1: same bit0/bit2 pattern, but plane 1 = (x>>1)&1 toggled in.
    let frame1 = make_frame_planes(16, 4, 3, Compression::None, &pal, |x, y| {
        ((x & 1) | (((x >> 1) & 1) << 1) | ((y & 1) << 2)) as u8
    });

    let pl0 = frame_planar(&frame0);
    let pl1 = frame_planar(&frame1);
    let bmhd = &frame1.bmhd;
    let n_planes = bmhd.n_planes as usize;
    let row_bytes = bmhd.row_bytes();
    let h = bmhd.height as usize;

    // Build an uncompressed XOR BODY that carries ONLY plane 1 (the only
    // changed plane), scanline-interleaved: for each scanline emit the
    // single masked plane-1 row of (new XOR old).
    let masked_plane = 1usize;
    let mut body = Vec::new();
    for y in 0..h {
        let idx = y * n_planes + masked_plane;
        let xor: Vec<u8> = pl1[idx]
            .iter()
            .zip(pl0[idx].iter())
            .map(|(a, b)| a ^ b)
            .collect();
        body.extend_from_slice(&xor);
    }
    assert_eq!(body.len(), h * row_bytes);

    // Sanity: planes 0 and 2 are identical between the two frames, so the
    // mask correctly excludes them.
    for p in [0usize, 2] {
        for y in 0..h {
            let idx = y * n_planes + p;
            assert_eq!(pl0[idx], pl1[idx], "plane {p} must be unchanged");
        }
    }

    let anhd = Anhd {
        operation: 1,
        mask: 1 << masked_plane, // 0b010
        w: 16,
        h: h as u16,
        x: 0,
        y: 0,
        ..Default::default()
    };
    let mut state = pl0.clone();
    apply_op1_for_test(&anhd, &mut state, &body, bmhd).unwrap();
    assert_eq!(state, pl1, "plane-masked XOR reconstructs frame1 planar");
}

#[test]
fn op1_plane_masked_byterun1_decodes() {
    // Same idea but with a ByteRun1-compressed single-plane XOR BODY.
    let pal: Vec<[u8; 3]> = (0..4).map(|i| [i as u8 * 64, 0, 0]).collect();
    let frame0 = make_frame_planes(24, 3, 2, Compression::ByteRun1, &pal, |_x, _y| 0);
    let frame1 = make_frame_planes(24, 3, 2, Compression::ByteRun1, &pal, |x, _y| {
        (((x >> 2) & 1) << 1) as u8 // toggles plane 1 only
    });
    let pl0 = frame_planar(&frame0);
    let pl1 = frame_planar(&frame1);
    let bmhd = &frame1.bmhd;
    let n_planes = bmhd.n_planes as usize;
    let h = bmhd.height as usize;

    // ByteRun1-encode each scanline's plane-1 XOR row.
    let masked_plane = 1usize;
    let mut body = Vec::new();
    for y in 0..h {
        let idx = y * n_planes + masked_plane;
        let xor: Vec<u8> = pl1[idx]
            .iter()
            .zip(pl0[idx].iter())
            .map(|(a, b)| a ^ b)
            .collect();
        body.extend_from_slice(&oxideav_iff::ilbm::byterun1_encode_row(&xor));
    }

    let anhd = Anhd {
        operation: 1,
        mask: 1 << masked_plane,
        w: 24,
        h: h as u16,
        x: 0,
        y: 0,
        ..Default::default()
    };
    let mut state = pl0.clone();
    apply_op1_for_test(&anhd, &mut state, &body, bmhd).unwrap();
    assert_eq!(state, pl1);
}

#[test]
fn op1_anhd_full_plane_mask_roundtrips() {
    // The encoder tags the delta ANHD with the all-planes mask; the
    // decoder accepts it as the full-frame case. A 3-plane frame's mask
    // should be 0b111 = 7.
    let pal: Vec<[u8; 3]> = (0..8).map(|i| [i as u8 * 32, 0, 0]).collect();
    let frame0 = make_frame_planes(16, 4, 3, Compression::ByteRun1, &pal, |_x, _y| 0);
    let frame1 = make_frame_planes(16, 4, 3, Compression::ByteRun1, &pal, |x, _y| (x & 7) as u8);
    let bytes = encode_anim_op1(&[frame0.clone(), frame1.clone()]).unwrap();
    // Find the ANHD and check operation == 1 and mask == 7.
    let mut i = 0;
    let mut checked = false;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"ANHD" {
            let body = i + 8;
            let anhd = Anhd::parse(&bytes[body..body + 40]).unwrap();
            assert_eq!(anhd.operation, 1);
            assert_eq!(anhd.mask, 0b111);
            checked = true;
            break;
        }
        i += 1;
    }
    assert!(checked, "ANHD chunk present");
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames[1].rgba, frame1.rgba);
}
