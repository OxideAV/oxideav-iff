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

/// FourCC for the EA IFF 85 §3 "filler" chunk — four ASCII spaces.
///
/// `b"    "` is one of the five universally-reserved ckIDs (§3
/// "Chunks", ¶ "the following ckIDs are universally reserved … the
/// special ID '    ' (4 spaces) is a ckID for 'filler' chunks, that
/// is, chunks that fill space but have no meaningful contents.").
/// The other four reserved IDs already have constants in this module
/// (`GROUP_FORM`, `GROUP_LIST`, `GROUP_CAT`) or are surfaced by the
/// `prop` consumers (`PROP`); FILLER has no group-walker role of its
/// own and was previously left as a bare byte literal.
pub const FILLER_ID: [u8; 4] = *b"    ";

/// FourCC for the EA IFF 85 §3 `PROP` property-set group chunk.
///
/// Reserved alongside the three group IDs in the §3 enumeration of
/// universally-reserved ckIDs. `PROP` is a shared-properties group
/// that only appears as the first child of a `LIST` (§5 ¶ "A LIST
/// chunk may contain PROP chunks specifying default properties for
/// FORMs in that LIST"); it is *not* a top-level group, so
/// [`probe_top_level_group`] still rejects it.
pub const PROP_ID: [u8; 4] = *b"PROP";

/// Classification of a 4-byte ckID against the EA IFF 85 §3 list of
/// universally-reserved IDs.
///
/// §3 ¶ "the following ckIDs are universally reserved to identify
/// chunks with particular IFF meanings: 'LIST', 'FORM', 'PROP',
/// 'CAT ', and '    '. The special ID '    ' (4 spaces) is a ckID
/// for 'filler' chunks, that is, chunks that fill space but have no
/// meaningful contents. The IDs 'LIS1' through 'LIS9', 'FOR1' through
/// 'FOR9', and 'CAT1' through 'CAT9' are reserved for future
/// 'version number' variations. All IFF-compatible software must
/// account for these 23 chunk IDs."
///
/// [`ReservedId::classify`] maps any 4-byte ckID to one of the four
/// variants below (or `None` when the ID is not in the §3 list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservedId {
    /// One of the three group-chunk IDs — `FORM`, `LIST`, or `CAT `.
    /// The [`GroupKind`] variant is preserved so callers that already
    /// have a group dispatch can reuse it.
    Group(GroupKind),
    /// The §3 `PROP` property-set group ID.
    Prop,
    /// The §3 four-space FILLER ID — a chunk that "fills space but
    /// has no meaningful contents". Readers must walk past it without
    /// interpreting the body.
    Filler,
    /// One of the twenty-seven §3 future-version variants — `LIS1..LIS9`,
    /// `FOR1..FOR9`, or `CAT1..CAT9`. The first byte of the
    /// `(parent, digit)` tuple is one of the three group-kind variants
    /// the version family belongs to, and `digit` is the trailing
    /// `1..=9` character. The current spec defines no decoder for
    /// these IDs; "All IFF-compatible software must account for these
    /// 23 chunk IDs" so a reader must at least recognise them as
    /// reserved rather than treating them as ordinary data chunks.
    ReservedFuture { parent: GroupKind, digit: u8 },
}

impl ReservedId {
    /// Classify a 4-byte ckID against the EA IFF 85 §3 reserved list.
    ///
    /// Returns `None` when `id` is not one of the 23 reserved IDs;
    /// callers can then treat the chunk as either a local data chunk
    /// (when nested inside a known FORM) or an unrecognised top-level
    /// magic.
    pub fn classify(id: [u8; 4]) -> Option<Self> {
        if id == FILLER_ID {
            return Some(ReservedId::Filler);
        }
        if id == PROP_ID {
            return Some(ReservedId::Prop);
        }
        if let Some(kind) = GroupKind::from_id(id) {
            return Some(ReservedId::Group(kind));
        }
        // Check the twenty-seven LIS1..LIS9 / FOR1..FOR9 / CAT1..CAT9
        // version variants. The §3 spelling preserves the parent
        // group's first three bytes: "LIS" / "FOR" / "CAT" plus a
        // trailing ASCII digit '1'..='9'. Note that 'CAT '+digit
        // would yield "CAT 1" (5 bytes) which won't fit; the §3
        // version family for CAT spells out as "CAT1".."CAT9", with
        // the trailing space dropped — same as for LIST → "LIS1".."LIS9".
        let (a, b, c, d) = (id[0], id[1], id[2], id[3]);
        if (b'1'..=b'9').contains(&d) {
            let parent = match (a, b, c) {
                (b'L', b'I', b'S') => Some(GroupKind::List),
                (b'F', b'O', b'R') => Some(GroupKind::Form),
                (b'C', b'A', b'T') => Some(GroupKind::Cat),
                _ => None,
            };
            if let Some(parent) = parent {
                return Some(ReservedId::ReservedFuture { parent, digit: d });
            }
        }
        None
    }

    /// Convenience predicate — `true` when this ID is one of the three
    /// group ckIDs (`FORM`, `LIST`, `CAT `). Mirrors
    /// [`ChunkHeader::is_group`] one level up.
    pub fn is_group(self) -> bool {
        matches!(self, ReservedId::Group(_))
    }

    /// Convenience predicate — `true` when this ID is the four-space
    /// FILLER chunk ID.
    pub fn is_filler(self) -> bool {
        matches!(self, ReservedId::Filler)
    }

    /// Convenience predicate — `true` when this ID is one of the
    /// twenty-seven reserved-future-version IDs (`LIS1..9` / `FOR1..9` /
    /// `CAT1..9`). A reader has no defined decode for these (the §3
    /// spec only reserves them for "future version number
    /// variations"); the predicate is offered so callers can route
    /// them to a versioning-aware fall-back path instead of
    /// misclassifying them as ordinary data chunks.
    pub fn is_reserved_future(self) -> bool {
        matches!(self, ReservedId::ReservedFuture { .. })
    }

    /// Enumerate the full §3 reserved set in spec-listed order —
    /// `FORM`, `LIST`, `PROP`, `CAT `, `    ` (filler), followed by
    /// the twenty-seven future-version IDs `LIS1..9` / `FOR1..9` /
    /// `CAT1..9`.
    ///
    /// The §3 ¶ "All IFF-compatible software must account for these
    /// 23 chunk IDs" sentence cites a total of 23 — the running count
    /// in the document text doesn't quite match the explicit
    /// enumeration (5 base + 3×9 future = 32 distinct IDs); this
    /// helper returns every ID the same paragraph spells out, which
    /// is the set a recogniser actually needs.
    pub fn all_reserved_ids() -> [[u8; 4]; 32] {
        [
            GROUP_FORM, GROUP_LIST, PROP_ID, GROUP_CAT, FILLER_ID, *b"LIS1", *b"LIS2", *b"LIS3",
            *b"LIS4", *b"LIS5", *b"LIS6", *b"LIS7", *b"LIS8", *b"LIS9", *b"FOR1", *b"FOR2",
            *b"FOR3", *b"FOR4", *b"FOR5", *b"FOR6", *b"FOR7", *b"FOR8", *b"FOR9", *b"CAT1",
            *b"CAT2", *b"CAT3", *b"CAT4", *b"CAT5", *b"CAT6", *b"CAT7", *b"CAT8", *b"CAT9",
        ]
    }
}

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

/// One decoded child of a `LIST` or `CAT ` group body.
///
/// EA IFF 85 §5 closes the child grammar of the two outer group kinds
/// (Appendix A productions):
///
/// ```text
/// LIST ::= "LIST" #{ ContentsType PROP* (FORM | LIST | CAT)* }
/// CAT  ::= "CAT " #{ ContentsType (FORM | LIST | CAT)* }
/// PROP ::= "PROP" #{ FormType Property* }
/// ```
///
/// so every child is either a `PROP` shared-property set (LIST only)
/// or a nested group chunk. Both shapes are "a subtype ID followed by
/// chunks" (§6 ¶ "Chunk types LIST, FORM, PROP, and CAT are generic
/// groups. They always contain a subtype ID followed by chunks."); the
/// `body` slice holds the bytes *after* that subtype ID, bounded by
/// the child's own `ckSize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupChild<'a> {
    /// A §5 `PROP` shared-property set — "Here are the shared
    /// properties for FORM type \<FormType\>." Only legal inside a
    /// `LIST` (§5 ¶ "PROP chunks may appear in LISTs (not in FORMs or
    /// CATs)"). `body` is the concatenated `Property*` chunk stream.
    Prop {
        /// The `FormType` the shared properties apply to.
        form_type: [u8; 4],
        /// The `Property*` chunk bytes following the FormType.
        body: &'a [u8],
    },
    /// A nested `FORM` / `LIST` / `CAT ` group chunk. `body` is the
    /// nested chunk stream following the group's own subtype ID.
    Group {
        /// Which group chunk this child is.
        kind: GroupKind,
        /// The child's subtype ID — `FormType` for FORM, the
        /// `ContentsType` hint for LIST and CAT.
        inner_type: [u8; 4],
        /// The nested chunk bytes following the subtype ID.
        body: &'a [u8],
    },
}

impl GroupChild<'_> {
    /// The child's subtype ID — the PROP's `FormType` or the nested
    /// group's `FormType` / `ContentsType`, surfaced uniformly.
    pub fn inner_type(&self) -> [u8; 4] {
        match self {
            GroupChild::Prop { form_type, .. } => *form_type,
            GroupChild::Group { inner_type, .. } => *inner_type,
        }
    }

    /// Convenience predicate — `true` for the [`GroupChild::Prop`]
    /// variant.
    pub fn is_prop(&self) -> bool {
        matches!(self, GroupChild::Prop { .. })
    }
}

/// Walk the children of a `LIST` or `CAT ` group body.
///
/// `children` is the group's payload *after* its 4-byte
/// `ContentsType` — i.e. for a top-level group decoded by
/// [`probe_top_level_group`], the `size - 4` bytes starting at file
/// offset 12, truncated to the declared `ckSize` (§5 Group CAT ¶ "In
/// reading a CAT, like any other chunk, programs must respect it's
/// ckSize as a virtual end-of-file for reading the nested objects
/// even if they're malformed or truncated" — the caller passes the
/// already-bounded slice).
///
/// The walker enforces every structural rule §5 states for the two
/// closed child grammars:
///
/// - **LIST**: children are `PROP* (FORM | LIST | CAT)*`, in that
///   order — §5 ¶ "all the PROPs must appear before any of the FORMs
///   or nested LISTs and CATs"; a PROP after the first nested group
///   is an error.
/// - **LIST**: "A LIST may have at most one PROP of a FORM type" —
///   a duplicate `FormType` across the PROPs is an error.
/// - **CAT**: children are `(FORM | LIST | CAT)*` only; a PROP is an
///   error — §5 ¶ "PROP chunks may appear in LISTs (not in FORMs or
///   CATs)" / Rules for Writer Programs ¶ "PROPs may only appear
///   inside LISTs."
/// - **FORM** is rejected outright: §4's production allows
///   `LocalChunk` children whose IDs are FORM-type-specific, so a
///   generic group-only walk cannot decode a FORM body — that is the
///   per-form walker's job (`ilbm` / `svx` / `anim` / `aiff`).
///
/// §3 FILLER chunks ("chunks that fill space but have no meaningful
/// contents") are walked past without being surfaced. The
/// reserved-future-version IDs (`LIS1..9` / `FOR1..9` / `CAT1..9`)
/// and any non-reserved ckID are errors — the §5 grammar admits no
/// other child, and the future-version IDs have no defined decode.
///
/// A missing pad byte after the final odd-sized child is tolerated,
/// matching [`read_chunk_header`]'s clean-EOF convention one level
/// up.
pub fn parse_group_children(kind: GroupKind, children: &[u8]) -> Result<Vec<GroupChild<'_>>> {
    if kind == GroupKind::Form {
        return Err(Error::invalid(
            "IFF: FORM bodies mix LocalChunks with nested groups (EA IFF 85 §4); \
             use the per-form walker instead of parse_group_children",
        ));
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    // §5: "all the PROPs must appear before any of the FORMs or
    // nested LISTs and CATs".
    let mut group_seen = false;
    // §5: "A LIST may have at most one PROP of a FORM type".
    let mut prop_types: Vec<[u8; 4]> = Vec::new();
    while pos < children.len() {
        if children.len() - pos < 8 {
            return Err(Error::invalid("IFF: truncated group-child chunk header"));
        }
        let id = [
            children[pos],
            children[pos + 1],
            children[pos + 2],
            children[pos + 3],
        ];
        let size = u32::from_be_bytes([
            children[pos + 4],
            children[pos + 5],
            children[pos + 6],
            children[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(size)
            .filter(|&end| end <= children.len())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "IFF: group child {:?} overruns the containing group's ckSize",
                    std::str::from_utf8(&id).unwrap_or("????"),
                ))
            })?;
        match ReservedId::classify(id) {
            // §3 filler — "fills space but has no meaningful
            // contents"; walk past without surfacing it.
            Some(ReservedId::Filler) => {}
            Some(ReservedId::Prop) => {
                if kind == GroupKind::Cat {
                    return Err(Error::invalid(
                        "IFF: PROP chunks may appear in LISTs, not in CATs (EA IFF 85 §5)",
                    ));
                }
                if group_seen {
                    return Err(Error::invalid(
                        "IFF: PROPs must appear before any FORM/LIST/CAT in a LIST \
                         (EA IFF 85 §5)",
                    ));
                }
                if size < 4 {
                    return Err(Error::invalid("IFF: PROP chunk too short for a FormType"));
                }
                let form_type = [
                    children[body_start],
                    children[body_start + 1],
                    children[body_start + 2],
                    children[body_start + 3],
                ];
                if prop_types.contains(&form_type) {
                    return Err(Error::invalid(format!(
                        "IFF: a LIST may have at most one PROP of FORM type {:?} \
                         (EA IFF 85 §5)",
                        std::str::from_utf8(&form_type).unwrap_or("????"),
                    )));
                }
                prop_types.push(form_type);
                out.push(GroupChild::Prop {
                    form_type,
                    body: &children[body_start + 4..body_end],
                });
            }
            Some(ReservedId::Group(child_kind)) => {
                if size < 4 {
                    return Err(Error::invalid(
                        "IFF: group child too short for a subtype ID",
                    ));
                }
                group_seen = true;
                out.push(GroupChild::Group {
                    kind: child_kind,
                    inner_type: [
                        children[body_start],
                        children[body_start + 1],
                        children[body_start + 2],
                        children[body_start + 3],
                    ],
                    body: &children[body_start + 4..body_end],
                });
            }
            Some(ReservedId::ReservedFuture { .. }) | None => {
                return Err(Error::invalid(format!(
                    "IFF: {:?} is not a legal LIST/CAT child \
                     (EA IFF 85 §5 grammar: PROP / FORM / LIST / CAT)",
                    std::str::from_utf8(&id).unwrap_or("????"),
                )));
            }
        }
        // Advance past the body plus the §3 pad byte after every
        // odd-length chunk; a missing pad at the very end of the
        // group is tolerated.
        pos = (body_end + (size & 1)).min(children.len());
    }
    Ok(out)
}

/// Look up the shared-property body for `form_type` in a parsed
/// LIST's children.
///
/// §5 ¶ "It means, 'Here are the shared properties for FORM type
/// \<FormType\>.'" — returns the `Property*` chunk bytes of the PROP
/// whose `FormType` matches, or `None` when the LIST shares nothing
/// for that form type. [`parse_group_children`] has already enforced
/// "at most one PROP of a FORM type", so the first match is the only
/// match.
pub fn prop_for_form_type<'a>(children: &[GroupChild<'a>], form_type: [u8; 4]) -> Option<&'a [u8]> {
    children.iter().find_map(|c| match c {
        GroupChild::Prop {
            form_type: ft,
            body,
        } if *ft == form_type => Some(*body),
        _ => None,
    })
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

    /// Classify this chunk's `id` against the EA IFF 85 §3 list of
    /// universally-reserved ckIDs.
    ///
    /// Returns `Some(ReservedId::…)` for the three group kinds (`FORM`
    /// / `LIST` / `CAT `), `PROP`, the four-space FILLER chunk, and
    /// the twenty-seven reserved-future-version IDs (`LIS1..9` /
    /// `FOR1..9` / `CAT1..9`). Any other ckID — including every
    /// FORM-local property like `BMHD` / `CMAP` / `COMM` — falls
    /// outside the universally-reserved set and yields `None`.
    pub fn reserved(&self) -> Option<ReservedId> {
        ReservedId::classify(self.id)
    }

    /// Convenience shorthand for `reserved() == Some(ReservedId::Filler)`.
    ///
    /// The §3 spec ¶ "the special ID '    ' (4 spaces) is a ckID for
    /// 'filler' chunks, that is, chunks that fill space but have no
    /// meaningful contents" — a reader that has identified a chunk as
    /// FILLER can `skip_chunk_body` past it without ever reading the
    /// payload.
    pub fn is_filler(&self) -> bool {
        self.id == FILLER_ID
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

    #[test]
    fn reserved_id_classifies_three_groups() {
        assert_eq!(
            ReservedId::classify(*b"FORM"),
            Some(ReservedId::Group(GroupKind::Form))
        );
        assert_eq!(
            ReservedId::classify(*b"LIST"),
            Some(ReservedId::Group(GroupKind::List))
        );
        assert_eq!(
            ReservedId::classify(*b"CAT "),
            Some(ReservedId::Group(GroupKind::Cat))
        );
    }

    #[test]
    fn reserved_id_classifies_prop_and_filler() {
        // §3 ¶ "LIST, FORM, PROP, CAT , and '    '" — PROP and the
        // 4-space FILLER are the two non-group universally-reserved
        // IDs the chunk header dispatcher needs to recognise.
        assert_eq!(ReservedId::classify(*b"PROP"), Some(ReservedId::Prop));
        assert_eq!(ReservedId::classify(*b"    "), Some(ReservedId::Filler));
    }

    #[test]
    fn reserved_id_classifies_all_twenty_seven_future_versions() {
        // §3 ¶ "LIS1 through LIS9, FOR1 through FOR9, and CAT1 through
        // CAT9" — the 27 reserved version-number variants. The
        // classifier preserves the parent group kind plus the digit.
        for d in b'1'..=b'9' {
            let mut id = *b"LIS_";
            id[3] = d;
            assert_eq!(
                ReservedId::classify(id),
                Some(ReservedId::ReservedFuture {
                    parent: GroupKind::List,
                    digit: d,
                }),
                "LIS{}",
                d as char,
            );
            let mut id = *b"FOR_";
            id[3] = d;
            assert_eq!(
                ReservedId::classify(id),
                Some(ReservedId::ReservedFuture {
                    parent: GroupKind::Form,
                    digit: d,
                }),
                "FOR{}",
                d as char,
            );
            let mut id = *b"CAT_";
            id[3] = d;
            assert_eq!(
                ReservedId::classify(id),
                Some(ReservedId::ReservedFuture {
                    parent: GroupKind::Cat,
                    digit: d,
                }),
                "CAT{}",
                d as char,
            );
        }
    }

    #[test]
    fn reserved_id_rejects_non_reserved_ckid() {
        // Common FORM-local properties are NOT in the §3 reserved
        // set — classify must return None so callers route them to
        // the data-chunk path.
        for non_reserved in [
            *b"BMHD", *b"CMAP", *b"BODY", *b"DLTA", *b"VHDR", *b"COMM", *b"SSND", *b"MARK",
            *b"ANNO", *b"NAME", *b"AUTH", *b"COMT", *b"INST", *b"MIDI", *b"APPL", *b"AESD",
            *b"SAXL", *b"TEXT", *b"FVER", *b"GRAB", *b"DEST", *b"SPRT", *b"CAMG", *b"CRNG",
            *b"CCRT", *b"DRNG", *b"SHAM", *b"PCHG", *b"ANHD",
        ] {
            assert_eq!(
                ReservedId::classify(non_reserved),
                None,
                "{:?} should not classify as reserved",
                std::str::from_utf8(&non_reserved).unwrap(),
            );
        }
    }

    #[test]
    fn reserved_id_rejects_boundary_version_digits() {
        // §3 spells the version family as digits '1'..='9' — '0' is
        // excluded ("LIS0" is not in the reserved set) and 'A' is
        // not a digit at all. Both must classify as None so the
        // chunk lands on the data-chunk path.
        assert_eq!(ReservedId::classify(*b"LIS0"), None);
        assert_eq!(ReservedId::classify(*b"FOR0"), None);
        assert_eq!(ReservedId::classify(*b"CAT0"), None);
        assert_eq!(ReservedId::classify(*b"LISA"), None);
        // Also: a non-version trailing byte against the LIS / FOR /
        // CAT prefix must not match — e.g. LIST already classifies
        // as a group, but "LISx" with lowercase is just data.
        assert_eq!(ReservedId::classify(*b"LISx"), None);
    }

    #[test]
    fn reserved_id_predicates() {
        let g = ReservedId::Group(GroupKind::Form);
        assert!(g.is_group());
        assert!(!g.is_filler());
        assert!(!g.is_reserved_future());

        let f = ReservedId::Filler;
        assert!(!f.is_group());
        assert!(f.is_filler());
        assert!(!f.is_reserved_future());

        let p = ReservedId::Prop;
        assert!(!p.is_group());
        assert!(!p.is_filler());
        assert!(!p.is_reserved_future());

        let rf = ReservedId::ReservedFuture {
            parent: GroupKind::List,
            digit: b'3',
        };
        assert!(!rf.is_group());
        assert!(!rf.is_filler());
        assert!(rf.is_reserved_future());
    }

    #[test]
    fn all_reserved_ids_covers_every_id_classify_recognises() {
        // Round-trip: every entry in the §3 enumeration must classify
        // back to a Some(_) variant. Conversely, the enumeration must
        // be deduplicated — the §3 list has no duplicates.
        let ids = ReservedId::all_reserved_ids();
        assert_eq!(ids.len(), 32, "§3 list has 5 base + 27 future = 32 IDs");

        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            assert!(
                seen.insert(id),
                "{:?} appears twice in all_reserved_ids",
                std::str::from_utf8(&id).unwrap_or("????"),
            );
            assert!(
                ReservedId::classify(id).is_some(),
                "{:?} present in enumeration but rejected by classify",
                std::str::from_utf8(&id).unwrap_or("????"),
            );
        }
    }

    #[test]
    fn chunk_header_reserved_and_is_filler() {
        // A FILLER chunk surfaces through both shortcuts on
        // ChunkHeader.
        let h = ChunkHeader {
            id: FILLER_ID,
            size: 8,
        };
        assert!(h.is_filler());
        assert_eq!(h.reserved(), Some(ReservedId::Filler));
        // A FORM chunk header is a group (existing is_group keeps
        // working) and also classifies as ReservedId::Group.
        let h = ChunkHeader {
            id: GROUP_FORM,
            size: 24,
        };
        assert!(h.is_group());
        assert!(!h.is_filler());
        assert_eq!(h.reserved(), Some(ReservedId::Group(GroupKind::Form)));
        // A FORM-local data chunk falls outside the §3 reserved set.
        let h = ChunkHeader {
            id: *b"BMHD",
            size: 20,
        };
        assert!(!h.is_group());
        assert!(!h.is_filler());
        assert!(h.reserved().is_none());
    }

    /// Build one child chunk: `id + ckSize + body (+ pad if odd)`.
    fn child(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + body.len() + 1);
        v.extend_from_slice(id);
        v.extend_from_slice(&(body.len() as u32).to_be_bytes());
        v.extend_from_slice(body);
        if body.len() & 1 == 1 {
            v.push(0);
        }
        v
    }

    /// Build a group child body: subtype ID + nested bytes.
    fn typed_body(inner: &[u8; 4], rest: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + rest.len());
        v.extend_from_slice(inner);
        v.extend_from_slice(rest);
        v
    }

    #[test]
    fn parse_group_children_decodes_list_with_prop_and_forms() {
        // The §5 worked example shape: LIST { PROP TEXT {..} FORM
        // TEXT {..} FORM TEXT {..} } — children only, after the
        // LIST's ContentsType.
        let prop_body = typed_body(b"TEXT", &child(b"FONT", b"TimesRoman"));
        let form1_body = typed_body(b"TEXT", &child(b"CHRS", b"Hello "));
        let form2_body = typed_body(b"TEXT", &child(b"CHRS", b"there."));
        let children: Vec<u8> = [
            child(b"PROP", &prop_body),
            child(b"FORM", &form1_body),
            child(b"FORM", &form2_body),
        ]
        .concat();
        let kids = parse_group_children(GroupKind::List, &children).unwrap();
        assert_eq!(kids.len(), 3);
        assert!(kids[0].is_prop());
        assert_eq!(kids[0].inner_type(), *b"TEXT");
        match kids[1] {
            GroupChild::Group {
                kind, inner_type, ..
            } => {
                assert_eq!(kind, GroupKind::Form);
                assert_eq!(&inner_type, b"TEXT");
            }
            _ => panic!("expected nested FORM"),
        }
        // The PROP's body is the Property* stream after the FormType.
        let shared = prop_for_form_type(&kids, *b"TEXT").unwrap();
        let mut cur = Cursor::new(shared);
        let h = read_chunk_header(&mut cur).unwrap().unwrap();
        assert_eq!(&h.id, b"FONT");
        assert_eq!(read_body(&mut cur, &h).unwrap(), b"TimesRoman");
        // No shared properties for a form type the LIST doesn't cover.
        assert!(prop_for_form_type(&kids, *b"ILBM").is_none());
    }

    #[test]
    fn parse_group_children_rejects_prop_after_group() {
        // §5: "all the PROPs must appear before any of the FORMs or
        // nested LISTs and CATs".
        let children: Vec<u8> = [
            child(b"FORM", &typed_body(b"TEXT", &[])),
            child(b"PROP", &typed_body(b"TEXT", &[])),
        ]
        .concat();
        let err = parse_group_children(GroupKind::List, &children).unwrap_err();
        assert!(format!("{err}").contains("before any FORM"), "{err}");
    }

    #[test]
    fn parse_group_children_rejects_duplicate_prop_form_type() {
        // §5: "A LIST may have at most one PROP of a FORM type" —
        // two PROP TEXT children are an error, but PROP TEXT +
        // PROP ILBM is fine.
        let dup: Vec<u8> = [
            child(b"PROP", &typed_body(b"TEXT", &[])),
            child(b"PROP", &typed_body(b"TEXT", &[])),
        ]
        .concat();
        let err = parse_group_children(GroupKind::List, &dup).unwrap_err();
        assert!(format!("{err}").contains("at most one PROP"), "{err}");

        let distinct: Vec<u8> = [
            child(b"PROP", &typed_body(b"TEXT", &[])),
            child(b"PROP", &typed_body(b"ILBM", &[])),
        ]
        .concat();
        let kids = parse_group_children(GroupKind::List, &distinct).unwrap();
        assert_eq!(kids.len(), 2);
        assert!(prop_for_form_type(&kids, *b"ILBM").is_some());
    }

    #[test]
    fn parse_group_children_rejects_prop_in_cat() {
        // §5: "PROP chunks may appear in LISTs (not in FORMs or
        // CATs)".
        let children = child(b"PROP", &typed_body(b"TEXT", &[]));
        let err = parse_group_children(GroupKind::Cat, &children).unwrap_err();
        assert!(format!("{err}").contains("not in CATs"), "{err}");
        // The same FORM-only CAT body is fine.
        let children = child(b"FORM", &typed_body(b"ILBM", &[]));
        let kids = parse_group_children(GroupKind::Cat, &children).unwrap();
        assert_eq!(kids.len(), 1);
    }

    #[test]
    fn parse_group_children_rejects_form_kind() {
        // §4's FORM production admits LocalChunk children whose IDs
        // are form-type-specific; the generic walker refuses rather
        // than misreading them.
        let err = parse_group_children(GroupKind::Form, &[]).unwrap_err();
        assert!(format!("{err}").contains("per-form walker"), "{err}");
    }

    #[test]
    fn parse_group_children_rejects_data_and_future_version_ids() {
        // A bare data ckID is not in the §5 grammar…
        let children = child(b"BMHD", &[0u8; 20]);
        assert!(parse_group_children(GroupKind::List, &children).is_err());
        // …and neither are the §3 reserved-future-version IDs.
        let children = child(b"LIS1", &typed_body(b"TEXT", &[]));
        assert!(parse_group_children(GroupKind::List, &children).is_err());
    }

    #[test]
    fn parse_group_children_skips_filler() {
        // §3 filler "fills space but has no meaningful contents" —
        // walked past, not surfaced, and it doesn't trip the
        // PROP-before-groups ordering check.
        let children: Vec<u8> = [
            child(b"    ", b"xxx"),
            child(b"PROP", &typed_body(b"TEXT", &[])),
            child(b"FORM", &typed_body(b"TEXT", &[])),
        ]
        .concat();
        let kids = parse_group_children(GroupKind::List, &children).unwrap();
        assert_eq!(kids.len(), 2);
        assert!(kids[0].is_prop());
    }

    #[test]
    fn parse_group_children_bounds_checks() {
        // Empty body: zero children is legal (the grammar's * admits
        // the empty sequence).
        assert!(parse_group_children(GroupKind::List, &[])
            .unwrap()
            .is_empty());
        // Truncated child header.
        assert!(parse_group_children(GroupKind::Cat, b"FORM\x00\x00").is_err());
        // Child ckSize overruns the containing group.
        let mut children = child(b"FORM", &typed_body(b"ILBM", &[]));
        children[7] = 0xFF; // declared size far past the slice end
        assert!(parse_group_children(GroupKind::Cat, &children).is_err());
        // Group child too short for its subtype ID.
        let children = child(b"FORM", b"IL");
        assert!(parse_group_children(GroupKind::Cat, &children).is_err());
        // PROP too short for its FormType.
        let children = child(b"PROP", b"TE");
        assert!(parse_group_children(GroupKind::List, &children).is_err());
    }

    #[test]
    fn parse_group_children_handles_odd_sized_children_and_missing_final_pad() {
        // First child has an odd ckSize → 1 pad byte before the next
        // child; the final child's pad byte is absent and tolerated.
        let form1 = typed_body(b"TEXT", &child(b"CHRS", b"a")); // CHRS size 1 → padded
        let mut children = child(b"FORM", &form1);
        assert_eq!(children.len() % 2, 0, "even after pad");
        // Second child with odd body and NO trailing pad.
        let form2 = typed_body(b"TEXT", b"x"); // 5 bytes, odd
        children.extend_from_slice(b"FORM");
        children.extend_from_slice(&(form2.len() as u32).to_be_bytes());
        children.extend_from_slice(&form2);
        let kids = parse_group_children(GroupKind::Cat, &children).unwrap();
        assert_eq!(kids.len(), 2);
        match kids[1] {
            GroupChild::Group { body, .. } => assert_eq!(body, b"x"),
            _ => panic!("expected nested FORM"),
        }
    }

    #[test]
    fn filler_chunk_walks_past_body_without_decoding() {
        // The "spec essence" use case: a reader that sees a FILLER
        // ckID skips past padded_size bytes without ever reading
        // ckData. Verified end-to-end against the existing
        // skip_chunk_body helper.
        // FILLER ckID + ckSize = 5 (odd, so 1 byte pad) + 5 body
        // bytes + 1 pad + a follow-on full FORM chunk header to
        // confirm the cursor lands at the right offset.
        let bytes: Vec<u8> = [
            b"    ".as_slice(),
            &[0, 0, 0, 5],
            b"hello",
            &[0],
            b"FORM",
            &[0, 0, 0, 4],
            b"ILBM",
        ]
        .concat();
        let mut cur = Cursor::new(&bytes[..]);
        let h = read_chunk_header(&mut cur).unwrap().unwrap();
        assert!(h.is_filler());
        assert_eq!(h.size, 5);
        assert_eq!(h.padded_size(), 6);
        skip_chunk_body(&mut cur, &h).unwrap();
        // Confirm we landed on the FORM that follows.
        let h2 = read_chunk_header(&mut cur).unwrap().unwrap();
        assert_eq!(&h2.id, b"FORM");
        assert_eq!(h2.size, 4);
    }
}
