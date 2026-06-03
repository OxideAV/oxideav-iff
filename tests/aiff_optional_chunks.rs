//! Round-209 coverage: AIFF / AIFF-C `COMT`, `AESD`, and `APPL`
//! optional-chunk surfacing through the FORM walker, plus
//! write-side helpers for `MARK` and `INST`.
//!
//! Each test builds a FORM containing the chunk(s) of interest at
//! the byte level and confirms the high-level [`parse`] surface
//! exposes them on the [`Form`] struct. The write-side tests
//! round-trip a structured chunk through the encoder and back into
//! the parser.

use oxideav_iff::aiff::{
    parse, AesdChunk, ApplicationChunk, ApplicationDialect, Comment, CommentsChunk,
    InstrumentChunk, Loop, Marker, MarkerChunk, MidiDataChunk, PlayMode,
};

/// 80-bit IEEE extended encoding for a sample rate in Hz. Mirrors
/// the `ext` helper inside the form-parser tests.
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

#[test]
fn surfaces_comt_chunk() {
    let mut body = Vec::new();
    body.extend_from_slice(&2_u16.to_be_bytes()); // numComments
                                                  // Comment 1: timestamp=0, marker=0, text="hello"
    body.extend_from_slice(&0_u32.to_be_bytes());
    body.extend_from_slice(&0_i16.to_be_bytes());
    body.extend_from_slice(&5_u16.to_be_bytes());
    body.extend_from_slice(b"hello");
    body.push(0); // pad (count=5 odd)
                  // Comment 2: timestamp=100, marker=2, text="ok"
    body.extend_from_slice(&100_u32.to_be_bytes());
    body.extend_from_slice(&2_i16.to_be_bytes());
    body.extend_from_slice(&2_u16.to_be_bytes());
    body.extend_from_slice(b"ok");

    let f = form_aiff(&[(b"COMT", body)]);
    let p = parse(&f).unwrap();
    let c = p.comments.as_ref().unwrap();
    assert_eq!(c.comments.len(), 2);
    assert_eq!(c.comments[0].text, "hello");
    assert_eq!(c.comments[0].linked_marker(), None);
    assert_eq!(c.comments[1].text, "ok");
    assert_eq!(c.comments[1].linked_marker(), Some(2));
}

#[test]
fn surfaces_aesd_chunk() {
    let mut body = [0u8; 24];
    body[0] = 0b0001_0100; // emphasis bits 2..=4 = 0b101
    body[5] = 0xAB;
    let f = form_aiff(&[(b"AESD", body.to_vec())]);
    let p = parse(&f).unwrap();
    let aesd = p.aesd.unwrap();
    assert_eq!(aesd.status[0], 0b0001_0100);
    assert_eq!(aesd.status[5], 0xAB);
    assert_eq!(aesd.emphasis().bits, 0b101);
}

#[test]
fn surfaces_multiple_appl_chunks_in_document_order() {
    // §12.0: any number of APPL chunks may exist per FORM.
    let mut a1 = Vec::new();
    a1.extend_from_slice(b"App1");
    a1.extend_from_slice(&[1, 2, 3, 4]);
    let mut a2 = Vec::new();
    a2.extend_from_slice(b"pdos");
    a2.push(3);
    a2.extend_from_slice(b"Pic");
    a2.extend_from_slice(&[0xAA, 0xBB]);
    let mut a3 = Vec::new();
    a3.extend_from_slice(b"App2");
    a3.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

    let f = form_aiff(&[(b"APPL", a1), (b"APPL", a2), (b"APPL", a3)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.applications.len(), 3);
    assert_eq!(&p.applications[0].signature, b"App1");
    assert_eq!(p.applications[0].dialect(), ApplicationDialect::Macintosh);
    assert_eq!(p.applications[0].data, vec![1, 2, 3, 4]);
    assert_eq!(&p.applications[1].signature, b"pdos");
    assert_eq!(p.applications[1].dialect(), ApplicationDialect::AppleII);
    assert_eq!(
        p.applications[1].application_name(),
        Some("Pic".to_string())
    );
    assert_eq!(p.applications[1].payload_after_name(), &[0xAA, 0xBB]);
    assert_eq!(&p.applications[2].signature, b"App2");
}

#[test]
fn surfaces_zero_appl_chunks_as_empty_vec() {
    let f = form_aiff(&[]);
    let p = parse(&f).unwrap();
    assert!(p.applications.is_empty());
    assert!(p.comments.is_none());
    assert!(p.aesd.is_none());
}

#[test]
fn rejects_duplicate_comt_chunks() {
    let mut empty = Vec::new();
    empty.extend_from_slice(&0_u16.to_be_bytes());
    let f = form_aiff(&[(b"COMT", empty.clone()), (b"COMT", empty)]);
    let r = parse(&f);
    assert!(r.is_err());
}

#[test]
fn rejects_duplicate_aesd_chunks() {
    let body = vec![0u8; 24];
    let f = form_aiff(&[(b"AESD", body.clone()), (b"AESD", body)]);
    let r = parse(&f);
    assert!(r.is_err());
}

#[test]
fn surfaces_mark_inst_comt_appl_aesd_together() {
    // Build a FORM exercising every newly-surfaced optional chunk in
    // one shot, plus the existing MARK / INST pair.
    let mut mark = Vec::new();
    mark.extend_from_slice(&1_u16.to_be_bytes());
    mark.extend_from_slice(&1_i16.to_be_bytes());
    mark.extend_from_slice(&0_u32.to_be_bytes());
    mark.push(3);
    mark.extend_from_slice(b"cue");

    // INST chunk body (20 bytes): baseNote=60, detune=0, lowNote=0,
    // highNote=127, lowVelocity=1, highVelocity=127, gain=0,
    // sustainLoop = 6 zero bytes, releaseLoop = 6 zero bytes.
    let mut inst = vec![60, 0, 0, 127, 1, 127];
    inst.extend_from_slice(&0_i16.to_be_bytes()); // gain
    inst.extend_from_slice(&[0u8; 12]); // both loops, 6 bytes each

    let mut comt = Vec::new();
    comt.extend_from_slice(&1_u16.to_be_bytes());
    comt.extend_from_slice(&0_u32.to_be_bytes());
    comt.extend_from_slice(&1_i16.to_be_bytes()); // links to marker id 1
    comt.extend_from_slice(&4_u16.to_be_bytes());
    comt.extend_from_slice(b"note");

    let aesd = vec![0xFF; 24];

    let mut appl = Vec::new();
    appl.extend_from_slice(b"Test");
    appl.extend_from_slice(&[0xCA, 0xFE]);

    let f = form_aiff(&[
        (b"MARK", mark),
        (b"INST", inst),
        (b"COMT", comt),
        (b"AESD", aesd),
        (b"APPL", appl),
    ]);
    let p = parse(&f).unwrap();
    assert!(p.markers.is_some());
    assert!(p.instrument.is_some());
    assert!(p.comments.is_some());
    assert!(p.aesd.is_some());
    assert_eq!(p.applications.len(), 1);
    let c = &p.comments.as_ref().unwrap().comments[0];
    let resolved = c.resolve_marker(p.markers.as_ref().unwrap()).unwrap();
    assert_eq!(resolved.name, "cue");
}

#[test]
fn marker_chunk_write_roundtrips_through_parse() {
    use oxideav_iff::aiff::write_marker_chunk;
    let m = MarkerChunk {
        markers: vec![
            Marker {
                id: 1,
                position: 0,
                name: "begin".into(),
            },
            Marker {
                id: 2,
                position: 1024,
                name: "end".into(),
            },
        ],
    };
    let body = write_marker_chunk(&m);
    let f = form_aiff(&[(b"MARK", body)]);
    let p = parse(&f).unwrap();
    let parsed = p.markers.as_ref().unwrap();
    assert_eq!(parsed.markers.len(), 2);
    assert_eq!(parsed.markers[0].name, "begin");
    assert_eq!(parsed.markers[1].position, 1024);
}

#[test]
fn instrument_chunk_write_roundtrips_through_parse() {
    use oxideav_iff::aiff::write_instrument_chunk;
    let inst = InstrumentChunk {
        base_note: 60,
        detune: -10,
        low_note: 48,
        high_note: 72,
        low_velocity: 1,
        high_velocity: 127,
        gain: 3,
        sustain_loop: Loop {
            play_mode: PlayMode::Forward,
            begin_loop: 1,
            end_loop: 2,
        },
        release_loop: Loop {
            play_mode: PlayMode::None,
            begin_loop: 0,
            end_loop: 0,
        },
    };
    let body = write_instrument_chunk(&inst);
    assert_eq!(body.len(), 20);
    let f = form_aiff(&[(b"INST", body.to_vec())]);
    let p = parse(&f).unwrap();
    let parsed = p.instrument.unwrap();
    assert_eq!(parsed, inst);
}

#[test]
fn comments_chunk_write_helper_roundtrips() {
    use oxideav_iff::aiff::write_comments_chunk;
    let c = CommentsChunk {
        comments: vec![
            Comment {
                timestamp: 0,
                marker: 0,
                text: "free".into(),
            },
            Comment {
                timestamp: 1_000_000,
                marker: 5,
                text: "linked".into(),
            },
        ],
    };
    let body = write_comments_chunk(&c);
    let f = form_aiff(&[(b"COMT", body)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.comments.unwrap(), c);
}

#[test]
fn appl_write_helper_roundtrips() {
    use oxideav_iff::aiff::write_appl_chunk;
    let a = ApplicationChunk {
        signature: *b"Foo!",
        data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34],
    };
    let body = write_appl_chunk(&a);
    let f = form_aiff(&[(b"APPL", body)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.applications.len(), 1);
    assert_eq!(p.applications[0], a);
}

#[test]
fn aesd_write_helper_roundtrips() {
    use oxideav_iff::aiff::write_aesd_chunk;
    let mut status = [0u8; 24];
    status[0] = 0x14;
    status[10] = 0x77;
    let a = AesdChunk { status };
    let body = write_aesd_chunk(&a);
    let f = form_aiff(&[(b"AESD", body.to_vec())]);
    let p = parse(&f).unwrap();
    assert_eq!(p.aesd.unwrap(), a);
}

#[test]
fn surfaces_single_midi_chunk() {
    // §10.0 SysEx-style body: F0 ... F7.
    let body = vec![
        0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7,
    ];
    let f = form_aiff(&[(b"MIDI", body.clone())]);
    let p = parse(&f).unwrap();
    assert_eq!(p.midi.len(), 1);
    assert_eq!(p.midi[0].data, body);
    assert!(p.midi[0].is_sysex());
    assert!(!p.midi[0].is_empty());
}

#[test]
fn surfaces_multiple_midi_chunks_in_document_order() {
    // §10.0: "Any number of MIDI Data Chunks may exist in a FORM AIFC."
    let m1 = vec![0xF0, 0x41, 0xF7];
    let m2 = vec![0x90, 0x3C, 0x7F]; // Note On, ch1
    let m3 = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
    let f = form_aiff(&[
        (b"MIDI", m1.clone()),
        (b"MIDI", m2.clone()),
        (b"MIDI", m3.clone()),
    ]);
    let p = parse(&f).unwrap();
    assert_eq!(p.midi.len(), 3);
    assert_eq!(p.midi[0].data, m1);
    assert_eq!(p.midi[1].data, m2);
    assert_eq!(p.midi[2].data, m3);
    assert!(p.midi[0].is_sysex());
    assert!(!p.midi[1].is_sysex());
    assert!(p.midi[2].is_sysex());
}

#[test]
fn surfaces_zero_midi_chunks_as_empty_vec() {
    let f = form_aiff(&[]);
    let p = parse(&f).unwrap();
    assert!(p.midi.is_empty());
}

#[test]
fn midi_chunk_with_odd_length_round_trips_through_chunk_walker() {
    // 5-byte odd-length MIDI body — outer chunk walker inserts a pad
    // byte but strips it before handing the body to the MIDI parser.
    let body = vec![0xC0, 0x00, 0xB0, 0x07, 0x40];
    let f = form_aiff(&[(b"MIDI", body.clone())]);
    let p = parse(&f).unwrap();
    assert_eq!(p.midi.len(), 1);
    assert_eq!(p.midi[0].data, body);
    assert_eq!(p.midi[0].len(), 5);
}

#[test]
fn midi_chunk_write_helper_roundtrips() {
    use oxideav_iff::aiff::write_midi_chunk;
    let m = MidiDataChunk {
        data: vec![0xF0, 0x7D, 0x00, 0x01, 0x02, 0xF7],
    };
    let body = write_midi_chunk(&m);
    let f = form_aiff(&[(b"MIDI", body)]);
    let p = parse(&f).unwrap();
    assert_eq!(p.midi.len(), 1);
    assert_eq!(p.midi[0], m);
}

#[test]
fn empty_midi_chunk_is_accepted() {
    // §10.0 doesn't forbid ckDataSize=0; the parser surfaces it as a
    // zero-length data buffer the caller can choose to ignore.
    let f = form_aiff(&[(b"MIDI", Vec::new())]);
    let p = parse(&f).unwrap();
    assert_eq!(p.midi.len(), 1);
    assert!(p.midi[0].is_empty());
    assert_eq!(p.midi[0].len(), 0);
    assert!(!p.midi[0].is_sysex());
}

#[test]
fn midi_chunk_coexists_with_other_optional_chunks() {
    // Build a FORM exercising MARK + INST + MIDI together — confirms
    // the new branch doesn't clobber the existing surfaces.
    let mut mark = Vec::new();
    mark.extend_from_slice(&1_u16.to_be_bytes());
    mark.extend_from_slice(&1_i16.to_be_bytes());
    mark.extend_from_slice(&0_u32.to_be_bytes());
    mark.push(3);
    mark.extend_from_slice(b"cue");

    let mut inst = vec![60, 0, 0, 127, 1, 127];
    inst.extend_from_slice(&0_i16.to_be_bytes());
    inst.extend_from_slice(&[0u8; 12]);

    let midi1 = vec![0xF0, 0x41, 0x10, 0xF7];
    let midi2 = vec![0xB0, 0x07, 0x64];

    let f = form_aiff(&[
        (b"MARK", mark),
        (b"INST", inst),
        (b"MIDI", midi1.clone()),
        (b"MIDI", midi2.clone()),
    ]);
    let p = parse(&f).unwrap();
    assert!(p.markers.is_some());
    assert!(p.instrument.is_some());
    assert_eq!(p.midi.len(), 2);
    assert_eq!(p.midi[0].data, midi1);
    assert_eq!(p.midi[1].data, midi2);
}
