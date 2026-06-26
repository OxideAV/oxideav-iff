//! Round-373 coverage: FORM TVPP (TVPaint project files) best-effort decode.
//!
//! TVPP is **non-canonical** (community RE; §2 of
//! `docs/image/iff/iff-truecolor-chunks.md`). It reuses the DEEP chunk
//! vocabulary (DGBL / DPEL / DLOC / DBOD / DCHG) for its raster layers and
//! adds three TVPP-specific chunks (MIXR / BGP1 / BGP2) whose byte layout is
//! not pinned down. `ilbm::parse_tvpp` decodes the DEEP-vocabulary raster
//! exactly as `parse_deep_frames` does (each DBOD is one layer) and surfaces
//! the MIXR / BGP1 / BGP2 chunks raw, without inventing their semantics.

use std::io::Cursor;

use oxideav_core::{ContainerRegistry, ReadSeek};
use oxideav_iff::ilbm::parse_tvpp;

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

fn dgbl(dw: u16, dh: u16, compression: u16) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b[0..2].copy_from_slice(&dw.to_be_bytes());
    b[2..4].copy_from_slice(&dh.to_be_bytes());
    b[4..6].copy_from_slice(&compression.to_be_bytes());
    b[6] = 1;
    b[7] = 1;
    b
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

#[test]
fn tvpp_single_layer_rgb_decode() {
    // 2x2 RGB888 chunky layer; dimensions from DGBL display size.
    #[rustfmt::skip]
    let body: Vec<u8> = vec![
        10, 20, 30,   40, 50, 60,
        70, 80, 90,   100, 110, 120,
    ];
    let file = iff_form(
        b"TVPP",
        &[
            (b"DGBL", dgbl(2, 2, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", body),
        ],
    );
    let img = parse_tvpp(&file).unwrap();
    assert_eq!(img.layers.len(), 1);
    assert_eq!((img.layers[0].width, img.layers[0].height), (2, 2));
    // First pixel R=10 G=20 B=30 A=255.
    assert_eq!(&img.layers[0].rgba[0..4], &[10, 20, 30, 255]);
    assert!(img.extra_chunks.is_empty());
}

#[test]
fn tvpp_multi_layer_with_extra_chunks() {
    let layer0: Vec<u8> = vec![1, 2, 3]; // 1x1 RGB
    let layer1: Vec<u8> = vec![4, 5, 6];
    let file = iff_form(
        b"TVPP",
        &[
            (b"DGBL", dgbl(1, 1, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"MIXR", vec![0xDE, 0xAD]),
            (b"DBOD", layer0),
            (b"BGP1", vec![0x11, 0x22, 0x33]),
            (b"DBOD", layer1),
            (b"BGP2", vec![0x44, 0x55, 0x66]),
        ],
    );
    let img = parse_tvpp(&file).unwrap();
    // Two DBODs -> two layers.
    assert_eq!(img.layers.len(), 2);
    assert_eq!(&img.layers[0].rgba[0..4], &[1, 2, 3, 255]);
    assert_eq!(&img.layers[1].rgba[0..4], &[4, 5, 6, 255]);
    // Three extra chunks, preserved raw in document order.
    assert_eq!(img.extra_chunks.len(), 3);
    assert_eq!(&img.extra_chunks[0].id, b"MIXR");
    assert_eq!(img.extra_chunks[0].data, vec![0xDE, 0xAD]);
    assert_eq!(&img.extra_chunks[1].id, b"BGP1");
    assert_eq!(img.extra_chunks[1].data, vec![0x11, 0x22, 0x33]);
    assert_eq!(&img.extra_chunks[2].id, b"BGP2");
    assert_eq!(img.extra_chunks[2].data, vec![0x44, 0x55, 0x66]);
}

#[test]
fn tvpp_wrong_form_type_rejected() {
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl(1, 1, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![1, 2, 3]),
        ],
    );
    assert!(parse_tvpp(&file).is_err());
}

#[test]
fn tvpp_missing_dbod_rejected() {
    let file = iff_form(
        b"TVPP",
        &[
            (b"DGBL", dgbl(1, 1, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
        ],
    );
    assert!(parse_tvpp(&file).is_err());
}

#[test]
fn tvpp_missing_dgbl_rejected() {
    let file = iff_form(
        b"TVPP",
        &[
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![1, 2, 3]),
        ],
    );
    assert!(parse_tvpp(&file).is_err());
}

#[test]
fn tvpp_dloc_sized_layer() {
    // A DLOC preceding a DBOD overrides the DGBL display size for that layer.
    // DLOC: w=1 h=1 x=0 y=0.
    let mut dloc = vec![0u8; 8];
    dloc[0..2].copy_from_slice(&1u16.to_be_bytes());
    dloc[2..4].copy_from_slice(&1u16.to_be_bytes());
    let file = iff_form(
        b"TVPP",
        &[
            (b"DGBL", dgbl(4, 4, 0)), // display size 4x4
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DLOC", dloc),
            (b"DBOD", vec![9, 8, 7]), // only 1 pixel -> must be DLOC-sized 1x1
        ],
    );
    let img = parse_tvpp(&file).unwrap();
    assert_eq!(img.layers.len(), 1);
    assert_eq!((img.layers[0].width, img.layers[0].height), (1, 1));
    assert!(img.layers[0].dloc.is_some());
    assert_eq!(&img.layers[0].rgba[0..4], &[9, 8, 7, 255]);
}

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

#[test]
fn tvpp_extension_and_probe_route_to_demuxer() {
    let reg = registry();
    assert_eq!(reg.container_for_extension("tvpp"), Some("iff_tvpp"));

    let file = iff_form(
        b"TVPP",
        &[
            (b"DGBL", dgbl(1, 1, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![1, 2, 3]),
        ],
    );
    let mut cur = Cursor::new(file.clone());
    let detected = reg.probe_input(&mut cur, None).unwrap();
    assert_eq!(detected, "iff_tvpp");
}

#[test]
fn tvpp_demuxer_emits_one_keyframe_per_layer() {
    let reg = registry();
    let file = iff_form(
        b"TVPP",
        &[
            (b"DGBL", dgbl(1, 1, 0)),
            (b"DPEL", dpel(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![1, 2, 3]),
            (b"DBOD", vec![4, 5, 6]),
        ],
    );
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = reg
        .open_demuxer("iff_tvpp", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    let p0 = dmx.next_packet().unwrap();
    assert_eq!(&p0.data[0..4], &[1, 2, 3, 255]);
    assert!(p0.flags.keyframe);
    let p1 = dmx.next_packet().unwrap();
    assert_eq!(&p1.data[0..4], &[4, 5, 6, 255]);
    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}
