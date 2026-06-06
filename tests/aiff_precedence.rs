//! AIFF-C §14 chunk-precedence integration tests.
//!
//! Doc reference: `docs/audio/aiff/aiff-c.txt` §14 ("Chunk
//! Precedence"), lines 1209–1259 of the staged spec text.

use oxideav_iff::aiff::{parse, ChunkClass};

/// Pack a chunk: ckID + ckSize + data + (pad byte if odd-sized).
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

/// Build the 10-byte 80-bit IEEE-extended encoding §4.0 requires for
/// the COMM `sampleRate` field. Mirrors the test helper used in
/// `crates/oxideav-iff/src/aiff/form.rs`.
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

fn comm_body_aiff(channels: u16, frames: u32, bits: u16, rate: f64) -> Vec<u8> {
    let mut comm = Vec::new();
    comm.extend_from_slice(&(channels as i16).to_be_bytes());
    comm.extend_from_slice(&frames.to_be_bytes());
    comm.extend_from_slice(&(bits as i16).to_be_bytes());
    comm.extend_from_slice(&ext(rate));
    comm
}

fn ssnd_body(samples: &[u8]) -> Vec<u8> {
    let mut ssnd = Vec::new();
    ssnd.extend_from_slice(&0_u32.to_be_bytes()); // offset
    ssnd.extend_from_slice(&0_u32.to_be_bytes()); // blockSize
    ssnd.extend_from_slice(samples);
    ssnd
}

fn wrap_form(form_type: &[u8; 4], inner_chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.extend_from_slice(form_type);
    for c in inner_chunks {
        inner.extend_from_slice(c);
    }
    let mut file = Vec::new();
    file.extend_from_slice(b"FORM");
    file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    file.extend_from_slice(&inner);
    file
}

#[test]
fn minimal_aiff_orders_common_above_sound_data() {
    // A FORM AIFF with only COMM + SSND must yield the precedence
    // sequence [Common, SoundData] — §14 ¶ Common-above-SSND.
    let file = wrap_form(
        b"AIFF",
        &[
            pack(b"COMM", &comm_body_aiff(1, 2, 16, 44_100.0)),
            pack(b"SSND", &ssnd_body(&[0u8; 4])),
        ],
    );
    let parsed = parse(&file).unwrap();
    let order = parsed.precedence_order();
    assert_eq!(order, vec![ChunkClass::Common, ChunkClass::SoundData]);
    assert_eq!(parsed.highest_precedence_class(), Some(ChunkClass::Common));
}

#[test]
fn aifc_with_fver_orders_format_version_first() {
    // §3.1 FVER precedes the §14 ranked block (it's the "which
    // draft does this FORM follow?" sentinel).
    let mut comm = comm_body_aiff(1, 2, 16, 44_100.0);
    // AIFF-C COMM extends with a 4-byte compressionType + pstring.
    comm.extend_from_slice(b"NONE");
    let name = "not compressed";
    comm.push(name.len() as u8);
    comm.extend_from_slice(name.as_bytes());
    if (1 + name.len()) % 2 == 1 {
        comm.push(0);
    }
    let fver = 0xA280_5140_u32.to_be_bytes();
    let file = wrap_form(
        b"AIFC",
        &[
            pack(b"FVER", &fver),
            pack(b"COMM", &comm),
            pack(b"SSND", &ssnd_body(&[0u8; 4])),
        ],
    );
    let parsed = parse(&file).unwrap();
    let order = parsed.precedence_order();
    assert_eq!(
        order,
        vec![
            ChunkClass::FormatVersion,
            ChunkClass::Common,
            ChunkClass::SoundData,
        ]
    );
    assert_eq!(
        parsed.highest_precedence_class(),
        Some(ChunkClass::FormatVersion)
    );
}

/// Build a MARK body containing a single marker at frame 0 with id 1
/// and an empty pstring name (mirrors `tests/aiff_markers.rs`).
fn mark_one_marker() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1_u16.to_be_bytes()); // numMarkers
    body.extend_from_slice(&1_i16.to_be_bytes()); // id
    body.extend_from_slice(&0_u32.to_be_bytes()); // position
    body.push(0); // pstring length 0
    body.push(0); // pad to even
    body
}

/// Build an INST body matching `tests/aiff_instrument.rs` defaults.
fn inst_default() -> Vec<u8> {
    // 6 leading bytes: baseNote, detune, lowNote, highNote,
    // lowVelocity, highVelocity. Then gain (i16) + sustainLoop
    // (playMode + beginLoop + endLoop = 3 × i16) + releaseLoop
    // (3 × i16).
    let mut body = vec![60u8, 0, 0, 127, 1, 127];
    body.extend_from_slice(&0_i16.to_be_bytes()); // gain
    body.extend_from_slice(&0_i16.to_be_bytes()); // sustainLoop.playMode
    body.extend_from_slice(&0_i16.to_be_bytes()); // sustainLoop.beginLoop
    body.extend_from_slice(&0_i16.to_be_bytes()); // sustainLoop.endLoop
    body.extend_from_slice(&0_i16.to_be_bytes()); // releaseLoop.playMode
    body.extend_from_slice(&0_i16.to_be_bytes()); // releaseLoop.beginLoop
    body.extend_from_slice(&0_i16.to_be_bytes()); // releaseLoop.endLoop
    body
}

#[test]
fn full_form_emits_all_classes_in_spec_order_even_when_wire_order_is_shuffled() {
    // Build a FORM where the on-wire chunk order deliberately
    // contradicts §14 — APPL before MIDI before AESD before ANNO
    // before COMM. The precedence_order helper must still report
    // the §14 ordering, NOT the on-wire ordering. (§14 is about
    // information precedence, not file layout — §4 of the staged
    // AIFF-AIFC layout doc is explicit that chunk order inside a
    // FORM is unspecified.)
    let mut comm = comm_body_aiff(1, 2, 16, 44_100.0);
    comm.extend_from_slice(b"NONE");
    let name = "not compressed";
    comm.push(name.len() as u8);
    comm.extend_from_slice(name.as_bytes());
    if (1 + name.len()) % 2 == 1 {
        comm.push(0);
    }

    let appl_body = {
        // §12 APPL: 4-byte signature + body bytes.
        let mut b = Vec::new();
        b.extend_from_slice(b"stoc");
        b.push(3); // pstring length
        b.extend_from_slice(b"abc");
        b.push(0); // pad to even
        b
    };
    let midi_body: Vec<u8> = vec![0xF0, 0x7E, 0x00, 0xF7]; // SysEx universal non-realtime
    let aesd_body: Vec<u8> = vec![0u8; 24]; // §11 AES channel-status block
    let anno_body = b"first annotation".to_vec();
    let anno_body2 = b"second annotation".to_vec();
    let name_body = b"piano hit".to_vec();
    let auth_body = b"author A".to_vec();
    let copyright_body = b"2026 Test".to_vec();
    let comt_body = {
        // §7 COMT: numComments(u16) + per-comment timestamp(u32) +
        // markerId(i16) + count(u16) + text+pad. One zero-text
        // comment.
        let mut b = Vec::new();
        b.extend_from_slice(&1_u16.to_be_bytes()); // numComments
        b.extend_from_slice(&0_u32.to_be_bytes()); // timestamp
        b.extend_from_slice(&0_i16.to_be_bytes()); // markerId 0 == none
        b.extend_from_slice(&0_u16.to_be_bytes()); // count 0
        b
    };
    let saxel_body = {
        // §8.0 SAXL: numSaxels(u16) + per-saxel id(i16) +
        // size(u16) + data. One zero-data saxel.
        let mut b = Vec::new();
        b.extend_from_slice(&1_u16.to_be_bytes()); // numSaxels
        b.extend_from_slice(&1_i16.to_be_bytes()); // id
        b.extend_from_slice(&0_u16.to_be_bytes()); // size
        b
    };
    let fver = 0xA280_5140_u32.to_be_bytes();

    // Deliberately scrambled on-wire order.
    let file = wrap_form(
        b"AIFC",
        &[
            pack(b"APPL", &appl_body),
            pack(b"MIDI", &midi_body),
            pack(b"AESD", &aesd_body),
            pack(b"ANNO", &anno_body),
            pack(b"SAXL", &saxel_body),
            pack(b"COMT", &comt_body),
            pack(b"INST", &inst_default()),
            pack(b"MARK", &mark_one_marker()),
            pack(b"SSND", &ssnd_body(&[0u8; 4])),
            pack(b"NAME", &name_body),
            pack(b"AUTH", &auth_body),
            pack(b"(c) ", &copyright_body),
            pack(b"ANNO", &anno_body2),
            pack(b"FVER", &fver),
            pack(b"COMM", &comm),
        ],
    );
    let parsed = parse(&file).unwrap();
    let order = parsed.precedence_order();
    // §14 sequence with one entry per present class. Note ANNO
    // appears twice (§14 ¶ "Annotation Chunk[s] -- in the order
    // they appear in the FORM").
    assert_eq!(
        order,
        vec![
            ChunkClass::FormatVersion,
            ChunkClass::Common,
            ChunkClass::Instrument,
            ChunkClass::Saxel,
            ChunkClass::Comments,
            ChunkClass::Marker,
            ChunkClass::SoundData,
            ChunkClass::Name,
            ChunkClass::Author,
            ChunkClass::Copyright,
            ChunkClass::Annotation,
            ChunkClass::Annotation,
            ChunkClass::AudioRecording,
            ChunkClass::MidiData,
            ChunkClass::ApplicationSpecific,
        ]
    );
}

#[test]
fn multi_instance_classes_repeat_per_document_order_entry() {
    // §14 ¶ "Annotation Chunk[s] -- in the order they appear in the
    // FORM" + §8.0/§10.0/§12.0 ¶ "any number of …" mean each
    // instance shows up once in precedence_order. Build a FORM with
    // 3 ANNO + 2 APPL + 2 MIDI + 2 SAXL and check the multiplicities.
    let mut comm = comm_body_aiff(1, 2, 16, 44_100.0);
    comm.extend_from_slice(b"NONE");
    let name = "not compressed";
    comm.push(name.len() as u8);
    comm.extend_from_slice(name.as_bytes());
    if (1 + name.len()) % 2 == 1 {
        comm.push(0);
    }
    let appl_body = {
        let mut b = Vec::new();
        b.extend_from_slice(b"stoc");
        b.push(3);
        b.extend_from_slice(b"abc");
        b.push(0);
        b
    };
    let midi_body: Vec<u8> = vec![0xF0, 0x7E, 0x00, 0xF7];
    let saxel_body = {
        let mut b = Vec::new();
        b.extend_from_slice(&1_u16.to_be_bytes());
        b.extend_from_slice(&1_i16.to_be_bytes());
        b.extend_from_slice(&0_u16.to_be_bytes());
        b
    };
    let file = wrap_form(
        b"AIFC",
        &[
            pack(b"FVER", &0xA280_5140_u32.to_be_bytes()),
            pack(b"COMM", &comm),
            pack(b"SSND", &ssnd_body(&[0u8; 4])),
            pack(b"ANNO", b"a1"),
            pack(b"ANNO", b"a2"),
            pack(b"ANNO", b"a3"),
            pack(b"APPL", &appl_body),
            pack(b"APPL", &appl_body),
            pack(b"MIDI", &midi_body),
            pack(b"MIDI", &midi_body),
            pack(b"SAXL", &saxel_body),
            pack(b"SAXL", &saxel_body),
        ],
    );
    let parsed = parse(&file).unwrap();
    let order = parsed.precedence_order();
    // Count occurrences of each multi-instance class.
    let n_anno = order
        .iter()
        .filter(|c| **c == ChunkClass::Annotation)
        .count();
    let n_appl = order
        .iter()
        .filter(|c| **c == ChunkClass::ApplicationSpecific)
        .count();
    let n_midi = order.iter().filter(|c| **c == ChunkClass::MidiData).count();
    let n_saxl = order.iter().filter(|c| **c == ChunkClass::Saxel).count();
    assert_eq!(n_anno, 3, "§13 ¶ 3 ANNO chunks");
    assert_eq!(n_appl, 2, "§12 ¶ 2 APPL chunks");
    assert_eq!(n_midi, 2, "§10 ¶ 2 MIDI chunks");
    assert_eq!(n_saxl, 2, "§8 ¶ 2 SAXL chunks");
}

#[test]
fn instrument_outranks_midi_per_section_14_example() {
    // §14 ¶ "the loop points in the Instrument Chunk take
    // precedence over conflicting loop points found in the MIDI
    // Data Chunk." Build a FORM that carries both, then confirm
    // Instrument's rank is numerically lower.
    let mut comm = comm_body_aiff(1, 2, 16, 44_100.0);
    comm.extend_from_slice(b"NONE");
    let name = "not compressed";
    comm.push(name.len() as u8);
    comm.extend_from_slice(name.as_bytes());
    if (1 + name.len()) % 2 == 1 {
        comm.push(0);
    }
    let file = wrap_form(
        b"AIFC",
        &[
            pack(b"FVER", &0xA280_5140_u32.to_be_bytes()),
            pack(b"COMM", &comm),
            pack(b"INST", &inst_default()),
            pack(b"MIDI", &[0xF0u8, 0xF7]),
            pack(b"SSND", &ssnd_body(&[0u8; 4])),
        ],
    );
    let parsed = parse(&file).unwrap();
    let order = parsed.precedence_order();
    let inst_pos = order.iter().position(|c| *c == ChunkClass::Instrument);
    let midi_pos = order.iter().position(|c| *c == ChunkClass::MidiData);
    assert!(inst_pos.is_some());
    assert!(midi_pos.is_some());
    assert!(
        inst_pos.unwrap() < midi_pos.unwrap(),
        "§14: Instrument must precede MIDI in precedence_order"
    );
    assert!(ChunkClass::Instrument.higher_precedence_than(ChunkClass::MidiData));
}

#[test]
fn highest_precedence_is_format_version_when_fver_present() {
    let mut comm = comm_body_aiff(1, 2, 16, 44_100.0);
    comm.extend_from_slice(b"NONE");
    let n = "not compressed";
    comm.push(n.len() as u8);
    comm.extend_from_slice(n.as_bytes());
    if (1 + n.len()) % 2 == 1 {
        comm.push(0);
    }
    let file = wrap_form(
        b"AIFC",
        &[
            pack(b"FVER", &0xA280_5140_u32.to_be_bytes()),
            pack(b"COMM", &comm),
            pack(b"SSND", &ssnd_body(&[0u8; 2])),
        ],
    );
    let parsed = parse(&file).unwrap();
    assert_eq!(
        parsed.highest_precedence_class(),
        Some(ChunkClass::FormatVersion)
    );
}

#[test]
fn highest_precedence_is_common_when_no_fver_present() {
    let file = wrap_form(
        b"AIFF",
        &[
            pack(b"COMM", &comm_body_aiff(1, 2, 16, 44_100.0)),
            pack(b"SSND", &ssnd_body(&[0u8; 2])),
        ],
    );
    let parsed = parse(&file).unwrap();
    assert_eq!(parsed.highest_precedence_class(), Some(ChunkClass::Common));
}
