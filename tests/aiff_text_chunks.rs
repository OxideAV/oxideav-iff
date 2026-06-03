//! Integration tests for §13.0 AIFF text chunks
//! (NAME / AUTH / `(c) ` / ANNO).
//!
//! Exercises the structured [`Form::name`] / [`Form::author`] /
//! [`Form::copyright`] / [`Form::annotations`] surfaces end-to-end
//! through [`oxideav_iff::aiff::parse`], plus the standalone
//! [`parse_text_chunk`] / [`write_text_chunk`] helpers callers can use
//! when building or interrogating a text chunk in isolation.

use oxideav_iff::aiff::{
    parse, parse_text_chunk, write_text_chunk, AiffError, TextChunk, TextKind,
};

/// Tiny 80-bit IEEE-extended encoding helper for the 44100 Hz tests
/// below. Produces the same 10-byte field the internal form tests use.
fn ext44100() -> [u8; 10] {
    // 44100 -> exponent 0x400E, mantissa 0xAC44 0000 0000 0000.
    let mut o = [0u8; 10];
    o[0] = 0x40;
    o[1] = 0x0E;
    o[2] = 0xAC;
    o[3] = 0x44;
    o
}

/// Pack a chunk: ckID + ckSize + data + (pad byte if odd length).
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

/// Build a minimal mono 16-bit/44.1 kHz AIFF wrapper with extra
/// chunks injected in document order between COMM and SSND.
fn build_aiff_with_extras(extras: &[Vec<u8>]) -> Vec<u8> {
    let pcm = [0x12_u8, 0x34];
    let mut comm_body = Vec::new();
    comm_body.extend_from_slice(&1_i16.to_be_bytes()); // numChannels
    comm_body.extend_from_slice(&1_u32.to_be_bytes()); // numSampleFrames
    comm_body.extend_from_slice(&16_i16.to_be_bytes()); // sampleSize
    comm_body.extend_from_slice(&ext44100());

    let mut ssnd_body = Vec::new();
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes()); // offset
    ssnd_body.extend_from_slice(&0_u32.to_be_bytes()); // blockSize
    ssnd_body.extend_from_slice(&pcm);

    let mut inner = Vec::new();
    inner.extend_from_slice(b"AIFF");
    inner.extend_from_slice(&pack(b"COMM", &comm_body));
    for e in extras {
        inner.extend_from_slice(e);
    }
    inner.extend_from_slice(&pack(b"SSND", &ssnd_body));

    let mut f = Vec::new();
    f.extend_from_slice(b"FORM");
    f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    f.extend_from_slice(&inner);
    f
}

#[test]
fn standalone_parse_round_trips_name() {
    // §13.0 standalone helper: TextChunk -> bytes -> TextChunk for a
    // freshly-built NAME body.
    let body = b"Helicopter take-off".to_vec();
    let c = TextChunk {
        kind: TextKind::Name,
        text: body.clone(),
    };
    let bytes = write_text_chunk(&c);
    assert_eq!(bytes, body);
    let back = parse_text_chunk(TextKind::Name, &bytes).unwrap();
    assert_eq!(back, c);
}

#[test]
fn standalone_parse_accepts_empty_body() {
    // §13.0 doesn't impose a minimum ckDataSize.
    let parsed = parse_text_chunk(TextKind::Annotation, &[]).unwrap();
    assert!(parsed.is_empty());
    assert_eq!(parsed.kind, TextKind::Annotation);
}

#[test]
fn form_surfaces_all_four_text_kinds_in_one_file() {
    let file = build_aiff_with_extras(&[
        pack(b"NAME", b"Helicopter take-off"),
        pack(b"AUTH", b"sound designer"),
        pack(b"(c) ", b"1991 Apple Computer, Inc."),
        pack(b"ANNO", b"recorded with mic A"),
        pack(b"ANNO", b"recorded with mic B"),
    ]);
    let parsed = parse(&file).unwrap();
    assert_eq!(
        parsed.name.as_ref().unwrap().as_str(),
        Some("Helicopter take-off")
    );
    assert_eq!(
        parsed.author.as_ref().unwrap().as_str(),
        Some("sound designer")
    );
    assert_eq!(
        parsed.copyright.as_ref().unwrap().as_str(),
        Some("1991 Apple Computer, Inc.")
    );
    assert_eq!(parsed.annotations.len(), 2);
    assert_eq!(parsed.annotations[0].as_str(), Some("recorded with mic A"));
    assert_eq!(parsed.annotations[1].as_str(), Some("recorded with mic B"));
}

#[test]
fn form_without_any_text_chunks_has_empty_surfaces() {
    let file = build_aiff_with_extras(&[]);
    let parsed = parse(&file).unwrap();
    assert!(parsed.name.is_none());
    assert!(parsed.author.is_none());
    assert!(parsed.copyright.is_none());
    assert!(parsed.annotations.is_empty());
}

#[test]
fn form_rejects_duplicate_name_chunk() {
    let file = build_aiff_with_extras(&[pack(b"NAME", b"first"), pack(b"NAME", b"second")]);
    assert!(matches!(
        parse(&file),
        Err(AiffError::DuplicateChunk("NAME"))
    ));
}

#[test]
fn form_rejects_duplicate_author_chunk() {
    let file = build_aiff_with_extras(&[pack(b"AUTH", b"alice"), pack(b"AUTH", b"bob")]);
    assert!(matches!(
        parse(&file),
        Err(AiffError::DuplicateChunk("AUTH"))
    ));
}

#[test]
fn form_rejects_duplicate_copyright_chunk() {
    let file = build_aiff_with_extras(&[pack(b"(c) ", b"1991"), pack(b"(c) ", b"1992")]);
    assert!(matches!(
        parse(&file),
        Err(AiffError::DuplicateChunk("(c) "))
    ));
}

#[test]
fn form_accepts_many_annotation_chunks_in_document_order() {
    let file = build_aiff_with_extras(&[
        pack(b"ANNO", b"alpha"),
        pack(b"ANNO", b"beta"),
        pack(b"ANNO", b"gamma"),
        pack(b"ANNO", b"delta"),
    ]);
    let parsed = parse(&file).unwrap();
    let bodies: Vec<&str> = parsed
        .annotations
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(bodies, vec!["alpha", "beta", "gamma", "delta"]);
}

#[test]
fn form_handles_odd_length_text_pad_byte_correctly() {
    // The §13.0 text body itself has no inner pad — the chunk walker
    // strips the outer odd-size pad byte. An NAME with a 5-byte
    // text field must round-trip as a 5-byte TextChunk.
    let file = build_aiff_with_extras(&[pack(b"NAME", b"hello")]);
    let parsed = parse(&file).unwrap();
    let n = parsed.name.as_ref().unwrap();
    assert_eq!(n.len(), 5);
    assert_eq!(n.text, b"hello");
}

#[test]
fn form_accepts_empty_text_bodies() {
    let file = build_aiff_with_extras(&[
        pack(b"NAME", b""),
        pack(b"AUTH", b""),
        pack(b"(c) ", b""),
        pack(b"ANNO", b""),
    ]);
    let parsed = parse(&file).unwrap();
    assert!(parsed.name.as_ref().unwrap().is_empty());
    assert!(parsed.author.as_ref().unwrap().is_empty());
    assert!(parsed.copyright.as_ref().unwrap().is_empty());
    assert_eq!(parsed.annotations.len(), 1);
    assert!(parsed.annotations[0].is_empty());
}

#[test]
fn copyright_ck_id_is_lowercase_c_round_bracket_space() {
    // §13.0 calls out the lowercase `c` and trailing space explicitly.
    assert_eq!(TextKind::Copyright.ck_id(), *b"(c) ");
    assert_eq!(TextKind::from_ck_id(b"(c) "), Some(TextKind::Copyright));
    // Common typo variants must not match.
    assert_eq!(TextKind::from_ck_id(b"(C) "), None);
    assert_eq!(TextKind::from_ck_id(b"(c)\x00"), None);
}
