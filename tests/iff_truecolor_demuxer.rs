//! Container-registry coverage for the Turbo-Silver true-colour FORM
//! demuxers (`iff_rgb8`, `iff_rgbn`).
//!
//! These exercise the registry wiring — extension table, byte-signature
//! probe, and `open_demuxer` → single `rawvideo` / `Rgba` keyframe — that
//! sits on top of the `ilbm::parse_rgb8` / `parse_rgbn` body decoders. The
//! per-pixel decode correctness lives in `iff_truecolor.rs`; here we only
//! confirm the demuxer surface decodes the same image, advertises the right
//! stream params, and is EOF after one packet.
//!
//! Spec reference: `docs/image/iff/iff-truecolor-chunks.md` §3.

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

fn bmhd(w: u16, h: u16, n_planes: u8, compression: u8) -> Vec<u8> {
    let mut b = vec![0u8; 20];
    b[0..2].copy_from_slice(&w.to_be_bytes());
    b[2..4].copy_from_slice(&h.to_be_bytes());
    b[8] = n_planes;
    b[10] = compression;
    b[14] = 1;
    b[15] = 1;
    b
}

fn rgb8_long(r: u8, g: u8, b: u8, lock: bool, count: u8) -> [u8; 4] {
    assert!((1..=127).contains(&count));
    let rgb = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    let w = (rgb << 8) | (u32::from(lock) << 7) | (u32::from(count) & 0x7F);
    w.to_be_bytes()
}

fn rgbn_word(r: u16, g: u16, b: u16, lock: bool, count: u16) -> [u8; 2] {
    assert!((1..=7).contains(&count));
    let rgb12 = (r & 0xF) << 8 | (g & 0xF) << 4 | (b & 0xF);
    let w = rgb12 << 4 | (u16::from(lock) << 3) | count;
    w.to_be_bytes()
}

fn rgb8_file() -> Vec<u8> {
    // 2x2: a run of 3 magenta then 1 green (the magenta run spills across the
    // row boundary because the body is a flat width*height stream).
    let mut bdy = Vec::new();
    bdy.extend_from_slice(&rgb8_long(0xC0, 0x10, 0xC0, false, 3));
    bdy.extend_from_slice(&rgb8_long(0x00, 0xFF, 0x00, false, 1));
    iff_form(
        b"RGB8",
        &[
            (b"BMHD", bmhd(2, 2, 25, 4)),
            (b"CAMG", vec![0, 0, 0, 0]),
            (b"BODY", bdy),
        ],
    )
}

fn rgbn_file() -> Vec<u8> {
    // 4x1: red run of 2 then white run of 2.
    let mut bdy = Vec::new();
    bdy.extend_from_slice(&rgbn_word(0xF, 0x0, 0x0, false, 2));
    bdy.extend_from_slice(&rgbn_word(0xF, 0xF, 0xF, false, 2));
    iff_form(
        b"RGBN",
        &[
            (b"BMHD", bmhd(4, 1, 13, 4)),
            (b"CAMG", vec![0, 0, 0, 0]),
            (b"BODY", bdy),
        ],
    )
}

fn open(reg: &ContainerRegistry, name: &str, bytes: Vec<u8>) -> Box<dyn oxideav_core::Demuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    reg.open_demuxer(name, rs, &oxideav_core::NullCodecResolver)
        .unwrap()
}

// ─────────────────────────── extension table ───────────────────────────

#[test]
fn extensions_route_to_true_colour_demuxers() {
    let reg = registry();
    assert_eq!(reg.container_for_extension("rgb8"), Some("iff_rgb8"));
    assert_eq!(reg.container_for_extension("rgbn"), Some("iff_rgbn"));
}

// ──────────────────────────── byte probe ───────────────────────────────

#[test]
fn probe_detects_each_true_colour_form() {
    let reg = registry();
    for (name, bytes) in [("iff_rgb8", rgb8_file()), ("iff_rgbn", rgbn_file())] {
        let mut cur = Cursor::new(bytes);
        let detected = reg.probe_input(&mut cur, None).unwrap();
        assert_eq!(detected, name, "probe should detect {name} by signature");
    }
}

#[test]
fn probe_rejects_unrelated_form() {
    let reg = registry();
    // A FORM 8SVX must not be claimed by any true-colour probe; the 8svx
    // demuxer (also registered) is the legitimate match here.
    let bytes = iff_form(b"8SVX", &[(b"VHDR", vec![0u8; 20])]);
    let mut cur = Cursor::new(bytes);
    let detected = reg.probe_input(&mut cur, None).unwrap();
    assert_ne!(detected, "iff_rgb8");
    assert_ne!(detected, "iff_rgbn");
}

// ──────────────────────────── demux RGB8 ───────────────────────────────

#[test]
fn rgb8_demuxer_emits_single_rgba_keyframe() {
    let reg = registry();
    let mut dmx = open(&reg, "iff_rgb8", rgb8_file());
    assert_eq!(dmx.format_name(), "iff_rgb8");

    let s = &dmx.streams()[0];
    assert_eq!(s.params.codec_id, CodecId::new("rawvideo"));
    assert_eq!(s.params.media_type, MediaType::Video);
    assert_eq!(s.params.width, Some(2));
    assert_eq!(s.params.height, Some(2));
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Rgba));

    let pkt = dmx.next_packet().unwrap();
    assert!(pkt.flags.keyframe);
    assert_eq!(pkt.data.len(), 2 * 2 * 4);
    // Default GenlockPolicy is "ignore — use the coded RGB".
    for px in 0..3 {
        assert_eq!(&pkt.data[px * 4..px * 4 + 4], &[0xC0, 0x10, 0xC0, 0xFF]);
    }
    assert_eq!(&pkt.data[12..16], &[0x00, 0xFF, 0x00, 0xFF]);

    // One image → one packet, then EOF.
    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}

// ──────────────────────────── demux RGBN ───────────────────────────────

#[test]
fn rgbn_demuxer_widens_4bit_guns() {
    let reg = registry();
    let mut dmx = open(&reg, "iff_rgbn", rgbn_file());
    assert_eq!(dmx.format_name(), "iff_rgbn");

    let s = &dmx.streams()[0];
    assert_eq!(s.params.width, Some(4));
    assert_eq!(s.params.height, Some(1));
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Rgba));

    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data.len(), 4 * 4);
    assert_eq!(&pkt.data[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
    assert_eq!(&pkt.data[4..8], &[0xFF, 0x00, 0x00, 0xFF]);
    assert_eq!(&pkt.data[8..12], &[0xFF, 0xFF, 0xFF, 0xFF]);
    assert_eq!(&pkt.data[12..16], &[0xFF, 0xFF, 0xFF, 0xFF]);

    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}

// ─────────────────────────── error surface ─────────────────────────────

#[test]
fn rgb8_demuxer_rejects_wrong_form_type() {
    let reg = registry();
    // An RGBN file opened through the RGB8 demuxer must error on the form
    // type mismatch, rather than mis-decode.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(rgbn_file()));
    assert!(reg
        .open_demuxer("iff_rgb8", rs, &oxideav_core::NullCodecResolver)
        .is_err());
}
