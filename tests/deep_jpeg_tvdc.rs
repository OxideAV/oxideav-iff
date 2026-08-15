//! `FORM DEEP` compressed-DBOD coverage for the two §1.5/§1.5b arms the
//! staged spec pins beyond RUNLENGTH:
//!
//! * **TVDC in a FORM** (Compression = 5, §1.5) — the 16-word delta table is
//!   stored with the file/companion data, not in a chunk, so the FORM-walking
//!   decoders take it from the caller: `parse_deep_with_tvdc_table` /
//!   `parse_deep_frames_with_tvdc_table`, with
//!   `encode_deep_frames_with_tvdc_table` as the multi-frame inverse.
//! * **JPEG** (Compression = 4, §1.5b) — the DBOD is a complete JFIF stream
//!   (SOI `FF D8` at byte 0 … EOI `FF D9` at the end), surfaced whole via
//!   `extract_deep_jpeg_frames` / `encode_deep_jpeg_frames` and demuxed as
//!   codec-id `"mjpeg"` packets; the pixel decode belongs downstream.
//!
//! Also asserts the §1.5b reject-with-diagnostic posture for HUFFMAN (2) and
//! DYNAMICHUFF (3), and the pinned subset of the CAMG dual-playfield pair
//! (`Camg::dual_playfield_priority`).
//!
//! Spec reference: `docs/image/iff/iff-truecolor-chunks.md` §1.5/§1.5b and
//! `docs/image/iff/camg-viewmode-pchg.md` §1.1.

use std::io::Cursor;

use oxideav_core::{CodecId, ContainerRegistry, MediaType, ReadSeek};
use oxideav_iff::ilbm::{
    encode_deep, encode_deep_frames_with_tvdc_table, encode_deep_jpeg_frames,
    extract_deep_jpeg_frames, parse_deep, parse_deep_frames, parse_deep_frames_with_tvdc_table,
    parse_deep_with_tvdc_table, Camg, Dchg, DeepCompression, Dloc, Dpel, PlayfieldPriority,
    CAMG_DUALPF, CAMG_PFBA, LORESDPF2_KEY, LORESDPF_KEY,
};

// ───────────────────────────── FORM builders ─────────────────────────────

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

fn dpel_bytes(elems: &[(u16, u16)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(elems.len() as u32).to_be_bytes());
    for (ct, depth) in elems {
        b.extend_from_slice(&ct.to_be_bytes());
        b.extend_from_slice(&depth.to_be_bytes());
    }
    b
}

fn dgbl_bytes(dw: u16, dh: u16, compression: u16) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b[0..2].copy_from_slice(&dw.to_be_bytes());
    b[2..4].copy_from_slice(&dh.to_be_bytes());
    b[4..6].copy_from_slice(&compression.to_be_bytes());
    b[6] = 1;
    b[7] = 1;
    b
}

fn rgb888_dpel() -> Dpel {
    Dpel::parse(&dpel_bytes(&[(1, 8), (2, 8), (3, 8)])).unwrap()
}

/// A minimal, structurally valid JFIF stand-in: SOI, opaque middle bytes,
/// EOI. §1.5b only pins the SOI/EOI framing at the container layer; the
/// stream's interior belongs to the downstream JPEG decoder.
fn fake_jfif(middle: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.extend_from_slice(middle);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

// ─────────────────────────── TVDC in a FORM ───────────────────────────

/// The delta table used across the TVDC tests: +1 / -1 / +7 / +8 steps
/// (nibble 0 stays the §1.5 run-length escape; the accumulator restarts at 0
/// on every component line, so each line's first value must be reachable in
/// one delta).
fn tvdc_table() -> [i16; 16] {
    let mut table = [0i16; 16];
    table[1] = 1;
    table[2] = -1;
    table[3] = 7;
    table[4] = 8;
    table
}

#[test]
fn tvdc_single_image_roundtrips_through_the_form_walker() {
    let table = tvdc_table();
    let dpel = rgb888_dpel();
    // R/G/B all start at 7 (+7 from the zero accumulator) then oscillate.
    let rgba = vec![7, 7, 7, 0xFF, 8, 8, 8, 0xFF, 7, 7, 7, 0xFF, 8, 8, 8, 0xFF];
    let file = encode_deep(&dpel, 4, 1, DeepCompression::Tvdc, Some(&table), &rgba).unwrap();

    let back = parse_deep_with_tvdc_table(&file, &table).unwrap();
    assert_eq!((back.width, back.height), (4, 1));
    assert_eq!(back.dgbl.compression, DeepCompression::Tvdc);
    assert_eq!(back.rgba, rgba);
}

#[test]
fn tvdc_multiframe_roundtrips_with_dchg_timing() {
    let table = tvdc_table();
    let dpel = rgb888_dpel();
    // Two 2x2 frames exercising runs (repeated values) and both delta signs.
    let f0 = vec![7, 7, 7, 0xFF, 7, 7, 7, 0xFF, 8, 8, 8, 0xFF, 8, 8, 8, 0xFF];
    let f1 = vec![7, 7, 7, 0xFF, 6, 6, 6, 0xFF, 7, 7, 7, 0xFF, 8, 8, 8, 0xFF];
    let dchg = Dchg { frame_rate: 40 };
    let file =
        encode_deep_frames_with_tvdc_table(&dpel, 2, 2, &table, Some(dchg), &[&f0, &f1]).unwrap();

    let movie = parse_deep_frames_with_tvdc_table(&file, &table).unwrap();
    assert_eq!(movie.dgbl.compression, DeepCompression::Tvdc);
    assert_eq!(movie.frames.len(), 2);
    assert_eq!(movie.frames[0].rgba, f0);
    assert_eq!(movie.frames[1].rgba, f1);
    assert_eq!(movie.frame_delay_millis(), Some(40));
    assert!(movie.is_animation());
}

#[test]
fn tvdc_form_without_table_still_reports_the_documented_gap() {
    let table = tvdc_table();
    let dpel = rgb888_dpel();
    let rgba = vec![7, 7, 7, 0xFF];
    let file = encode_deep(&dpel, 1, 1, DeepCompression::Tvdc, Some(&table), &rgba).unwrap();
    let err = parse_deep(&file).unwrap_err().to_string();
    assert!(err.contains("delta table"), "unexpected error: {err}");
    let err2 = parse_deep_frames(&file).unwrap_err().to_string();
    assert!(err2.contains("delta table"), "unexpected error: {err2}");
}

#[test]
fn tvdc_table_variant_also_decodes_uncompressed_bodies() {
    // A caller holding a table need not pre-inspect DGBL: NOCOMPRESSION
    // decodes through the _with_tvdc_table entry points unchanged.
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl_bytes(1, 1, 0)),
            (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![9, 8, 7]),
        ],
    );
    let table = tvdc_table();
    let img = parse_deep_with_tvdc_table(&file, &table).unwrap();
    assert_eq!(&img.rgba, &[9, 8, 7, 0xFF]);
    let movie = parse_deep_frames_with_tvdc_table(&file, &table).unwrap();
    assert_eq!(&movie.frames[0].rgba, &[9, 8, 7, 0xFF]);
}

#[test]
fn tvdc_truncated_in_form_body_is_rejected() {
    // A 2x1 TVDC frame needs two component bytes per line; a body that runs
    // out of nibbles mid-line must error, not panic or hang.
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl_bytes(4, 4, 5)),
            (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![0x11]),
        ],
    );
    let table = tvdc_table();
    assert!(parse_deep_with_tvdc_table(&file, &table).is_err());
    assert!(parse_deep_frames_with_tvdc_table(&file, &table).is_err());
}

#[test]
fn tvdc_multiframe_encoder_validates_input() {
    let table = tvdc_table();
    let dpel = rgb888_dpel();
    // Empty frame list.
    let none: &[&[u8]] = &[];
    assert!(encode_deep_frames_with_tvdc_table(&dpel, 1, 1, &table, None, none).is_err());
    // Mis-sized RGBA buffer.
    let short = [0u8; 3];
    let frames: &[&[u8]] = &[&short];
    assert!(encode_deep_frames_with_tvdc_table(&dpel, 1, 1, &table, None, frames).is_err());
    // Sub-8-bit DPEL component (TVDC emits one byte per component).
    let dpel4 = Dpel::parse(&dpel_bytes(&[(1, 4), (2, 4), (3, 4)])).unwrap();
    let px = [1u8, 2, 3, 255];
    let frames: &[&[u8]] = &[&px];
    assert!(encode_deep_frames_with_tvdc_table(&dpel4, 1, 1, &table, None, frames).is_err());
}

// ─────────────────────────── JPEG DBOD surfacing ───────────────────────────

#[test]
fn jpeg_frames_roundtrip_byte_exact() {
    let dpel = rgb888_dpel();
    let j0 = fake_jfif(&[0x01, 0x02, 0x03]);
    let j1 = fake_jfif(&[0x04]);
    let dchg = Dchg { frame_rate: 100 };
    let file = encode_deep_jpeg_frames(&dpel, 8, 6, Some(dchg), &[&j0, &j1]).unwrap();

    let movie = extract_deep_jpeg_frames(&file).unwrap();
    assert_eq!(movie.dgbl.compression, DeepCompression::Jpeg);
    assert_eq!(
        (movie.dgbl.display_width, movie.dgbl.display_height),
        (8, 6)
    );
    assert_eq!(movie.frames.len(), 2);
    assert_eq!(movie.frames[0].jfif, j0);
    assert_eq!(movie.frames[1].jfif, j1);
    // No DLOC emitted → container dims fall back to the DGBL display size.
    assert_eq!((movie.frames[0].width, movie.frames[0].height), (8, 6));
    assert_eq!(movie.dchg.unwrap().frame_rate, 100);
}

#[test]
fn jpeg_dloc_binds_to_the_next_dbod() {
    // §1.3: a DLOC gives the FOLLOWING DBOD's dimensions.
    let dl = Dloc {
        w: 3,
        h: 2,
        x: 1,
        y: 1,
    };
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl_bytes(16, 16, 4)),
            (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
            (b"DLOC", dl.write().to_vec()),
            (b"DBOD", fake_jfif(&[0xAA])),
        ],
    );
    let movie = extract_deep_jpeg_frames(&file).unwrap();
    assert_eq!((movie.frames[0].width, movie.frames[0].height), (3, 2));
    assert_eq!(movie.frames[0].dloc.unwrap(), dl);
}

#[test]
fn jpeg_extract_rejects_non_jpeg_compression() {
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl_bytes(1, 1, 0)),
            (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![9, 8, 7]),
        ],
    );
    let err = extract_deep_jpeg_frames(&file).unwrap_err().to_string();
    assert!(err.contains("Compression 0"), "unexpected error: {err}");
}

#[test]
fn jpeg_pixel_parsers_point_at_the_surfacing_path() {
    let dpel = rgb888_dpel();
    let jf = fake_jfif(&[0x00]);
    let file = encode_deep_jpeg_frames(&dpel, 1, 1, None, &[&jf]).unwrap();
    let err = parse_deep(&file).unwrap_err().to_string();
    assert!(
        err.contains("extract_deep_jpeg_frames"),
        "unexpected error: {err}"
    );
}

#[test]
fn jpeg_hostile_bodies_are_rejected() {
    let mk = |body: Vec<u8>| {
        iff_form(
            b"DEEP",
            &[
                (b"DGBL", dgbl_bytes(1, 1, 4)),
                (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
                (b"DBOD", body),
            ],
        )
    };
    // Shorter than SOI + EOI.
    assert!(extract_deep_jpeg_frames(&mk(vec![0xFF, 0xD8, 0xFF])).is_err());
    // Missing SOI.
    assert!(extract_deep_jpeg_frames(&mk(vec![0x00, 0x00, 0xFF, 0xD9])).is_err());
    // Missing EOI.
    assert!(extract_deep_jpeg_frames(&mk(vec![0xFF, 0xD8, 0x00, 0x00])).is_err());
    // Encode-side validation mirrors it.
    let dpel = rgb888_dpel();
    let bad = [0xFFu8, 0xD8, 0x00, 0x00];
    let frames: &[&[u8]] = &[&bad];
    assert!(encode_deep_jpeg_frames(&dpel, 1, 1, None, frames).is_err());
}

#[test]
fn jpeg_truncated_chunk_is_rejected() {
    // A DBOD whose declared size runs past the FORM end must error cleanly.
    let mut file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl_bytes(1, 1, 4)),
            (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", fake_jfif(&[0x11, 0x22])),
        ],
    );
    // Inflate the DBOD's declared size field (last chunk in the FORM).
    let dbod_at = file.windows(4).position(|w| w == b"DBOD").unwrap();
    file[dbod_at + 4..dbod_at + 8].copy_from_slice(&0xFFFFu32.to_be_bytes());
    assert!(extract_deep_jpeg_frames(&file).is_err());
}

// ──────────────────────── demuxer: JPEG passthrough ────────────────────────

#[test]
fn deep_demuxer_passes_jpeg_bodies_through_as_mjpeg_packets() {
    let dpel = rgb888_dpel();
    let j0 = fake_jfif(&[0x10, 0x20]);
    let j1 = fake_jfif(&[0x30]);
    let dchg = Dchg { frame_rate: 50 };
    let file = encode_deep_jpeg_frames(&dpel, 4, 4, Some(dchg), &[&j0, &j1]).unwrap();

    let reg = registry();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = reg
        .open_demuxer("iff_deep", input, &oxideav_core::NullCodecResolver)
        .unwrap();

    let stream = &dmx.streams()[0];
    assert_eq!(stream.params.media_type, MediaType::Video);
    assert_eq!(stream.params.codec_id, CodecId::new("mjpeg"));
    assert_eq!(stream.params.width, Some(4));
    assert_eq!(stream.params.height, Some(4));
    // Compressed stream: no raw pixel format is declared.
    assert_eq!(stream.params.pixel_format, None);

    let p0 = dmx.next_packet().unwrap();
    assert_eq!(p0.data, j0);
    assert!(p0.flags.keyframe);
    assert_eq!(p0.pts, Some(0));
    assert_eq!(p0.duration, Some(50));
    let p1 = dmx.next_packet().unwrap();
    assert_eq!(p1.data, j1);
    assert_eq!(p1.pts, Some(50));
    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
    assert_eq!(dmx.duration_micros(), Some(100_000));
}

#[test]
fn deep_demuxer_rejects_hostile_jpeg_body() {
    // The demuxer path applies the same §1.5b SOI/EOI validation.
    let file = iff_form(
        b"DEEP",
        &[
            (b"DGBL", dgbl_bytes(1, 1, 4)),
            (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
            (b"DBOD", vec![0x00, 0x01, 0x02, 0x03]),
        ],
    );
    let reg = registry();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    assert!(reg
        .open_demuxer("iff_deep", input, &oxideav_core::NullCodecResolver)
        .is_err());
}

// ─────────────── HUFFMAN / DYNAMICHUFF §1.5b reject posture ───────────────

#[test]
fn huffman_and_dynamichuff_are_rejected_with_named_diagnostics() {
    for (code, name) in [(2u16, "HUFFMAN"), (3u16, "DYNAMICHUFF")] {
        let file = iff_form(
            b"DEEP",
            &[
                (b"DGBL", dgbl_bytes(1, 1, code)),
                (b"DPEL", dpel_bytes(&[(1, 8), (2, 8), (3, 8)])),
                (b"DBOD", vec![0x00, 0x01]),
            ],
        );
        let err = parse_deep(&file).unwrap_err().to_string();
        assert!(err.contains(name), "Compression {code}: {err}");
        assert!(err.contains("fixture"), "Compression {code}: {err}");
        let err2 = parse_deep_frames(&file).unwrap_err().to_string();
        assert!(err2.contains(name), "Compression {code}: {err2}");
        // The table-supplied walkers hit the same wall — the table is a TVDC
        // concept and does not unlock the undocumented Huffman codings.
        let table = tvdc_table();
        assert!(parse_deep_with_tvdc_table(&file, &table).is_err());
        // And the demuxer surfaces the same rejection.
        let reg = registry();
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
        assert!(reg
            .open_demuxer("iff_deep", input, &oxideav_core::NullCodecResolver)
            .is_err());
    }
}

// ──────────────────── CAMG dual-playfield (pinned subset) ────────────────────

#[test]
fn dual_playfield_priority_follows_the_pfba_bit() {
    // No DUALPF → no dual-playfield display, PFBA has nothing to order.
    let single = Camg { raw: CAMG_PFBA };
    assert_eq!(single.dual_playfield_priority(), None);

    let dpf = Camg { raw: CAMG_DUALPF };
    assert_eq!(
        dpf.dual_playfield_priority(),
        Some(PlayfieldPriority::Playfield1InFront)
    );

    let dpf2 = Camg {
        raw: CAMG_DUALPF | CAMG_PFBA,
    };
    assert_eq!(
        dpf2.dual_playfield_priority(),
        Some(PlayfieldPriority::Playfield2InFront)
    );
}

#[test]
fn dual_playfield_mode_keys_encode_the_same_distinction() {
    // The staged §1.3 mode-key table pins LORESDPF_KEY (DUALPF alone) vs
    // LORESDPF2_KEY (DUALPF | PFBA); the flag accessors agree with them.
    let k1 = Camg { raw: LORESDPF_KEY };
    assert!(k1.is_dualpf() && !k1.is_pfba());
    assert_eq!(
        k1.dual_playfield_priority(),
        Some(PlayfieldPriority::Playfield1InFront)
    );
    let k2 = Camg { raw: LORESDPF2_KEY };
    assert!(k2.is_dualpf() && k2.is_pfba());
    assert_eq!(
        k2.dual_playfield_priority(),
        Some(PlayfieldPriority::Playfield2InFront)
    );
}
