//! Integration tests for AIFF / AIFF-C `MARK` chunk parsing.
//!
//! These exercise the public `oxideav_iff::aiff::*` surface (rather
//! than the internal module path used by the unit tests) so a
//! regression that breaks the re-exports surfaces here too.

use oxideav_iff::aiff::{parse, parse_marker_chunk, AiffError, MarkerChunk};

/// Build a 10-byte 80-bit IEEE-extended encoding of `rate`. Mirrors
/// the helper used in the in-crate unit tests (`form::tests::ext`).
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

fn pack(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len() + 1);
    v.extend_from_slice(id);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

fn build_mark_body(markers: &[(i16, u32, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(markers.len() as u16).to_be_bytes());
    for (id, pos, name) in markers {
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&pos.to_be_bytes());
        body.push(name.len() as u8);
        body.extend_from_slice(name.as_bytes());
        if (1 + name.len()) % 2 == 1 {
            body.push(0);
        }
    }
    body
}

fn build_aiff_with_marks(
    channels: u16,
    frames: u32,
    bits: u16,
    rate: f64,
    samples: &[u8],
    markers: &[(i16, u32, &str)],
) -> Vec<u8> {
    let mut comm_body = Vec::new();
    comm_body.extend_from_slice(&(channels as i16).to_be_bytes());
    comm_body.extend_from_slice(&frames.to_be_bytes());
    comm_body.extend_from_slice(&(bits as i16).to_be_bytes());
    comm_body.extend_from_slice(&ext(rate));

    let mut ssnd_body = Vec::new();
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
    ssnd_body.extend_from_slice(samples);

    let mark_body = build_mark_body(markers);

    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFF");
    inner.extend_from_slice(&pack(b"COMM", &comm_body));
    inner.extend_from_slice(&pack(b"MARK", &mark_body));
    inner.extend_from_slice(&pack(b"SSND", &ssnd_body));

    let mut file = Vec::new();
    file.extend_from_slice(b"FORM");
    file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    file.extend_from_slice(&inner);
    file
}

#[test]
fn full_form_with_loop_markers_round_trips() {
    // Two-second 44.1 kHz mono fixture with begin/end loop markers
    // at frames 0 and 88200 — typical sampler-loop layout described
    // by §9.0 of the AIFF-C spec.
    let pcm = vec![0u8; 4]; // 2 frames, 16-bit mono
    let f = build_aiff_with_marks(
        1,
        2,
        16,
        44_100.0,
        &pcm,
        &[(1, 0, "loop start"), (2, 88_200, "loop end")],
    );
    let parsed = parse(&f).unwrap();
    let marks = parsed.markers.as_ref().expect("MARK should be present");
    assert_eq!(marks.markers.len(), 2);
    assert_eq!(marks.markers[0].id, 1);
    assert_eq!(marks.markers[0].position, 0);
    assert_eq!(marks.markers[0].name, "loop start");
    assert_eq!(marks.markers[1].id, 2);
    assert_eq!(marks.markers[1].position, 88_200);
    assert_eq!(marks.markers[1].name, "loop end");

    // by_id helper works.
    assert_eq!(marks.by_id(2).unwrap().position, 88_200);
}

#[test]
fn aifc_with_marker_and_compression() {
    // FORM(AIFC) with NONE compression and a single marker. Confirms
    // MARK + FVER + COMM all parse alongside each other.
    let pcm = [0x00_u8, 0x01];
    let rate = 22_050.0;
    let mut comm_body = Vec::new();
    comm_body.extend_from_slice(&1_i16.to_be_bytes());
    comm_body.extend_from_slice(&1_u32.to_be_bytes());
    comm_body.extend_from_slice(&16_i16.to_be_bytes());
    comm_body.extend_from_slice(&ext(rate));
    comm_body.extend_from_slice(b"NONE");
    comm_body.push(0);
    comm_body.push(0);

    let mut ssnd_body = Vec::new();
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
    ssnd_body.extend_from_slice(&pcm);

    let fver_body = 0xA280_5140_u32.to_be_bytes();
    let mark_body = build_mark_body(&[(42, 1, "cue")]);

    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFC");
    inner.extend_from_slice(&pack(b"FVER", &fver_body));
    inner.extend_from_slice(&pack(b"COMM", &comm_body));
    inner.extend_from_slice(&pack(b"MARK", &mark_body));
    inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
    let mut f = Vec::new();
    f.extend_from_slice(b"FORM");
    f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    f.extend_from_slice(&inner);

    let parsed = parse(&f).unwrap();
    assert_eq!(&parsed.form_type, b"AIFC");
    assert_eq!(parsed.fver_timestamp, Some(0xA280_5140));
    let marks = parsed.markers.as_ref().unwrap();
    assert_eq!(marks.markers.len(), 1);
    assert_eq!(marks.markers[0].id, 42);
    assert_eq!(marks.markers[0].position, 1);
    assert_eq!(marks.markers[0].name, "cue");
}

#[test]
fn duplicate_mark_chunk_is_rejected() {
    // Two MARK chunks inside a single FORM — explicit spec violation
    // (§6.0 "No more than one Marker Chunk can appear in a FORM
    // AIFC."). Must surface as DuplicateChunk("MARK").
    let pcm = [0x00_u8, 0x01];
    let mut comm_body = Vec::new();
    comm_body.extend_from_slice(&1_i16.to_be_bytes());
    comm_body.extend_from_slice(&1_u32.to_be_bytes());
    comm_body.extend_from_slice(&16_i16.to_be_bytes());
    comm_body.extend_from_slice(&ext(44_100.0));

    let mut ssnd_body = Vec::new();
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
    ssnd_body.extend_from_slice(&pcm);

    let mark_body_a = build_mark_body(&[(1, 0, "a")]);
    let mark_body_b = build_mark_body(&[(2, 0, "b")]);

    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFF");
    inner.extend_from_slice(&pack(b"COMM", &comm_body));
    inner.extend_from_slice(&pack(b"MARK", &mark_body_a));
    inner.extend_from_slice(&pack(b"MARK", &mark_body_b));
    inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
    let mut f = Vec::new();
    f.extend_from_slice(b"FORM");
    f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    f.extend_from_slice(&inner);

    let r = parse(&f);
    assert!(matches!(r, Err(AiffError::DuplicateChunk("MARK"))));
}

#[test]
fn marker_chunk_helper_round_trip() {
    // Direct exercise of the public free-function entry point — same
    // wire bytes the FORM walker hands it.
    let body = build_mark_body(&[(10, 100, "intro"), (20, 200, "verse")]);
    let m: MarkerChunk = parse_marker_chunk(&body).unwrap();
    assert_eq!(m.markers.len(), 2);
    assert_eq!(m.markers[0].id, 10);
    assert_eq!(m.markers[1].name, "verse");
}
