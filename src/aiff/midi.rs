//! `MIDI` (MIDI Data) chunk parser — raw MIDI byte stream container.
//!
//! Per `docs/audio/aiff/aiff-c.txt` §10.0 (MIDI Data Chunk) the wire
//! layout is:
//!
//! ```text
//! ckID       : 'MIDI'
//! ckSize     : int32        (byte length of MIDIdata)
//! MIDIdata   : unsigned char[]  (raw MIDI byte stream)
//! ```
//!
//! §10.0 quotes:
//!
//! * "The MIDI Data Chunk can be used to store MIDI data."
//! * "The primary purpose of this chunk is to store MIDI System
//!   Exclusive messages, although other types of MIDI data can be
//!   stored in this block as well."
//! * "MIDIData contains a stream of MIDI data."
//! * "The MIDI Data Chunk is optional.  Any number of MIDI Data Chunks
//!   may exist in a FORM AIFC."
//! * "If MIDI System Exclusive messages for several instruments are to
//!   be stored in a FORM AIFC, it is better to use one MIDI Data Chunk
//!   per instrument than one big MIDI Data Chunk for all of the
//!   instruments."
//!
//! Unlike `MARK` / `INST` / `COMT` / `AESD`, §10.0 permits multiple
//! `MIDI` chunks per FORM; the FORM walker therefore accumulates them
//! into a `Vec<MidiDataChunk>` in document order rather than rejecting
//! duplicates. The body is opaque to AIFF — the spec doesn't impose
//! any framing on `MIDIdata` beyond "stream of MIDI data" — so we
//! preserve the raw byte payload verbatim and surface a couple of
//! lightweight observers (`is_sysex`, `len`, `is_empty`) for callers
//! that want a quick classification without re-parsing.
//!
//! An MMA Standard MIDI File-style decode (MThd / MTrk / variable-
//! length quantity / running status / per-event semantics) belongs in
//! the `oxideav-midi` sibling crate, not here. AIFF's `MIDI` chunk is
//! a transport container — the byte stream may or may not be a full
//! SMF; this module's job is just to surface it cleanly.

use crate::aiff::error::Result;

/// Parsed contents of a single `MIDI` (MIDI Data) chunk.
///
/// `data` is the raw MIDI byte stream verbatim — the chunk walker has
/// already stripped the `'MIDI'` ckID, the 4-byte `ckSize`, and any
/// trailing pad byte the outer container inserted for odd-size
/// alignment, so the length here matches the spec's `MIDIdata` field
/// exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiDataChunk {
    /// Raw MIDI byte stream (the spec's `MIDIdata` field). Opaque to
    /// the AIFF layer — callers handing this to a MIDI parser should
    /// expect anything from a single System Exclusive message to a
    /// fully-framed Standard MIDI File.
    pub data: Vec<u8>,
}

impl MidiDataChunk {
    /// Number of bytes in the MIDI stream.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// `true` if the MIDI stream carries zero bytes. §10.0 doesn't
    /// forbid an empty chunk (`ckDataSize == 0`) so we accept it and
    /// let callers decide whether to ignore it.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Quick classifier: returns `true` if the chunk starts with the
    /// MIDI System Exclusive status byte `0xF0`.
    ///
    /// §10.0 calls out SysEx as the chunk's "primary purpose" and a
    /// full SysEx message is delimited by a leading `0xF0` and a
    /// trailing `0xF7`. Empty chunks return `false`. This is a
    /// surface-level helper only — it doesn't validate the trailing
    /// `0xF7` or any data-byte constraints (those belong in a real
    /// MIDI parser).
    pub fn is_sysex(&self) -> bool {
        matches!(self.data.first(), Some(&0xF0))
    }
}

/// Parse the body of a `MIDI` chunk. `data` is the ckData slice the
/// chunk walker handed up — the 8-byte ckID/ckSize prefix and the pad
/// byte (if any) have already been stripped.
///
/// §10.0 doesn't impose any structure on `MIDIdata`: it's a "stream
/// of MIDI data" of any length, including zero. The parser therefore
/// accepts any byte length and preserves the payload verbatim.
pub fn parse_midi_chunk(data: &[u8]) -> Result<MidiDataChunk> {
    Ok(MidiDataChunk {
        data: data.to_vec(),
    })
}

/// Encode a [`MidiDataChunk`] body in wire format — just the raw MIDI
/// byte stream. The chunk header (`'MIDI' + ckSize`) and any
/// odd-length pad byte are the caller's responsibility (mirroring
/// every other write-side helper in this module).
pub fn write_midi_chunk(c: &MidiDataChunk) -> Vec<u8> {
    c.data.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_chunk() {
        // §10.0 doesn't forbid an empty MIDI chunk.
        let c = parse_midi_chunk(&[]).unwrap();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert!(!c.is_sysex());
    }

    #[test]
    fn preserves_byte_stream_verbatim() {
        // A short SysEx message: F0 <manufacturer> <data...> F7.
        let body = [
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7,
        ];
        let c = parse_midi_chunk(&body).unwrap();
        assert_eq!(c.data, &body[..]);
        assert_eq!(c.len(), body.len());
        assert!(!c.is_empty());
        assert!(c.is_sysex());
    }

    #[test]
    fn is_sysex_false_when_first_byte_is_not_f0() {
        // A short channel-voice message (Note On, channel 1).
        let body = [0x90, 0x3C, 0x7F];
        let c = parse_midi_chunk(&body).unwrap();
        assert!(!c.is_sysex());
    }

    #[test]
    fn write_round_trips() {
        let body = vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
        let c = MidiDataChunk { data: body.clone() };
        let bytes = write_midi_chunk(&c);
        assert_eq!(bytes, body);
        let parsed = parse_midi_chunk(&bytes).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn write_round_trips_empty_chunk() {
        let c = MidiDataChunk { data: Vec::new() };
        let bytes = write_midi_chunk(&c);
        assert!(bytes.is_empty());
        let parsed = parse_midi_chunk(&bytes).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn classifies_odd_length_stream() {
        // 5-byte body — chunks of odd ckDataSize get a pad byte
        // appended by the outer container, but the chunk walker strips
        // it before handing us the body, so our `data` is exactly the
        // 5 bytes the spec calls for.
        let body = [0xC0, 0x00, 0xB0, 0x07, 0x40];
        let c = parse_midi_chunk(&body).unwrap();
        assert_eq!(c.len(), 5);
        assert!(!c.is_sysex());
    }

    #[test]
    fn accepts_large_body() {
        // 1 KiB of arbitrary bytes — exercises that there's no
        // length cap on the chunk parse path.
        let body: Vec<u8> = (0..1024_usize).map(|i| (i % 256) as u8).collect();
        let c = parse_midi_chunk(&body).unwrap();
        assert_eq!(c.len(), 1024);
        assert_eq!(c.data, body);
    }
}
