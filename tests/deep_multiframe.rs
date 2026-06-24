//! Multi-image / cel-anim `FORM DEEP` decode + encode (§1.4 DBOD, §1.3 DLOC,
//! §1.6 DCHG). A FORM DEEP may carry several DBOD frames; `parse_deep_frames`
//! decodes every one, honouring per-frame DLOC dimensions and the optional
//! DCHG inter-frame timing, and `encode_deep_frames` is its inverse for the
//! round-trippable body codings.
//!
//! Source: docs/image/iff/iff-truecolor-chunks.md §1.

use oxideav_iff::ilbm::{
    encode_deep_frames, parse_deep, parse_deep_frames, Dchg, DeepCType, DeepCompression,
    DpelElement,
};
use oxideav_iff::ilbm::{Dloc, Dpel};

/// A plain 24-bit RGB DPEL (RED/GREEN/BLUE each 8 bits).
fn rgb888_dpel() -> Dpel {
    Dpel {
        elements: vec![
            DpelElement {
                c_type: DeepCType::Red,
                c_bit_depth: 8,
            },
            DpelElement {
                c_type: DeepCType::Green,
                c_bit_depth: 8,
            },
            DpelElement {
                c_type: DeepCType::Blue,
                c_bit_depth: 8,
            },
        ],
    }
}

/// A solid `width × height` RGBA frame of one colour.
fn solid(width: u16, height: u16, rgba: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for _ in 0..(width as usize * height as usize) {
        out.extend_from_slice(&rgba);
    }
    out
}

#[test]
fn single_dbod_matches_parse_deep() {
    // A one-frame movie must agree with the existing single-image parse_deep.
    let dpel = rgb888_dpel();
    let frame = solid(2, 2, [10, 20, 30, 255]);
    let bytes = encode_deep_frames(&dpel, 2, 2, DeepCompression::None, None, &[&frame]).unwrap();

    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.frames.len(), 1);
    assert!(!movie.is_animation());
    assert_eq!(movie.frames[0].rgba, frame);

    let single = parse_deep(&bytes).unwrap();
    assert_eq!(single.rgba, movie.frames[0].rgba);
    assert_eq!(single.width, movie.frames[0].width);
    assert_eq!(single.height, movie.frames[0].height);
}

#[test]
fn three_frames_decode_in_document_order() {
    let dpel = rgb888_dpel();
    let f0 = solid(2, 1, [255, 0, 0, 255]);
    let f1 = solid(2, 1, [0, 255, 0, 255]);
    let f2 = solid(2, 1, [0, 0, 255, 255]);
    let bytes = encode_deep_frames(
        &dpel,
        2,
        1,
        DeepCompression::None,
        Some(Dchg { frame_rate: 100 }),
        &[&f0, &f1, &f2],
    )
    .unwrap();

    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.frames.len(), 3);
    assert!(movie.is_animation());
    assert_eq!(movie.frames[0].rgba, f0);
    assert_eq!(movie.frames[1].rgba, f1);
    assert_eq!(movie.frames[2].rgba, f2);
    // DCHG carried a 100 ms delay.
    assert_eq!(movie.frame_delay_millis(), Some(100));

    // parse_deep still returns just the first frame.
    let first = parse_deep(&bytes).unwrap();
    assert_eq!(first.rgba, f0);
}

#[test]
fn runlength_multiframe_round_trips() {
    // The §1.5b RUNLENGTH (ByteRun1) coding also round-trips frame-by-frame.
    let dpel = rgb888_dpel();
    let f0 = solid(4, 2, [7, 7, 7, 255]);
    let f1 = solid(4, 2, [200, 1, 99, 255]);
    let bytes = encode_deep_frames(
        &dpel,
        4,
        2,
        DeepCompression::RunLength,
        Some(Dchg { frame_rate: 40 }),
        &[&f0, &f1],
    )
    .unwrap();

    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.dgbl.compression, DeepCompression::RunLength);
    assert_eq!(movie.frames.len(), 2);
    assert_eq!(movie.frames[0].rgba, f0);
    assert_eq!(movie.frames[1].rgba, f1);
}

#[test]
fn dchg_not_animation_sentinel_disables_is_animation() {
    // FrameRate == -1 marks frame boundaries but is "not an animation" (§1.6).
    let dpel = rgb888_dpel();
    let f0 = solid(1, 1, [1, 2, 3, 255]);
    let f1 = solid(1, 1, [4, 5, 6, 255]);
    let bytes = encode_deep_frames(
        &dpel,
        1,
        1,
        DeepCompression::None,
        Some(Dchg {
            frame_rate: Dchg::NOT_AN_ANIMATION,
        }),
        &[&f0, &f1],
    )
    .unwrap();

    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.frames.len(), 2);
    assert!(movie.dchg.unwrap().is_not_animation());
    assert!(!movie.is_animation());
    assert_eq!(movie.frame_delay_millis(), None);
}

#[test]
fn dchg_as_fast_as_possible_sentinel_has_no_literal_delay() {
    let dpel = rgb888_dpel();
    let f0 = solid(1, 1, [1, 2, 3, 255]);
    let f1 = solid(1, 1, [4, 5, 6, 255]);
    let bytes = encode_deep_frames(
        &dpel,
        1,
        1,
        DeepCompression::None,
        Some(Dchg {
            frame_rate: Dchg::AS_FAST_AS_POSSIBLE,
        }),
        &[&f0, &f1],
    )
    .unwrap();

    let movie = parse_deep_frames(&bytes).unwrap();
    // Multiple frames + a non-"-1" DCHG → still an animation, just unpaced.
    assert!(movie.is_animation());
    assert_eq!(movie.frame_delay_millis(), None);
}

#[test]
fn dchg_parse_write_round_trips() {
    for fr in [0i32, -1, 1, 40, 1000, i32::MAX, i32::MIN, -2] {
        let d = Dchg { frame_rate: fr };
        let wire = d.write();
        assert_eq!(Dchg::parse(&wire).unwrap(), d);
    }
    // Short body rejected.
    assert!(Dchg::parse(&[0, 0, 0]).is_err());
}

#[test]
fn dchg_delay_millis_semantics() {
    assert_eq!(Dchg { frame_rate: 0 }.delay_millis(), None);
    assert_eq!(Dchg { frame_rate: -1 }.delay_millis(), None);
    assert_eq!(Dchg { frame_rate: -42 }.delay_millis(), None);
    assert_eq!(Dchg { frame_rate: 33 }.delay_millis(), Some(33));
}

#[test]
fn per_frame_dloc_drives_dimensions() {
    // Hand-build a FORM DEEP with two DBODs, each preceded by its own DLOC
    // giving a different geometry. parse_deep_frames must honour each DLOC.
    let dpel = rgb888_dpel();

    fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
    }

    let dgbl_body = {
        // display 1x1, compression 0, aspect 1:1
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.push(1);
        b.push(1);
        b
    };

    // Frame 0: 2x1, frame 1: 1x2 — both 3 bytes/pixel chunky.
    let f0_dloc = Dloc {
        w: 2,
        h: 1,
        x: 0,
        y: 0,
    };
    let f1_dloc = Dloc {
        w: 1,
        h: 2,
        x: 0,
        y: 0,
    };
    let f0_body = vec![1, 2, 3, 4, 5, 6]; // 2 pixels
    let f1_body = vec![7, 8, 9, 10, 11, 12]; // 2 pixels

    let mut chunks = Vec::new();
    push_chunk(&mut chunks, b"DGBL", &dgbl_body);
    push_chunk(&mut chunks, b"DPEL", &dpel.write());
    push_chunk(&mut chunks, b"DLOC", &f0_dloc.write());
    push_chunk(&mut chunks, b"DBOD", &f0_body);
    push_chunk(&mut chunks, b"DLOC", &f1_dloc.write());
    push_chunk(&mut chunks, b"DBOD", &f1_body);

    let mut form = Vec::new();
    form.extend_from_slice(b"FORM");
    form.extend_from_slice(&((4 + chunks.len()) as u32).to_be_bytes());
    form.extend_from_slice(b"DEEP");
    form.extend_from_slice(&chunks);

    let movie = parse_deep_frames(&form).unwrap();
    assert_eq!(movie.frames.len(), 2);
    assert_eq!((movie.frames[0].width, movie.frames[0].height), (2, 1));
    assert_eq!((movie.frames[1].width, movie.frames[1].height), (1, 2));
    // Frame 0 pixel 0 = (1,2,3), pixel 1 = (4,5,6).
    assert_eq!(&movie.frames[0].rgba[0..4], &[1, 2, 3, 255]);
    assert_eq!(&movie.frames[0].rgba[4..8], &[4, 5, 6, 255]);
    assert_eq!(&movie.frames[1].rgba[0..4], &[7, 8, 9, 255]);
}

#[test]
fn empty_frame_list_rejected() {
    let dpel = rgb888_dpel();
    let frames: &[&[u8]] = &[];
    assert!(encode_deep_frames(&dpel, 1, 1, DeepCompression::None, None, frames).is_err());
}

#[test]
fn tvdc_multiframe_encode_rejected() {
    // TVDC has no round-trip emit path in the multi-frame encoder.
    let dpel = rgb888_dpel();
    let f0 = solid(1, 1, [1, 2, 3, 255]);
    assert!(encode_deep_frames(&dpel, 1, 1, DeepCompression::Tvdc, None, &[&f0]).is_err());
}

/// Hand-build a FORM DEEP with a given display size and a list of
/// `(Option<Dloc>, chunky_body)` frames for the composite tests.
fn deep_with_frames(
    dgbl_w: u16,
    dgbl_h: u16,
    dpel: &Dpel,
    frames: &[(Option<Dloc>, Vec<u8>)],
) -> Vec<u8> {
    fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        if body.len() & 1 == 1 {
            out.push(0);
        }
    }
    let mut dgbl_body = Vec::new();
    dgbl_body.extend_from_slice(&dgbl_w.to_be_bytes());
    dgbl_body.extend_from_slice(&dgbl_h.to_be_bytes());
    dgbl_body.extend_from_slice(&0u16.to_be_bytes()); // NOCOMPRESSION
    dgbl_body.push(1);
    dgbl_body.push(1);

    let mut chunks = Vec::new();
    push_chunk(&mut chunks, b"DGBL", &dgbl_body);
    push_chunk(&mut chunks, b"DPEL", &dpel.write());
    for (dloc, body) in frames {
        if let Some(dl) = dloc {
            push_chunk(&mut chunks, b"DLOC", &dl.write());
        }
        push_chunk(&mut chunks, b"DBOD", body);
    }

    let mut form = Vec::new();
    form.extend_from_slice(b"FORM");
    form.extend_from_slice(&((4 + chunks.len()) as u32).to_be_bytes());
    form.extend_from_slice(b"DEEP");
    form.extend_from_slice(&chunks);
    form
}

#[test]
fn composite_places_sub_rectangle_at_dloc_offset() {
    // 4x4 display; a single 2x2 red sprite placed at (1,1). Everything else
    // stays transparent black.
    let dpel = rgb888_dpel();
    // 2x2 chunky RGB888 = 4 pixels * 3 bytes.
    let sprite: Vec<u8> = vec![
        255, 0, 0, 255, 0, 0, // row 0: two red pixels
        255, 0, 0, 255, 0, 0, // row 1
    ];
    let bytes = deep_with_frames(
        4,
        4,
        &dpel,
        &[(
            Some(Dloc {
                w: 2,
                h: 2,
                x: 1,
                y: 1,
            }),
            sprite,
        )],
    );
    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.display_size(), (4, 4));

    let canvas = movie.composite_frame(0).unwrap();
    assert_eq!(canvas.len(), 4 * 4 * 4);

    // Helper: pixel at (x,y) on the 4-wide canvas.
    let px = |x: usize, y: usize| -> [u8; 4] {
        let o = (y * 4 + x) * 4;
        [canvas[o], canvas[o + 1], canvas[o + 2], canvas[o + 3]]
    };
    // (0,0) untouched → transparent black.
    assert_eq!(px(0, 0), [0, 0, 0, 0]);
    // The red 2x2 block lands at (1,1)..(2,2).
    assert_eq!(px(1, 1), [255, 0, 0, 255]);
    assert_eq!(px(2, 1), [255, 0, 0, 255]);
    assert_eq!(px(1, 2), [255, 0, 0, 255]);
    assert_eq!(px(2, 2), [255, 0, 0, 255]);
    // (3,3) outside the sprite → still transparent.
    assert_eq!(px(3, 3), [0, 0, 0, 0]);
}

#[test]
fn composite_clips_frame_that_overruns_canvas() {
    // A 3x3 sprite placed at (2,2) on a 4x4 canvas: only the top-left 2x2
    // corner of the sprite is on-canvas; the rest is clipped, no panic.
    let dpel = rgb888_dpel();
    let mut sprite = Vec::new();
    for _ in 0..9 {
        sprite.extend_from_slice(&[9, 8, 7]); // 3x3 chunky RGB888
    }
    let bytes = deep_with_frames(
        4,
        4,
        &dpel,
        &[(
            Some(Dloc {
                w: 3,
                h: 3,
                x: 2,
                y: 2,
            }),
            sprite,
        )],
    );
    let movie = parse_deep_frames(&bytes).unwrap();
    let canvas = movie.composite_frame(0).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let o = (y * 4 + x) * 4;
        [canvas[o], canvas[o + 1], canvas[o + 2], canvas[o + 3]]
    };
    assert_eq!(px(2, 2), [9, 8, 7, 255]);
    assert_eq!(px(3, 3), [9, 8, 7, 255]);
    // (1,1) is left of the sprite → transparent.
    assert_eq!(px(1, 1), [0, 0, 0, 0]);
}

#[test]
fn composite_negative_offset_clips_top_left() {
    // A 2x2 sprite at (-1,-1): only its bottom-right pixel is visible at (0,0).
    let dpel = rgb888_dpel();
    let sprite: Vec<u8> = vec![
        1, 1, 1, 2, 2, 2, // row 0
        3, 3, 3, 4, 4, 4, // row 1
    ];
    let bytes = deep_with_frames(
        2,
        2,
        &dpel,
        &[(
            Some(Dloc {
                w: 2,
                h: 2,
                x: -1,
                y: -1,
            }),
            sprite,
        )],
    );
    let movie = parse_deep_frames(&bytes).unwrap();
    let canvas = movie.composite_frame(0).unwrap();
    // Sprite pixel (1,1) = (4,4,4) lands on canvas (0,0).
    assert_eq!(&canvas[0..4], &[4, 4, 4, 255]);
    // (1,0) and (0,1) get sprite (1,1)'s row/col neighbours? No: sprite (1,0)
    // would map to canvas (0,-1) off-canvas. Canvas (1,0) untouched.
    assert_eq!(&canvas[4..8], &[0, 0, 0, 0]);
}

#[test]
fn composite_out_of_range_index_is_none() {
    let dpel = rgb888_dpel();
    let f0 = solid(2, 2, [1, 2, 3, 255]);
    let bytes = encode_deep_frames(&dpel, 2, 2, DeepCompression::None, None, &[&f0]).unwrap();
    let movie = parse_deep_frames(&bytes).unwrap();
    assert!(movie.composite_frame(0).is_some());
    assert!(movie.composite_frame(1).is_none());
}

#[test]
fn composite_no_dloc_frame_covers_display_at_origin() {
    // A full-display frame with no DLOC composites 1:1 onto the canvas.
    let dpel = rgb888_dpel();
    let f0 = solid(2, 2, [7, 7, 7, 255]);
    let bytes = encode_deep_frames(&dpel, 2, 2, DeepCompression::None, None, &[&f0]).unwrap();
    let movie = parse_deep_frames(&bytes).unwrap();
    let canvas = movie.composite_frame(0).unwrap();
    assert_eq!(canvas, f0);
}
