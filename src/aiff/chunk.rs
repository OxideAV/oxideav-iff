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
}
