//! Per-frame ANHD timing (§2.1 `abstime` / `reltime`) across the whole
//! delta-encoder family. `encode_anim_op0_timed` / `op5_timed` landed
//! earlier; this pins the same contract for op-1 (XOR), op-2/op-3
//! (Long/Short Delta), op-4 (generalized delta), op-7 (short/long
//! vertical) and op-8 (Anim8): authored `rel_time` / `abs_time` values
//! round-trip through `parse_anim` into `frame_timing`, pixels are
//! unaffected, and the untimed wrappers keep the default 1-jiffy pace.

use oxideav_iff::anim::{
    encode_anim_op1, encode_anim_op1_timed, encode_anim_op2, encode_anim_op2_timed,
    encode_anim_op3, encode_anim_op3_timed, encode_anim_op4, encode_anim_op4_timed,
    encode_anim_op7, encode_anim_op7_timed, encode_anim_op8, encode_anim_op8_timed, parse_anim,
    FrameTiming,
};
use oxideav_iff::ilbm::{Bmhd, Compression, IlbmImage, Masking};

fn frames() -> Vec<IlbmImage> {
    let palette = vec![
        [0x00, 0x00, 0x00],
        [0xFF, 0x00, 0x00],
        [0x00, 0xFF, 0x00],
        [0x00, 0x00, 0xFF],
    ];
    // 32 px wide → row_bytes = 4, satisfying the long-data (4-byte
    // item) width requirement of op-4 and op-7.
    let bmhd = Bmhd {
        width: 32,
        height: 4,
        x_origin: 0,
        y_origin: 0,
        n_planes: 2,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 32,
        page_height: 4,
    };
    (0u8..3)
        .map(|seed| {
            let mut rgba = Vec::with_capacity(32 * 4 * 4);
            for y in 0..4usize {
                for x in 0..32usize {
                    let p = palette[(x + y + seed as usize) % 4];
                    rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
                }
            }
            IlbmImage {
                width: 32,
                height: 4,
                bmhd,
                palette: palette.clone(),
                rgba,
                ..IlbmImage::default()
            }
        })
        .collect()
}

fn timing() -> Vec<FrameTiming> {
    vec![
        FrameTiming::default(), // seed @ t = 0
        FrameTiming {
            rel_time: 10,
            abs_time: 10,
        },
        FrameTiming {
            rel_time: 25,
            abs_time: 35,
        },
    ]
}

fn assert_contract(timed: Vec<u8>, untimed: Vec<u8>, label: &str) {
    let dec = parse_anim(&timed).unwrap();
    let plain = parse_anim(&untimed).unwrap();
    assert_eq!(dec.frames.len(), 3, "{label}: frame count");
    assert_eq!(dec.frame_timing[1].rel_time, 10, "{label}: rel_time[1]");
    assert_eq!(dec.frame_timing[1].abs_time, 10, "{label}: abs_time[1]");
    assert_eq!(dec.frame_timing[2].rel_time, 25, "{label}: rel_time[2]");
    assert_eq!(dec.frame_timing[2].abs_time, 35, "{label}: abs_time[2]");
    for i in 0..3 {
        assert_eq!(
            dec.frames[i].rgba, plain.frames[i].rgba,
            "{label}: timing must not disturb frame {i} pixels"
        );
    }
    // The untimed wrapper keeps the default 1-jiffy pace.
    assert_eq!(plain.frame_timing[1].rel_time, 1, "{label}: default pace");
    assert_eq!(plain.frame_timing[2].rel_time, 1, "{label}: default pace");
}

#[test]
fn op1_timed_roundtrip() {
    let f = frames();
    assert_contract(
        encode_anim_op1_timed(&f, &timing()).unwrap(),
        encode_anim_op1(&f).unwrap(),
        "op 1",
    );
}

#[test]
fn op2_timed_roundtrip() {
    let f = frames();
    assert_contract(
        encode_anim_op2_timed(&f, &timing()).unwrap(),
        encode_anim_op2(&f).unwrap(),
        "op 2",
    );
}

#[test]
fn op3_timed_roundtrip() {
    let f = frames();
    assert_contract(
        encode_anim_op3_timed(&f, &timing()).unwrap(),
        encode_anim_op3(&f).unwrap(),
        "op 3",
    );
}

#[test]
fn op4_timed_roundtrip_short_and_long() {
    let f = frames();
    for long in [false, true] {
        assert_contract(
            encode_anim_op4_timed(&f, long, &timing()).unwrap(),
            encode_anim_op4(&f, long).unwrap(),
            if long { "op 4 long" } else { "op 4 short" },
        );
    }
}

#[test]
fn op7_timed_roundtrip_short_and_long() {
    let f = frames();
    for long in [false, true] {
        assert_contract(
            encode_anim_op7_timed(&f, long, &timing()).unwrap(),
            encode_anim_op7(&f, long).unwrap(),
            if long { "op 7 long" } else { "op 7 short" },
        );
    }
}

#[test]
fn op8_timed_roundtrip_short_and_long() {
    let f = frames();
    for long in [false, true] {
        assert_contract(
            encode_anim_op8_timed(&f, long, &timing()).unwrap(),
            encode_anim_op8(&f, long).unwrap(),
            if long { "op 8 long" } else { "op 8 short" },
        );
    }
}

#[test]
fn timing_length_mismatch_is_rejected_across_the_family() {
    let f = frames();
    let short = &timing()[..2];
    assert!(encode_anim_op1_timed(&f, short).is_err());
    assert!(encode_anim_op2_timed(&f, short).is_err());
    assert!(encode_anim_op3_timed(&f, short).is_err());
    assert!(encode_anim_op4_timed(&f, false, short).is_err());
    assert!(encode_anim_op7_timed(&f, true, short).is_err());
    assert!(encode_anim_op8_timed(&f, true, short).is_err());
}
