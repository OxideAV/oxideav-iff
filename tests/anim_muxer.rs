//! Container-level `iff_anim` muxer: rawvideo/Rgba packets → op-5
//! (Byte Vertical Delta) `FORM ANIM`, read back through the registered
//! demuxer for pixel-exact and timing round-trips. Also covers the
//! `encode_anim_op5_timed` authoring API directly.
//!
//! Spec reference: `docs/image/iff/anim.txt` §2.1 (`ANHD` `reltime` /
//! `abstime`, jiffies of 1/60 s) and the op-5 delta coding.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, ContainerRegistry, Error, MediaType, Packet, PixelFormat, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};

use oxideav_iff::anim::{encode_anim_op5_timed, parse_anim, FrameTiming};
use oxideav_iff::ilbm::{Bmhd, Compression, IlbmImage, Masking};

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

fn tmp_path(tag: &str) -> std::path::PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-iff-animmux-{tag}-{}-{n}.anim",
        std::process::id()
    ))
}

fn video_stream(width: u32, height: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(PixelFormat::Rgba);
    StreamInfo {
        index: 0,
        // The same jiffy time base the iff_anim demuxer advertises, so
        // demux → mux → demux passes durations through unchanged.
        time_base: TimeBase::new(1, 60),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// A width×height frame of `colors[i % colors.len()]` stripes, one colour
/// per column, shifted by `phase` — cheap distinct-but-overlapping frames.
fn stripes(width: usize, height: usize, colors: &[[u8; 3]], phase: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    for _y in 0..height {
        for x in 0..width {
            let c = colors[(x + phase) % colors.len()];
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    rgba
}

fn mux_frames(stream: &StreamInfo, frames: &[(Vec<u8>, i64, Option<i64>)]) -> Vec<u8> {
    let reg = registry();
    let path = tmp_path("mux");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = reg
            .open_muxer("iff_anim", ws, std::slice::from_ref(stream))
            .unwrap();
        assert_eq!(mux.format_name(), "iff_anim");
        mux.write_header().unwrap();
        for (data, pts, duration) in frames {
            let mut pkt = Packet::new(0, stream.time_base, data.clone());
            pkt.pts = Some(*pts);
            pkt.dts = Some(*pts);
            pkt.duration = *duration;
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

#[test]
fn anim_mux_roundtrips_pixels_through_the_demuxer() {
    let colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    let (w, h) = (16usize, 4usize);
    let stream = video_stream(w as u32, h as u32);
    let frames: Vec<(Vec<u8>, i64, Option<i64>)> = (0..3)
        .map(|i| (stripes(w, h, &colors, i), i as i64 * 5, Some(5)))
        .collect();
    let bytes = mux_frames(&stream, &frames);
    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"ANIM");

    let reg = registry();
    let mut cur = Cursor::new(bytes.clone());
    assert_eq!(reg.probe_input(&mut cur, None).unwrap(), "iff_anim");
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_anim", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    for (expect, _pts, _dur) in &frames {
        let pkt = dmx.next_packet().unwrap();
        assert_eq!(&pkt.data, expect, "frame must round-trip pixel-exactly");
    }
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

#[test]
fn anim_mux_preserves_durations_in_jiffies() {
    // Uniform 5-jiffy durations at the demuxer's own 1/60 time base: the
    // wire rel_time chain reproduces the timeline exactly.
    let colors = [[10, 20, 30], [40, 50, 60]];
    let (w, h) = (8usize, 2usize);
    let stream = video_stream(w as u32, h as u32);
    let frames: Vec<(Vec<u8>, i64, Option<i64>)> = (0..3)
        .map(|i| (stripes(w, h, &colors, i), i as i64 * 5, Some(5)))
        .collect();
    let bytes = mux_frames(&stream, &frames);

    let reg = registry();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_anim", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    let p0 = dmx.next_packet().unwrap();
    assert_eq!((p0.pts, p0.duration), (Some(0), Some(5)));
    let p1 = dmx.next_packet().unwrap();
    assert_eq!((p1.pts, p1.duration), (Some(5), Some(5)));
    let p2 = dmx.next_packet().unwrap();
    // Last frame: the wire has no trailing delay; players fall back to the
    // last delta's rel_time (= 5 here, same as the authored duration).
    assert_eq!((p2.pts, p2.duration), (Some(10), Some(5)));
    assert_eq!(dmx.duration_micros(), Some(15 * 1_000_000 / 60));
}

#[test]
fn anim_mux_single_frame_and_bad_input() {
    let stream = video_stream(2, 2);
    let rgba = stripes(2, 2, &[[1, 2, 3]], 0);
    // A single frame is a valid one-frame ANIM (seed only).
    let bytes = mux_frames(&stream, &[(rgba.clone(), 0, None)]);
    let anim = parse_anim(&bytes).unwrap();
    assert_eq!(anim.frames.len(), 1);
    assert_eq!(anim.frames[0].rgba, rgba);

    let reg = registry();
    // Mis-sized packet.
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mux = reg
        .open_muxer("iff_anim", out, std::slice::from_ref(&stream))
        .unwrap();
    mux.write_header().unwrap();
    let mut bad = Packet::new(0, stream.time_base, vec![0u8; 3]);
    bad.flags.keyframe = true;
    assert!(mux.write_packet(&bad).is_err());
    // Trailer with no frames.
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mux = reg
        .open_muxer("iff_anim", out, std::slice::from_ref(&stream))
        .unwrap();
    mux.write_header().unwrap();
    assert!(mux.write_trailer().is_err());
    // Wrong pixel format.
    let mut bad_stream = video_stream(2, 2);
    bad_stream.params.pixel_format = None;
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    assert!(reg
        .open_muxer("iff_anim", out, std::slice::from_ref(&bad_stream))
        .is_err());
}

#[test]
fn op5_timed_authoring_roundtrips_frame_timing() {
    // Direct API: author two delta frames with distinct rel_times and read
    // the same values back through parse_anim's frame_timing.
    let bmhd = Bmhd {
        width: 8,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 1,
        masking: Masking::None,
        compression: Compression::ByteRun1,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 8,
        page_height: 2,
    };
    let palette = vec![[0, 0, 0], [255, 255, 255]];
    let mk = |bit: u8| {
        let mut rgba = Vec::new();
        for i in 0..16 {
            let on = (i as u8 & 1) ^ bit;
            let v = if on == 1 { 255 } else { 0 };
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
        IlbmImage {
            width: 8,
            height: 2,
            bmhd,
            palette: palette.clone(),
            rgba,
            ..IlbmImage::default()
        }
    };
    let frames = vec![mk(0), mk(1), mk(0)];
    let timing = vec![
        FrameTiming {
            rel_time: 0,
            abs_time: 0,
        },
        FrameTiming {
            rel_time: 7,
            abs_time: 7,
        },
        FrameTiming {
            rel_time: 13,
            abs_time: 20,
        },
    ];
    let bytes = encode_anim_op5_timed(&frames, &timing).unwrap();
    let anim = parse_anim(&bytes).unwrap();
    assert_eq!(anim.frames.len(), 3);
    assert_eq!(anim.frame_timing[1].rel_time, 7);
    assert_eq!(anim.frame_timing[2].rel_time, 13);
    assert_eq!(anim.frame_timing[2].abs_time, 20);
    // Pixels survive the timed path too.
    for (got, want) in anim.frames.iter().zip(frames.iter()) {
        assert_eq!(got.rgba, want.rgba);
    }

    // Mismatched timing length is rejected.
    assert!(encode_anim_op5_timed(&frames, &timing[..2]).is_err());
}
