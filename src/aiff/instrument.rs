//! `INST` (Instrument) chunk parser — sampler playback parameters.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §9 (Instrument Chunk) the wire
//! layout is fixed at 20 bytes of ckData:
//!
//! ```text
//! ckID         : 'INST'
//! ckSize       : int32             (always 20)
//! baseNote     : char              (MIDI note 0..=127, middle C = 60)
//! detune       : char              (signed; cents -50..=+50)
//! lowNote      : char              (MIDI note 0..=127)
//! highNote     : char              (MIDI note 0..=127)
//! lowVelocity  : char              (1..=127)
//! highVelocity : char              (1..=127)
//! gain         : short             (signed; decibels)
//! sustainLoop  : Loop              (6 bytes — see below)
//! releaseLoop  : Loop              (6 bytes — see below)
//! ```
//!
//! and each `Loop` is:
//!
//! ```text
//! playMode  : short                (0 = none, 1 = fwd, 2 = ping-pong)
//! beginLoop : MarkerId (i16, > 0 when referenced)
//! endLoop   : MarkerId (i16, > 0 when referenced)
//! ```
//!
//! The chunk is optional and the spec forbids more than one INST per
//! FORM; the FORM walker enforces that via [`AiffError::DuplicateChunk`].
//!
//! Per §9 ¶ "playMode": when a loop's `beginLoop` does NOT precede
//! its `endLoop` in the MARK list, the spec explicitly says "[If
//! this is not the case, then ignore this loop segment. No looping
//! takes place.]" — i.e. ill-ordered loop endpoints are *not* a
//! parse error, they are a runtime no-op. We surface that as
//! [`Loop::is_effective`] / [`InstrumentChunk::resolve_sustain_loop`]
//! / [`InstrumentChunk::resolve_release_loop`] so the caller can ask
//! "what does the spec say to actually play?" and get `None` for the
//! ignored cases.
//!
//! Field-range constraints the spec calls out explicitly:
//!
//! * `baseNote` / `lowNote` / `highNote` — MIDI note numbers
//!   (`0..=127`).
//! * `detune` — cents, `-50..=+50`.
//! * `lowVelocity` / `highVelocity` — MIDI velocity, `1..=127`
//!   ("1 [lowest velocity] through 127 [highest velocity]").
//! * `playMode` — `0..=2`.
//!
//! Out-of-range values produce [`AiffError::InvalidValue`].

use crate::aiff::error::{AiffError, Result};
use crate::aiff::marker::{Marker, MarkerChunk};

/// `playMode` values defined by §9 ¶ "Looping".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayMode {
    /// `NoLooping` (0) — ignore the loop's begin/end markers; the
    /// caller plays the sound through without repeating.
    None,
    /// `ForwardLooping` (1) — play the loop segment forwards over
    /// and over.
    Forward,
    /// `ForwardBackwardLooping` (2) — play forwards, then backwards,
    /// then forwards again ("ping-pong"). Automatically seamless.
    ForwardBackward,
}

impl PlayMode {
    /// Decode the `playMode` short.
    pub(crate) fn from_short(v: i16) -> Result<Self> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Forward),
            2 => Ok(Self::ForwardBackward),
            _ => Err(AiffError::InvalidValue {
                what: "playMode",
                value: v as i64,
            }),
        }
    }
}

/// A single `Loop` substructure of [`InstrumentChunk`].
///
/// Holds the on-wire `playMode` (decoded into [`PlayMode`]) and the
/// two `MarkerId`s referencing entries in the FORM's `MARK` chunk.
/// Spec §9: "beginLoop and endLoop are marker ids that mark the
/// begin and end positions of the loop segment."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loop {
    /// `playMode` — what looping behaviour the sampler should
    /// apply.
    pub play_mode: PlayMode,
    /// `beginLoop` — `MarkerId` of the start-of-loop marker. Per
    /// §6.0 a `MarkerId` is a positive non-zero `i16`; the parser
    /// allows any `i16` here so that a `PlayMode::None` loop can
    /// carry the typical 0/0 endpoint pair some encoders emit
    /// when there's no looping.
    pub begin_loop: i16,
    /// `endLoop` — `MarkerId` of the end-of-loop marker.
    pub end_loop: i16,
}

impl Loop {
    /// Returns `true` when this loop should actually be played per
    /// §9: `playMode != None`, both endpoints are positive marker
    /// ids, and `beginLoop != endLoop`. Use
    /// [`InstrumentChunk::resolve_sustain_loop`] /
    /// [`InstrumentChunk::resolve_release_loop`] to additionally
    /// require that *both* endpoints actually resolve in a given
    /// [`MarkerChunk`] and that the begin marker's `position`
    /// precedes the end marker's.
    pub fn is_effective(&self) -> bool {
        !matches!(self.play_mode, PlayMode::None)
            && self.begin_loop > 0
            && self.end_loop > 0
            && self.begin_loop != self.end_loop
    }
}

/// A `Loop` resolved against a [`MarkerChunk`].
///
/// Returned by [`InstrumentChunk::resolve_sustain_loop`] /
/// [`InstrumentChunk::resolve_release_loop`] when the loop is
/// playable per §9 (effective `playMode`, both endpoints found in
/// the MARK list, and the begin frame strictly precedes the end
/// frame). Borrows the matched [`Marker`] entries so the caller
/// can read the marker names alongside the positions without a
/// second `by_id` walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedLoop<'m> {
    /// The decoded `playMode` that triggered this resolution.
    pub play_mode: PlayMode,
    /// The marker entry referenced by `beginLoop`.
    pub begin: &'m Marker,
    /// The marker entry referenced by `endLoop`.
    pub end: &'m Marker,
}

/// Parsed contents of an `INST` chunk.
///
/// All raw on-wire fields are preserved; the [`PlayMode`] decode and
/// the [`Loop`] sub-structure split out the bits the spec calls out
/// as a discriminated value but every other field is round-trippable
/// at the byte level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentChunk {
    /// `baseNote` — MIDI note (`0..=127`) of the originally
    /// recorded pitch. Middle C is 60.
    pub base_note: u8,
    /// `detune` — cents (`-50..=+50`). Negative lowers pitch,
    /// positive raises it.
    pub detune: i8,
    /// `lowNote` — lowest MIDI note in the suggested playback
    /// range (`0..=127`).
    pub low_note: u8,
    /// `highNote` — highest MIDI note in the suggested playback
    /// range (`0..=127`).
    pub high_note: u8,
    /// `lowVelocity` — lowest MIDI velocity (`1..=127`) for which
    /// this sound is suggested.
    pub low_velocity: u8,
    /// `highVelocity` — highest MIDI velocity (`1..=127`).
    pub high_velocity: u8,
    /// `gain` — playback gain adjustment in decibels (signed
    /// `i16`; 0 = no change).
    pub gain: i16,
    /// `sustainLoop` — what to play while the note is held.
    pub sustain_loop: Loop,
    /// `releaseLoop` — what to play after the note is released.
    pub release_loop: Loop,
}

impl InstrumentChunk {
    /// Resolve the sustain loop against a [`MarkerChunk`]. Returns
    /// `None` when:
    ///
    /// * `playMode == NoLooping`, OR
    /// * either `beginLoop` or `endLoop` is not a positive
    ///   `MarkerId`, OR
    /// * either id is missing from the supplied [`MarkerChunk`], OR
    /// * the begin marker's frame position is *not* strictly less
    ///   than the end marker's — per §9 ¶ "beginLoop and endLoop":
    ///   "[If this is not the case, then ignore this loop segment.
    ///   No looping takes place.]"
    pub fn resolve_sustain_loop<'m>(&self, markers: &'m MarkerChunk) -> Option<ResolvedLoop<'m>> {
        resolve_against(&self.sustain_loop, markers)
    }

    /// Resolve the release loop. Same rules as
    /// [`Self::resolve_sustain_loop`].
    pub fn resolve_release_loop<'m>(&self, markers: &'m MarkerChunk) -> Option<ResolvedLoop<'m>> {
        resolve_against(&self.release_loop, markers)
    }
}

fn resolve_against<'m>(lp: &Loop, markers: &'m MarkerChunk) -> Option<ResolvedLoop<'m>> {
    if !lp.is_effective() {
        return None;
    }
    let begin = markers.by_id(lp.begin_loop)?;
    let end = markers.by_id(lp.end_loop)?;
    // §9: "The begin position must be less than the end position so
    // the loop segment will have a positive length. [If this is not
    // the case, then ignore this loop segment. No looping takes
    // place.]"
    if begin.position >= end.position {
        return None;
    }
    Some(ResolvedLoop {
        play_mode: lp.play_mode,
        begin,
        end,
    })
}

/// Parse the body of an `INST` chunk. `data` is the ckData slice
/// the chunk walker handed up — the 8-byte ckID/ckSize prefix and
/// the pad byte (if any) have already been stripped.
///
/// Spec §9: `ckDataSize` is always 20. We accept exactly 20 bytes
/// and reject anything shorter as [`AiffError::Truncated`]; longer
/// is reported as [`AiffError::InvalidValue { what: "INST ckSize", ... }`]
/// since the spec is explicit ("always 20") and a trailing pad byte
/// inside the chunk would already have been stripped by the chunk
/// walker.
pub fn parse_instrument_chunk(data: &[u8]) -> Result<InstrumentChunk> {
    if data.len() < 20 {
        return Err(AiffError::Truncated("INST chunk"));
    }
    if data.len() > 20 {
        return Err(AiffError::InvalidValue {
            what: "INST ckSize",
            value: data.len() as i64,
        });
    }

    let base_note = data[0];
    let detune = data[1] as i8;
    let low_note = data[2];
    let high_note = data[3];
    let low_velocity = data[4];
    let high_velocity = data[5];
    let gain = i16::from_be_bytes([data[6], data[7]]);

    // MIDI note numbers occupy the low 7 bits. The wire field is a
    // `char` (signed byte on the original Mac), but valid notes are
    // `0..=127`, i.e. the unsigned byte range minus the high bit.
    check_midi_note("baseNote", base_note)?;
    check_midi_note("lowNote", low_note)?;
    check_midi_note("highNote", high_note)?;
    check_detune(detune)?;
    check_velocity("lowVelocity", low_velocity)?;
    check_velocity("highVelocity", high_velocity)?;

    let sustain_loop = parse_loop(&data[8..14])?;
    let release_loop = parse_loop(&data[14..20])?;

    Ok(InstrumentChunk {
        base_note,
        detune,
        low_note,
        high_note,
        low_velocity,
        high_velocity,
        gain,
        sustain_loop,
        release_loop,
    })
}

/// Encode an [`InstrumentChunk`] body in wire format — exactly the
/// 20 bytes the spec calls for. The chunk header itself
/// (`'INST' + ckSize`) is the caller's responsibility.
///
/// Field order matches §9 ¶ "Instrument Chunk Format":
/// `baseNote(1) + detune(1) + lowNote(1) + highNote(1) +
/// lowVelocity(1) + highVelocity(1) + gain(2 BE) + sustainLoop(6)
/// + releaseLoop(6)`. The `Loop` substructure writes
/// `playMode(2 BE) + beginLoop(2 BE) + endLoop(2 BE)`.
pub fn write_instrument_chunk(inst: &InstrumentChunk) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[0] = inst.base_note;
    out[1] = inst.detune as u8;
    out[2] = inst.low_note;
    out[3] = inst.high_note;
    out[4] = inst.low_velocity;
    out[5] = inst.high_velocity;
    out[6..8].copy_from_slice(&inst.gain.to_be_bytes());
    write_loop_into(&mut out[8..14], &inst.sustain_loop);
    write_loop_into(&mut out[14..20], &inst.release_loop);
    out
}

fn write_loop_into(slot: &mut [u8], lp: &Loop) {
    debug_assert_eq!(slot.len(), 6);
    let mode = match lp.play_mode {
        PlayMode::None => 0_i16,
        PlayMode::Forward => 1_i16,
        PlayMode::ForwardBackward => 2_i16,
    };
    slot[0..2].copy_from_slice(&mode.to_be_bytes());
    slot[2..4].copy_from_slice(&lp.begin_loop.to_be_bytes());
    slot[4..6].copy_from_slice(&lp.end_loop.to_be_bytes());
}

fn parse_loop(bytes: &[u8]) -> Result<Loop> {
    // 6-byte Loop: playMode(2) + beginLoop(2) + endLoop(2).
    debug_assert_eq!(bytes.len(), 6);
    let play_mode = PlayMode::from_short(i16::from_be_bytes([bytes[0], bytes[1]]))?;
    let begin_loop = i16::from_be_bytes([bytes[2], bytes[3]]);
    let end_loop = i16::from_be_bytes([bytes[4], bytes[5]]);
    Ok(Loop {
        play_mode,
        begin_loop,
        end_loop,
    })
}

fn check_midi_note(what: &'static str, n: u8) -> Result<()> {
    // MIDI notes are 7-bit (0..=127); the high bit is reserved.
    if n > 127 {
        return Err(AiffError::InvalidValue {
            what,
            value: n as i64,
        });
    }
    Ok(())
}

fn check_detune(d: i8) -> Result<()> {
    if !(-50..=50).contains(&d) {
        return Err(AiffError::InvalidValue {
            what: "detune",
            value: d as i64,
        });
    }
    Ok(())
}

fn check_velocity(what: &'static str, v: u8) -> Result<()> {
    if !(1..=127).contains(&v) {
        return Err(AiffError::InvalidValue {
            what,
            value: v as i64,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aiff::marker::Marker;

    /// Pack the 20-byte INST ckData body in spec field order.
    #[allow(clippy::too_many_arguments)]
    fn pack(
        base_note: u8,
        detune: i8,
        low_note: u8,
        high_note: u8,
        low_velocity: u8,
        high_velocity: u8,
        gain: i16,
        sustain: (i16, i16, i16),
        release: (i16, i16, i16),
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(20);
        v.push(base_note);
        v.push(detune as u8);
        v.push(low_note);
        v.push(high_note);
        v.push(low_velocity);
        v.push(high_velocity);
        v.extend_from_slice(&gain.to_be_bytes());
        v.extend_from_slice(&sustain.0.to_be_bytes());
        v.extend_from_slice(&sustain.1.to_be_bytes());
        v.extend_from_slice(&sustain.2.to_be_bytes());
        v.extend_from_slice(&release.0.to_be_bytes());
        v.extend_from_slice(&release.1.to_be_bytes());
        v.extend_from_slice(&release.2.to_be_bytes());
        v
    }

    fn fixed_markers() -> MarkerChunk {
        MarkerChunk {
            markers: vec![
                Marker {
                    id: 1,
                    position: 0,
                    name: "begin".into(),
                },
                Marker {
                    id: 2,
                    position: 1_000,
                    name: "end".into(),
                },
                Marker {
                    id: 3,
                    position: 500,
                    name: "mid".into(),
                },
            ],
        }
    }

    #[test]
    fn parses_minimal_no_looping() {
        // Middle C, no detune, suggested C3..C5 range, full velocity,
        // 0 gain, both loops NoLooping with 0/0 endpoints (which the
        // spec allows because playMode == 0 means "ignore these loop
        // points during playback").
        let body = pack(60, 0, 48, 72, 1, 127, 0, (0, 0, 0), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert_eq!(inst.base_note, 60);
        assert_eq!(inst.detune, 0);
        assert_eq!(inst.low_note, 48);
        assert_eq!(inst.high_note, 72);
        assert_eq!(inst.low_velocity, 1);
        assert_eq!(inst.high_velocity, 127);
        assert_eq!(inst.gain, 0);
        assert_eq!(inst.sustain_loop.play_mode, PlayMode::None);
        assert_eq!(inst.release_loop.play_mode, PlayMode::None);
        assert_eq!(inst.sustain_loop.begin_loop, 0);
        assert_eq!(inst.sustain_loop.end_loop, 0);
        assert!(!inst.sustain_loop.is_effective());
        assert!(!inst.release_loop.is_effective());
    }

    #[test]
    fn parses_forward_loop() {
        // Sustain loop: id 1 (frame 0) → id 2 (frame 1000).
        let body = pack(60, 0, 0, 127, 1, 127, 0, (1, 1, 2), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert_eq!(inst.sustain_loop.play_mode, PlayMode::Forward);
        assert_eq!(inst.sustain_loop.begin_loop, 1);
        assert_eq!(inst.sustain_loop.end_loop, 2);
        assert!(inst.sustain_loop.is_effective());

        let markers = fixed_markers();
        let r = inst.resolve_sustain_loop(&markers).unwrap();
        assert_eq!(r.play_mode, PlayMode::Forward);
        assert_eq!(r.begin.id, 1);
        assert_eq!(r.end.id, 2);
        assert_eq!(r.begin.position, 0);
        assert_eq!(r.end.position, 1_000);
        assert!(inst.resolve_release_loop(&markers).is_none());
    }

    #[test]
    fn parses_ping_pong_loop() {
        let body = pack(60, 0, 0, 127, 1, 127, 0, (2, 1, 2), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert_eq!(inst.sustain_loop.play_mode, PlayMode::ForwardBackward);
    }

    #[test]
    fn parses_release_loop_separately() {
        // No sustain, but the release loop is active id 1 → id 2.
        let body = pack(60, 0, 0, 127, 1, 127, 0, (0, 0, 0), (1, 1, 2));
        let inst = parse_instrument_chunk(&body).unwrap();
        let markers = fixed_markers();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
        let r = inst.resolve_release_loop(&markers).unwrap();
        assert_eq!(r.play_mode, PlayMode::Forward);
        assert_eq!(r.begin.id, 1);
        assert_eq!(r.end.id, 2);
    }

    #[test]
    fn detune_negative_round_trips() {
        let body = pack(60, -25, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert_eq!(inst.detune, -25);
    }

    #[test]
    fn gain_signed_round_trips() {
        let body = pack(60, 0, 0, 127, 1, 127, -6, (0, 0, 0), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert_eq!(inst.gain, -6);
    }

    #[test]
    fn rejects_unknown_play_mode() {
        // playMode == 3 is undefined.
        let body = pack(60, 0, 0, 127, 1, 127, 0, (3, 1, 2), (0, 0, 0));
        let r = parse_instrument_chunk(&body);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "playMode",
                value: 3
            })
        ));
    }

    #[test]
    fn rejects_midi_note_out_of_range() {
        let body = pack(128, 0, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));
        let r = parse_instrument_chunk(&body);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "baseNote",
                value: 128
            })
        ));
    }

    #[test]
    fn rejects_detune_out_of_range() {
        let body = pack(60, 51, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));
        let r = parse_instrument_chunk(&body);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "detune",
                value: 51
            })
        ));
        let body = pack(60, -51, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));
        let r = parse_instrument_chunk(&body);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "detune",
                value: -51
            })
        ));
    }

    #[test]
    fn rejects_zero_velocity() {
        // Velocity 0 is reserved on MIDI (note-off); spec demands
        // 1..=127 here.
        let body = pack(60, 0, 0, 127, 0, 127, 0, (0, 0, 0), (0, 0, 0));
        let r = parse_instrument_chunk(&body);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "lowVelocity",
                value: 0
            })
        ));
    }

    #[test]
    fn rejects_truncated_chunk() {
        let r = parse_instrument_chunk(&[0u8; 19]);
        assert!(matches!(r, Err(AiffError::Truncated("INST chunk"))));
    }

    #[test]
    fn rejects_oversized_chunk() {
        // Spec is explicit: ckDataSize is always 20.
        let r = parse_instrument_chunk(&[0u8; 21]);
        assert!(matches!(
            r,
            Err(AiffError::InvalidValue {
                what: "INST ckSize",
                ..
            })
        ));
    }

    #[test]
    fn resolve_returns_none_when_begin_marker_missing() {
        // Sustain loop references id 99 (not in fixed_markers).
        let body = pack(60, 0, 0, 127, 1, 127, 0, (1, 99, 2), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        let markers = fixed_markers();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
    }

    #[test]
    fn resolve_returns_none_when_end_marker_missing() {
        let body = pack(60, 0, 0, 127, 1, 127, 0, (1, 1, 99), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        let markers = fixed_markers();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
    }

    #[test]
    fn resolve_returns_none_when_begin_after_end_in_marker_table() {
        // beginLoop = id 2 (frame 1000), endLoop = id 1 (frame 0).
        // Spec §9: "[If this is not the case, then ignore this loop
        // segment. No looping takes place.]"
        let body = pack(60, 0, 0, 127, 1, 127, 0, (1, 2, 1), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        let markers = fixed_markers();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
    }

    #[test]
    fn resolve_returns_none_when_endpoints_share_position() {
        // Two markers at the same frame — zero-length loop, also
        // covered by the begin < end rule.
        let markers = MarkerChunk {
            markers: vec![
                Marker {
                    id: 1,
                    position: 500,
                    name: "a".into(),
                },
                Marker {
                    id: 2,
                    position: 500,
                    name: "b".into(),
                },
            ],
        };
        let body = pack(60, 0, 0, 127, 1, 127, 0, (1, 1, 2), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
    }

    #[test]
    fn resolve_returns_none_when_play_mode_is_none() {
        // Endpoints are valid and ordered, but playMode is NoLooping
        // so the spec says "ignore these loop points during playback".
        let body = pack(60, 0, 0, 127, 1, 127, 0, (0, 1, 2), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        let markers = fixed_markers();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
    }

    #[test]
    fn resolve_returns_none_when_endpoints_are_equal_ids() {
        // beginLoop == endLoop; not a valid loop segment.
        let body = pack(60, 0, 0, 127, 1, 127, 0, (1, 1, 1), (0, 0, 0));
        let inst = parse_instrument_chunk(&body).unwrap();
        assert!(!inst.sustain_loop.is_effective());
        let markers = fixed_markers();
        assert!(inst.resolve_sustain_loop(&markers).is_none());
    }

    #[test]
    fn write_round_trips_through_parse() {
        let inst = InstrumentChunk {
            base_note: 60,
            detune: -25,
            low_note: 48,
            high_note: 72,
            low_velocity: 1,
            high_velocity: 127,
            gain: -6,
            sustain_loop: Loop {
                play_mode: PlayMode::Forward,
                begin_loop: 1,
                end_loop: 2,
            },
            release_loop: Loop {
                play_mode: PlayMode::ForwardBackward,
                begin_loop: 3,
                end_loop: 4,
            },
        };
        let bytes = write_instrument_chunk(&inst);
        let parsed = parse_instrument_chunk(&bytes).unwrap();
        assert_eq!(parsed, inst);
    }

    #[test]
    fn write_matches_hand_packed_layout() {
        let inst = InstrumentChunk {
            base_note: 60,
            detune: 5,
            low_note: 36,
            high_note: 96,
            low_velocity: 1,
            high_velocity: 127,
            gain: 3,
            sustain_loop: Loop {
                play_mode: PlayMode::Forward,
                begin_loop: 1,
                end_loop: 2,
            },
            release_loop: Loop {
                play_mode: PlayMode::None,
                begin_loop: 0,
                end_loop: 0,
            },
        };
        let bytes = write_instrument_chunk(&inst);
        let expected = pack(60, 5, 36, 96, 1, 127, 3, (1, 1, 2), (0, 0, 0));
        assert_eq!(bytes.as_slice(), expected.as_slice());
    }

    #[test]
    fn write_emits_exactly_20_bytes() {
        let inst = InstrumentChunk {
            base_note: 60,
            detune: 0,
            low_note: 0,
            high_note: 127,
            low_velocity: 1,
            high_velocity: 127,
            gain: 0,
            sustain_loop: Loop {
                play_mode: PlayMode::None,
                begin_loop: 0,
                end_loop: 0,
            },
            release_loop: Loop {
                play_mode: PlayMode::None,
                begin_loop: 0,
                end_loop: 0,
            },
        };
        let bytes = write_instrument_chunk(&inst);
        assert_eq!(bytes.len(), 20);
    }

    #[test]
    fn write_preserves_negative_detune_and_gain() {
        let inst = InstrumentChunk {
            base_note: 60,
            detune: -50,
            low_note: 0,
            high_note: 127,
            low_velocity: 1,
            high_velocity: 127,
            gain: -12,
            sustain_loop: Loop {
                play_mode: PlayMode::None,
                begin_loop: 0,
                end_loop: 0,
            },
            release_loop: Loop {
                play_mode: PlayMode::None,
                begin_loop: 0,
                end_loop: 0,
            },
        };
        let bytes = write_instrument_chunk(&inst);
        let parsed = parse_instrument_chunk(&bytes).unwrap();
        assert_eq!(parsed.detune, -50);
        assert_eq!(parsed.gain, -12);
    }
}
