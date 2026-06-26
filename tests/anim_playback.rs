//! Round-373 coverage: ANIM per-frame timing + the [`AnimPlayback`]
//! cumulative-timeline driver.
//!
//! The 1988 ANIM spec (§2.1) stores timing as a per-frame `reltime` —
//! the jiffy (1/60 s) delay *after the previous frame* before this frame
//! is flipped up — plus a (historically unused) `abstime`. `parse_anim`
//! now lifts both into [`AnimImage::frame_timing`], and
//! [`AnimImage::playback`] inverts the per-frame deltas into an absolute
//! timeline (cumulative start time + display duration per frame, plus a
//! `frame_at_jiffies` / `frame_at_micros` scrubber).
//!
//! These tests author ANIMs with non-trivial `rel_time` values via
//! [`encode_anim_op0_timed`], decode them back through [`parse_anim`],
//! and assert the per-frame timing and derived timeline match.

use oxideav_iff::anim::{encode_anim_op0, encode_anim_op0_timed, parse_anim, FrameTiming};
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

fn three_frames() -> Vec<IlbmImage> {
    let pal = vec![[0u8, 0, 0], [255u8, 255, 255]];
    vec![
        make_frame(16, 8, &pal, |x, _| (x & 1) as u8),
        make_frame(16, 8, &pal, |x, y| ((x ^ y) & 1) as u8),
        make_frame(16, 8, &pal, |_, y| (y & 1) as u8),
    ]
}

#[test]
fn default_op0_timing_is_one_jiffy_per_delta() {
    // The plain op-0 encoder writes rel_time = 1 for every delta frame.
    let frames = three_frames();
    let bytes = encode_anim_op0(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3);
    assert_eq!(dec.frame_timing.len(), 3);
    // Seed frame is t = 0.
    assert_eq!(dec.frame_timing[0], FrameTiming::default());
    // Delta frames carry rel_time = 1.
    assert_eq!(dec.frame_timing[1].rel_time, 1);
    assert_eq!(dec.frame_timing[2].rel_time, 1);
}

#[test]
fn custom_timing_round_trips_through_parse() {
    let frames = three_frames();
    let timing = vec![
        FrameTiming::default(), // seed @ t = 0
        FrameTiming {
            rel_time: 10,
            abs_time: 0,
        },
        FrameTiming {
            rel_time: 25,
            abs_time: 0,
        },
    ];
    let bytes = encode_anim_op0_timed(&frames, &timing).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frame_timing[0].rel_time, 0);
    assert_eq!(dec.frame_timing[1].rel_time, 10);
    assert_eq!(dec.frame_timing[2].rel_time, 25);
    // Pixels still round-trip correctly alongside the timing.
    for (a, b) in dec.frames.iter().zip(frames.iter()) {
        assert_eq!(a.rgba, b.rgba);
    }
}

#[test]
fn playback_timeline_accumulates_start_times() {
    let frames = three_frames();
    let timing = vec![
        FrameTiming::default(),
        FrameTiming {
            rel_time: 10,
            abs_time: 0,
        },
        FrameTiming {
            rel_time: 25,
            abs_time: 0,
        },
    ];
    let bytes = encode_anim_op0_timed(&frames, &timing).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    let pb = dec.playback();
    assert_eq!(pb.len(), 3);
    assert!(!pb.is_empty());

    // Frame 0 starts at 0, shown for rel_time(1) = 10 jiffies.
    assert_eq!(pb.frames[0].frame_index, 0);
    assert_eq!(pb.frames[0].start_jiffies, 0);
    assert_eq!(pb.frames[0].duration_jiffies, 10);

    // Frame 1 starts at 10 (after frame 0's delay), shown for rel_time(2)=25.
    assert_eq!(pb.frames[1].start_jiffies, 10);
    assert_eq!(pb.frames[1].duration_jiffies, 25);

    // Frame 2 starts at 10+25=35. Last frame holds its own rel_time (25).
    assert_eq!(pb.frames[2].start_jiffies, 35);
    assert_eq!(pb.frames[2].duration_jiffies, 25);

    // Total = 10 + 25 + 25 = 60 jiffies = exactly 1 second.
    assert_eq!(pb.total_jiffies(), 60);
    assert_eq!(pb.total_micros(), 1_000_000);
}

#[test]
fn frame_at_jiffies_scrubs_the_timeline() {
    let frames = three_frames();
    let timing = vec![
        FrameTiming::default(),
        FrameTiming {
            rel_time: 10,
            abs_time: 0,
        },
        FrameTiming {
            rel_time: 25,
            abs_time: 0,
        },
    ];
    let bytes = encode_anim_op0_timed(&frames, &timing).unwrap();
    let pb = parse_anim(&bytes).unwrap().playback();

    // t=0..9 → frame 0; t=10..34 → frame 1; t>=35 → frame 2.
    assert_eq!(pb.frame_at_jiffies(0), Some(0));
    assert_eq!(pb.frame_at_jiffies(9), Some(0));
    assert_eq!(pb.frame_at_jiffies(10), Some(1));
    assert_eq!(pb.frame_at_jiffies(34), Some(1));
    assert_eq!(pb.frame_at_jiffies(35), Some(2));
    // Past the end clamps to the last frame.
    assert_eq!(pb.frame_at_jiffies(10_000), Some(2));
}

#[test]
fn frame_at_micros_matches_jiffy_lookup() {
    let frames = three_frames();
    let timing = vec![
        FrameTiming::default(),
        FrameTiming {
            rel_time: 30,
            abs_time: 0,
        }, // half a second
        FrameTiming {
            rel_time: 30,
            abs_time: 0,
        },
    ];
    let bytes = encode_anim_op0_timed(&frames, &timing).unwrap();
    let pb = parse_anim(&bytes).unwrap().playback();

    // Frame 1 begins at 30 jiffies = 500 ms.
    assert_eq!(pb.frame_at_micros(0), Some(0));
    assert_eq!(pb.frame_at_micros(499_000), Some(0));
    assert_eq!(pb.frame_at_micros(500_000), Some(1));
    assert_eq!(pb.frame_at_micros(999_000), Some(1));
    assert_eq!(pb.frame_at_micros(1_000_000), Some(2));
}

#[test]
fn playback_frame_micros_helpers() {
    let frames = three_frames();
    let timing = vec![
        FrameTiming::default(),
        FrameTiming {
            rel_time: 60,
            abs_time: 0,
        }, // 1 s
        FrameTiming {
            rel_time: 60,
            abs_time: 0,
        },
    ];
    let bytes = encode_anim_op0_timed(&frames, &timing).unwrap();
    let pb = parse_anim(&bytes).unwrap().playback();
    // Frame 1 starts at 60 jiffies = 1 s.
    assert_eq!(pb.frames[1].start_micros(), 1_000_000);
    assert_eq!(pb.frames[1].duration_micros(), 1_000_000);
}

#[test]
fn timed_encode_rejects_mismatched_timing_length() {
    let frames = three_frames();
    let timing = vec![FrameTiming::default()]; // too few
    assert!(encode_anim_op0_timed(&frames, &timing).is_err());
}

#[test]
fn single_frame_playback_has_unit_duration() {
    // A one-frame ANIM: the lone frame must still report a non-zero
    // duration so a looping player advances.
    let frames = vec![three_frames().remove(0)];
    let bytes = encode_anim_op0(&frames).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    let pb = dec.playback();
    assert_eq!(pb.len(), 1);
    assert_eq!(pb.frames[0].start_jiffies, 0);
    assert_eq!(pb.frames[0].duration_jiffies, 1);
    assert_eq!(pb.frame_at_jiffies(0), Some(0));
    assert_eq!(pb.frame_at_jiffies(100), Some(0));
}

#[test]
fn looping_lookup_wraps_modulo_total() {
    let frames = three_frames();
    let timing = vec![
        FrameTiming::default(),
        FrameTiming {
            rel_time: 10,
            abs_time: 0,
        },
        FrameTiming {
            rel_time: 25,
            abs_time: 0,
        },
    ];
    let bytes = encode_anim_op0_timed(&frames, &timing).unwrap();
    let pb = parse_anim(&bytes).unwrap().playback();
    // Total = 60 jiffies. A single playthrough clamps past-the-end to the
    // last frame; the looping lookup instead wraps back to the start.
    assert_eq!(pb.total_jiffies(), 60);
    assert_eq!(pb.frame_at_jiffies(60), Some(2)); // clamped (single)
    assert_eq!(pb.frame_at_jiffies_looping(60), Some(0)); // wrapped (loop)
    assert_eq!(pb.frame_at_jiffies_looping(70), Some(1)); // 70 % 60 = 10 -> f1
    assert_eq!(pb.frame_at_jiffies_looping(95), Some(2)); // 95 % 60 = 35 -> f2
    assert_eq!(pb.frame_at_jiffies_looping(120), Some(0)); // 120 % 60 = 0 -> f0
                                                           // Micros loop mirror: 60 jiffies = 1 s; t = 1.0 s wraps to frame 0.
    assert_eq!(pb.frame_at_micros_looping(1_000_000), Some(0));
    // 1.2 s = 72 jiffies; 72 % 60 = 12 jiffies into the loop -> frame 1.
    assert_eq!(pb.frame_at_micros_looping(1_200_000), Some(1));
}
