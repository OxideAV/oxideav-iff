//! `APPL` (Application Specific) chunk parser.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §12.0 (Application Specific
//! Chunk) the wire layout is:
//!
//! ```text
//! ckID                : 'APPL'
//! ckSize              : int32
//! applicationSignature: OSType (4 bytes, ASCII signature)
//! data                : char[]   (application-specific bytes, padded
//!                                  to even total chunk length)
//! ```
//!
//! Unlike `MARK` / `INST` / `COMT` / `AESD`, §12.0 ¶ "The Application
//! Specific Chunk is optional. **Any number of Application Specific
//! Chunks may exist in a single FORM AIFC.**" — multiple APPL chunks
//! ARE permitted, each with its own application signature. The FORM
//! walker accumulates them into a `Vec<ApplicationChunk>` instead of
//! rejecting duplicates.
//!
//! §12.0 documents three signature dialects that affect how the
//! `data` payload starts:
//!
//! * `pdos` (`0x70646F73`) — Apple II application. `data` begins with
//!   a Pascal-style string carrying the application's name.
//! * `stoc` — non-Apple application. `data` begins with a Pascal-style
//!   string carrying the application's name.
//! * Macintosh application signatures (any other four-byte FourCC)
//!   carry application-specific bytes with no required leading
//!   structure.
//!
//! §12.0 specifies a chunk-level pad-to-even on `data` but does NOT
//! call for an inner pad after the leading Pascal-string for
//! `pdos`/`stoc`, so [`ApplicationChunk::payload_after_name`] steps
//! by exactly `1 + length_byte` to reach the application payload.
//!
//! We surface the signature dialect via [`ApplicationChunk::dialect`]
//! and decode the leading Pascal string for `pdos` / `stoc` chunks
//! via [`ApplicationChunk::application_name`], leaving the remainder
//! addressable through [`ApplicationChunk::payload_after_name`].

use crate::aiff::error::{AiffError, Result};

/// The `applicationSignature` dialect a parsed APPL chunk falls
/// into, per §12.0 of the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationDialect {
    /// `pdos` — Apple II application. The data area starts with a
    /// Pascal-string application name.
    AppleII,
    /// `stoc` — non-Apple-computer application. The data area
    /// starts with a Pascal-string application name.
    NonApple,
    /// Any other four-byte signature — Macintosh application, raw
    /// `data` bytes.
    Macintosh,
}

/// Parsed contents of a single `APPL` (Application Specific) chunk.
///
/// `signature` and `data` are the raw on-disk fields verbatim — the
/// Pascal-string leading-name decode for `pdos` / `stoc` is exposed
/// via methods rather than baked in so callers can choose between
/// "I want the raw bytes" and "I want the application name".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationChunk {
    /// `applicationSignature` — 4 bytes identifying which
    /// application wrote this chunk.
    pub signature: [u8; 4],
    /// Everything after the signature, byte-for-byte. The chunk
    /// walker has already stripped the per-chunk pad byte so this
    /// length matches the spec's `data` field exactly.
    pub data: Vec<u8>,
}

impl ApplicationChunk {
    /// Classify this chunk's signature into one of the three
    /// dialects §12.0 defines.
    pub fn dialect(&self) -> ApplicationDialect {
        match &self.signature {
            b"pdos" => ApplicationDialect::AppleII,
            b"stoc" => ApplicationDialect::NonApple,
            _ => ApplicationDialect::Macintosh,
        }
    }

    /// Decode the leading Pascal-string application name when the
    /// dialect is [`AppleII`](ApplicationDialect::AppleII) or
    /// [`NonApple`](ApplicationDialect::NonApple). Returns `None`
    /// for Macintosh-dialect signatures (which carry raw bytes with
    /// no required leading structure) or when the chunk's `data` is
    /// too short to hold a length byte plus the declared chars.
    ///
    /// The returned name is UTF-8 lossy-decoded; the spec is silent
    /// on encoding and any printable subset will round-trip cleanly.
    pub fn application_name(&self) -> Option<String> {
        match self.dialect() {
            ApplicationDialect::Macintosh => None,
            ApplicationDialect::AppleII | ApplicationDialect::NonApple => {
                if self.data.is_empty() {
                    return None;
                }
                let len = self.data[0] as usize;
                if 1 + len > self.data.len() {
                    return None;
                }
                Some(String::from_utf8_lossy(&self.data[1..1 + len]).into_owned())
            }
        }
    }

    /// Return the slice of `data` that follows the leading
    /// Pascal-string application name when the dialect is `pdos`
    /// or `stoc`. The spec calls out only chunk-level pad-to-even
    /// for the APPL chunk (§12.0 ¶ "data must be padded with a byte
    /// at the end as needed to make it an even number of bytes
    /// long") — it does NOT specify an inner pad for the leading
    /// pstring — so we step by exactly `1 + len`.
    ///
    /// For Macintosh-dialect signatures or when the leading pstring
    /// can't be decoded this returns the entire `data` slice
    /// untouched.
    pub fn payload_after_name(&self) -> &[u8] {
        match self.dialect() {
            ApplicationDialect::Macintosh => &self.data,
            ApplicationDialect::AppleII | ApplicationDialect::NonApple => {
                if self.data.is_empty() {
                    return &self.data;
                }
                let len = self.data[0] as usize;
                let raw = 1 + len;
                if raw > self.data.len() {
                    return &self.data;
                }
                &self.data[raw..]
            }
        }
    }
}

/// Parse the body of an `APPL` chunk. `data` is the ckData slice the
/// chunk walker handed up — the 8-byte ckID/ckSize prefix and the
/// pad byte (if any) have already been stripped.
///
/// Spec §12.0: the first 4 bytes are the application signature; the
/// remainder is application-defined `data`. A truncated chunk (fewer
/// than 4 bytes) is rejected as [`AiffError::Truncated`].
pub fn parse_appl_chunk(data: &[u8]) -> Result<ApplicationChunk> {
    if data.len() < 4 {
        return Err(AiffError::Truncated("APPL signature"));
    }
    let mut signature = [0u8; 4];
    signature.copy_from_slice(&data[..4]);
    Ok(ApplicationChunk {
        signature,
        data: data[4..].to_vec(),
    })
}

/// Encode an [`ApplicationChunk`] body in wire format — the bytes
/// that would follow an `APPL` chunk header (signature + data).
/// The chunk header itself (`'APPL' + ckSize`) is the caller's
/// responsibility.
pub fn write_appl_chunk(c: &ApplicationChunk) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + c.data.len());
    out.extend_from_slice(&c.signature);
    out.extend_from_slice(&c.data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macintosh_signature_carries_raw_data() {
        // Signature 'TEST' + 6 raw bytes.
        let mut body = Vec::new();
        body.extend_from_slice(b"TEST");
        body.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let c = parse_appl_chunk(&body).unwrap();
        assert_eq!(&c.signature, b"TEST");
        assert_eq!(c.data, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(c.dialect(), ApplicationDialect::Macintosh);
        assert_eq!(c.application_name(), None);
        assert_eq!(c.payload_after_name(), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn parses_appleii_signature_decodes_leading_pstring() {
        // 'pdos' + pstring("Pic", len=3; 1+3=4 even, no pad) + 2 raw bytes.
        let mut body = Vec::new();
        body.extend_from_slice(b"pdos");
        body.push(3); // pstring length
        body.extend_from_slice(b"Pic");
        body.extend_from_slice(&[0xAA, 0xBB]);
        let c = parse_appl_chunk(&body).unwrap();
        assert_eq!(c.dialect(), ApplicationDialect::AppleII);
        assert_eq!(c.application_name(), Some("Pic".to_string()));
        assert_eq!(c.payload_after_name(), &[0xAA, 0xBB]);
    }

    #[test]
    fn payload_after_name_with_zero_length_pstring() {
        // 'pdos' + pstring("" len=0) + 3 raw bytes. Verifies the
        // payload starts at offset 1 (just past the length byte).
        let mut body = Vec::new();
        body.extend_from_slice(b"pdos");
        body.push(0);
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let c = parse_appl_chunk(&body).unwrap();
        assert_eq!(c.application_name(), Some("".to_string()));
        assert_eq!(c.payload_after_name(), &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn parses_stoc_signature_decodes_leading_pstring() {
        // 'stoc' + pstring("MyEditor", 8 chars; raw step = 1 + 8 = 9).
        let mut body = Vec::new();
        body.extend_from_slice(b"stoc");
        body.push(8);
        body.extend_from_slice(b"MyEditor");
        body.extend_from_slice(&[0xCC, 0xDD, 0xEE]);
        let c = parse_appl_chunk(&body).unwrap();
        assert_eq!(c.dialect(), ApplicationDialect::NonApple);
        assert_eq!(c.application_name(), Some("MyEditor".to_string()));
        assert_eq!(c.payload_after_name(), &[0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn stoc_with_short_pstring_payload_starts_after_name() {
        // 'stoc' + pstring("abc", 3 chars; step = 1 + 3 = 4) + 2 raw.
        let mut body = Vec::new();
        body.extend_from_slice(b"stoc");
        body.push(3);
        body.extend_from_slice(b"abc");
        body.extend_from_slice(&[0x11, 0x22]);
        let c = parse_appl_chunk(&body).unwrap();
        assert_eq!(c.application_name(), Some("abc".to_string()));
        assert_eq!(c.payload_after_name(), &[0x11, 0x22]);
    }

    #[test]
    fn empty_data_has_no_leading_name() {
        // 'pdos' + zero data bytes.
        let body = b"pdos";
        let c = parse_appl_chunk(body).unwrap();
        assert_eq!(c.application_name(), None);
        assert!(c.payload_after_name().is_empty());
    }

    #[test]
    fn truncated_chunk_errors() {
        assert!(matches!(
            parse_appl_chunk(&[]),
            Err(AiffError::Truncated("APPL signature"))
        ));
        assert!(matches!(
            parse_appl_chunk(b"abc"),
            Err(AiffError::Truncated("APPL signature"))
        ));
    }

    #[test]
    fn malformed_pstring_returns_none_name() {
        // 'pdos' + length byte 99 but only 2 chars follow — too short.
        let mut body = Vec::new();
        body.extend_from_slice(b"pdos");
        body.push(99);
        body.extend_from_slice(b"AB");
        let c = parse_appl_chunk(&body).unwrap();
        assert_eq!(c.application_name(), None);
    }

    #[test]
    fn write_round_trips_macintosh() {
        let c = ApplicationChunk {
            signature: *b"VENZ",
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let bytes = write_appl_chunk(&c);
        let parsed = parse_appl_chunk(&bytes).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn write_round_trips_pdos() {
        let mut data = Vec::new();
        data.push(3);
        data.extend_from_slice(b"Pic");
        data.extend_from_slice(&[1, 2, 3]);
        let c = ApplicationChunk {
            signature: *b"pdos",
            data,
        };
        let bytes = write_appl_chunk(&c);
        let parsed = parse_appl_chunk(&bytes).unwrap();
        assert_eq!(parsed, c);
        assert_eq!(parsed.application_name(), Some("Pic".to_string()));
        assert_eq!(parsed.payload_after_name(), &[1, 2, 3]);
    }
}
