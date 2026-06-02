//! `MARK` chunk parser — named positions inside the sound data.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §6.0 (Marker Chunk) the wire
//! layout is:
//!
//! ```text
//! ckID         : 'MARK'
//! ckSize       : int32
//! numMarkers   : unsigned short      (big-endian)
//! markers[]    : Marker repeated numMarkers times
//! ```
//!
//! and each `Marker` is:
//!
//! ```text
//! id           : short (MarkerId, must be > 0)
//! position     : unsigned long       (big-endian, sample-frame index)
//! markerName   : pstring             (1 length byte + chars +
//!                                     pad-to-even total)
//! ```
//!
//! Markers conceptually fall *between* two sample frames; a marker
//! at `position == 0` precedes the first sample frame, and a marker
//! at `position == numSampleFrames` follows the last sample frame.
//! For compressed AIFF-C streams (`ima4`, `ulaw`, …) `position` is
//! measured in expanded (decoded) sample frames, not in compressed
//! bytes — see §6.0 ¶3.
//!
//! The chunk is optional and the spec forbids more than one MARK per
//! FORM; the FORM walker enforces that via [`AiffError::DuplicateChunk`].
//!
//! Per §6.0 ¶ "Markers" each marker's *id* must be a positive,
//! unique-within-the-FORM integer. We reject `id <= 0`
//! ([`AiffError::InvalidValue`]) and a duplicate `id`
//! ([`AiffError::DuplicateMarkerId`]) — both are encoder bugs the
//! spec explicitly bans.

use crate::aiff::error::{AiffError, Result};

/// A single named position inside the sound data.
///
/// Constructed by [`parse_marker_chunk`]; `id` / `position` are the
/// raw on-disk fields, `name` is the decoded Pascal-string `markerName`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// `MarkerId` (spec: `short`, must be > 0 and unique inside the
    /// FORM). Parser-rejected outside `1..=i16::MAX`.
    pub id: i16,
    /// Sample-frame position. Marker 0 sits before the first frame,
    /// marker `numSampleFrames` sits after the last. For AIFF-C
    /// compressed forms the position is measured in *expanded*
    /// (decoded) frames per §6.0 ¶3.
    pub position: u32,
    /// `markerName` — the human-readable mark name (pstring decoded
    /// as UTF-8 with lossy replacement; empty is valid).
    pub name: String,
}

/// Parsed contents of a `MARK` chunk: a [`numMarkers`]-long list of
/// [`Marker`] entries.
///
/// [`numMarkers`]: https://docs.rs/oxideav-iff/latest/oxideav_iff/aiff/marker/struct.MarkerChunk.html
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkerChunk {
    /// The markers in *document order*. The spec is explicit that
    /// markers "need not be ordered in any particular manner" so we
    /// preserve whatever order the encoder wrote.
    pub markers: Vec<Marker>,
}

impl MarkerChunk {
    /// Look up a marker by its `MarkerId`. Returns `None` when no
    /// marker carries that id; markers are stored in document order
    /// so this is an O(n) scan, but n is bounded by `u16::MAX` and
    /// in practice tiny (loop endpoints, named cue regions).
    pub fn by_id(&self, id: i16) -> Option<&Marker> {
        self.markers.iter().find(|m| m.id == id)
    }
}

/// Parse the body of a `MARK` chunk. `data` is the ckData slice the
/// chunk walker handed up — the 8-byte ckID/ckSize prefix and the
/// pad byte (if any) have already been stripped.
pub fn parse_marker_chunk(data: &[u8]) -> Result<MarkerChunk> {
    if data.len() < 2 {
        return Err(AiffError::Truncated("MARK numMarkers"));
    }
    let num = u16::from_be_bytes([data[0], data[1]]) as usize;

    // Empty list is legal — numMarkers may be zero per §6.0
    // ("numMarkers, if non-zero, is followed by the markers themselves").
    let mut out = MarkerChunk {
        markers: Vec::with_capacity(num),
    };

    let mut cursor = 2usize;
    for _ in 0..num {
        // Each marker is at least 2 (id) + 4 (position) + 1 (pstring
        // length byte) = 7 bytes.
        if cursor.saturating_add(7) > data.len() {
            return Err(AiffError::Truncated("MARK marker entry"));
        }
        let id_i = i16::from_be_bytes([data[cursor], data[cursor + 1]]);
        if id_i < 1 {
            return Err(AiffError::InvalidValue {
                what: "MarkerId",
                value: id_i as i64,
            });
        }
        let position = u32::from_be_bytes([
            data[cursor + 2],
            data[cursor + 3],
            data[cursor + 4],
            data[cursor + 5],
        ]);
        let name_off = cursor + 6;
        let (name, advance) = parse_marker_pstring(&data[name_off..])?;
        cursor = name_off + advance;

        // Spec §6.0: every id must be unique inside the FORM.
        if out.markers.iter().any(|m| m.id == id_i) {
            return Err(AiffError::DuplicateMarkerId(id_i));
        }

        out.markers.push(Marker {
            id: id_i,
            position,
            name,
        });
    }

    Ok(out)
}

/// Encode a [`MarkerChunk`] body in wire format — the bytes that
/// would follow a `MARK` chunk header: `numMarkers(2) + markers[]`
/// where each marker is `id(2) + position(4) + pstring + pad-to-even`.
///
/// Useful for round-tripping MARK chunks through write-side
/// container encoders; the chunk header itself (`'MARK' + ckSize`)
/// is the caller's responsibility.
///
/// Per §6.0 the marker name is a Pascal-string limited to 255
/// characters (one-byte length field). A name longer than that is
/// truncated at 255 bytes; the parser will round-trip the truncated
/// form. Document order is preserved verbatim — §6.0 explicitly
/// allows any order so we don't sort or renumber.
///
/// `numMarkers` is `u16` on the wire; lists longer than `u16::MAX`
/// are truncated at `u16::MAX` (consistent with [`super::comment::
/// write_comments_chunk`]).
pub fn write_marker_chunk(m: &MarkerChunk) -> Vec<u8> {
    let count = m.markers.len().min(u16::MAX as usize);
    let mut out = Vec::new();
    out.extend_from_slice(&(count as u16).to_be_bytes());
    for marker in &m.markers[..count] {
        out.extend_from_slice(&marker.id.to_be_bytes());
        out.extend_from_slice(&marker.position.to_be_bytes());
        let name_bytes = marker.name.as_bytes();
        let name_len = name_bytes.len().min(u8::MAX as usize);
        out.push(name_len as u8);
        out.extend_from_slice(&name_bytes[..name_len]);
        // pstring pad: (1 + name_len) must be even per §6.0.
        if (1 + name_len) % 2 == 1 {
            out.push(0);
        }
    }
    out
}

/// Decode a Pascal-string marker name. Returns the decoded name and
/// the number of bytes consumed (including the length byte and the
/// optional pad byte that aligns the total to an even count).
///
/// Per spec §6.0: "all fields in a marker are an even number of
/// bytes in length […] markers are packed together with no unused
/// bytes between them." Combined with the id (2 bytes) + position
/// (4 bytes) preamble, the pstring itself must therefore consume
/// an even number of bytes too: `length_byte + length_chars` rounded
/// up to the nearest even total.
fn parse_marker_pstring(data: &[u8]) -> Result<(String, usize)> {
    if data.is_empty() {
        return Err(AiffError::Truncated("MARK markerName length byte"));
    }
    let len = data[0] as usize;
    if 1 + len > data.len() {
        return Err(AiffError::Truncated("MARK markerName body"));
    }
    let name = String::from_utf8_lossy(&data[1..1 + len]).into_owned();
    // Round up the (length-byte + chars) total to an even count;
    // tolerate a missing pad byte at end-of-chunk so encoders that
    // skipped the trailing pad on the very last marker still parse.
    let raw = 1 + len;
    let consumed = if raw % 2 == 1 {
        if raw < data.len() {
            raw + 1
        } else {
            raw
        }
    } else {
        raw
    };
    Ok((name, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a wire-format marker entry: id + position + pstring,
    /// pad to even total.
    fn pack(id: i16, position: u32, name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&id.to_be_bytes());
        v.extend_from_slice(&position.to_be_bytes());
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
        // pstring pad: (1 + len) must be even.
        if (1 + name.len()) % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn pack_chunk(markers: &[(i16, u32, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(markers.len() as u16).to_be_bytes());
        for (id, pos, name) in markers {
            body.extend_from_slice(&pack(*id, *pos, name));
        }
        body
    }

    #[test]
    fn parses_empty_marker_list() {
        let body = pack_chunk(&[]);
        let m = parse_marker_chunk(&body).unwrap();
        assert!(m.markers.is_empty());
    }

    #[test]
    fn parses_single_marker_with_even_name() {
        // Name "loop" — 4 chars, length byte 4 -> 5 total -> pad to 6.
        let body = pack_chunk(&[(1, 1024, "loop")]);
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.markers.len(), 1);
        assert_eq!(m.markers[0].id, 1);
        assert_eq!(m.markers[0].position, 1024);
        assert_eq!(m.markers[0].name, "loop");
    }

    #[test]
    fn parses_single_marker_with_odd_name() {
        // Name "cue" — 3 chars, length byte 3 -> 4 total, no pad needed.
        let body = pack_chunk(&[(7, 0, "cue")]);
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.markers[0].id, 7);
        assert_eq!(m.markers[0].position, 0);
        assert_eq!(m.markers[0].name, "cue");
    }

    #[test]
    fn parses_empty_marker_name() {
        // pstring of length 0 -> 1 length byte + 1 pad byte.
        let body = pack_chunk(&[(2, 5, "")]);
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.markers[0].id, 2);
        assert_eq!(m.markers[0].position, 5);
        assert_eq!(m.markers[0].name, "");
    }

    #[test]
    fn parses_multiple_markers_in_document_order() {
        // Two markers: id=10 at frame 0, id=20 at frame 44100, plus a
        // third id=5 inserted between them. Order must be preserved.
        let body = pack_chunk(&[(10, 0, "begin"), (5, 22050, "mid"), (20, 44100, "end")]);
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.markers.len(), 3);
        assert_eq!(m.markers[0].id, 10);
        assert_eq!(m.markers[1].id, 5);
        assert_eq!(m.markers[2].id, 20);
    }

    #[test]
    fn by_id_finds_marker() {
        let body = pack_chunk(&[(1, 0, "a"), (2, 10, "b"), (3, 20, "c")]);
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.by_id(2).unwrap().position, 10);
        assert_eq!(m.by_id(3).unwrap().name, "c");
        assert!(m.by_id(99).is_none());
    }

    #[test]
    fn rejects_zero_id() {
        let body = pack_chunk(&[(0, 0, "bad")]);
        assert!(matches!(
            parse_marker_chunk(&body),
            Err(AiffError::InvalidValue {
                what: "MarkerId",
                value: 0
            })
        ));
    }

    #[test]
    fn rejects_negative_id() {
        // pack() takes i16, so -1 packs as 0xFFFF -> i16 -1.
        let body = pack_chunk(&[(-1, 5, "x")]);
        assert!(matches!(
            parse_marker_chunk(&body),
            Err(AiffError::InvalidValue {
                what: "MarkerId",
                value: -1
            })
        ));
    }

    #[test]
    fn rejects_duplicate_id() {
        let body = pack_chunk(&[(7, 0, "a"), (7, 10, "b")]);
        assert!(matches!(
            parse_marker_chunk(&body),
            Err(AiffError::DuplicateMarkerId(7))
        ));
    }

    #[test]
    fn rejects_truncated_num_markers() {
        assert!(matches!(
            parse_marker_chunk(&[0x00]),
            Err(AiffError::Truncated(_))
        ));
        assert!(matches!(
            parse_marker_chunk(&[]),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn rejects_truncated_marker_entry() {
        // Declares 1 marker but the body has only the count + 3 bytes.
        let mut body = 1_u16.to_be_bytes().to_vec();
        body.extend_from_slice(&[0x00, 0x01, 0x00]); // id + 1 byte of position
        assert!(matches!(
            parse_marker_chunk(&body),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn rejects_truncated_pstring_body() {
        // id=1, position=0, name length declared = 10 but only 2 chars follow.
        let mut body = 1_u16.to_be_bytes().to_vec();
        body.extend_from_slice(&1_i16.to_be_bytes());
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.push(10);
        body.extend_from_slice(b"AB");
        assert!(matches!(
            parse_marker_chunk(&body),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn tolerates_missing_pad_on_last_marker() {
        // Manually build a chunk whose final marker has an odd-total
        // pstring and the buffer ends exactly at the last name byte —
        // no trailing pad. The parser must accept it (mirroring the
        // chunk walker's same tolerance for end-of-buffer pad).
        let mut body = 1_u16.to_be_bytes().to_vec();
        body.extend_from_slice(&3_i16.to_be_bytes()); // id
        body.extend_from_slice(&42_u32.to_be_bytes()); // position
        body.push(2); // pstring length
        body.extend_from_slice(b"hi"); // 2 chars; (1+2)=3 odd, but no pad here
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.markers[0].id, 3);
        assert_eq!(m.markers[0].position, 42);
        assert_eq!(m.markers[0].name, "hi");
    }

    #[test]
    fn parses_marker_with_high_position() {
        // u32::MAX is a legal sample-frame index per the spec
        // (unsigned long). Confirm it round-trips through the parser.
        let body = pack_chunk(&[(1, u32::MAX, "max")]);
        let m = parse_marker_chunk(&body).unwrap();
        assert_eq!(m.markers[0].position, u32::MAX);
    }

    #[test]
    fn write_round_trips_through_parse() {
        let chunk = MarkerChunk {
            markers: vec![
                Marker {
                    id: 1,
                    position: 0,
                    name: "begin".into(),
                },
                Marker {
                    id: 2,
                    position: 44_100,
                    name: "end".into(),
                },
                Marker {
                    id: 3,
                    position: 88_200,
                    name: "".into(),
                },
            ],
        };
        let bytes = write_marker_chunk(&chunk);
        let parsed = parse_marker_chunk(&bytes).unwrap();
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn write_empty_chunk() {
        let chunk = MarkerChunk::default();
        let bytes = write_marker_chunk(&chunk);
        assert_eq!(bytes, vec![0, 0]);
        let parsed = parse_marker_chunk(&bytes).unwrap();
        assert_eq!(parsed, chunk);
    }

    #[test]
    fn write_matches_hand_packed_layout() {
        // Hand-pack one marker and confirm write_marker_chunk
        // produces the same bytes.
        let chunk = MarkerChunk {
            markers: vec![Marker {
                id: 7,
                position: 256,
                name: "cue".into(),
            }],
        };
        let bytes = write_marker_chunk(&chunk);
        let expected = pack_chunk(&[(7, 256, "cue")]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn write_preserves_document_order() {
        // §6.0: "markers need not be ordered in any particular manner"
        // — write must preserve whatever the caller passed.
        let chunk = MarkerChunk {
            markers: vec![
                Marker {
                    id: 10,
                    position: 100,
                    name: "z".into(),
                },
                Marker {
                    id: 5,
                    position: 50,
                    name: "a".into(),
                },
                Marker {
                    id: 7,
                    position: 200,
                    name: "m".into(),
                },
            ],
        };
        let bytes = write_marker_chunk(&chunk);
        let parsed = parse_marker_chunk(&bytes).unwrap();
        assert_eq!(parsed.markers[0].id, 10);
        assert_eq!(parsed.markers[1].id, 5);
        assert_eq!(parsed.markers[2].id, 7);
    }
}
