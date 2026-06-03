//! Round-227 coverage: AIFF / AIFF-C `SAXL` (Sound Accelerator)
//! optional-chunk surfacing through the FORM walker, plus the
//! write-side helper.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §8.0 + Appendix D the chunk
//! carries a `numSaxels` count followed by per-saxel records of
//! `(MarkerId id, u16 size, byte[size] saxelData)`. §8.0 permits
//! "any number of Saxel Chunks" per FORM so the walker accumulates
//! them in document order, mirroring how §10.0 MIDI and §12.0 APPL
//! handle the "any-number-per-FORM" rule.

use oxideav_iff::aiff::{parse, write_saxel_chunk, Marker, MarkerChunk, Saxel, SaxelChunk};

/// 80-bit IEEE extended encoding for a sample rate in Hz.
fn ext(rate: f64) -> [u8; 10] {
    let sign = rate.is_sign_negative();
    let mag = rate.abs();
    let bits = mag.to_bits();
    let f64_exp = ((bits >> 52) & 0x7ff) as i32;
    let f64_frac = bits & 0x000f_ffff_ffff_ffff;
    let (mantissa_64, exp_unbiased): (u64, i32) = if f64_exp == 0 {
        let lead = f64_frac.leading_zeros() as i32 - 11;
        let mantissa = f64_frac << (12 + lead);
        let true_exp = -1022 - lead;
        (mantissa, true_exp)
    } else {
        let mantissa = (1_u64 << 63) | (f64_frac << 11);
        let true_exp = f64_exp - 1023;
        (mantissa, true_exp)
    };
    let biased_ext = exp_unbiased + 16_383;
    let exp_field = biased_ext as u16 & 0x7fff;
    let mut o = [0u8; 10];
    o[0] = ((exp_field >> 8) as u8) | if sign { 0x80 } else { 0 };
    o[1] = (exp_field & 0xff) as u8;
    o[2..10].copy_from_slice(&mantissa_64.to_be_bytes());
    o
}

fn pack_chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len() + 1);
    v.extend_from_slice(id);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

fn comm_body() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&1_i16.to_be_bytes()); // mono
    b.extend_from_slice(&2_u32.to_be_bytes()); // 2 frames
    b.extend_from_slice(&16_i16.to_be_bytes()); // 16-bit
    b.extend_from_slice(&ext(44_100.0));
    b
}

fn ssnd_body() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0_u32.to_be_bytes());
    b.extend_from_slice(&0_u32.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x01, 0x02, 0x03]);
    b
}

fn form_aiff(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFF");
    inner.extend_from_slice(&pack_chunk(b"COMM", &comm_body()));
    for (id, body) in chunks {
        inner.extend_from_slice(&pack_chunk(id, body));
    }
    inner.extend_from_slice(&pack_chunk(b"SSND", &ssnd_body()));
    let mut f = Vec::new();
    f.extend_from_slice(b"FORM");
    f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    f.extend_from_slice(&inner);
    f
}

/// Pack a per-saxel record: id + size + data + pad-if-odd.
fn pack_saxel(id: i16, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + data.len() + 1);
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(&(data.len() as u16).to_be_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

/// Pack a SAXL chunk body: numSaxels + saxels.
fn pack_saxel_body(saxels: &[(i16, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(saxels.len() as u16).to_be_bytes());
    for (id, data) in saxels {
        body.extend_from_slice(&pack_saxel(*id, data));
    }
    body
}

#[test]
fn surfaces_single_saxel_chunk() {
    // One SAXL chunk carrying two saxels — the canonical 48-byte ACE/MAC
    // priming payload size per Appendix D ¶ "Saxels for ACE and
    // Macintosh compressed sound data".
    let body = pack_saxel_body(&[
        (1, &(0..48u8).collect::<Vec<_>>()),
        (2, &(100..132u8).collect::<Vec<_>>()),
    ]);
    let f = form_aiff(&[(b"SAXL", body)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.saxels.len(), 1);
    let saxl = &p.saxels[0];
    assert_eq!(saxl.saxels.len(), 2);
    assert_eq!(saxl.saxels[0].id, 1);
    assert_eq!(saxl.saxels[0].len(), 48);
    assert_eq!(saxl.saxels[1].id, 2);
    assert_eq!(saxl.saxels[1].len(), 32);
}

#[test]
fn surfaces_multiple_saxl_chunks_in_document_order() {
    // §8.0 / Appendix D permits "any number of Saxel Chunks" per FORM.
    // Order is preserved verbatim.
    let body_a = pack_saxel_body(&[(10, &[0xAA, 0xAA])]);
    let body_b = pack_saxel_body(&[(20, &[0xBB, 0xBB, 0xBB, 0xBB])]);
    let body_c = pack_saxel_body(&[(30, &[0xCC])]);

    let f = form_aiff(&[(b"SAXL", body_a), (b"SAXL", body_b), (b"SAXL", body_c)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.saxels.len(), 3);
    assert_eq!(p.saxels[0].saxels[0].id, 10);
    assert_eq!(p.saxels[1].saxels[0].id, 20);
    assert_eq!(p.saxels[2].saxels[0].id, 30);
    assert_eq!(p.saxels[2].saxels[0].data, &[0xCC]);
}

#[test]
fn surfaces_zero_saxl_chunks_as_empty_vec() {
    // A FORM with no SAXL chunks at all must expose an empty Vec, not
    // None, mirroring how the MIDI / APPL / ANNO fields handle the
    // "any-number-per-FORM" rule.
    let f = form_aiff(&[]);
    let p = parse(&f).unwrap();
    assert!(p.saxels.is_empty());
}

#[test]
fn saxl_chunk_with_empty_saxel_list_surfaces() {
    // numSaxels=0 — chunk is present but carries no saxels. This is
    // legal per the spec; the parser must still expose the chunk.
    let body = pack_saxel_body(&[]);
    let f = form_aiff(&[(b"SAXL", body)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.saxels.len(), 1);
    assert!(p.saxels[0].saxels.is_empty());
}

#[test]
fn saxl_chunk_with_odd_size_saxeldata_round_trips() {
    // Per-saxel pad byte exercised: 5-byte saxelData on the first
    // saxel, 4-byte on the second. The chunk walker strips the outer
    // pad (if any) and the SAXL parser strips the per-saxel pad.
    let body = pack_saxel_body(&[
        (1, &[0xDE, 0xAD, 0xBE, 0xEF, 0x42]),
        (2, &[0x11, 0x22, 0x33, 0x44]),
    ]);
    let f = form_aiff(&[(b"SAXL", body)]);
    let p = parse(&f).unwrap();
    let saxl = &p.saxels[0];
    assert_eq!(saxl.saxels[0].data, &[0xDE, 0xAD, 0xBE, 0xEF, 0x42]);
    assert_eq!(saxl.saxels[1].data, &[0x11, 0x22, 0x33, 0x44]);
}

#[test]
fn saxl_write_helper_roundtrips() {
    // Build a SaxelChunk via the structured surface, encode through
    // write_saxel_chunk, wrap into a FORM, parse back — every field
    // must survive.
    let original = SaxelChunk {
        saxels: vec![
            Saxel {
                id: 1,
                data: (0..48u8).collect(),
            },
            Saxel {
                id: 7,
                data: vec![0xCA, 0xFE, 0xBA, 0xBE, 0xDE],
            },
            Saxel {
                id: 12,
                data: vec![],
            },
        ],
    };
    let body = write_saxel_chunk(&original);
    let f = form_aiff(&[(b"SAXL", body)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.saxels.len(), 1);
    assert_eq!(p.saxels[0], original);
}

#[test]
fn saxl_resolve_marker_against_form_mark_chunk() {
    // §8.0 ¶ "id identifies the marker for which the sound accelerator
    // data is to be used." — exercising the resolve_marker helper
    // against the FORM's MARK chunk.
    let mut mark_body = Vec::new();
    mark_body.extend_from_slice(&2_u16.to_be_bytes()); // numMarkers
                                                       // Marker 1: id=3, position=1024, name="loop start"
                                                       // pstring length(1) + 10 chars = 11 bytes, odd → pad byte.
    mark_body.extend_from_slice(&3i16.to_be_bytes());
    mark_body.extend_from_slice(&1024u32.to_be_bytes());
    mark_body.push(10);
    mark_body.extend_from_slice(b"loop start");
    mark_body.push(0); // pstring pad-to-even
                       // Marker 2: id=7, position=2048, name="loop end"
                       // pstring length(1) + 8 chars = 9 bytes, odd → pad byte.
    mark_body.extend_from_slice(&7i16.to_be_bytes());
    mark_body.extend_from_slice(&2048u32.to_be_bytes());
    mark_body.push(8);
    mark_body.extend_from_slice(b"loop end");
    mark_body.push(0); // pstring pad-to-even

    let saxl_body = pack_saxel_body(&[(3, &[0x01; 48]), (7, &[0x02; 48])]);

    let f = form_aiff(&[(b"MARK", mark_body), (b"SAXL", saxl_body)]);
    let p = parse(&f).unwrap();
    let markers = p.markers.as_ref().unwrap();
    let saxl = &p.saxels[0];

    let m1 = saxl.saxels[0].resolve_marker(markers).unwrap();
    assert_eq!(m1.name, "loop start");
    assert_eq!(m1.position, 1024);

    let m2 = saxl.saxels[1].resolve_marker(markers).unwrap();
    assert_eq!(m2.name, "loop end");
    assert_eq!(m2.position, 2048);
}

#[test]
fn saxl_by_marker_id_finds_saxel() {
    let body = pack_saxel_body(&[(1, &[0x11]), (5, &[0x55]), (9, &[0x99])]);
    let f = form_aiff(&[(b"SAXL", body)]);
    let p = parse(&f).unwrap();
    let saxl = &p.saxels[0];
    assert_eq!(saxl.by_marker_id(5).unwrap().data, &[0x55]);
    assert_eq!(saxl.by_marker_id(9).unwrap().data, &[0x99]);
    assert!(saxl.by_marker_id(99).is_none());
}

#[test]
fn saxl_coexists_with_other_optional_chunks() {
    // SAXL alongside MARK + COMT + APPL + ANNO + MIDI — exercise that
    // adding a new chunk class doesn't disturb the existing
    // chunk-class routing in the FORM walker.
    let mut mark_body = Vec::new();
    mark_body.extend_from_slice(&1_u16.to_be_bytes());
    mark_body.extend_from_slice(&1i16.to_be_bytes());
    mark_body.extend_from_slice(&0u32.to_be_bytes());
    // pstring: length(1) + 5 chars = 6 bytes, even — no pad needed.
    mark_body.push(5);
    mark_body.extend_from_slice(b"start");

    let mut comt_body = Vec::new();
    comt_body.extend_from_slice(&1_u16.to_be_bytes()); // numComments
    comt_body.extend_from_slice(&0u32.to_be_bytes()); // timestamp
    comt_body.extend_from_slice(&0i16.to_be_bytes()); // marker (unlinked)
    comt_body.extend_from_slice(&2u16.to_be_bytes()); // count
    comt_body.extend_from_slice(b"hi");

    let mut appl_body = Vec::new();
    appl_body.extend_from_slice(b"App1");
    appl_body.extend_from_slice(&[1, 2, 3, 4]);

    let saxl_body = pack_saxel_body(&[(1, &[0xAA; 48])]);
    let midi_body = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];

    let f = form_aiff(&[
        (b"MARK", mark_body),
        (b"COMT", comt_body),
        (b"APPL", appl_body),
        (b"MIDI", midi_body),
        (b"SAXL", saxl_body),
        (b"ANNO", b"annotation".to_vec()),
    ]);
    let p = parse(&f).unwrap();
    // Every chunk class must surface independently.
    assert_eq!(p.markers.as_ref().unwrap().markers.len(), 1);
    assert_eq!(p.comments.as_ref().unwrap().comments.len(), 1);
    assert_eq!(p.applications.len(), 1);
    assert_eq!(p.midi.len(), 1);
    assert_eq!(p.saxels.len(), 1);
    assert_eq!(p.annotations.len(), 1);
    // SAXL link resolves against the FORM's MARK chunk.
    let saxl = &p.saxels[0];
    let markers = p.markers.as_ref().unwrap();
    assert_eq!(
        saxl.saxels[0].resolve_marker(markers).unwrap().name,
        "start"
    );
}

#[test]
fn saxl_resolve_marker_ignores_zero_or_negative_id() {
    // A degenerate / mis-encoded saxel with id == 0 must not collide
    // with a real marker that has id == 1. resolve_marker returns
    // None per §6.0 ¶ "the id can be any positive non-zero integer".
    let body = pack_saxel_body(&[(0, &[0x42])]);
    let f = form_aiff(&[(b"SAXL", body)]);
    let p = parse(&f).unwrap();
    let saxl = &p.saxels[0];
    let markers = MarkerChunk {
        markers: vec![Marker {
            id: 1,
            position: 0,
            name: "x".into(),
        }],
    };
    assert!(saxl.saxels[0].resolve_marker(&markers).is_none());
}
