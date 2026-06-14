//! Generic EA IFF 85 chunk walker.
//!
//! Per `docs/audio/aiff/aiff-aifc-format.md` §1, every AIFF / AIFF-C
//! chunk has the same shape:
//!
//! ```text
//! ckID    : 4 bytes  (FourCC, ASCII)
//! ckSize  : int32    (big-endian; bytes that follow, NOT counting
//!                     ckID/ckSize)
//! ckData  : ckSize bytes
//! [pad]   : 1 byte   if ckSize is odd  (16-bit alignment, NOT
//!                     counted in ckSize)
//! ```
//!
//! [`ChunkIter`] yields successive [`Chunk`] borrows of the input
//! buffer until the buffer is exhausted. It tolerates a missing
//! trailing pad byte at EOF (a real-world quirk that some encoders
//! produce when `ckSize` is odd and the file just ends there) but
//! flags an oversized chunk that extends beyond the buffer.

use crate::aiff::error::{AiffError, Result};

/// A borrowed view of a single EA IFF 85 chunk inside its parent
/// container. The `data` slice excludes the 8-byte ckID/ckSize
/// header and the trailing pad byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// The 4-byte FourCC chunk identifier.
    pub id: [u8; 4],
    /// The declared chunk-data size (i.e. `ckSize`); equals
    /// `data.len()`.
    pub size: u32,
    /// The chunk-data slice. Does NOT include the trailing pad byte
    /// for odd-size chunks (the iterator skips that pad
    /// transparently when advancing).
    pub data: &'a [u8],
}

impl Chunk<'_> {
    /// Returns the chunk id as `&str` if it's ASCII-printable, else
    /// `None`. Convenience for matching on `"COMM"`, `"SSND"`, etc.
    /// in user code; the canonical match is on `chunk.id`.
    pub fn id_str(&self) -> Option<&str> {
        if self.id.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            core::str::from_utf8(&self.id).ok()
        } else {
            None
        }
    }
}

/// Iterator over the chunks inside a parent container buffer.
///
/// Construct with [`ChunkIter::new`]. Each `next()` returns
/// `Some(Ok(chunk))` for a valid chunk, `Some(Err(_))` if the
/// declared size would overflow the buffer, or `None` once the
/// buffer is exhausted.
#[derive(Debug, Clone)]
pub struct ChunkIter<'a> {
    rest: &'a [u8],
}

impl<'a> ChunkIter<'a> {
    /// Wrap a buffer in a chunk iterator.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { rest: buf }
    }

    /// The unparsed bytes still in front of the iterator.
    pub fn remaining(&self) -> &'a [u8] {
        self.rest
    }
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = Result<Chunk<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        // EOF is the canonical end-of-iteration signal.
        if self.rest.is_empty() {
            return None;
        }

        // Each chunk header is exactly 8 bytes: 4 ckID + 4 ckSize.
        if self.rest.len() < 8 {
            self.rest = &[];
            return Some(Err(AiffError::Truncated("chunk header")));
        }
        let mut id = [0u8; 4];
        id.copy_from_slice(&self.rest[..4]);
        let size = u32::from_be_bytes([self.rest[4], self.rest[5], self.rest[6], self.rest[7]]);

        // Validate that the chunk fits in the remaining buffer
        // before slicing it out.
        let data_start = 8usize;
        let data_end = data_start
            .checked_add(size as usize)
            .ok_or(AiffError::OversizedChunk {
                id,
                declared: size,
                available: self.rest.len().saturating_sub(data_start) as u32,
            });
        let data_end = match data_end {
            Ok(v) => v,
            Err(e) => {
                self.rest = &[];
                return Some(Err(e));
            }
        };
        if data_end > self.rest.len() {
            self.rest = &[];
            return Some(Err(AiffError::OversizedChunk {
                id,
                declared: size,
                available: self.rest.len().saturating_sub(data_start) as u32,
            }));
        }

        let data = &self.rest[data_start..data_end];

        // Advance past the chunk plus the optional pad byte. Per
        // spec, the pad is present iff `size` is odd and is NOT
        // counted in ckSize. Tolerate a missing pad at end-of-buffer
        // (some encoders just stop).
        let mut next_start = data_end;
        if size & 1 == 1 && next_start < self.rest.len() {
            next_start += 1;
        }
        self.rest = &self.rest[next_start..];

        Some(Ok(Chunk { id, size, data }))
    }
}

/// The `timestamp` value of version 1 of the AIFF-C specification —
/// the 8/26/91 draft — as carried by the `FVER` Format Version chunk.
///
/// Per `docs/audio/aiff/aiff-aifc-format.md` §3.1 and the AIFF-C draft
/// (`docs/audio/aiff/aiff-c.txt`, `#define AIFCVersion1 0xA2805140`),
/// the value is `0xA280_5140` — the number of seconds since the
/// 1904-01-01 Macintosh epoch at which that AIFF-C release was issued.
/// Every AIFF-C file carries exactly this timestamp; §3.1 instructs
/// applications not to alter it.
pub const AIFC_VERSION_1: u32 = 0xA280_5140;

/// Frame a chunk body in EA IFF 85 wire format — the exact inverse of
/// [`ChunkIter`].
///
/// Prepends the 8-byte header (4-byte `ckID` + big-endian `int32`
/// `ckSize`, where `ckSize == body.len()`) to `body`, then appends a
/// single `0x00` pad byte iff `body.len()` is odd. Per
/// `docs/audio/aiff/aiff-aifc-format.md` §1 the pad byte enforces
/// 16-bit alignment and is **not** counted in `ckSize`.
///
/// Every per-chunk `write_*` helper in this module emits a chunk
/// *body* and documents that "the chunk header (`ckID + ckSize`) and
/// any odd-length pad byte are the caller's responsibility." This is
/// that responsibility, factored into one place so a caller building a
/// FORM does not have to re-derive the big-endian size encoding and
/// the odd-length pad rule by hand for every chunk.
///
/// Returns [`AiffError::OversizedChunk`] when `body.len()` exceeds
/// `u32::MAX` and therefore cannot be represented in the 32-bit
/// `ckSize` field (`available` carries the actual body length, clamped
/// to `u32::MAX` for display).
pub fn frame_chunk(id: &[u8; 4], body: &[u8]) -> Result<Vec<u8>> {
    let size = u32::try_from(body.len()).map_err(|_| AiffError::OversizedChunk {
        id: *id,
        declared: u32::MAX,
        available: u32::MAX,
    })?;

    let pad = (size & 1) as usize;
    let mut out = Vec::with_capacity(8 + body.len() + pad);
    out.extend_from_slice(id);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(body);
    if pad == 1 {
        out.push(0);
    }
    Ok(out)
}

/// Encode an `FVER` (Format Version) chunk *body* — the 4-byte
/// big-endian `timestamp`.
///
/// `timestamp` is [`AIFC_VERSION_1`] for any AIFF-C file Apple's draft
/// describes (§3.1 ¶ "ckSize is always 4. timeStamp ... = AIFCVersion1
/// = 0xA2805140"). The body is exactly 4 bytes; the chunk header
/// (`'FVER' + ckSize`) is the caller's responsibility, matching every
/// other write-side helper in this module — pair this with
/// [`frame_chunk`] (which never needs a pad byte here since the body
/// is even-length).
pub fn write_fver_chunk(timestamp: u32) -> [u8; 4] {
    timestamp.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_yields_no_chunks() {
        let mut it = ChunkIter::new(&[]);
        assert!(it.next().is_none());
    }

    #[test]
    fn single_even_size_chunk() {
        // 'COMM' + size=4 + 4 bytes payload, no pad.
        let buf = [b'C', b'O', b'M', b'M', 0, 0, 0, 4, 0xde, 0xad, 0xbe, 0xef];
        let chunks: Vec<_> = ChunkIter::new(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0].id, b"COMM");
        assert_eq!(chunks[0].size, 4);
        assert_eq!(chunks[0].data, &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn odd_size_chunk_consumes_pad_byte() {
        // 'NAME' + size=3 + 3 chars + 1 pad byte + 'COMT' + size=0.
        let buf = [
            b'N', b'A', b'M', b'E', 0, 0, 0, 3, b'A', b'B', b'C', 0x00, b'C', b'O', b'M', b'T', 0,
            0, 0, 0,
        ];
        let chunks: Vec<_> = ChunkIter::new(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(&chunks[0].id, b"NAME");
        assert_eq!(chunks[0].size, 3);
        assert_eq!(chunks[0].data, b"ABC");
        assert_eq!(&chunks[1].id, b"COMT");
        assert_eq!(chunks[1].size, 0);
        assert!(chunks[1].data.is_empty());
    }

    #[test]
    fn odd_size_chunk_at_eof_tolerates_missing_pad() {
        // Last chunk has odd size and the buffer ends right at the
        // payload — no pad byte after. Should still yield the chunk.
        let buf = [b'A', b'B', b'C', b'D', 0, 0, 0, 1, 0x42];
        let chunks: Vec<_> = ChunkIter::new(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0].id, b"ABCD");
        assert_eq!(chunks[0].size, 1);
        assert_eq!(chunks[0].data, &[0x42]);
    }

    #[test]
    fn truncated_header_errors() {
        let buf = [b'X', b'Y', b'Z']; // 3 bytes — no full header.
        let mut it = ChunkIter::new(&buf);
        assert!(matches!(it.next(), Some(Err(AiffError::Truncated(_)))));
        assert!(it.next().is_none());
    }

    #[test]
    fn oversized_chunk_errors() {
        // declares size=1_000_000 but only ~4 bytes follow.
        let buf = [b'X', b'Y', b'Z', b'W', 0x00, 0x0F, 0x42, 0x40, 0, 0, 0, 0];
        let mut it = ChunkIter::new(&buf);
        let first = it.next().unwrap();
        assert!(matches!(
            first,
            Err(AiffError::OversizedChunk { ref id, declared: 1_000_000, .. })
                if *id == *b"XYZW"
        ));
        assert!(it.next().is_none());
    }

    #[test]
    fn id_str_round_trips() {
        let buf = [b'C', b'O', b'M', b'M', 0, 0, 0, 0];
        let c = ChunkIter::new(&buf).next().unwrap().unwrap();
        assert_eq!(c.id_str(), Some("COMM"));
    }

    #[test]
    fn id_str_returns_none_for_non_ascii() {
        let buf = [0x00, 0xff, 0x10, 0x80, 0, 0, 0, 0];
        let c = ChunkIter::new(&buf).next().unwrap().unwrap();
        assert_eq!(c.id_str(), None);
    }

    #[test]
    fn frame_chunk_even_body_has_no_pad() {
        let framed = frame_chunk(b"COMT", &[0xde, 0xad, 0xbe, 0xef]).unwrap();
        // 8-byte header + 4-byte body, no pad.
        assert_eq!(framed.len(), 12);
        assert_eq!(&framed[..4], b"COMT");
        assert_eq!(&framed[4..8], &4u32.to_be_bytes());
        assert_eq!(&framed[8..], &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn frame_chunk_odd_body_gets_pad_byte() {
        let framed = frame_chunk(b"NAME", b"ABC").unwrap();
        // 8-byte header + 3-byte body + 1 pad byte = 12; ckSize stays 3.
        assert_eq!(framed.len(), 12);
        assert_eq!(&framed[4..8], &3u32.to_be_bytes());
        assert_eq!(&framed[8..11], b"ABC");
        assert_eq!(framed[11], 0x00);
    }

    #[test]
    fn frame_chunk_empty_body() {
        let framed = frame_chunk(b"FVER", &[]).unwrap();
        assert_eq!(framed.len(), 8);
        assert_eq!(&framed[..4], b"FVER");
        assert_eq!(&framed[4..8], &0u32.to_be_bytes());
    }

    #[test]
    fn frame_chunk_is_inverse_of_iter() {
        // Frame two chunks (odd then even), then walk them back with
        // ChunkIter — the pad byte must be transparent to the reader.
        let mut buf = frame_chunk(b"NAME", b"hello").unwrap(); // odd → pad
        buf.extend(frame_chunk(b"AUTH", b"me!!").unwrap()); // even → no pad
        let chunks: Vec<_> = ChunkIter::new(&buf).collect::<Result<_>>().unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(&chunks[0].id, b"NAME");
        assert_eq!(chunks[0].size, 5);
        assert_eq!(chunks[0].data, b"hello");
        assert_eq!(&chunks[1].id, b"AUTH");
        assert_eq!(chunks[1].size, 4);
        assert_eq!(chunks[1].data, b"me!!");
    }

    #[test]
    fn write_fver_round_trips_through_frame_and_iter() {
        let body = write_fver_chunk(AIFC_VERSION_1);
        assert_eq!(body, AIFC_VERSION_1.to_be_bytes());
        let framed = frame_chunk(b"FVER", &body).unwrap();
        let c = ChunkIter::new(&framed).next().unwrap().unwrap();
        assert_eq!(&c.id, b"FVER");
        assert_eq!(c.size, 4);
        let ts = u32::from_be_bytes([c.data[0], c.data[1], c.data[2], c.data[3]]);
        assert_eq!(ts, AIFC_VERSION_1);
        assert_eq!(ts, 0xA280_5140);
    }
}
