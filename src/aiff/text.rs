//! Text chunks — `NAME` / `AUTH` / `(c) ` / `ANNO`.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §13.0 (Text Chunks — Name, Author,
//! Copyright, Annotation) the four chunks share an identical wire
//! layout: a four-byte ckID, a four-byte big-endian `ckSize`, and a
//! flat run of ASCII bytes whose length is the ckSize value.
//!
//! ```text
//! ckID       : 'NAME' | 'AUTH' | '[c] ' | 'ANNO'
//! ckSize     : int32           (byte length of text)
//! text       : char[ckSize]    (pure ASCII, neither pstring nor C string)
//! ```
//!
//! §13.0 quotes (verbatim from the staged text spec):
//!
//! * "These four chunks are included in the definition of many IFF
//!   FORMs. All are text chunks; their data portion consists solely of
//!   text. Each of these chunks is optional."
//! * "text contains pure ASCII characters. It is neither a pstring nor a
//!   C string. The number of characters in text is determined by
//!   ckDataSize."
//! * "Name Chunk text contains the name of the sampled sound. […] No
//!   more than one Name Chunk may exist within a FORM AIFC."
//! * "Author Chunk text contains one or more author names. […] No more
//!   than one Author Chunk may exist within a FORM AIFC."
//! * "The Copyright Chunk contains a copyright notice for the sound.
//!   […] No more than one Copyright Chunk may exist within a FORM AIFC."
//! * "Annotation Chunk text contains a comment. […] Any number of
//!   Annotation Chunks may exist within a FORM AIFC."
//!
//! NAME / AUTH / `(c) ` are therefore at-most-one-per-FORM and routed
//! through the parser as duplicate-checked [`Form::name`] /
//! [`Form::author`] / [`Form::copyright`] singletons; ANNO is
//! unconstrained and accumulated into [`Form::annotations`] in document
//! order, mirroring how §10.0 MIDI and §12.0 APPL handle the
//! "any-number" rule.
//!
//! The text body itself is preserved verbatim — the spec is explicit
//! that the field is "neither a pstring nor a C string", so no
//! trailing-NUL trimming or pstring-length read happens here. UTF-8
//! lossy decoding is offered as a method ([`TextChunk::as_string_lossy`])
//! for callers that prefer a `String`, since real-world AIFF files
//! occasionally carry MacRoman or other 8-bit codepages that the spec's
//! "pure ASCII" wording doesn't cover but parsers in the wild tolerate.
//!
//! Per ckSize semantics in §13.0, an empty text body (`ckDataSize == 0`)
//! is well-formed: the field is then a zero-length ASCII string. The
//! parser accepts that without complaint, mirroring the MIDI chunk's
//! "zero-length payload is fine" rule.
//!
//! [`Form::name`]: super::form::Form::name
//! [`Form::author`]: super::form::Form::author
//! [`Form::copyright`]: super::form::Form::copyright
//! [`Form::annotations`]: super::form::Form::annotations

use crate::aiff::error::Result;

/// Which of the four §13.0 text chunks a parsed [`TextChunk`] came from.
///
/// The kind is set by whichever ckID the FORM walker dispatched on, so
/// downstream code can route NAME / AUTH / `(c) ` / ANNO via the
/// `kind` field without re-inspecting the original ckID bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// `NAME` — the name of the sampled sound. At most one per FORM.
    Name,
    /// `AUTH` — author name(s). At most one per FORM.
    Author,
    /// `[c] ` — copyright notice. At most one per FORM. (The lowercase
    /// `c` and the trailing space are part of the on-wire ckID per
    /// §13.0; the chunk-ID character itself stands in for the © glyph.)
    Copyright,
    /// `ANNO` — free-form annotation. Any number per FORM.
    Annotation,
}

impl TextKind {
    /// On-wire ckID for this text-chunk kind. Mirrors the
    /// `NameID` / `AuthorID` / `CopyrightID` / `AnnotationID` `#define`s
    /// in §13.0.
    pub fn ck_id(self) -> [u8; 4] {
        match self {
            Self::Name => *b"NAME",
            Self::Author => *b"AUTH",
            Self::Copyright => *b"(c) ",
            Self::Annotation => *b"ANNO",
        }
    }

    /// Inverse of [`TextKind::ck_id`] — returns the matching kind for
    /// one of the four §13.0 ckIDs, or `None` for anything else.
    pub fn from_ck_id(id: &[u8; 4]) -> Option<Self> {
        match id {
            b"NAME" => Some(Self::Name),
            b"AUTH" => Some(Self::Author),
            b"(c) " => Some(Self::Copyright),
            b"ANNO" => Some(Self::Annotation),
            _ => None,
        }
    }
}

/// Parsed contents of a single §13.0 text chunk.
///
/// `text` is the raw byte payload verbatim — §13.0 says "pure ASCII"
/// but the parser preserves whatever bytes the chunk walker handed up,
/// so downstream code can salvage MacRoman / Latin-1 strings produced
/// by older encoders too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    /// Which of NAME / AUTH / `(c) ` / ANNO this chunk is. Set from
    /// the on-wire ckID by the FORM walker.
    pub kind: TextKind,
    /// The text body, byte-for-byte. The chunk walker has already
    /// stripped the 8-byte ckID/ckSize prefix and any trailing
    /// odd-size pad byte the outer container inserted.
    pub text: Vec<u8>,
}

impl TextChunk {
    /// Number of bytes in the text body.
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// `true` when `ckDataSize == 0`. §13.0 doesn't forbid an empty
    /// body, so the parser accepts it and lets callers decide whether
    /// to skip the chunk entirely.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Borrow the text body as a `&str` when it is valid UTF-8.
    /// Returns `None` for non-UTF-8 byte sequences so callers can fall
    /// back to [`TextChunk::as_string_lossy`] without paying the
    /// allocation cost when the body is already valid.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.text).ok()
    }

    /// UTF-8 lossy decode of the text body. Invalid byte sequences are
    /// replaced with `U+FFFD`.
    pub fn as_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.text).into_owned()
    }
}

/// Parse the body of a text chunk into a [`TextChunk`].
///
/// `kind` is supplied by the caller (the FORM walker maps ckID →
/// [`TextKind`] before dispatching here), and `data` is the ckData
/// slice the chunk walker handed up — the 8-byte ckID/ckSize prefix
/// and the pad byte (if any) have already been stripped.
///
/// §13.0 places no minimum, maximum, or alignment constraints on the
/// text body beyond "the number of characters in text is determined by
/// ckDataSize", so any byte length (including zero) is accepted and
/// preserved verbatim.
pub fn parse_text_chunk(kind: TextKind, data: &[u8]) -> Result<TextChunk> {
    Ok(TextChunk {
        kind,
        text: data.to_vec(),
    })
}

/// Encode a [`TextChunk`] body in wire format — just the raw text
/// bytes. The chunk header (`ckID + ckSize`) and any odd-length pad
/// byte are the caller's responsibility, mirroring every other
/// write-side helper in this module.
pub fn write_text_chunk(c: &TextChunk) -> Vec<u8> {
    c.text.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_ck_ids() {
        for &k in &[
            TextKind::Name,
            TextKind::Author,
            TextKind::Copyright,
            TextKind::Annotation,
        ] {
            let id = k.ck_id();
            assert_eq!(TextKind::from_ck_id(&id), Some(k));
        }
        // Unrelated ckIDs do not map.
        assert_eq!(TextKind::from_ck_id(b"COMM"), None);
        assert_eq!(TextKind::from_ck_id(b"SSND"), None);
        assert_eq!(TextKind::from_ck_id(b"COMT"), None);
    }

    #[test]
    fn copyright_ck_id_matches_spec_literal() {
        // §13.0 specifies `'[c] '` with a lowercase `c`, a space after
        // the close paren, and no NUL — but the C source uses `[`/`]`
        // brackets to denote "this slot holds the copyright character".
        // The on-wire ckID is actually four bytes `(`, `c`, `)`, ` `;
        // a literal `[c] ` would be a typographic transcription of the
        // round-bracket form. We use the canonical round-bracket form
        // throughout: `0x28 0x63 0x29 0x20`.
        assert_eq!(TextKind::Copyright.ck_id(), [b'(', b'c', b')', b' ']);
    }

    #[test]
    fn parses_empty_body() {
        // §13.0 doesn't forbid ckDataSize == 0.
        let c = parse_text_chunk(TextKind::Annotation, &[]).unwrap();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!(c.as_str(), Some(""));
        assert_eq!(c.as_string_lossy(), "");
    }

    #[test]
    fn preserves_ascii_verbatim() {
        let body = b"Helicopter take-off";
        let c = parse_text_chunk(TextKind::Name, body).unwrap();
        assert_eq!(c.text, body);
        assert_eq!(c.len(), body.len());
        assert!(!c.is_empty());
        assert_eq!(c.as_str(), Some("Helicopter take-off"));
    }

    #[test]
    fn preserves_non_ascii_bytes_for_legacy_files() {
        // §13.0 says "pure ASCII" but real files occasionally carry
        // high-bit bytes (MacRoman, Latin-1, …). The parser must not
        // reject those — downstream code should still be able to
        // look at the bytes and decide how to decode.
        let body = [b'a', 0xa9, b'b']; // MacRoman ©
        let c = parse_text_chunk(TextKind::Copyright, &body).unwrap();
        assert_eq!(c.text, body);
        assert!(c.as_str().is_none());
        let lossy = c.as_string_lossy();
        assert!(lossy.contains('a'));
        assert!(lossy.contains('b'));
    }

    #[test]
    fn write_round_trips_simple_body() {
        let body = b"author 1, author 2".to_vec();
        let c = TextChunk {
            kind: TextKind::Author,
            text: body.clone(),
        };
        let bytes = write_text_chunk(&c);
        assert_eq!(bytes, body);
        let parsed = parse_text_chunk(TextKind::Author, &bytes).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn write_round_trips_empty_body() {
        let c = TextChunk {
            kind: TextKind::Name,
            text: Vec::new(),
        };
        let bytes = write_text_chunk(&c);
        assert!(bytes.is_empty());
        let parsed = parse_text_chunk(TextKind::Name, &bytes).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn odd_length_body_does_not_need_inner_pad() {
        // §13.0's pad-to-even is a chunk-walker-level concern; the
        // body itself is `ckDataSize` characters long with no inner
        // pad. A 5-byte body must parse to a 5-byte text field even
        // when the outer chunk would round up to 6.
        let body = b"hello"; // 5 bytes
        let c = parse_text_chunk(TextKind::Annotation, body).unwrap();
        assert_eq!(c.len(), 5);
        assert_eq!(c.text, body);
    }

    #[test]
    fn accepts_large_body() {
        // 4 KiB — exercises that there's no length cap on the text
        // chunk parse path (mirrors the MIDI / APPL surface).
        let body: Vec<u8> = (0..4096_usize).map(|i| ((i % 95) + 32) as u8).collect();
        let c = parse_text_chunk(TextKind::Annotation, &body).unwrap();
        assert_eq!(c.len(), 4096);
        assert_eq!(c.text, body);
        // Body is printable ASCII by construction, so as_str succeeds.
        assert!(c.as_str().is_some());
    }
}
