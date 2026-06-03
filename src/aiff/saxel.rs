//! `SAXL` (Sound Accelerator) chunk parser — per-marker decompressor
//! priming data.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §8.0 (Sound Accelerator [Saxel]
//! Chunk) and Appendix D (which carries the full data-layout
//! definition the §8.0 prose defers to) the wire layout is:
//!
//! ```text
//! ckID       : 'SAXL'
//! ckSize     : long          (byte length of the body)
//! numSaxels  : unsigned short (big-endian; count of Saxel entries)
//! saxels[]   : Saxel repeated numSaxels times
//! ```
//!
//! Each `Saxel` is:
//!
//! ```text
//! id         : MarkerId (i16)  (links the accelerator data to a marker)
//! size       : unsigned short  (length of saxelData, in bytes)
//! saxelData  : char[size]      (compression-type-specific priming data;
//!                              padded with one byte at the end as needed
//!                              to make the per-saxel total an even byte
//!                              count, and the pad byte is NOT included
//!                              in `size`)
//! ```
//!
//! Appendix D quotes the relevant invariants:
//!
//! * "id identifies the marker for which the sound accelerator data is
//!   to be used."
//! * "size indicates the length in bytes of the sound accelerator data,
//!   saxelData. The data must be padded with a byte at the end as
//!   needed to make it an even number of bytes long. This pad byte, if
//!   present, is not included in size."
//! * "saxelData contains the specific sound accelerator data which is
//!   compression-type specific."
//! * "numSaxels is the number of saxels in the Saxel Chunk. Multiple
//!   Saxel Chunks are allowed in a single FORM AIFC file."
//! * "Since each saxel occupies an even number of bytes, the saxels
//!   are packed together with no unused bytes between them. The
//!   saxels need not be ordered in any particular manner."
//! * "The Saxel Chunk is optional. Any number of Saxel Chunks may
//!   appear in a FORM AIFC."
//!
//! The chunk's body is the per-saxel header (4 bytes: i16 id + u16
//! size) plus `size` data bytes plus a pad byte when `size` is odd.
//! Per-saxel headers are 4 bytes (even) so the pad is needed iff
//! `size` is odd. The trailing pad on the last saxel in the chunk is
//! tolerated as either present or absent (mirroring the MARK / COMT
//! end-of-chunk pad tolerance for legacy encoders that skipped the
//! tail pad).
//!
//! §8.0 of the spec is explicit that the Saxel mechanism remained
//! "Under Construction" / "rough proposal" status — Appendix D ¶
//! "Caution" reinforces this — so the read path here does NOT try to
//! interpret `saxelData` against any specific compression algorithm.
//! It preserves the raw bytes verbatim and lets the caller decide
//! whether to feed them into a decompressor's state-priming entry
//! point. The `id` field is exposed as the same `i16` `MarkerId`
//! type used by §6.0 MARK + §7.0 COMT linkage, with
//! [`Saxel::resolve_marker`] joining against a [`MarkerChunk`].
//!
//! §8.0 permits "any number of Saxel Chunks" per FORM AIFC (unlike
//! the `MARK` / `INST` / `COMT` / `AESD` chunks which are at-most-one
//! per FORM), so the FORM walker accumulates them in document order
//! into a `Vec<SaxelChunk>` just as it does for `APPL`, `MIDI`, and
//! `ANNO`.

use crate::aiff::error::{AiffError, Result};
use crate::aiff::marker::{Marker, MarkerChunk};

/// A single Saxel entry inside a [`SaxelChunk`].
///
/// Constructed by [`parse_saxel_chunk`]; the raw on-disk `id`, `size`,
/// and `data` fields are preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saxel {
    /// `id` — `MarkerId` of the marker this accelerator data primes
    /// playback for. Per §6.0 a `MarkerId` is a positive non-zero
    /// `i16`; Appendix D ¶ "id" calls this out as a link target and
    /// does not redefine the encoding, so the same rules apply.
    pub id: i16,
    /// `saxelData` — the compression-type-specific priming bytes,
    /// length matches the `size` field (the trailing pad byte, if
    /// any, has been stripped). Opaque to the AIFF layer; the caller
    /// hands these to the decompressor's state-priming entry point.
    pub data: Vec<u8>,
}

impl Saxel {
    /// Number of bytes in `data`. Matches the on-wire `size` field
    /// (does NOT include the pad byte, per Appendix D ¶ "size").
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// `true` when `data` is empty. The spec doesn't forbid an empty
    /// saxelData (size == 0); the per-saxel header is still 4 bytes
    /// and even, so no pad byte is required.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Look the linked marker up inside the supplied [`MarkerChunk`].
    /// Returns `None` when the `id` isn't a positive `MarkerId` per
    /// §6.0, or when no marker in the chunk has that id. Useful for
    /// callers asking "which marker does this saxel prime playback
    /// for?" without re-implementing the §6.0 lookup.
    pub fn resolve_marker<'m>(&self, markers: &'m MarkerChunk) -> Option<&'m Marker> {
        if self.id <= 0 {
            return None;
        }
        markers.by_id(self.id)
    }
}

/// Parsed contents of a single `SAXL` chunk.
///
/// Multiple `SAXL` chunks are legal per §8.0 / Appendix D and the
/// FORM walker accumulates them in document order. Within a single
/// chunk the saxels themselves are also preserved in document order;
/// Appendix D ¶ "The saxels need not be ordered in any particular
/// manner" so we don't re-sort.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SaxelChunk {
    /// Saxels in document order; the spec doesn't impose a sort.
    pub saxels: Vec<Saxel>,
}

impl SaxelChunk {
    /// Look up a saxel by its linked `MarkerId`. Returns `None` when
    /// no saxel in this chunk references the given id. Convenience
    /// for callers that already know which marker they want to start
    /// playback from.
    pub fn by_marker_id(&self, id: i16) -> Option<&Saxel> {
        self.saxels.iter().find(|s| s.id == id)
    }
}

/// Parse the body of a `SAXL` chunk. `data` is the ckData slice the
/// chunk walker handed up — the 8-byte ckID/ckSize prefix and the
/// pad byte (if any) have already been stripped.
///
/// Per Appendix D the body is `numSaxels: u16 BE` followed by
/// `numSaxels` saxel records of `i16 id + u16 size + size bytes +
/// pad-to-even-per-saxel`. End-of-chunk pad on the last saxel is
/// tolerated as either present or absent to match the chunk-walker
/// tolerance.
pub fn parse_saxel_chunk(data: &[u8]) -> Result<SaxelChunk> {
    if data.len() < 2 {
        return Err(AiffError::Truncated("SAXL numSaxels"));
    }
    let num = u16::from_be_bytes([data[0], data[1]]) as usize;

    let mut out = SaxelChunk {
        saxels: Vec::with_capacity(num),
    };

    let mut cursor = 2usize;
    for _ in 0..num {
        // Per-saxel header: 2 (id) + 2 (size) = 4 bytes.
        if cursor.saturating_add(4) > data.len() {
            return Err(AiffError::Truncated("SAXL saxel header"));
        }
        let id = i16::from_be_bytes([data[cursor], data[cursor + 1]]);
        let size = u16::from_be_bytes([data[cursor + 2], data[cursor + 3]]) as usize;
        cursor += 4;
        if cursor.saturating_add(size) > data.len() {
            return Err(AiffError::Truncated("SAXL saxelData"));
        }
        let body = data[cursor..cursor + size].to_vec();
        cursor += size;
        // Appendix D: "The data must be padded with a byte at the end
        // as needed to make it an even number of bytes long. This pad
        // byte, if present, is not included in size." The per-saxel
        // header is 4 bytes (even), so the pad is needed iff `size`
        // is odd. Tolerate a missing pad at end-of-chunk to match the
        // MARK / COMT tail-pad tolerance.
        if size % 2 == 1 && cursor < data.len() {
            cursor += 1;
        }

        out.saxels.push(Saxel { id, data: body });
    }

    Ok(out)
}

/// Encode a [`SaxelChunk`] body in wire format — the bytes that
/// would follow a `SAXL` chunk header. Each saxel is laid out as
/// `id(2) + size(2) + data + pad-to-even-when-size-odd`.
///
/// Useful for round-tripping SAXL chunks through write-side
/// container encoders; the chunk header itself (`'SAXL' + ckSize`)
/// is the caller's responsibility.
pub fn write_saxel_chunk(c: &SaxelChunk) -> Vec<u8> {
    let mut out = Vec::new();
    // numSaxels is `unsigned short` — cap silently at u16::MAX. A
    // saxel chunk with more than 65,535 entries would be unusable in
    // practice and the encoder convention everywhere else in this
    // crate is to truncate rather than fail.
    let n = c.saxels.len().min(u16::MAX as usize);
    out.extend_from_slice(&(n as u16).to_be_bytes());
    for s in c.saxels.iter().take(n) {
        write_one_saxel(&mut out, s);
    }
    out
}

fn write_one_saxel(out: &mut Vec<u8>, s: &Saxel) {
    out.extend_from_slice(&s.id.to_be_bytes());
    // `size` is `unsigned short` on the wire — cap silently at
    // u16::MAX. Real-world Apple ACE/MAC saxelData runs are ~48
    // sample frames, well under the cap.
    let size = s.data.len().min(u16::MAX as usize);
    out.extend_from_slice(&(size as u16).to_be_bytes());
    out.extend_from_slice(&s.data[..size]);
    // Pad byte to keep the per-saxel total even (Appendix D ¶
    // "padded with a byte at the end as needed").
    if size % 2 == 1 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack a single saxel: id + size + data + pad-if-odd.
    fn pack_one(id: i16, data: &[u8]) -> Vec<u8> {
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
    fn pack_chunk(saxels: &[(i16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(saxels.len() as u16).to_be_bytes());
        for (id, data) in saxels {
            body.extend_from_slice(&pack_one(*id, data));
        }
        body
    }

    #[test]
    fn parses_empty_saxel_list() {
        // numSaxels = 0, no saxel records.
        let body = pack_chunk(&[]);
        let c = parse_saxel_chunk(&body).unwrap();
        assert!(c.saxels.is_empty());
    }

    #[test]
    fn parses_single_saxel_with_even_data() {
        // 48-byte saxelData (the Appendix D ¶ "Saxels for ACE and
        // Macintosh compressed sound data" recommended length).
        let data: Vec<u8> = (0..48u8).collect();
        let body = pack_chunk(&[(1, &data)]);
        let c = parse_saxel_chunk(&body).unwrap();
        assert_eq!(c.saxels.len(), 1);
        assert_eq!(c.saxels[0].id, 1);
        assert_eq!(c.saxels[0].data, data);
        assert_eq!(c.saxels[0].len(), 48);
        assert!(!c.saxels[0].is_empty());
    }

    #[test]
    fn parses_single_saxel_with_odd_data() {
        // Odd-length saxelData forces the per-saxel pad byte.
        let data = [0xDE_u8, 0xAD, 0xBE, 0xEF, 0x42];
        let body = pack_chunk(&[(7, &data)]);
        // Sanity: pack_one inserts one pad byte.
        assert_eq!(
            body.len(),
            2 /* numSaxels */ + 4 /* hdr */ + 5 /* data */ + 1 /* pad */
        );
        let c = parse_saxel_chunk(&body).unwrap();
        assert_eq!(c.saxels[0].id, 7);
        assert_eq!(c.saxels[0].data, &data);
    }

    #[test]
    fn parses_empty_saxel_data() {
        // size == 0 is legal per the spec — the per-saxel block is
        // just the 4-byte header.
        let body = pack_chunk(&[(3, &[])]);
        let c = parse_saxel_chunk(&body).unwrap();
        assert_eq!(c.saxels[0].id, 3);
        assert!(c.saxels[0].is_empty());
    }

    #[test]
    fn parses_multiple_saxels_in_document_order() {
        // Appendix D ¶ "The saxels need not be ordered in any
        // particular manner" — we preserve whatever order the
        // encoder wrote.
        let a = vec![1u8, 2, 3, 4];
        let b = vec![0xAA_u8, 0xBB];
        let c_data = vec![9u8, 9, 9];
        let body = pack_chunk(&[(5, &a), (1, &b), (3, &c_data)]);
        let parsed = parse_saxel_chunk(&body).unwrap();
        assert_eq!(parsed.saxels.len(), 3);
        assert_eq!(parsed.saxels[0].id, 5);
        assert_eq!(parsed.saxels[0].data, a);
        assert_eq!(parsed.saxels[1].id, 1);
        assert_eq!(parsed.saxels[1].data, b);
        assert_eq!(parsed.saxels[2].id, 3);
        assert_eq!(parsed.saxels[2].data, c_data);
    }

    #[test]
    fn parses_multiple_saxels_with_mixed_pad() {
        // odd / even / odd payloads exercise the inter-saxel pad
        // handling — each odd-size payload has a single pad byte
        // before the next saxel header.
        let body = pack_chunk(&[
            (1, &[0xA1]),
            (2, &[0xB1, 0xB2]),
            (3, &[0xC1, 0xC2, 0xC3]),
            (4, &[0xD1, 0xD2, 0xD3, 0xD4]),
        ]);
        let parsed = parse_saxel_chunk(&body).unwrap();
        assert_eq!(parsed.saxels.len(), 4);
        assert_eq!(parsed.saxels[0].data, &[0xA1]);
        assert_eq!(parsed.saxels[1].data, &[0xB1, 0xB2]);
        assert_eq!(parsed.saxels[2].data, &[0xC1, 0xC2, 0xC3]);
        assert_eq!(parsed.saxels[3].data, &[0xD1, 0xD2, 0xD3, 0xD4]);
    }

    #[test]
    fn by_marker_id_finds_saxel() {
        let body = pack_chunk(&[(1, &[0x11]), (2, &[0x22]), (3, &[0x33])]);
        let c = parse_saxel_chunk(&body).unwrap();
        assert_eq!(c.by_marker_id(2).unwrap().data, &[0x22]);
        assert_eq!(c.by_marker_id(3).unwrap().data, &[0x33]);
        assert!(c.by_marker_id(99).is_none());
    }

    #[test]
    fn resolve_marker_returns_some_for_valid_link() {
        let body = pack_chunk(&[(7, &[0x42; 48])]);
        let c = parse_saxel_chunk(&body).unwrap();
        let markers = MarkerChunk {
            markers: vec![Marker {
                id: 7,
                position: 1024,
                name: "loop start".into(),
            }],
        };
        let m = c.saxels[0].resolve_marker(&markers).unwrap();
        assert_eq!(m.position, 1024);
        assert_eq!(m.name, "loop start");
    }

    #[test]
    fn resolve_marker_returns_none_when_id_missing() {
        let body = pack_chunk(&[(99, &[0x00])]);
        let c = parse_saxel_chunk(&body).unwrap();
        let markers = MarkerChunk {
            markers: vec![Marker {
                id: 1,
                position: 0,
                name: "x".into(),
            }],
        };
        assert!(c.saxels[0].resolve_marker(&markers).is_none());
    }

    #[test]
    fn resolve_marker_returns_none_for_zero_or_negative_id() {
        // §6.0: MarkerId must be > 0. A saxel with id == 0 or id < 0
        // is a degenerate / mis-encoded link and resolve_marker must
        // not attempt the lookup.
        let body = pack_chunk(&[(0, &[]), (-1, &[])]);
        let c = parse_saxel_chunk(&body).unwrap();
        let markers = MarkerChunk {
            markers: vec![Marker {
                id: 1,
                position: 0,
                name: "x".into(),
            }],
        };
        assert!(c.saxels[0].resolve_marker(&markers).is_none());
        assert!(c.saxels[1].resolve_marker(&markers).is_none());
    }

    #[test]
    fn rejects_truncated_num_saxels() {
        assert!(matches!(
            parse_saxel_chunk(&[]),
            Err(AiffError::Truncated("SAXL numSaxels"))
        ));
        assert!(matches!(
            parse_saxel_chunk(&[0x00]),
            Err(AiffError::Truncated("SAXL numSaxels"))
        ));
    }

    #[test]
    fn rejects_truncated_saxel_header() {
        // numSaxels=1 followed by only 3 bytes of header (need 4).
        let body = [0x00, 0x01, 0x00, 0x01, 0x00];
        assert!(matches!(
            parse_saxel_chunk(&body),
            Err(AiffError::Truncated("SAXL saxel header"))
        ));
    }

    #[test]
    fn rejects_truncated_saxel_data() {
        // numSaxels=1, id=1, size=8, but only 4 data bytes follow.
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes()); // numSaxels
        body.extend_from_slice(&1i16.to_be_bytes()); // id
        body.extend_from_slice(&8u16.to_be_bytes()); // size
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // only 4 of 8 bytes
        assert!(matches!(
            parse_saxel_chunk(&body),
            Err(AiffError::Truncated("SAXL saxelData"))
        ));
    }

    #[test]
    fn tolerates_missing_pad_on_last_saxel() {
        // Build an odd-size last saxel but DON'T append the trailing
        // pad byte (mirroring an encoder that elided the end-of-chunk
        // pad). The chunk walker tolerates this for MARK / COMT and
        // we mirror that behaviour here.
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes()); // numSaxels
        body.extend_from_slice(&5i16.to_be_bytes()); // id
        body.extend_from_slice(&3u16.to_be_bytes()); // size = 3 (odd)
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        // No trailing pad.
        let parsed = parse_saxel_chunk(&body).unwrap();
        assert_eq!(parsed.saxels[0].id, 5);
        assert_eq!(parsed.saxels[0].data, &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn write_round_trips_through_parse() {
        let original = SaxelChunk {
            saxels: vec![
                Saxel {
                    id: 1,
                    data: (0..48u8).collect(),
                },
                Saxel {
                    id: 2,
                    data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x42],
                },
                Saxel {
                    id: 3,
                    data: vec![],
                },
                Saxel {
                    id: 4,
                    data: vec![0x11, 0x22, 0x33],
                },
            ],
        };
        let bytes = write_saxel_chunk(&original);
        let parsed = parse_saxel_chunk(&bytes).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn write_empty_chunk() {
        let c = SaxelChunk::default();
        let bytes = write_saxel_chunk(&c);
        // Just numSaxels = 0.
        assert_eq!(bytes, vec![0, 0]);
        let parsed = parse_saxel_chunk(&bytes).unwrap();
        assert!(parsed.saxels.is_empty());
    }

    #[test]
    fn write_matches_hand_packed_layout() {
        // Verify the byte-for-byte layout against pack_chunk.
        let original = SaxelChunk {
            saxels: vec![
                Saxel {
                    id: 1,
                    data: vec![0xAA, 0xBB],
                },
                Saxel {
                    id: 2,
                    data: vec![0xCC, 0xDD, 0xEE],
                },
            ],
        };
        let bytes = write_saxel_chunk(&original);
        let expected = pack_chunk(&[(1, &[0xAA, 0xBB]), (2, &[0xCC, 0xDD, 0xEE])]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn write_preserves_document_order() {
        // Saxels written in document order, not sorted by id.
        let original = SaxelChunk {
            saxels: vec![
                Saxel {
                    id: 9,
                    data: vec![0x09],
                },
                Saxel {
                    id: 1,
                    data: vec![0x01],
                },
                Saxel {
                    id: 5,
                    data: vec![0x05],
                },
            ],
        };
        let bytes = write_saxel_chunk(&original);
        let parsed = parse_saxel_chunk(&bytes).unwrap();
        let ids: Vec<i16> = parsed.saxels.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![9, 1, 5]);
    }
}
