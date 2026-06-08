//! IFF 85 chunk reader (EA IFF 85, 1985).
//!
//! Each chunk is `[4-byte FourCC id][4-byte BE size][size bytes of data]`
//! with a single pad byte following if `size` is odd. Group chunks (`FORM`,
//! `LIST`, `CAT `) reserve the first 4 bytes of their data for a "form
//! type" and then contain nested chunks.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{Error, Result};

/// FourCC constants for the three group chunk types.
pub const GROUP_FORM: [u8; 4] = *b"FORM";
pub const GROUP_LIST: [u8; 4] = *b"LIST";
pub const GROUP_CAT: [u8; 4] = *b"CAT ";

/// Which of the three EA IFF 85 group-chunk kinds occupies the
/// top-level slot of the file.
///
/// EA IFF 85 §6 ("An IFF file is just a single chunk of type FORM,
/// LIST, or CAT") restricts a conforming file to exactly one of these
/// three at offset 0, optionally followed by trailing bytes the spec
/// requires readers to ignore. The 4-byte ID immediately after the
/// 8-byte chunk header carries the FORM's `FormType` (e.g. `ILBM`,
/// `8SVX`, `AIFF`, `AIFC`, `ANIM`) or the LIST/CAT's `ContentsType`
/// (§5: `LIST    `+`PROP` and `CAT `'s untyped grouping); both are
/// surfaced uniformly by [`TopLevelGroup::inner_type`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    /// `FORM` — a single self-contained data object (§4).
    Form,
    /// `LIST` — a typed group of FORMs that can share PROP chunks (§5).
    List,
    /// `CAT ` — an untyped concatenation of data objects (§5).
    Cat,
}

impl GroupKind {
    /// The 4-byte chunk ID this kind serialises to on the wire.
    pub fn id(self) -> [u8; 4] {
        match self {
            GroupKind::Form => GROUP_FORM,
            GroupKind::List => GROUP_LIST,
            GroupKind::Cat => GROUP_CAT,
        }
    }

    fn from_id(id: [u8; 4]) -> Option<Self> {
        match id {
            GROUP_FORM => Some(GroupKind::Form),
            GROUP_LIST => Some(GroupKind::List),
            GROUP_CAT => Some(GroupKind::Cat),
            _ => None,
        }
    }
}

/// Decoded top-level EA IFF 85 group header.
///
/// Produced by [`probe_top_level_group`] (in-memory buffer) and
/// [`read_top_level_group`] (`Read`-style stream). Holds just the
/// outer envelope: the [`GroupKind`] (`FORM`/`LIST`/`CAT `), the
/// `size` word the file declares, and the 4-byte inner type ID that
/// follows the chunk header.
///
/// `size` is the wire `ckSize` — it counts the inner type ID **plus**
/// every nested chunk byte but excludes the 8-byte outer header itself
/// (per §3 "Chunks"). Use [`TopLevelGroup::declared_total_len`] to get
/// the full envelope length (`8 + size`, with the trailing pad byte if
/// `size` is odd), which matches the "expected file length" most
/// downstream walkers compare against the buffer size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopLevelGroup {
    /// Which group chunk occupies the top-level slot.
    pub kind: GroupKind,
    /// The wire `ckSize` of the outer group chunk — body bytes
    /// (including the 4-byte inner type ID) but not the 8-byte header.
    pub size: u32,
    /// The 4-byte inner type ID — `FormType` for FORM/PROP, the
    /// `ContentsType` hint for LIST and CAT.
    pub inner_type: [u8; 4],
}

impl TopLevelGroup {
    /// `kind.id()` shorthand — the 4-byte chunk ID this group
    /// serialises to (e.g. `*b"FORM"`).
    pub fn kind_id(&self) -> [u8; 4] {
        self.kind.id()
    }

    /// Lossy UTF-8 view of [`inner_type`], for diagnostics. EA IFF 85
    /// restricts IDs to printable ASCII (§3), so a well-formed file
    /// always produces clean text here.
    ///
    /// [`inner_type`]: TopLevelGroup::inner_type
    pub fn inner_type_str(&self) -> &str {
        std::str::from_utf8(&self.inner_type).unwrap_or("????")
    }

    /// Total expected on-disk length of this top-level group:
    /// `8` (outer chunk header) + `size` + `1` if `size` is odd (the
    /// final pad byte every IFF chunk gets per §3).
    ///
    /// Returns a `u64` because legal `size` values can run up to
    /// `u32::MAX`, and adding the header plus pad byte can overflow
    /// `u32` at the boundary.
    pub fn declared_total_len(&self) -> u64 {
        (self.size as u64) + 8 + (self.size & 1) as u64
    }
}

/// Probe a byte buffer for a single top-level EA IFF 85 group chunk.
///
/// Returns `Ok(None)` when `buf` is too short to hold the 12 bytes
/// required to identify a group (the chunk header plus the inner type
/// ID). Returns an [`Error`] only when the first 4 bytes are a
/// well-formed FourCC that is *not* `FORM`/`LIST`/`CAT ` — this
/// matches EA IFF 85 §6's "If it doesn't start with 'FORM', 'LIST',
/// or 'CAT ', it's not IFF" guidance and keeps the probe usable as
/// the front-half of a format-dispatch check.
///
/// The 4-byte `ckSize` word is parsed as big-endian, matching every
/// IFF FourCC field on the wire; it is not validated against
/// `buf.len()` here — callers that need that check use
/// [`TopLevelGroup::declared_total_len`] against their full file
/// length (see e.g. the per-form `parse_*` walkers).
pub fn probe_top_level_group(buf: &[u8]) -> Result<Option<TopLevelGroup>> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let id = [buf[0], buf[1], buf[2], buf[3]];
    let Some(kind) = GroupKind::from_id(id) else {
        return Err(Error::invalid(format!(
            "IFF: not a top-level group chunk (got {:?})",
            std::str::from_utf8(&id).unwrap_or("????"),
        )));
    };
    let size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let inner_type = [buf[8], buf[9], buf[10], buf[11]];
    Ok(Some(TopLevelGroup {
        kind,
        size,
        inner_type,
    }))
}

/// Stream variant of [`probe_top_level_group`].
///
/// Reads exactly 12 bytes (the 8-byte outer chunk header plus the
/// 4-byte inner type ID) from `r` and returns the decoded envelope.
/// `Ok(None)` is returned only when the stream is empty at the very
/// first byte (matching [`read_chunk_header`]'s clean-EOF
/// convention); a partial read of fewer than 12 bytes is reported as
/// an error since EA IFF 85 §6 requires a top-level group at offset
/// 0. A well-formed FourCC that is not `FORM`/`LIST`/`CAT ` is
/// surfaced as an `Error::invalid` so callers can fall through to a
/// different container probe.
pub fn read_top_level_group<R: Read + ?Sized>(r: &mut R) -> Result<Option<TopLevelGroup>> {
    // Pull the outer chunk header first so an empty stream returns
    // Ok(None) (clean EOF) rather than an error.
    let Some(header) = read_chunk_header(r)? else {
        return Ok(None);
    };
    let Some(kind) = GroupKind::from_id(header.id) else {
        return Err(Error::invalid(format!(
            "IFF: not a top-level group chunk (got {:?})",
            std::str::from_utf8(&header.id).unwrap_or("????"),
        )));
    };
    let inner_type = read_form_type(r)?;
    Ok(Some(TopLevelGroup {
        kind,
        size: header.size,
        inner_type,
    }))
}

/// Header of a single IFF chunk.
#[derive(Clone, Copy, Debug)]
pub struct ChunkHeader {
    pub id: [u8; 4],
    pub size: u32,
}

impl ChunkHeader {
    pub fn id_str(&self) -> &str {
        std::str::from_utf8(&self.id).unwrap_or("????")
    }

    /// Number of bytes to advance past the chunk body including any pad byte.
    pub fn padded_size(&self) -> u64 {
        (self.size as u64) + (self.size & 1) as u64
    }

    pub fn is_group(&self) -> bool {
        matches!(self.id, GROUP_FORM | GROUP_LIST | GROUP_CAT)
    }
}

/// Read a single chunk header, returning `Ok(None)` at clean EOF.
pub fn read_chunk_header<R: Read + ?Sized>(r: &mut R) -> Result<Option<ChunkHeader>> {
    let mut buf = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        match r.read(&mut buf[got..]) {
            Ok(0) => {
                return if got == 0 {
                    Ok(None)
                } else {
                    Err(Error::invalid("IFF: truncated chunk header"))
                };
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let id = [buf[0], buf[1], buf[2], buf[3]];
    let size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok(Some(ChunkHeader { id, size }))
}

/// Read the 4-byte form-type identifier at the start of a group chunk's body.
pub fn read_form_type<R: Read + ?Sized>(r: &mut R) -> Result<[u8; 4]> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(b)
}

/// Read the entire body of a chunk (excluding the pad byte).
pub fn read_body<R: Read + ?Sized>(r: &mut R, header: &ChunkHeader) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; header.size as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Skip a chunk's body and its pad byte.
pub fn skip_chunk_body<R: Seek + ?Sized>(r: &mut R, header: &ChunkHeader) -> Result<()> {
    let n = header.padded_size();
    if n > 0 {
        r.seek(SeekFrom::Current(n as i64))?;
    }
    Ok(())
}

/// Skip the pad byte after a fully-consumed chunk body (if `size` is odd).
pub fn skip_pad<R: Seek + ?Sized>(r: &mut R, header: &ChunkHeader) -> Result<()> {
    if header.size & 1 == 1 {
        r.seek(SeekFrom::Current(1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn padded_size_even_odd() {
        let a = ChunkHeader {
            id: *b"BODY",
            size: 10,
        };
        assert_eq!(a.padded_size(), 10);
        let b = ChunkHeader {
            id: *b"NAME",
            size: 9,
        };
        assert_eq!(b.padded_size(), 10);
    }

    #[test]
    fn read_chunk_header_parses_bytes() {
        let bytes = [b'V', b'H', b'D', b'R', 0, 0, 0, 20];
        let mut cur = Cursor::new(&bytes[..]);
        let h = read_chunk_header(&mut cur).unwrap().unwrap();
        assert_eq!(&h.id, b"VHDR");
        assert_eq!(h.size, 20);
    }

    #[test]
    fn probe_top_level_group_returns_none_for_short_buf() {
        // Anything shorter than the 12 bytes needed to decode kind +
        // ckSize + inner type is reported as "not enough yet" rather
        // than an error: a container-registry probe pipeline keeps
        // walking past it.
        assert!(probe_top_level_group(&[]).unwrap().is_none());
        assert!(probe_top_level_group(b"FORM").unwrap().is_none());
        assert!(probe_top_level_group(b"FORM\x00\x00\x00\x10ILB")
            .unwrap()
            .is_none());
    }

    #[test]
    fn probe_top_level_group_decodes_form_ilbm() {
        // FORM ckSize=0x0000_0010 ILBM ...  — minimal well-formed
        // header. ckSize counts the form type + body bytes only.
        let bytes = b"FORM\x00\x00\x00\x10ILBM\x00\x00\x00\x00";
        let g = probe_top_level_group(bytes).unwrap().unwrap();
        assert_eq!(g.kind, GroupKind::Form);
        assert_eq!(g.size, 0x10);
        assert_eq!(&g.inner_type, b"ILBM");
        assert_eq!(g.kind_id(), GROUP_FORM);
        assert_eq!(g.inner_type_str(), "ILBM");
        // 8 (header) + 0x10 (size, already even — no pad byte) = 24.
        assert_eq!(g.declared_total_len(), 24);
    }

    #[test]
    fn probe_top_level_group_decodes_list_and_cat() {
        // LIST with contents-type ILBM and a ckSize of zero (an empty
        // shell, but a legal one for the front-half probe).
        let bytes = b"LIST\x00\x00\x00\x04ILBM";
        let g = probe_top_level_group(bytes).unwrap().unwrap();
        assert_eq!(g.kind, GroupKind::List);
        assert_eq!(g.size, 4);
        assert_eq!(&g.inner_type, b"ILBM");
        assert_eq!(g.declared_total_len(), 12);

        // CAT  with the "JJJJ" blank contents-type the spec calls out
        // for heterogeneous catalogues (ea-iff-85 §5 "Group CAT").
        let bytes = b"CAT \x00\x00\x00\x04JJJJ";
        let g = probe_top_level_group(bytes).unwrap().unwrap();
        assert_eq!(g.kind, GroupKind::Cat);
        assert_eq!(&g.kind_id(), b"CAT ");
        assert_eq!(g.inner_type_str(), "JJJJ");
    }

    #[test]
    fn probe_top_level_group_odd_size_adds_pad_byte() {
        // ckSize = 5 (odd) → declared_total_len = 8 + 5 + 1 pad = 14.
        let bytes = b"FORM\x00\x00\x00\x05ANIM\x00";
        let g = probe_top_level_group(bytes).unwrap().unwrap();
        assert_eq!(g.size, 5);
        assert_eq!(g.declared_total_len(), 14);
    }

    #[test]
    fn probe_top_level_group_rejects_non_group_id() {
        // A bare data-chunk magic at offset 0 is "not IFF" per
        // ea-iff-85 §6 — the probe escalates so the caller can fall
        // through to a different container.
        let err = probe_top_level_group(b"VHDR\x00\x00\x00\x00????").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not a top-level group chunk"), "{msg}");
    }

    #[test]
    fn read_top_level_group_returns_none_at_clean_eof() {
        let mut cur = Cursor::new(&[][..]);
        assert!(read_top_level_group(&mut cur).unwrap().is_none());
    }

    #[test]
    fn read_top_level_group_matches_probe() {
        let bytes = b"FORM\x00\x00\x00\x12AIFC\x00\x00\x00\x00\x00\x00";
        let probed = probe_top_level_group(bytes).unwrap().unwrap();
        let mut cur = Cursor::new(&bytes[..]);
        let streamed = read_top_level_group(&mut cur).unwrap().unwrap();
        assert_eq!(probed, streamed);
        assert_eq!(streamed.kind, GroupKind::Form);
        assert_eq!(&streamed.inner_type, b"AIFC");
    }

    #[test]
    fn read_top_level_group_rejects_non_group_id() {
        let bytes = b"VHDR\x00\x00\x00\x00????";
        let mut cur = Cursor::new(&bytes[..]);
        let err = read_top_level_group(&mut cur).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not a top-level group chunk"), "{msg}");
    }

    #[test]
    fn read_top_level_group_partial_header_is_error() {
        // 4 bytes of FORM but cut off before ckSize completes —
        // EA IFF 85 §6 requires a full top-level group at offset 0,
        // so this is a truncation error rather than a clean EOF.
        let bytes = b"FORM\x00\x00";
        let mut cur = Cursor::new(&bytes[..]);
        assert!(read_top_level_group(&mut cur).is_err());
    }

    #[test]
    fn group_kind_round_trip() {
        for k in [GroupKind::Form, GroupKind::List, GroupKind::Cat] {
            assert_eq!(GroupKind::from_id(k.id()), Some(k));
        }
        assert_eq!(GroupKind::from_id(*b"PROP"), None);
    }
}
