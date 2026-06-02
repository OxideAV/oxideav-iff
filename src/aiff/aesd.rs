//! `AESD` (Audio Recording) chunk parser — AES channel-status data.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §11.0 (Audio Recording Chunk) the
//! wire layout is:
//!
//! ```text
//! ckID                  : 'AESD'
//! ckSize                : int32   (always 24)
//! AESChannelStatusData  : char[24]
//! ```
//!
//! The 24 bytes carry the AES Channel Status Data block as defined by
//! the AES "Recommended Practice for Digital Audio Engineering -
//! Serial Transmission Format for Linearly Represented Digital Audio
//! Data" §7.1. The spec calls out bits 2..=4 of byte 0 as "recording
//! emphasis" — we surface those as a structured helper but keep the
//! raw 24 bytes available so callers needing the full AES decode can
//! reach in without re-parsing.
//!
//! The chunk is optional and §11.0 forbids more than one AESD per
//! FORM; the FORM walker enforces that via [`AiffError::DuplicateChunk`].

use crate::aiff::error::{AiffError, Result};

/// Recording-emphasis field decoded out of bits 2..=4 of byte 0 of
/// the AES channel-status block.
///
/// Per §11.0 ¶ "Of general interest would be bits 2, 3, and 4 of
/// byte 0, which describe recording emphasis." The exact AES
/// semantics (50/15 µs vs CCITT vs none vs reserved encodings) live
/// in AES3-2003 §4 — we surface the three-bit raw field and let
/// callers map it however their application needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emphasis {
    /// The raw 3-bit value as recorded in byte 0 bits 2..=4 (i.e.
    /// `(byte0 >> 2) & 0b111`).
    pub bits: u8,
}

/// Parsed contents of an `AESD` (Audio Recording) chunk.
///
/// The 24-byte AES channel-status block is preserved verbatim in
/// [`Self::status`]; callers that only need the recording-emphasis
/// field can read it via [`Self::emphasis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AesdChunk {
    /// The 24-byte AES channel-status block, byte-for-byte as read
    /// off the wire.
    pub status: [u8; 24],
}

impl AesdChunk {
    /// Extract the 3-bit recording-emphasis field from byte 0 (bits
    /// 2..=4) of the channel-status block, per §11.0.
    pub fn emphasis(&self) -> Emphasis {
        Emphasis {
            bits: (self.status[0] >> 2) & 0b111,
        }
    }
}

/// Parse the body of an `AESD` chunk. `data` is the ckData slice the
/// chunk walker handed up — the 8-byte ckID/ckSize prefix and the
/// pad byte (if any) have already been stripped.
///
/// Spec §11.0: `ckDataSize` is always 24. We accept exactly 24 bytes
/// and reject anything shorter as [`AiffError::Truncated`]; longer is
/// reported as [`AiffError::InvalidValue { what: "AESD ckSize", ... }`]
/// since the spec is explicit ("always 24") and a trailing pad byte
/// inside the chunk would already have been stripped by the chunk
/// walker.
pub fn parse_aesd_chunk(data: &[u8]) -> Result<AesdChunk> {
    if data.len() < 24 {
        return Err(AiffError::Truncated("AESD chunk"));
    }
    if data.len() > 24 {
        return Err(AiffError::InvalidValue {
            what: "AESD ckSize",
            value: data.len() as i64,
        });
    }
    let mut status = [0u8; 24];
    status.copy_from_slice(&data[..24]);
    Ok(AesdChunk { status })
}

/// Encode an [`AesdChunk`] body in wire format — exactly the 24
/// channel-status bytes the spec calls for. The chunk header
/// (`'AESD' + ckSize`) is the caller's responsibility.
pub fn write_aesd_chunk(c: &AesdChunk) -> [u8; 24] {
    c.status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_24_byte_chunk() {
        let mut body = [0u8; 24];
        // Byte 0 bits 2..=4 = 0b101 (5) -> recording emphasis raw 5.
        body[0] = 0b0001_0100;
        for (i, b) in body.iter_mut().enumerate().skip(1) {
            *b = i as u8;
        }
        let c = parse_aesd_chunk(&body).unwrap();
        assert_eq!(c.status[0], 0b0001_0100);
        assert_eq!(c.status[23], 23);
        assert_eq!(c.emphasis().bits, 0b101);
    }

    #[test]
    fn rejects_truncated_chunk() {
        assert!(matches!(
            parse_aesd_chunk(&[0u8; 23]),
            Err(AiffError::Truncated("AESD chunk"))
        ));
        assert!(matches!(
            parse_aesd_chunk(&[]),
            Err(AiffError::Truncated("AESD chunk"))
        ));
    }

    #[test]
    fn rejects_oversized_chunk() {
        let r = parse_aesd_chunk(&[0u8; 25]);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "AESD ckSize",
                value: 25
            })
        ));
    }

    #[test]
    fn emphasis_zero_means_no_emphasis_bits_set() {
        let c = AesdChunk { status: [0u8; 24] };
        assert_eq!(c.emphasis().bits, 0);
    }

    #[test]
    fn emphasis_isolates_bits_2_through_4() {
        // Byte 0 = 0xFF — all bits set; emphasis = (0xFF >> 2) & 0b111 = 0b111.
        let mut s = [0u8; 24];
        s[0] = 0xFF;
        let c = AesdChunk { status: s };
        assert_eq!(c.emphasis().bits, 0b111);
    }

    #[test]
    fn write_round_trips() {
        let mut s = [0u8; 24];
        for (i, b) in s.iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
        let c = AesdChunk { status: s };
        let bytes = write_aesd_chunk(&c);
        let parsed = parse_aesd_chunk(&bytes).unwrap();
        assert_eq!(parsed, c);
    }
}
