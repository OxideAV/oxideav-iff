//! Integration tests for AIFF / AIFF-C `INST` chunk parsing.
//!
//! Exercises the public `oxideav_iff::aiff::*` surface so a
//! regression in the re-exports (or in `Form::instrument` /
//! `InstrumentChunk::resolve_sustain_loop`) surfaces here too.

use oxideav_iff::aiff::{
    parse, parse_instrument_chunk, AiffError, InstrumentChunk, Marker, MarkerChunk, PlayMode,
};

/// Build a 10-byte 80-bit IEEE-extended encoding of `rate`. Mirrors
/// the helper used in the sibling `aiff_markers.rs` integration test.
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

/// 20-byte INST ckData body in the order the spec defines.
#[allow(clippy::too_many_arguments)]
fn build_inst_body(
    base_note: u8,
    detune: i8,
    low_note: u8,
    high_note: u8,
    low_velocity: u8,
    high_velocity: u8,
    gain: i16,
    sustain: (i16, i16, i16),
    release: (i16, i16, i16),
) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    v.push(base_note);
    v.push(detune as u8);
    v.push(low_note);
    v.push(high_note);
    v.push(low_velocity);
    v.push(high_velocity);
    v.extend_from_slice(&gain.to_be_bytes());
    for triple in [sustain, release] {
        v.extend_from_slice(&triple.0.to_be_bytes());
        v.extend_from_slice(&triple.1.to_be_bytes());
        v.extend_from_slice(&triple.2.to_be_bytes());
    }
    v
}

fn build_aiff_with_mark_and_inst(mark: &[(i16, u32, &str)], inst: &[u8]) -> Vec<u8> {
    let pcm: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    let mut comm = Vec::new();
    comm.extend_from_slice(&1_i16.to_be_bytes()); // numChannels
    comm.extend_from_slice(&2_u32.to_be_bytes()); // numSampleFrames
    comm.extend_from_slice(&16_i16.to_be_bytes()); // sampleSize
    comm.extend_from_slice(&ext(44_100.0));

    let mut ssnd = Vec::new();
    ssnd.extend_from_slice(&0_u32.to_be_bytes()); // offset
    ssnd.extend_from_slice(&0_u32.to_be_bytes()); // blockSize
    ssnd.extend_from_slice(&pcm);

    let mark_body = build_mark_body(mark);

    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFF");
    inner.extend_from_slice(&pack(b"COMM", &comm));
    inner.extend_from_slice(&pack(b"MARK", &mark_body));
    inner.extend_from_slice(&pack(b"INST", inst));
    inner.extend_from_slice(&pack(b"SSND", &ssnd));

    let mut file = Vec::new();
    file.extend_from_slice(b"FORM");
    file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    file.extend_from_slice(&inner);
    file
}

#[test]
fn standalone_parse_decodes_every_field() {
    // Middle C, +25 cents detune, suggested D3..C5, velocity 20..120,
    // -3 dB gain, sustain loop ping-pong on id 1→id 2.
    let body = build_inst_body(60, 25, 50, 72, 20, 120, -3, (2, 1, 2), (1, 3, 4));
    let inst: InstrumentChunk = parse_instrument_chunk(&body).unwrap();
    assert_eq!(inst.base_note, 60);
    assert_eq!(inst.detune, 25);
    assert_eq!(inst.low_note, 50);
    assert_eq!(inst.high_note, 72);
    assert_eq!(inst.low_velocity, 20);
    assert_eq!(inst.high_velocity, 120);
    assert_eq!(inst.gain, -3);
    assert_eq!(inst.sustain_loop.play_mode, PlayMode::ForwardBackward);
    assert_eq!(inst.sustain_loop.begin_loop, 1);
    assert_eq!(inst.sustain_loop.end_loop, 2);
    assert_eq!(inst.release_loop.play_mode, PlayMode::Forward);
    assert_eq!(inst.release_loop.begin_loop, 3);
    assert_eq!(inst.release_loop.end_loop, 4);
}

#[test]
fn form_surfaces_instrument_chunk() {
    let inst = build_inst_body(60, 0, 0, 127, 1, 127, 0, (1, 1, 2), (0, 0, 0));
    let f = build_aiff_with_mark_and_inst(&[(1, 0, "begin"), (2, 1, "end")], &inst);

    let parsed = parse(&f).unwrap();
    let i = parsed.instrument.as_ref().unwrap();
    assert_eq!(i.base_note, 60);
    let m = parsed.markers.as_ref().unwrap();
    assert_eq!(m.markers.len(), 2);
}

#[test]
fn resolves_sustain_loop_through_form() {
    let inst = build_inst_body(60, 0, 0, 127, 1, 127, 0, (1, 1, 2), (0, 0, 0));
    let f = build_aiff_with_mark_and_inst(&[(1, 0, "begin"), (2, 1, "end")], &inst);

    let parsed = parse(&f).unwrap();
    let i = parsed.instrument.unwrap();
    let m = parsed.markers.as_ref().unwrap();
    let r = i.resolve_sustain_loop(m).unwrap();
    assert_eq!(r.play_mode, PlayMode::Forward);
    assert_eq!(r.begin.name, "begin");
    assert_eq!(r.end.name, "end");
}

#[test]
fn resolve_returns_none_when_loop_endpoints_are_inverted() {
    // INST references markers (id 2 → id 1) but in the marker chunk
    // id 2's position (10) sits AFTER id 1's position (0), so
    // begin.position >= end.position once resolved → spec says
    // "ignore this loop segment."
    let inst = build_inst_body(60, 0, 0, 127, 1, 127, 0, (1, 2, 1), (0, 0, 0));
    let f = build_aiff_with_mark_and_inst(&[(1, 0, "a"), (2, 10, "b")], &inst);
    let parsed = parse(&f).unwrap();
    let i = parsed.instrument.unwrap();
    let m = parsed.markers.as_ref().unwrap();
    assert!(i.resolve_sustain_loop(m).is_none());
}

#[test]
fn duplicate_inst_rejected() {
    // Hand-roll a FORM with two INST chunks.
    let pcm: [u8; 2] = [0x12, 0x34];
    let mut comm = Vec::new();
    comm.extend_from_slice(&1_i16.to_be_bytes());
    comm.extend_from_slice(&1_u32.to_be_bytes());
    comm.extend_from_slice(&16_i16.to_be_bytes());
    comm.extend_from_slice(&ext(44_100.0));

    let mut ssnd = Vec::new();
    ssnd.extend_from_slice(&0_u32.to_be_bytes());
    ssnd.extend_from_slice(&0_u32.to_be_bytes());
    ssnd.extend_from_slice(&pcm);

    let inst_a = build_inst_body(60, 0, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));
    let inst_b = build_inst_body(67, 0, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));

    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFF");
    inner.extend_from_slice(&pack(b"COMM", &comm));
    inner.extend_from_slice(&pack(b"INST", &inst_a));
    inner.extend_from_slice(&pack(b"INST", &inst_b));
    inner.extend_from_slice(&pack(b"SSND", &ssnd));
    let mut file = Vec::new();
    file.extend_from_slice(b"FORM");
    file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    file.extend_from_slice(&inner);

    let r = parse(&file);
    assert!(matches!(r, Err(AiffError::DuplicateChunk("INST"))));
}

#[test]
fn missing_marker_id_does_not_panic_resolution() {
    // INST sustain references id 99 — not present in the MARK chunk.
    let inst = build_inst_body(60, 0, 0, 127, 1, 127, 0, (1, 99, 100), (0, 0, 0));
    let markers = MarkerChunk {
        markers: vec![Marker {
            id: 1,
            position: 0,
            name: "only".into(),
        }],
    };
    let i = parse_instrument_chunk(&inst).unwrap();
    assert!(i.resolve_sustain_loop(&markers).is_none());
}
