//! Container-level muxer parity for the true-colour FORMs: `iff_deep`
//! (multi-frame `FORM DEEP` with NOCOMPRESSION / RUNLENGTH auto pick and
//! DCHG timing) and `iff_rgb8` / `iff_rgbn` (single-frame Turbo-Silver
//! genlock-RLE FORMs). Each muxer's output is read back through the
//! matching registered demuxer so the whole registry path round-trips.
//!
//! Spec reference: `docs/image/iff/iff-truecolor-chunks.md` §1 (DEEP) and
//! §3 (RGB8/RGBN).

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, ContainerRegistry, MediaType, Muxer, Packet, PixelFormat, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};

use oxideav_iff::ilbm::{parse_deep_frames, DeepCompression, DeepMuxer, DeepMuxerCompression};

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

/// Returns a fresh path under `std::env::temp_dir()`. Tests in the same
/// process may run in parallel, so an atomic counter plus the test tag keeps
/// writes disjoint.
fn tmp_path(tag: &str) -> std::path::PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-iff-truecolor-{tag}-{}-{n}.bin",
        std::process::id()
    ))
}

fn video_stream(width: u32, height: u32, time_base: TimeBase) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(PixelFormat::Rgba);
    StreamInfo {
        index: 0,
        time_base,
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn rgba_packet(stream: &StreamInfo, data: Vec<u8>, pts: i64, duration: Option<i64>) -> Packet {
    let mut pkt = Packet::new(0, stream.time_base, data);
    pkt.pts = Some(pts);
    pkt.dts = Some(pts);
    pkt.duration = duration;
    pkt.flags.keyframe = true;
    pkt
}

/// Mux `frames` through the named registered muxer (via a temp file, since
/// the muxer owns its output sink) and return the emitted bytes.
fn mux_frames(
    format: &str,
    stream: &StreamInfo,
    frames: &[(Vec<u8>, i64, Option<i64>)],
) -> Vec<u8> {
    let reg = registry();
    let path = tmp_path(format);
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = reg
            .open_muxer(format, ws, std::slice::from_ref(stream))
            .unwrap();
        assert_eq!(mux.format_name(), format);
        mux.write_header().unwrap();
        for (data, pts, duration) in frames {
            mux.write_packet(&rgba_packet(stream, data.clone(), *pts, *duration))
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

// ────────────────────────────── iff_deep ──────────────────────────────

#[test]
fn deep_mux_single_opaque_frame_roundtrips_as_rgb888() {
    let stream = video_stream(2, 2, TimeBase::new(1, 1));
    let rgba = vec![
        10, 20, 30, 255, 40, 50, 60, 255, //
        70, 80, 90, 255, 11, 22, 33, 255,
    ];
    let bytes = mux_frames("iff_deep", &stream, &[(rgba.clone(), 0, None)]);
    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"DEEP");

    // Fully-opaque input → the minimal §1.2 RGB 8:8:8 layout, no alpha
    // component on the wire.
    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.dpel.elements.len(), 3);
    assert_eq!(movie.frames.len(), 1);
    assert_eq!(movie.frames[0].rgba, rgba);
    assert!(movie.dchg.is_none());

    // And back through the registered demuxer.
    let reg = registry();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_deep", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, rgba);
}

#[test]
fn deep_mux_alpha_frame_keeps_the_alpha_component() {
    let stream = video_stream(2, 1, TimeBase::new(1, 1));
    let rgba = vec![10, 20, 30, 0x80, 40, 50, 60, 0xC0];
    let bytes = mux_frames("iff_deep", &stream, &[(rgba.clone(), 0, None)]);
    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.dpel.elements.len(), 4, "expected an ALPHA component");
    assert_eq!(movie.frames[0].rgba, rgba);
}

#[test]
fn deep_mux_multiframe_derives_dchg_from_packet_durations() {
    // A 1/1000-second time base with 50-tick durations = 50 ms per frame —
    // exactly what the demuxer advertises for a DCHG 50 FORM, so a
    // demux → mux → demux chain preserves the timing.
    let stream = video_stream(2, 1, TimeBase::new(1, 1000));
    let f0 = vec![1, 2, 3, 255, 4, 5, 6, 255];
    let f1 = vec![7, 8, 9, 255, 10, 11, 12, 255];
    let bytes = mux_frames(
        "iff_deep",
        &stream,
        &[(f0.clone(), 0, Some(50)), (f1.clone(), 50, Some(50))],
    );

    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.frames.len(), 2);
    assert_eq!(movie.frame_delay_millis(), Some(50));
    assert_eq!(movie.frames[0].rgba, f0);
    assert_eq!(movie.frames[1].rgba, f1);

    let reg = registry();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_deep", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    let p0 = dmx.next_packet().unwrap();
    assert_eq!((p0.pts, p0.duration), (Some(0), Some(50)));
    let p1 = dmx.next_packet().unwrap();
    assert_eq!((p1.pts, p1.duration), (Some(50), Some(50)));
}

#[test]
fn deep_mux_auto_picks_runlength_for_flat_frames() {
    // A solid-colour frame compresses massively under ByteRun1; Auto must
    // pick the RUNLENGTH FORM.
    let stream = video_stream(64, 4, TimeBase::new(1, 1));
    let rgba: Vec<u8> = std::iter::repeat([9u8, 9, 9, 255])
        .take(64 * 4)
        .flatten()
        .collect();
    let bytes = mux_frames("iff_deep", &stream, &[(rgba.clone(), 0, None)]);
    let movie = parse_deep_frames(&bytes).unwrap();
    assert_eq!(movie.dgbl.compression, DeepCompression::RunLength);
    assert_eq!(movie.frames[0].rgba, rgba);
}

#[test]
fn deep_mux_forced_compression_modes_roundtrip() {
    let stream = video_stream(3, 1, TimeBase::new(1, 1));
    let rgba = vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255];
    for (choice, expect) in [
        (DeepMuxerCompression::None, DeepCompression::None),
        (DeepMuxerCompression::RunLength, DeepCompression::RunLength),
    ] {
        let path = tmp_path("forced");
        {
            let f = std::fs::File::create(&path).unwrap();
            let ws: Box<dyn WriteSeek> = Box::new(f);
            let mut mux = DeepMuxer::new(ws, std::slice::from_ref(&stream))
                .unwrap()
                .with_compression(choice);
            mux.write_header().unwrap();
            mux.write_packet(&rgba_packet(&stream, rgba.clone(), 0, None))
                .unwrap();
            mux.write_trailer().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let movie = parse_deep_frames(&bytes).unwrap();
        assert_eq!(movie.dgbl.compression, expect);
        assert_eq!(movie.frames[0].rgba, rgba);
    }
}

#[test]
fn deep_mux_rejects_bad_input() {
    let stream = video_stream(2, 2, TimeBase::new(1, 1));
    let reg = registry();

    // Mis-sized packet.
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mux = reg
        .open_muxer("iff_deep", out, std::slice::from_ref(&stream))
        .unwrap();
    mux.write_header().unwrap();
    assert!(mux
        .write_packet(&rgba_packet(&stream, vec![0u8; 3], 0, None))
        .is_err());

    // Trailer with no frames.
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mux = reg
        .open_muxer("iff_deep", out, std::slice::from_ref(&stream))
        .unwrap();
    mux.write_header().unwrap();
    assert!(mux.write_trailer().is_err());

    // Wrong pixel format.
    let mut bad = video_stream(2, 2, TimeBase::new(1, 1));
    bad.params.pixel_format = None;
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    assert!(reg
        .open_muxer("iff_deep", out, std::slice::from_ref(&bad))
        .is_err());

    // Oversized dimensions (IFF raster headers are 16-bit).
    let big = video_stream(70_000, 2, TimeBase::new(1, 1));
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    assert!(reg
        .open_muxer("iff_deep", out, std::slice::from_ref(&big))
        .is_err());
}

// ─────────────────────────── iff_rgb8 / iff_rgbn ───────────────────────────

#[test]
fn rgb8_mux_roundtrips_through_the_demuxer() {
    let stream = video_stream(3, 2, TimeBase::new(1, 1));
    // A run of 5 identical pixels (spills across the row break) + 1 distinct.
    let mut rgba = Vec::new();
    for _ in 0..5 {
        rgba.extend_from_slice(&[0x12, 0x34, 0x56, 255]);
    }
    rgba.extend_from_slice(&[0xAA, 0xBB, 0xCC, 255]);
    let bytes = mux_frames("iff_rgb8", &stream, &[(rgba.clone(), 0, None)]);

    let reg = registry();
    let mut cur = Cursor::new(bytes.clone());
    assert_eq!(reg.probe_input(&mut cur, None).unwrap(), "iff_rgb8");
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_rgb8", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, rgba);
}

#[test]
fn rgbn_mux_roundtrips_nibble_replicated_colours() {
    let stream = video_stream(4, 1, TimeBase::new(1, 1));
    // Nibble-replicated guns (high nibble == low nibble) survive the §3.1
    // 12-bit quantisation exactly.
    let mut rgba = Vec::new();
    rgba.extend_from_slice(&[0xFF, 0x00, 0x00, 255]);
    rgba.extend_from_slice(&[0xFF, 0xFF, 0xFF, 255]);
    rgba.extend_from_slice(&[0x44, 0x88, 0xCC, 255]);
    rgba.extend_from_slice(&[0x44, 0x88, 0xCC, 255]);
    let bytes = mux_frames("iff_rgbn", &stream, &[(rgba.clone(), 0, None)]);

    let reg = registry();
    let mut cur = Cursor::new(bytes.clone());
    assert_eq!(reg.probe_input(&mut cur, None).unwrap(), "iff_rgbn");
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_rgbn", input, &oxideav_core::NullCodecResolver)
        .unwrap();
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, rgba);
}

#[test]
fn rgb8_mux_is_single_frame() {
    let stream = video_stream(1, 1, TimeBase::new(1, 1));
    let reg = registry();
    let out: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mux = reg
        .open_muxer("iff_rgb8", out, std::slice::from_ref(&stream))
        .unwrap();
    mux.write_header().unwrap();
    mux.write_packet(&rgba_packet(&stream, vec![1, 2, 3, 255], 0, None))
        .unwrap();
    assert!(mux
        .write_packet(&rgba_packet(&stream, vec![4, 5, 6, 255], 1, None))
        .is_err());
}
