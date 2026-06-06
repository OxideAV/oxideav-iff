//! AIFF-C §14 chunk-precedence enforcement.
//!
//! Several local chunks for `FORM AIFC` may carry overlapping
//! information — the §14 example is loop endpoints surfaced both by
//! the Instrument Chunk and by MIDI System-Exclusive bytes inside a
//! MIDI Data Chunk. The §14 paragraph resolves the conflict by
//! ranking the eleven chunk classes the spec defines and declaring
//! the higher-ranked class authoritative:
//!
//! > "Information in the Common Chunk always takes precedence over
//! > conflicting information in any other chunk. The Application
//! > Specific Chunk always loses in conflicts with other chunks. By
//! > looking at the chunk hierarchy, for example, one sees that the
//! > loop points in the Instrument Chunk take precedence over
//! > conflicting loop points found in the MIDI Data Chunk."
//!
//! This module surfaces the §14 ordering as a typed [`ChunkClass`]
//! enum with a numeric [`ChunkClass::rank`] (lower rank == higher
//! precedence), plus convenience helpers on [`super::Form`] for
//! sorting the chunks a parsed FORM actually contains into spec
//! precedence order.
//!
//! Doc reference: `docs/audio/aiff/aiff-c.txt` §14 ("Chunk
//! Precedence"), lines 1209–1259 of the staged spec text.
//!
//! Note on §14 wording: the spec lists "Format Version Chunk" by
//! itself, then a blank line, then the "Highest precedence …
//! Lowest precedence" block running from Common to Application
//! Specific. The Format Version Chunk therefore sits above the
//! ranked block as the §3.1 dating/identification record rather
//! than as a competing information source — applications consult
//! `FVER.timestamp` for "which AIFF-C draft does this FORM follow?"
//! before they reason about any of the §14 conflicts, so it is
//! exposed as [`ChunkClass::FormatVersion`] with the canonical
//! rank `0`. The eleven §14 entries then occupy ranks `1..=11`.

use crate::aiff::form::Form;

/// One of the chunk classes the AIFF-C §14 precedence table ranks.
///
/// Variants are listed in the same order §14 prints them, top
/// (highest precedence) to bottom (lowest). The `repr(u8)` value is
/// the chunk's precedence rank — lower means higher precedence.
/// [`ChunkClass::rank`] returns the same number as a `u8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ChunkClass {
    /// `FVER` — the §3 Format Version Chunk. Sits above the §14
    /// "Highest precedence … Lowest precedence" block as the
    /// AIFF-C draft identifier ("the AIFF-C version") rather than
    /// as a competing-information source. Exposed at rank `0` so
    /// the §14 ranked classes occupy `1..=11` cleanly.
    FormatVersion = 0,
    /// `COMM` — the §4 Common Chunk. §14 ¶ "Information in the
    /// Common Chunk always takes precedence over conflicting
    /// information in any other chunk."
    Common = 1,
    /// `INST` — the §9 Instrument Chunk. §14 example: "the loop
    /// points in the Instrument Chunk take precedence over
    /// conflicting loop points found in the MIDI Data Chunk."
    Instrument = 2,
    /// `SAXL` — the §8.0 / Appendix D Sound Accelerator Chunk.
    Saxel = 3,
    /// `COMT` — the §7 Comments Chunk.
    Comments = 4,
    /// `MARK` — the §6 Marker Chunk.
    Marker = 5,
    /// `SSND` — the §5 Sound Data Chunk.
    SoundData = 6,
    /// `NAME` — the §13 Name text chunk.
    Name = 7,
    /// `AUTH` — the §13 Author text chunk.
    Author = 8,
    /// `(c) ` — the §13 Copyright text chunk.
    Copyright = 9,
    /// `ANNO` — the §13 Annotation text chunk. §14 ¶ "Annotation
    /// Chunk[s] -- in the order they appear in the FORM"; the
    /// document-order rule is honoured by
    /// [`Form::precedence_order`] which surfaces annotations in
    /// `Form::annotations` order.
    Annotation = 10,
    /// `AESD` — the §11 Audio Recording Chunk.
    AudioRecording = 11,
    /// `MIDI` — the §10 MIDI Data Chunk. §14 ranks MIDI below
    /// Audio Recording; conflicting loop-point bytes inside a MIDI
    /// SysEx packet therefore lose to the Instrument Chunk's loop
    /// points (the §14 worked example).
    MidiData = 12,
    /// `APPL` — the §12 Application Specific Chunk. §14 ¶ "The
    /// Application Specific Chunk always loses in conflicts with
    /// other chunks."
    ApplicationSpecific = 13,
}

impl ChunkClass {
    /// Precedence rank — lower is higher precedence. `FVER` is `0`;
    /// the §14 ranked block runs `1..=13` from `COMM` to `APPL`.
    ///
    /// The §14 paragraph itself ranks Common at the top and
    /// Application Specific at the bottom; the absolute numbers
    /// here are an internal ordering aid, but the **relative** order
    /// is the spec contract — see [`ChunkClass::higher_precedence_than`].
    pub fn rank(self) -> u8 {
        self as u8
    }

    /// On-wire ckID (4 ASCII bytes) for this chunk class, per the
    /// AIFF-C spec sections each variant cites.
    ///
    /// The `Copyright` variant returns `b"(c) "` exactly (open
    /// paren, lowercase `c`, close paren, space) per §13.0
    /// ¶ "the 'c' is lowercase and there is a space [0x20] after
    /// the close parenthesis." Callers comparing against `[u8; 4]`
    /// chunk-iter keys should therefore use this helper rather than
    /// hard-coding the ASCII bytes themselves.
    pub fn ck_id(self) -> &'static [u8; 4] {
        match self {
            ChunkClass::FormatVersion => b"FVER",
            ChunkClass::Common => b"COMM",
            ChunkClass::Instrument => b"INST",
            ChunkClass::Saxel => b"SAXL",
            ChunkClass::Comments => b"COMT",
            ChunkClass::Marker => b"MARK",
            ChunkClass::SoundData => b"SSND",
            ChunkClass::Name => b"NAME",
            ChunkClass::Author => b"AUTH",
            ChunkClass::Copyright => b"(c) ",
            ChunkClass::Annotation => b"ANNO",
            ChunkClass::AudioRecording => b"AESD",
            ChunkClass::MidiData => b"MIDI",
            ChunkClass::ApplicationSpecific => b"APPL",
        }
    }

    /// Compare two chunk classes by §14 precedence. Returns `true`
    /// when `self` is ranked higher (= has a lower rank number)
    /// than `other`. Equal classes return `false`.
    ///
    /// Worked §14 example:
    /// ```
    /// use oxideav_iff::aiff::ChunkClass;
    /// // §14 ¶ "the loop points in the Instrument Chunk take
    /// // precedence over conflicting loop points found in the
    /// // MIDI Data Chunk."
    /// assert!(ChunkClass::Instrument.higher_precedence_than(ChunkClass::MidiData));
    /// assert!(!ChunkClass::MidiData.higher_precedence_than(ChunkClass::Instrument));
    /// ```
    pub fn higher_precedence_than(self, other: ChunkClass) -> bool {
        (self as u8) < (other as u8)
    }

    /// Every §14 chunk class in spec precedence order, highest first.
    ///
    /// The §3.1 [`ChunkClass::FormatVersion`] sentinel is the first
    /// entry; the eleven §14 ranked classes follow `COMM` →
    /// `APPL`. The slice contains thirteen entries because §13
    /// splits the four text chunks (`NAME`, `AUTH`, `(c) `, `ANNO`)
    /// into four separate ranks and `MARK` / `SSND` / `AESD` / `MIDI`
    /// likewise each occupy their own rank.
    pub const fn all_in_precedence_order() -> &'static [ChunkClass] {
        &[
            ChunkClass::FormatVersion,
            ChunkClass::Common,
            ChunkClass::Instrument,
            ChunkClass::Saxel,
            ChunkClass::Comments,
            ChunkClass::Marker,
            ChunkClass::SoundData,
            ChunkClass::Name,
            ChunkClass::Author,
            ChunkClass::Copyright,
            ChunkClass::Annotation,
            ChunkClass::AudioRecording,
            ChunkClass::MidiData,
            ChunkClass::ApplicationSpecific,
        ]
    }
}

impl<'a> Form<'a> {
    /// Return the §14 precedence-ordered list of chunk classes
    /// **that this parsed FORM actually contains**.
    ///
    /// The returned vector follows the §14 ordering top-to-bottom
    /// (highest precedence first). Single-instance classes appear
    /// at most once. Multi-instance classes (§8.0 [`ChunkClass::Saxel`],
    /// §13.0 [`ChunkClass::Annotation`], §10.0 [`ChunkClass::MidiData`],
    /// §12.0 [`ChunkClass::ApplicationSpecific`]) appear once per
    /// instance, preserving the document order each `Vec<...>` field
    /// on [`Form`] already records — matching the §14 ¶ "Annotation
    /// Chunk[s] -- in the order they appear in the FORM" qualifier
    /// for `ANNO` and the analogous §8.0 ¶ "the saxels need not be
    /// ordered in any particular manner" / §10.0 ¶ "any number" /
    /// §12.0 ¶ "any number" wording for `SAXL`, `MIDI`, and `APPL`.
    ///
    /// Empty optional fields are skipped. A `FORM` carrying only a
    /// `COMM` + `SSND` therefore yields `[Common, SoundData]`
    /// (and `[FormatVersion, Common, SoundData]` for an AIFF-C with
    /// the §3.1 `FVER` chunk present).
    pub fn precedence_order(&self) -> Vec<ChunkClass> {
        let mut out = Vec::new();
        if self.fver_timestamp.is_some() {
            out.push(ChunkClass::FormatVersion);
        }
        // COMM is always present (parse rejects its absence).
        out.push(ChunkClass::Common);
        if self.instrument.is_some() {
            out.push(ChunkClass::Instrument);
        }
        // SAXL is multi-instance per §8.0.
        for _ in &self.saxels {
            out.push(ChunkClass::Saxel);
        }
        if self.comments.is_some() {
            out.push(ChunkClass::Comments);
        }
        if self.markers.is_some() {
            out.push(ChunkClass::Marker);
        }
        if self.sound.is_some() {
            out.push(ChunkClass::SoundData);
        }
        if self.name.is_some() {
            out.push(ChunkClass::Name);
        }
        if self.author.is_some() {
            out.push(ChunkClass::Author);
        }
        if self.copyright.is_some() {
            out.push(ChunkClass::Copyright);
        }
        // ANNO is multi-instance per §13.0; §14 ¶ "in the order they
        // appear in the FORM" — `Form::annotations` already records
        // document order so one entry per element preserves it.
        for _ in &self.annotations {
            out.push(ChunkClass::Annotation);
        }
        if self.aesd.is_some() {
            out.push(ChunkClass::AudioRecording);
        }
        // MIDI is multi-instance per §10.0.
        for _ in &self.midi {
            out.push(ChunkClass::MidiData);
        }
        // APPL is multi-instance per §12.0.
        for _ in &self.applications {
            out.push(ChunkClass::ApplicationSpecific);
        }
        out
    }

    /// Return the highest-precedence chunk class present in this
    /// FORM. Always `Some` because `COMM` is mandatory; for an
    /// AIFF-C FORM with a `FVER` chunk the answer is
    /// [`ChunkClass::FormatVersion`].
    pub fn highest_precedence_class(&self) -> Option<ChunkClass> {
        self.precedence_order().into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_order_matches_spec_order() {
        // §14 lists eleven ranked classes Common → APPL; with FVER
        // at rank 0 the full table is 14 entries (NAME/AUTH/(c) /ANNO
        // each occupy their own rank).
        let order = ChunkClass::all_in_precedence_order();
        assert_eq!(order.len(), 14);
        for (i, &c) in order.iter().enumerate() {
            assert_eq!(c.rank() as usize, i, "rank mismatch at index {i}");
        }
    }

    #[test]
    fn ck_id_round_trips_through_lookup() {
        for &c in ChunkClass::all_in_precedence_order() {
            let id = c.ck_id();
            // ckID must be exactly four bytes; the round-paren
            // copyright tag is the only entry that includes a
            // non-letter byte.
            assert_eq!(id.len(), 4);
            if c == ChunkClass::Copyright {
                assert_eq!(id, b"(c) ");
            } else {
                assert!(id.iter().all(|b| b.is_ascii()));
            }
        }
    }

    #[test]
    fn copyright_id_uses_round_paren_lowercase_c_space() {
        // §13.0 ¶ "the 'c' is lowercase and there is a space [0x20]
        // after the close parenthesis."
        assert_eq!(ChunkClass::Copyright.ck_id(), b"(c) ");
        let bytes = ChunkClass::Copyright.ck_id();
        assert_eq!(bytes[0], b'(');
        assert_eq!(bytes[1], b'c');
        assert_eq!(bytes[2], b')');
        assert_eq!(bytes[3], b' ');
    }

    #[test]
    fn higher_precedence_matches_section_14_example() {
        // §14 ¶ "the loop points in the Instrument Chunk take
        // precedence over conflicting loop points found in the MIDI
        // Data Chunk."
        assert!(ChunkClass::Instrument.higher_precedence_than(ChunkClass::MidiData));
        assert!(!ChunkClass::MidiData.higher_precedence_than(ChunkClass::Instrument));
    }

    #[test]
    fn common_outranks_everything_below_it() {
        // §14 ¶ "Information in the Common Chunk always takes
        // precedence over conflicting information in any other
        // chunk."
        for &c in ChunkClass::all_in_precedence_order() {
            if c == ChunkClass::Common || c == ChunkClass::FormatVersion {
                continue;
            }
            assert!(
                ChunkClass::Common.higher_precedence_than(c),
                "COMM should outrank {c:?}"
            );
        }
    }

    #[test]
    fn application_specific_loses_to_everything_above_it() {
        // §14 ¶ "The Application Specific Chunk always loses in
        // conflicts with other chunks."
        for &c in ChunkClass::all_in_precedence_order() {
            if c == ChunkClass::ApplicationSpecific {
                continue;
            }
            assert!(
                c.higher_precedence_than(ChunkClass::ApplicationSpecific),
                "{c:?} should outrank APPL"
            );
        }
    }

    #[test]
    fn higher_precedence_than_is_irreflexive() {
        for &c in ChunkClass::all_in_precedence_order() {
            assert!(
                !c.higher_precedence_than(c),
                "{c:?} should not outrank itself"
            );
        }
    }

    #[test]
    fn ord_matches_rank() {
        // The derived Ord is the rank ordering — handy for using
        // `BTreeMap<ChunkClass, …>` indexed by precedence.
        let mut shuffled = vec![
            ChunkClass::ApplicationSpecific,
            ChunkClass::Common,
            ChunkClass::MidiData,
            ChunkClass::FormatVersion,
            ChunkClass::Instrument,
        ];
        shuffled.sort();
        assert_eq!(
            shuffled,
            vec![
                ChunkClass::FormatVersion,
                ChunkClass::Common,
                ChunkClass::Instrument,
                ChunkClass::MidiData,
                ChunkClass::ApplicationSpecific,
            ]
        );
    }
}
