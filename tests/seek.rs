//! Integration test for `SvxDemuxer::seek_to`.
//!
//! 8SVX is keyframe-only `pcm_s8` (after the demuxer transparently
//! decompresses Fibonacci-delta on open), so `seek_to(pts)` must land
//! exactly on `pts` (no keyframe quantisation) and the next packet must
//! start at that sample-frame boundary with matching payload bytes.
//!
//! This file is a portmanteau: most assertions cover seek mechanics on
//! a raw-PCM voice (the closest analogue of the wav_seek test), with a
//! tail block that exercises seek through a Fibonacci-compressed body
//! to prove the decoded-buffer cursor model holds across both
//! `sCompression` values supported by the demuxer.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Packet, SampleFormat, StreamInfo, TimeBase,
};
use oxideav_core::{ContainerRegistry, Muxer, ReadSeek, WriteSeek};

use oxideav_iff::svx::{Compression, SvxMuxer};

/// Build a mono pcm_s8 stream at `sample_rate` Hz.
fn mono_s8_stream(sample_rate: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s8"));
    params.media_type = MediaType::Audio;
    params.channels = Some(1);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(SampleFormat::S8);
    params.bit_rate = Some(8 * sample_rate as u64);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// 1 second of mono `pcm_s8` sawtooth at `sample_rate`. Each sample is a
/// deterministic function of its index so a seek lands on a known byte.
fn synth_1s_s8(sample_rate: u32) -> Vec<u8> {
    let n = sample_rate as usize;
    (0..n)
        .map(|i| {
            // Ramp -64..63 wrapped through the i8 range. Steps of 3 give
            // a smooth signal that Fibonacci-delta can also track to
            // within ±2 LSBs.
            (((i as i32 * 3).rem_euclid(128)) - 64) as i8 as u8
        })
        .collect()
}

fn tmp_path(tag: &str) -> std::path::PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-iff-seek-{tag}-{}-{n}.8svx",
        std::process::id()
    ))
}

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

/// Build a raw-PCM 8SVX bytestream for the given mono `pcm_s8` payload.
fn build_raw_8svx(payload: &[u8], sample_rate: u32) -> Vec<u8> {
    let stream = mono_s8_stream(sample_rate);
    let path = tmp_path("raw");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = SvxMuxer::new(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        mux.write_packet(&Packet::new(0, stream.time_base, payload.to_vec()))
            .unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

#[test]
fn seek_to_zero_resets_to_start() {
    let sr = 8000u32;
    let payload = synth_1s_s8(sr);
    let bytes = build_raw_8svx(&payload, sr);

    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .expect("open 8svx demuxer");

    // Drain a few packets to advance the cursor away from 0.
    let _ = dmx.next_packet().unwrap();
    let _ = dmx.next_packet().unwrap();

    // Seek back to 0; next packet must start at pts=0 and the first
    // sample must equal the synthetic source byte 0.
    let landed = dmx.seek_to(0, 0).expect("seek_to(0)");
    assert_eq!(landed, 0);

    let pkt = dmx.next_packet().expect("packet after seek(0)");
    assert_eq!(pkt.pts, Some(0));
    assert_eq!(pkt.dts, Some(0));
    assert!(pkt.flags.keyframe);
    assert_eq!(pkt.data[0], payload[0]);

    // Drain everything and confirm byte-for-byte equality with input.
    let mut out = Vec::new();
    out.extend_from_slice(&pkt.data);
    loop {
        match dmx.next_packet() {
            Ok(p) => out.extend_from_slice(&p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(out, payload, "full re-stream after seek(0) must match");
}

#[test]
fn seek_to_half_second_lands_at_exact_sample() {
    let sr = 8000u32;
    let payload = synth_1s_s8(sr);
    assert_eq!(payload.len(), sr as usize);
    let bytes = build_raw_8svx(&payload, sr);

    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .expect("open 8svx demuxer");

    let target = (sr / 2) as i64; // 4000
    let landed = dmx.seek_to(0, target).expect("seek_to");
    assert_eq!(
        landed, target,
        "8SVX is keyframe-only PCM — landed pts must equal target pts"
    );

    let pkt = dmx.next_packet().expect("next_packet after seek");
    assert_eq!(pkt.pts, Some(target), "next packet pts must equal target");
    assert_eq!(pkt.dts, Some(target));
    assert!(pkt.flags.keyframe);

    // S8 mono → 1 byte/frame. The first byte of the post-seek packet
    // must equal `payload[target]`.
    let want_start = target as usize;
    let want_end = want_start + pkt.data.len();
    assert_eq!(
        pkt.data,
        &payload[want_start..want_end],
        "packet bytes must match the source at the seek offset"
    );
}

#[test]
fn seek_past_end_clamps() {
    let sr = 8000u32;
    let payload = synth_1s_s8(sr);
    let bytes = build_raw_8svx(&payload, sr);

    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .expect("open 8svx demuxer");

    // Seeking past EOF clamps to the total frame count.
    let clamped = dmx.seek_to(0, i64::MAX).expect("seek past EOF clamps");
    assert_eq!(clamped, sr as i64);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));

    // Negative target clamps to 0.
    let zero = dmx.seek_to(0, -1).expect("seek negative clamps to 0");
    assert_eq!(zero, 0);
    let pkt = dmx.next_packet().expect("packet after seek(-1)");
    assert_eq!(pkt.pts, Some(0));
}

#[test]
fn seek_invalid_stream_index_errors() {
    let sr = 8000u32;
    let payload = synth_1s_s8(sr);
    let bytes = build_raw_8svx(&payload, sr);

    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .expect("open 8svx demuxer");

    assert!(dmx.seek_to(1, 0).is_err());
    assert!(dmx.seek_to(42, 0).is_err());
}

/// Seek must still be exact across a Fibonacci-compressed BODY because
/// the demuxer fully expands the compressed nibble stream into a flat
/// `pcm_s8` buffer at `open()` time. After seek, the next packet must
/// resume at the decoded sample boundary — not at the compressed-byte
/// boundary, which doesn't map to a fixed sample stride.
#[test]
fn seek_through_fibonacci_body() {
    let sr = 8000u32;
    // Smooth signal that Fibonacci-delta can track within ±2 LSBs.
    let payload: Vec<u8> = (0..sr as usize)
        .map(|i| {
            let v = (60.0 * (i as f64 * std::f64::consts::TAU * 120.0 / sr as f64).sin()).round();
            (v as i8) as u8
        })
        .collect();

    // Mux with Fibonacci compression.
    let stream = mono_s8_stream(sr);
    let path = tmp_path("fib");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = SvxMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_compression(Compression::Fibonacci);
        mux.write_header().unwrap();
        mux.write_packet(&Packet::new(0, stream.time_base, payload.clone()))
            .unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    // Decode straight through to capture the lossy reference stream;
    // we compare seeked output against THIS, not the pre-encoded
    // payload (Fibonacci is lossy).
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let mut dmx = reg
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    let mut reference = Vec::<u8>::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => reference.extend_from_slice(&p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(reference.len(), sr as usize);

    // Re-open and seek to a non-trivial offset.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    let target = 1234i64;
    let landed = dmx.seek_to(0, target).unwrap();
    assert_eq!(landed, target);
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.pts, Some(target));
    let want_start = target as usize;
    let want_end = want_start + pkt.data.len();
    assert_eq!(
        pkt.data,
        &reference[want_start..want_end],
        "Fibonacci-decoded samples after seek must match the reference at the seek offset"
    );
}
