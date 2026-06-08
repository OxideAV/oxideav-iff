//! Integration coverage for the EA IFF 85 §3 universally-reserved
//! ckID classifier (`oxideav_iff::chunk::ReservedId`).
//!
//! §3 ¶ "the following ckIDs are universally reserved to identify
//! chunks with particular IFF meanings: 'LIST', 'FORM', 'PROP',
//! 'CAT ', and '    '. […] The IDs 'LIS1' through 'LIS9', 'FOR1'
//! through 'FOR9', and 'CAT1' through 'CAT9' are reserved for future
//! 'version number' variations." (`docs/image/iff/ea-iff-85.txt`
//! lines 524–531).

use oxideav_iff::chunk::{
    read_chunk_header, skip_chunk_body, ChunkHeader, GroupKind, ReservedId, FILLER_ID, GROUP_CAT,
    GROUP_FORM, GROUP_LIST, PROP_ID,
};
use std::io::Cursor;

#[test]
fn classifier_matches_chunk_header_reserved_for_every_reserved_id() {
    // Round-trip the §3 enumeration through both the free function
    // and the ChunkHeader convenience accessor — they must agree.
    for id in ReservedId::all_reserved_ids() {
        let header = ChunkHeader { id, size: 0 };
        let classified = ReservedId::classify(id);
        assert_eq!(header.reserved(), classified, "id = {:?}", id);
        assert!(
            classified.is_some(),
            "{:?} should classify as a reserved §3 ID",
            std::str::from_utf8(&id).unwrap_or("????"),
        );
    }
}

#[test]
fn classifier_recognises_constants() {
    assert_eq!(
        ReservedId::classify(GROUP_FORM),
        Some(ReservedId::Group(GroupKind::Form))
    );
    assert_eq!(
        ReservedId::classify(GROUP_LIST),
        Some(ReservedId::Group(GroupKind::List))
    );
    assert_eq!(
        ReservedId::classify(GROUP_CAT),
        Some(ReservedId::Group(GroupKind::Cat))
    );
    assert_eq!(ReservedId::classify(PROP_ID), Some(ReservedId::Prop));
    assert_eq!(ReservedId::classify(FILLER_ID), Some(ReservedId::Filler));
}

#[test]
fn reader_skips_a_filler_chunk_before_dispatching_a_form() {
    // End-to-end: a wrapper that pads its IFF stream with a FILLER
    // chunk before the FORM still walks cleanly. The reader must use
    // the §3 filler classification to skip the body without touching
    // it (FILLER has no meaningful contents — §3 ¶ "chunks that fill
    // space but have no meaningful contents").

    // 1) FILLER ckID + ckSize = 8 + 8 bytes of arbitrary padding.
    // 2) FORM ckID + ckSize = 4 + inner type ID "ILBM".
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&FILLER_ID);
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);
    bytes.extend_from_slice(&GROUP_FORM);
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(b"ILBM");

    let mut cur = Cursor::new(bytes);

    // First chunk: the FILLER pad. ReservedId::classify says skip it.
    let h = read_chunk_header(&mut cur).unwrap().unwrap();
    assert_eq!(h.reserved(), Some(ReservedId::Filler));
    assert!(h.is_filler());
    skip_chunk_body(&mut cur, &h).unwrap();

    // Second chunk: the actual FORM/ILBM the consumer was after.
    let h = read_chunk_header(&mut cur).unwrap().unwrap();
    assert_eq!(
        h.reserved(),
        Some(ReservedId::Group(GroupKind::Form)),
        "expected FORM after FILLER, got {:?}",
        std::str::from_utf8(&h.id).unwrap_or("????"),
    );
}

#[test]
fn classifier_routes_future_version_variants_via_parent_group() {
    // §3 future-version family for each parent group. The parent
    // tag preserved on the ReservedFuture variant is what a
    // versioning-aware reader switches on.
    for d in b'1'..=b'9' {
        let mut id = [0u8; 4];
        id.copy_from_slice(&[b'L', b'I', b'S', d]);
        assert_eq!(
            ReservedId::classify(id),
            Some(ReservedId::ReservedFuture {
                parent: GroupKind::List,
                digit: d,
            })
        );

        id.copy_from_slice(&[b'F', b'O', b'R', d]);
        assert_eq!(
            ReservedId::classify(id),
            Some(ReservedId::ReservedFuture {
                parent: GroupKind::Form,
                digit: d,
            })
        );

        id.copy_from_slice(&[b'C', b'A', b'T', d]);
        assert_eq!(
            ReservedId::classify(id),
            Some(ReservedId::ReservedFuture {
                parent: GroupKind::Cat,
                digit: d,
            })
        );
    }
}

#[test]
fn classifier_rejects_form_local_chunk_ids() {
    // Spot-check: the FORM-local data and property chunks across the
    // four shipped forms (ILBM / 8SVX / AIFF / ANIM) must all
    // classify as None — they are not in the §3 universal reserved
    // set, even when sandwich-defined alongside the group IDs.
    for id in [
        // ILBM properties + body.
        *b"BMHD", *b"CMAP", *b"BODY", *b"CAMG", *b"GRAB", *b"DEST", *b"SPRT", *b"CRNG", *b"CCRT",
        *b"DRNG", *b"SHAM", *b"PCHG", // 8SVX properties.
        *b"VHDR", *b"CHAN", *b"ANNO", *b"NAME", *b"AUTH",
        // AIFF / AIFF-C properties.
        *b"COMM", *b"SSND", *b"MARK", *b"INST", *b"COMT", *b"AESD", *b"APPL", *b"MIDI", *b"SAXL",
        *b"FVER", // ANIM properties.
        *b"ANHD", *b"DLTA",
    ] {
        assert_eq!(
            ReservedId::classify(id),
            None,
            "FORM-local id {:?} must not classify as §3-reserved",
            std::str::from_utf8(&id).unwrap_or("????"),
        );
    }
}
