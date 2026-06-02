//! `COMT` (Comments) chunk parser — timestamped annotations.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §7.0 (Comments Chunk) the wire
//! layout is:
//!
//! ```text
//! ckID         : 'COMT'
//! ckSize       : int32
//! numComments  : unsigned short      (big-endian)
//! comments[]   : Comment repeated numComments times
//! ```
//!
//! and each `Comment` is:
//!
//! ```text
//! timeStamp    : unsigned long       (big-endian; seconds since
//!                                     1904-01-01, the classic Mac epoch)
//! marker       : MarkerId (i16)      (0 = comment is not linked to a
//!                                     marker; otherwise references a
//!                                     MARK chunk entry)
//! count        : unsigned short      (number of bytes of `text`)
//! text         : char[count]         (the comment body; padded to an
//!                                     even byte count, pad NOT
//!                                     included in `count`)
//! ```
//!
//! The chunk is optional and §7.0 ¶ "Comments Chunk Format" forbids
//! more than one COMT per FORM; the FORM walker enforces that via
//! [`AiffError::DuplicateChunk`].
//!
//! Per §7.0 ¶ "text": "This text must be padded with a byte at the
//! end as needed to make it an even number of bytes long. This pad
//! byte, if present, is not included in count." The parser honours
//! that — when `1 + 2 + 2 + count` (the per-comment header + text
//! bytes) is odd we step over a one-byte pad before reading the next
//! comment.
//!
//! Per §7.0 ¶ "marker": "A comment can be linked to a marker. […]
//! If the comment is referring to a marker, then marker is the ID of
//! that marker. Otherwise, marker is zero, indicating that this
//! comment is not linked to a marker." We surface that distinction
//! with [`Comment::linked_marker`] returning `Option<i16>`.

use crate::aiff::error::{AiffError, Result};
use crate::aiff::marker::{Marker, MarkerChunk};

/// A single comment entry inside a [`CommentsChunk`].
///
/// Constructed by [`parse_comments_chunk`]; the raw on-disk
/// `timestamp`, `marker`, and `text` fields are preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// `timeStamp` — seconds since `1904-01-01 00:00:00` UTC (Mac
    /// epoch). A `0` is a legal "unknown" sentinel.
    pub timestamp: u32,
    /// `marker` — `MarkerId` of the marker this comment annotates,
    /// or `0` when the comment is not linked to any marker. Per
    /// §6.0 a `MarkerId` is a positive non-zero `i16`; this field
    /// uses the same encoding with `0` as the unlinked sentinel.
    pub marker: i16,
    /// `text` — the comment body. Stored decoded as UTF-8 (lossy
    /// replacement on invalid bytes); the pad byte (if any) is NOT
    /// part of the text.
    pub text: String,
}

impl Comment {
    /// Returns the linked marker id when `marker > 0`, else `None`
    /// per §7.0 ¶ "marker": a zero marker means the comment is not
    /// linked. Negative marker ids are clamped to `None` because
    /// `MarkerId` is positive per §6.0.
    pub fn linked_marker(&self) -> Option<i16> {
        if self.marker > 0 {
            Some(self.marker)
        } else {
            None
        }
    }

    /// Look the linked marker up inside the supplied [`MarkerChunk`].
    /// Returns `None` when [`Self::linked_marker`] returns `None` or
    /// the id is not present in the chunk.
    pub fn resolve_marker<'m>(&self, markers: &'m MarkerChunk) -> Option<&'m Marker> {
        let id = self.linked_marker()?;
        markers.by_id(id)
    }
}

/// Parsed contents of a `COMT` chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommentsChunk {
    /// Comments in document order; the spec doesn't impose a sort,
    /// so we preserve whatever the encoder wrote.
    pub comments: Vec<Comment>,
}

/// Parse the body of a `COMT` chunk. `data` is the ckData slice the
/// chunk walker handed up — the 8-byte ckID/ckSize prefix and the
/// pad byte (if any) have already been stripped.
pub fn parse_comments_chunk(data: &[u8]) -> Result<CommentsChunk> {
    if data.len() < 2 {
        return Err(AiffError::Truncated("COMT numComments"));
    }
    let num = u16::from_be_bytes([data[0], data[1]]) as usize;

    let mut out = CommentsChunk {
        comments: Vec::with_capacity(num),
    };

    let mut cursor = 2usize;
    for _ in 0..num {
        // Per-comment header: 4 (timestamp) + 2 (marker) + 2 (count) = 8 bytes.
        if cursor.saturating_add(8) > data.len() {
            return Err(AiffError::Truncated("COMT comment header"));
        }
        let timestamp = u32::from_be_bytes([
            data[cursor],
            data[cursor + 1],
            data[cursor + 2],
            data[cursor + 3],
        ]);
        let marker = i16::from_be_bytes([data[cursor + 4], data[cursor + 5]]);
        let count = u16::from_be_bytes([data[cursor + 6], data[cursor + 7]]) as usize;
        cursor += 8;
        if cursor.saturating_add(count) > data.len() {
            return Err(AiffError::Truncated("COMT comment text"));
        }
        let text = String::from_utf8_lossy(&data[cursor..cursor + count]).into_owned();
        cursor += count;
        // §7.0: the per-comment block must be an even length. Header
        // is 8 bytes (even), so a pad is needed iff `count` is odd.
        // Tolerate a missing pad byte at end-of-chunk so encoders that
        // skipped the trailing pad on the last comment still parse,
        // mirroring the MARK / chunk-walker tolerance.
        if count % 2 == 1 && cursor < data.len() {
            cursor += 1;
        }

        out.comments.push(Comment {
            timestamp,
            marker,
            text,
        });
    }

    Ok(out)
}

/// Encode a [`CommentsChunk`] body in wire format — the bytes that
/// would follow a `COMT` chunk header. Each comment is laid out as
/// `timestamp(4) + marker(2) + count(2) + text + pad-to-even`.
///
/// Useful for round-tripping COMT chunks through write-side
/// container encoders; the chunk header itself (`'COMT' + ckSize`)
/// is the caller's responsibility.
pub fn write_comments_chunk(c: &CommentsChunk) -> Vec<u8> {
    let mut out = Vec::new();
    if c.comments.len() > u16::MAX as usize {
        // The on-wire numComments is u16; cap silently — the FORM
        // walker would reject anything larger anyway. Practical comment
        // counts are tiny (typically 0..10).
        // We still write what fits; the over-cap entries are dropped.
        out.extend_from_slice(&u16::MAX.to_be_bytes());
        for c in &c.comments[..u16::MAX as usize] {
            write_one_comment(&mut out, c);
        }
        return out;
    }
    out.extend_from_slice(&(c.comments.len() as u16).to_be_bytes());
    for c in &c.comments {
        write_one_comment(&mut out, c);
    }
    out
}

fn write_one_comment(out: &mut Vec<u8>, c: &Comment) {
    out.extend_from_slice(&c.timestamp.to_be_bytes());
    out.extend_from_slice(&c.marker.to_be_bytes());
    let text_bytes = c.text.as_bytes();
    let count = text_bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(count as u16).to_be_bytes());
    out.extend_from_slice(&text_bytes[..count]);
    // §7.0 pad byte: text padded to even byte count, not included in count.
    if count % 2 == 1 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aiff::marker::Marker;

    fn pack_one(timestamp: u32, marker: i16, text: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&timestamp.to_be_bytes());
        v.extend_from_slice(&marker.to_be_bytes());
        let text_b = text.as_bytes();
        v.extend_from_slice(&(text_b.len() as u16).to_be_bytes());
        v.extend_from_slice(text_b);
        if text_b.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn pack_chunk(comments: &[(u32, i16, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(comments.len() as u16).to_be_bytes());
        for (t, m, s) in comments {
            body.extend_from_slice(&pack_one(*t, *m, s));
        }
        body
    }

    #[test]
    fn parses_empty_comment_list() {
        let body = pack_chunk(&[]);
        let c = parse_comments_chunk(&body).unwrap();
        assert!(c.comments.is_empty());
    }

    #[test]
    fn parses_single_comment_with_even_text() {
        let body = pack_chunk(&[(0, 0, "abcd")]);
        let c = parse_comments_chunk(&body).unwrap();
        assert_eq!(c.comments.len(), 1);
        assert_eq!(c.comments[0].timestamp, 0);
        assert_eq!(c.comments[0].marker, 0);
        assert_eq!(c.comments[0].text, "abcd");
        assert!(c.comments[0].linked_marker().is_none());
    }

    #[test]
    fn parses_single_comment_with_odd_text() {
        // text "hi" is 2 bytes (even, no pad). text "hi!" is 3 bytes
        // and needs a pad byte before the next comment.
        let body = pack_chunk(&[(1234, 0, "hi!")]);
        let c = parse_comments_chunk(&body).unwrap();
        assert_eq!(c.comments[0].text, "hi!");
        assert_eq!(c.comments[0].timestamp, 1234);
    }

    #[test]
    fn parses_multiple_comments_with_mixed_pad() {
        // odd / even / odd payloads exercise the inter-comment pad
        // handling.
        let body = pack_chunk(&[(10, 0, "a"), (20, 0, "bb"), (30, 0, "ccc"), (40, 0, "dddd")]);
        let c = parse_comments_chunk(&body).unwrap();
        assert_eq!(c.comments.len(), 4);
        assert_eq!(c.comments[0].text, "a");
        assert_eq!(c.comments[1].text, "bb");
        assert_eq!(c.comments[2].text, "ccc");
        assert_eq!(c.comments[3].text, "dddd");
    }

    #[test]
    fn parses_comment_linked_to_marker() {
        let body = pack_chunk(&[(0, 7, "see marker 7")]);
        let c = parse_comments_chunk(&body).unwrap();
        assert_eq!(c.comments[0].marker, 7);
        assert_eq!(c.comments[0].linked_marker(), Some(7));

        let markers = MarkerChunk {
            markers: vec![Marker {
                id: 7,
                position: 1024,
                name: "intro".into(),
            }],
        };
        let m = c.comments[0].resolve_marker(&markers).unwrap();
        assert_eq!(m.position, 1024);
        assert_eq!(m.name, "intro");
    }

    #[test]
    fn resolve_marker_returns_none_for_unlinked() {
        let body = pack_chunk(&[(0, 0, "free-floating")]);
        let c = parse_comments_chunk(&body).unwrap();
        let markers = MarkerChunk {
            markers: vec![Marker {
                id: 1,
                position: 0,
                name: "x".into(),
            }],
        };
        assert!(c.comments[0].resolve_marker(&markers).is_none());
    }

    #[test]
    fn resolve_marker_returns_none_when_id_missing() {
        let body = pack_chunk(&[(0, 99, "dangling")]);
        let c = parse_comments_chunk(&body).unwrap();
        let markers = MarkerChunk {
            markers: vec![Marker {
                id: 1,
                position: 0,
                name: "x".into(),
            }],
        };
        assert!(c.comments[0].resolve_marker(&markers).is_none());
    }

    #[test]
    fn empty_text_round_trips() {
        // count = 0; no pad needed.
        let body = pack_chunk(&[(99, 0, "")]);
        let c = parse_comments_chunk(&body).unwrap();
        assert_eq!(c.comments[0].text, "");
    }

    #[test]
    fn rejects_truncated_num_comments() {
        assert!(matches!(
            parse_comments_chunk(&[]),
            Err(AiffError::Truncated(_))
        ));
        assert!(matches!(
            parse_comments_chunk(&[0u8]),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn rejects_truncated_comment_header() {
        // declares 1 comment but only 5 of the 8 header bytes follow.
        let mut body = 1_u16.to_be_bytes().to_vec();
        body.extend_from_slice(&[0u8; 5]);
        assert!(matches!(
            parse_comments_chunk(&body),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn rejects_truncated_comment_text() {
        // count declared = 10 but only 4 chars follow.
        let mut body = 1_u16.to_be_bytes().to_vec();
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.extend_from_slice(&0_i16.to_be_bytes());
        body.extend_from_slice(&10_u16.to_be_bytes());
        body.extend_from_slice(b"AAAA");
        assert!(matches!(
            parse_comments_chunk(&body),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn write_round_trip() {
        let c = CommentsChunk {
            comments: vec![
                Comment {
                    timestamp: 100,
                    marker: 1,
                    text: "first".into(),
                },
                Comment {
                    timestamp: 200,
                    marker: 0,
                    text: "second comment".into(),
                },
                Comment {
                    timestamp: 300,
                    marker: 3,
                    text: "odd".into(),
                },
            ],
        };
        let bytes = write_comments_chunk(&c);
        let parsed = parse_comments_chunk(&bytes).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn write_empty_chunk() {
        let c = CommentsChunk::default();
        let bytes = write_comments_chunk(&c);
        assert_eq!(bytes, vec![0u8, 0u8]);
        let parsed = parse_comments_chunk(&bytes).unwrap();
        assert!(parsed.comments.is_empty());
    }

    #[test]
    fn negative_marker_id_clamps_to_none() {
        // Encoders shouldn't emit negative ids, but the field is
        // structurally `i16`; defensive parse + linked_marker should
        // treat it as "not linked".
        let mut body = 1_u16.to_be_bytes().to_vec();
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.extend_from_slice(&(-1_i16).to_be_bytes());
        body.extend_from_slice(&0_u16.to_be_bytes());
        let c = parse_comments_chunk(&body).unwrap();
        assert_eq!(c.comments[0].marker, -1);
        assert!(c.comments[0].linked_marker().is_none());
    }
}
