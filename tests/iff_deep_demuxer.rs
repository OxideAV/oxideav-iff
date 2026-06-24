//! Container-registry coverage for the `iff_deep` demuxer.
//!
//! Exercises the registry wiring — extension table, byte-signature probe,
//! and `open_demuxer` → single `rawvideo` / `Rgba` keyframe — that sits on
//! top of the `ilbm::parse_deep` body decoder. The per-pixel decode
//! correctness lives in `iff_truecolor.rs`; here we only confirm the demuxer
//! surface decodes the same image, advertises the right stream params, and
//! is EOF after one packet — plus that an undecodable body coding (TVDC)
//! surfaces the same `parse_deep` error through the demuxer.
//!
//! Spec reference: `docs/image/iff/iff-truecolor-chunks.md` §1.

use std::io::Cursor;

use oxideav_core::{CodecId, ContainerRegistry, MediaType, PixelFormat, ReadSeek};

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

/// Wrap a list of `(id, payload)` chunks in an even-padded IFF FORM.
fn iff_form(form_type: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(form_type);
    for (id, payload) in chunks {
        body.extend_from_slice(*id);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(payload);
        if payload.len() & 1 == 1 {
            body.push(0);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn dpel(elems: &[(u16, u16)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(elems.len() as u32).to_be_bytes());
    for (ct, depth) in elems {
        b.extend_from_slice(&ct.to_be_bytes());
        b.extend_from_slice(&depth.to_be_bytes());
    }
    b
}

fn dgbl(dw: u16, dh: u16, compression: u16) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b[0..2].copy_from_slice(&dw.to_be_bytes());
    b[2..4].copy_from_slice(&dh.to_be_bytes());
    b[4..6].copy_from_slice(&compression.to_be_bytes());
    b[6] = 1;
    b[7] = 1;
    b
}

fn deep_file(compression: u16, body: Vec<u8>) -> Vec<u8> {
    iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl(2, 2, compression)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", body),
        ],
    )
}

fn deep_nocompression_file() -> Vec<u8> {
    // 2x2 RGB888 chunky NOCOMPRESSION body; dimensions from the DGBL.
    let body: Vec<u8> = vec![
        10, 11, 12, 20, 21, 22, // row 0
        30, 31, 32, 40, 41, 42, // row 1
    ];
    deep_file(0, body)
}

// ─────────────────────────── extension table ───────────────────────────

#[test]
fn deep_extension_routes_to_demuxer() {
    let reg = registry();
    assert_eq!(reg.container_for_extension("deep"), Some("iff_deep"));
}

// ──────────────────────────── byte probe ───────────────────────────────

#[test]
fn probe_detects_deep_form() {
    let reg = registry();
    let mut cur = Cursor::new(deep_nocompression_file());
    assert_eq!(reg.probe_input(&mut cur, None).unwrap(), "iff_deep");
}

// ──────────────────────────── demux DEEP ───────────────────────────────

#[test]
fn deep_demuxer_emits_chunky_rgb888() {
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(deep_nocompression_file()));
    let mut dmx = reg
        .open_demuxer("iff_deep", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    assert_eq!(dmx.format_name(), "iff_deep");

    let s = &dmx.streams()[0];
    assert_eq!(s.params.codec_id, CodecId::new("rawvideo"));
    assert_eq!(s.params.media_type, MediaType::Video);
    assert_eq!(s.params.width, Some(2));
    assert_eq!(s.params.height, Some(2));
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Rgba));

    let pkt = dmx.next_packet().unwrap();
    assert!(pkt.flags.keyframe);
    assert_eq!(pkt.data.len(), 2 * 2 * 4);
    assert_eq!(&pkt.data[0..4], &[10, 11, 12, 0xFF]);
    assert_eq!(&pkt.data[4..8], &[20, 21, 22, 0xFF]);
    assert_eq!(&pkt.data[8..12], &[30, 31, 32, 0xFF]);
    assert_eq!(&pkt.data[12..16], &[40, 41, 42, 0xFF]);

    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}

#[test]
fn deep_demuxer_emits_runlength_rgb888() {
    // DGBL.Compression == 1 (RUNLENGTH): the §1.5b best-effort ByteRun1
    // coding decodes through the demuxer to the same RGBA the chunky body
    // produces. Body = whole-DBOD ByteRun1 of the 2x2 chunky stream above.
    let reg = registry();
    let chunky: Vec<u8> = vec![
        10, 11, 12, 20, 21, 22, // row 0
        30, 31, 32, 40, 41, 42, // row 1
    ];
    // All 12 bytes differ from their neighbours, so a single literal run.
    let mut body = vec![(chunky.len() as i8 - 1) as u8];
    body.extend_from_slice(&chunky);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(deep_file(1, body)));
    let mut dmx = reg
        .open_demuxer("iff_deep", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data.len(), 2 * 2 * 4);
    assert_eq!(&pkt.data[0..4], &[10, 11, 12, 0xFF]);
    assert_eq!(&pkt.data[12..16], &[40, 41, 42, 0xFF]);
    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}

#[test]
fn deep_demuxer_plays_every_frame_of_a_cel_anim() {
    // A FORM DEEP with two DBOD frames + a DCHG 50 ms FrameRate (§1.4 / §1.6):
    // the demuxer must emit one keyframe packet per DBOD, with per-frame PTS
    // advancing by the DCHG delay, and EOF only after the last frame.
    let reg = registry();
    let f0: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]; // 2x2 RGB888
    let f1: Vec<u8> = vec![21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32];
    let mut dchg = vec![0u8; 4];
    dchg[0..4].copy_from_slice(&50i32.to_be_bytes());
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl(2, 2, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DCHG", dchg),
            (b"DBOD", f0),
            (b"DBOD", f1),
        ],
    );

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = reg
        .open_demuxer("iff_deep", rs, &oxideav_core::NullCodecResolver)
        .unwrap();

    // Stream duration = 2 frames * 50 ms = 100_000 us.
    assert_eq!(dmx.duration_micros(), Some(100_000));
    assert_eq!(
        dmx.streams()[0].time_base,
        oxideav_core::TimeBase::new(1, 1000)
    );

    let p0 = dmx.next_packet().unwrap();
    assert!(p0.flags.keyframe);
    assert_eq!(p0.pts, Some(0));
    assert_eq!(p0.duration, Some(50));
    assert_eq!(&p0.data[0..4], &[1, 2, 3, 0xFF]);

    let p1 = dmx.next_packet().unwrap();
    assert!(p1.flags.keyframe);
    assert_eq!(p1.pts, Some(50));
    assert_eq!(&p1.data[0..4], &[21, 22, 23, 0xFF]);

    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}

// ─────────────────────────── error surface ─────────────────────────────

#[test]
fn deep_demuxer_rejects_tvdc_body_from_form() {
    let reg = registry();
    // DGBL.Compression == 5 (TVDC): the §1.5 delta table is not carried
    // in-FORM, so a from-FORM decode must error (same as `parse_deep`),
    // rather than mis-decode the body.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(deep_file(5, vec![0u8; 8])));
    assert!(reg
        .open_demuxer("iff_deep", rs, &oxideav_core::NullCodecResolver)
        .is_err());
}

#[test]
fn deep_demuxer_rejects_wrong_form_type() {
    let reg = registry();
    // A FORM RGB8 opened through the DEEP demuxer must error on the form
    // type mismatch.
    let rgb8 = iff_form(b"RGB8", &[(b"BMHD", vec![0u8; 20])]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(rgb8));
    assert!(reg
        .open_demuxer("iff_deep", rs, &oxideav_core::NullCodecResolver)
        .is_err());
}
