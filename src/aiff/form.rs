//! Top-level FORM parser.
//!
//! Walks the outermost `FORM` chunk and collects the COMM, SSND, and
//! any optional metadata chunks (NAME, AUTH, ANNO, (c) , COMT, MARK,
//! INST, MIDI, AESD, APPL, FVER). Chunk order inside a FORM is not
//! prescribed by the spec — `docs/audio/aiff/aiff-aifc-format.md` §4
//! is explicit on this — so we scan all chunks and route by ckID. The
//! `MIDI` chunk (§10.0), `APPL` chunk (§12.0), and `ANNO` chunk
//! (§13.0) are explicitly "any-number-per-FORM" per the spec, so we
//! accumulate them in document order rather than rejecting duplicates.
//! `NAME`, `AUTH`, and `(c) ` are §13.0 at-most-one-per-FORM
//! singletons and surfaced as such.

use crate::aiff::aesd::{parse_aesd_chunk, AesdChunk};
use crate::aiff::appl::{parse_appl_chunk, ApplicationChunk};
use crate::aiff::chunk::ChunkIter;
use crate::aiff::comment::{parse_comments_chunk, CommentsChunk};
use crate::aiff::common::{parse_common, CommonChunk};
use crate::aiff::error::{AiffError, Result};
use crate::aiff::instrument::{parse_instrument_chunk, InstrumentChunk};
use crate::aiff::marker::{parse_marker_chunk, MarkerChunk};
use crate::aiff::midi::{parse_midi_chunk, MidiDataChunk};
use crate::aiff::text::{parse_text_chunk, TextChunk, TextKind};

/// Parsed SSND (Sound Data) chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundData<'a> {
    /// `offset` field from SSND — bytes from the end of the SSND
    /// header to the first sample frame. Used by encoders to align
    /// the audio payload on a disk block; almost always 0 in
    /// real-world files.
    pub offset: u32,
    /// `blockSize` field. Almost always 0.
    pub block_size: u32,
    /// The sample-frame bytes themselves (already past `offset`).
    /// For an uncompressed AIFF this is a `numSampleFrames *
    /// frame_bytes` PCM blob; for AIFF-C compressed forms it's the
    /// codec's own packing.
    pub samples: &'a [u8],
}

/// Result of walking a FORM chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct Form<'a> {
    /// `formType` field — either `b"AIFF"` (v1.3 uncompressed) or
    /// `b"AIFC"` (compressed-capable).
    pub form_type: [u8; 4],
    /// Parsed COMM chunk. Required per spec — its absence is an
    /// error.
    pub common: CommonChunk,
    /// Parsed SSND chunk. Optional per spec when
    /// `numSampleFrames == 0` (an "audio-format declaration only"
    /// file).
    pub sound: Option<SoundData<'a>>,
    /// FVER chunk's `timestamp`, when present. Required by AIFF-C
    /// per §3.1; we surface it without insisting on it so files
    /// missing the chunk still parse.
    pub fver_timestamp: Option<u32>,
    /// Parsed MARK chunk, when present. Per §6.0 of the AIFF-C spec
    /// only one MARK chunk may appear per FORM (the parser rejects
    /// duplicates with [`AiffError::DuplicateChunk`]); a FORM with no
    /// MARK chunk yields `None`.
    pub markers: Option<MarkerChunk>,
    /// Parsed INST chunk, when present. Per §9 of the AIFF-C spec at
    /// most one INST chunk may appear per FORM (duplicates are
    /// rejected as [`AiffError::DuplicateChunk`]); a FORM with no
    /// INST chunk yields `None`.
    pub instrument: Option<InstrumentChunk>,
    /// Parsed COMT chunk, when present. Per §7.0 at most one COMT
    /// chunk may appear per FORM (duplicates are rejected as
    /// [`AiffError::DuplicateChunk`]); a FORM with no COMT chunk
    /// yields `None`. The chunk may legally carry zero comments.
    pub comments: Option<CommentsChunk>,
    /// Parsed AESD chunk, when present. Per §11.0 at most one AESD
    /// chunk may appear per FORM (duplicates are rejected as
    /// [`AiffError::DuplicateChunk`]); a FORM with no AESD chunk
    /// yields `None`.
    pub aesd: Option<AesdChunk>,
    /// Parsed APPL chunks in document order. Per §12.0 any number
    /// of APPL chunks may appear per FORM, so this is a `Vec`
    /// rather than an `Option`. An empty vector means no APPL
    /// chunks were present.
    pub applications: Vec<ApplicationChunk>,
    /// Parsed MIDI chunks in document order. Per §10.0 any number of
    /// MIDI Data chunks may appear per FORM AIFC, so this is a `Vec`
    /// rather than an `Option`. An empty vector means no MIDI chunks
    /// were present.
    pub midi: Vec<MidiDataChunk>,
    /// Parsed `NAME` chunk, when present. Per §13.0 at most one NAME
    /// chunk may appear per FORM (duplicates are rejected as
    /// [`AiffError::DuplicateChunk`]); a FORM with no NAME chunk
    /// yields `None`.
    pub name: Option<TextChunk>,
    /// Parsed `AUTH` chunk, when present. Per §13.0 at most one AUTH
    /// chunk may appear per FORM (duplicates are rejected as
    /// [`AiffError::DuplicateChunk`]); a FORM with no AUTH chunk
    /// yields `None`.
    pub author: Option<TextChunk>,
    /// Parsed `(c) ` (copyright) chunk, when present. Per §13.0 at
    /// most one Copyright chunk may appear per FORM (duplicates are
    /// rejected as [`AiffError::DuplicateChunk`]); a FORM with no
    /// Copyright chunk yields `None`.
    pub copyright: Option<TextChunk>,
    /// Parsed `ANNO` chunks in document order. Per §13.0 "Any number
    /// of Annotation Chunks may exist within a FORM AIFC", so this
    /// is a `Vec` rather than an `Option`. An empty vector means no
    /// ANNO chunks were present.
    pub annotations: Vec<TextChunk>,
}

/// Parse a complete AIFF / AIFF-C file. `buf` is the raw file
/// bytes; the outermost `FORM` header and `formType` field are
/// validated, then the inner chunks walked into the [`Form`] tree.
pub fn parse(buf: &[u8]) -> Result<Form<'_>> {
    // Outer FORM header: ckID('FORM') + ckSize + formType (4 bytes).
    if buf.len() < 12 {
        return Err(AiffError::Truncated("FORM header"));
    }
    if &buf[0..4] != b"FORM" {
        let mut found = [0u8; 4];
        found.copy_from_slice(&buf[0..4]);
        return Err(AiffError::NotForm { found });
    }
    let form_size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let mut form_type = [0u8; 4];
    form_type.copy_from_slice(&buf[8..12]);
    if &form_type != b"AIFF" && &form_type != b"AIFC" {
        return Err(AiffError::UnknownFormType { found: form_type });
    }

    // ckSize for FORM counts the formType (4 bytes) PLUS every
    // contained chunk. The inner-chunk slice is therefore
    // `form_size - 4` bytes starting at offset 12. We tolerate a
    // FORM that declares more bytes than the buffer holds by
    // clamping (some encoders set FORM ckSize to 0xFFFFFFFF as a
    // "streaming" marker).
    let inner_start = 12usize;
    let inner_end_declared = inner_start
        .checked_add(form_size.saturating_sub(4))
        .unwrap_or(buf.len());
    let inner_end = inner_end_declared.min(buf.len());
    let inner = &buf[inner_start..inner_end];

    let mut common: Option<CommonChunk> = None;
    let mut sound: Option<SoundData<'_>> = None;
    let mut fver_timestamp: Option<u32> = None;
    let mut markers: Option<MarkerChunk> = None;
    let mut instrument: Option<InstrumentChunk> = None;
    let mut comments: Option<CommentsChunk> = None;
    let mut aesd: Option<AesdChunk> = None;
    let mut applications: Vec<ApplicationChunk> = Vec::new();
    let mut midi: Vec<MidiDataChunk> = Vec::new();
    let mut name: Option<TextChunk> = None;
    let mut author: Option<TextChunk> = None;
    let mut copyright: Option<TextChunk> = None;
    let mut annotations: Vec<TextChunk> = Vec::new();

    for chunk in ChunkIter::new(inner) {
        let chunk = chunk?;
        match &chunk.id {
            b"COMM" => {
                let c = parse_common(chunk.data, form_type)?;
                common = Some(c);
            }
            b"MARK" => {
                // §6.0: "No more than one Marker Chunk can appear in
                // a FORM AIFC." Reject a second one rather than
                // silently dropping the older parse.
                if markers.is_some() {
                    return Err(AiffError::DuplicateChunk("MARK"));
                }
                markers = Some(parse_marker_chunk(chunk.data)?);
            }
            b"INST" => {
                // §9: "No more than one Instrument Chunk can appear
                // in a FORM AIFC."
                if instrument.is_some() {
                    return Err(AiffError::DuplicateChunk("INST"));
                }
                instrument = Some(parse_instrument_chunk(chunk.data)?);
            }
            b"COMT" => {
                // §7.0: "No more than one Comments Chunk may appear
                // in a single FORM AIFC."
                if comments.is_some() {
                    return Err(AiffError::DuplicateChunk("COMT"));
                }
                comments = Some(parse_comments_chunk(chunk.data)?);
            }
            b"AESD" => {
                // §11.0: "No more than one Audio Recording Chunk may
                // appear in a FORM AIFC."
                if aesd.is_some() {
                    return Err(AiffError::DuplicateChunk("AESD"));
                }
                aesd = Some(parse_aesd_chunk(chunk.data)?);
            }
            b"APPL" => {
                // §12.0: "Any number of Application Specific Chunks
                // may exist in a single FORM AIFC." — accumulate in
                // document order, no duplicate check.
                applications.push(parse_appl_chunk(chunk.data)?);
            }
            b"MIDI" => {
                // §10.0: "Any number of MIDI Data Chunks may exist
                // in a FORM AIFC." — accumulate in document order,
                // no duplicate check.
                midi.push(parse_midi_chunk(chunk.data)?);
            }
            b"NAME" => {
                // §13.0: "No more than one Name Chunk may exist
                // within a FORM AIFC."
                if name.is_some() {
                    return Err(AiffError::DuplicateChunk("NAME"));
                }
                name = Some(parse_text_chunk(TextKind::Name, chunk.data)?);
            }
            b"AUTH" => {
                // §13.0: "No more than one Author Chunk may exist
                // within a FORM AIFC."
                if author.is_some() {
                    return Err(AiffError::DuplicateChunk("AUTH"));
                }
                author = Some(parse_text_chunk(TextKind::Author, chunk.data)?);
            }
            b"(c) " => {
                // §13.0: "No more than one Copyright Chunk may exist
                // within a FORM AIFC." The on-wire ckID is the four
                // bytes `(`, `c`, `)`, ` ` — the chunk ID character
                // itself stands in for the © glyph (§13.0 ¶ "the 'c'
                // is lowercase and there is a space [0x20] after the
                // close parenthesis").
                if copyright.is_some() {
                    return Err(AiffError::DuplicateChunk("(c) "));
                }
                copyright = Some(parse_text_chunk(TextKind::Copyright, chunk.data)?);
            }
            b"ANNO" => {
                // §13.0: "Any number of Annotation Chunks may exist
                // within a FORM AIFC." — accumulate in document order,
                // no duplicate check.
                annotations.push(parse_text_chunk(TextKind::Annotation, chunk.data)?);
            }
            b"SSND" => {
                if chunk.data.len() < 8 {
                    return Err(AiffError::Truncated("SSND chunk header"));
                }
                let offset = u32::from_be_bytes([
                    chunk.data[0],
                    chunk.data[1],
                    chunk.data[2],
                    chunk.data[3],
                ]);
                let block_size = u32::from_be_bytes([
                    chunk.data[4],
                    chunk.data[5],
                    chunk.data[6],
                    chunk.data[7],
                ]);
                let payload_start = 8usize + offset as usize;
                let samples = if payload_start <= chunk.data.len() {
                    &chunk.data[payload_start..]
                } else {
                    return Err(AiffError::OversizedChunk {
                        id: *b"SSND",
                        declared: chunk.size,
                        available: (chunk.data.len() - 8) as u32,
                    });
                };
                sound = Some(SoundData {
                    offset,
                    block_size,
                    samples,
                });
            }
            b"FVER" => {
                if chunk.data.len() < 4 {
                    return Err(AiffError::Truncated("FVER chunk"));
                }
                fver_timestamp = Some(u32::from_be_bytes([
                    chunk.data[0],
                    chunk.data[1],
                    chunk.data[2],
                    chunk.data[3],
                ]));
            }
            _ => {
                // Unrecognised chunks: skip silently. §4 of the
                // staged AIFF-AIFC layout is explicit that chunk
                // order is unspecified and that callers must
                // tolerate ckIDs they do not recognise.
            }
        }
    }

    let common = common.ok_or(AiffError::MissingChunk("COMM"))?;
    // If the file has zero sample frames an SSND is permitted to be
    // absent per spec.
    if common.num_sample_frames > 0 && sound.is_none() {
        return Err(AiffError::MissingChunk("SSND"));
    }

    Ok(Form {
        form_type,
        common,
        sound,
        fver_timestamp,
        markers,
        instrument,
        comments,
        aesd,
        applications,
        midi,
        name,
        author,
        copyright,
        annotations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 10-byte 80-bit extended encoding for tests. Same as
    /// the helper in `common::tests`.
    fn ext(rate: f64) -> [u8; 10] {
        let sign = rate.is_sign_negative();
        let mag = rate.abs();
        let bits = mag.to_bits();
        let f64_exp = ((bits >> 52) & 0x7ff) as i32;
        let f64_frac = bits & 0x000f_ffff_ffff_ffff;
        let (mantissa_64, exp_unbiased): (u64, i32) = if f64_exp == 0 {
            let lead = f64_frac.leading_zeros() as i32 - 11;
            let mantissa = f64_frac << (12 + lead);
            let true_exp = -1022 - lead;
            (mantissa, true_exp)
        } else {
            let mantissa = (1_u64 << 63) | (f64_frac << 11);
            let true_exp = f64_exp - 1023;
            (mantissa, true_exp)
        };
        let biased_ext = exp_unbiased + 16_383;
        let exp_field = biased_ext as u16 & 0x7fff;
        let mut o = [0u8; 10];
        o[0] = ((exp_field >> 8) as u8) | if sign { 0x80 } else { 0 };
        o[1] = (exp_field & 0xff) as u8;
        o[2..10].copy_from_slice(&mantissa_64.to_be_bytes());
        o
    }

    /// Pack a chunk: ckID + ckSize + data + (pad byte if odd).
    fn pack(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + data.len() + 1);
        v.extend_from_slice(id);
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        if data.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    /// Build a minimal AIFF file: FORM('AIFF') wrapping COMM + SSND.
    fn build_aiff_file(
        channels: u16,
        frames: u32,
        bits: u16,
        rate: f64,
        samples: &[u8],
    ) -> Vec<u8> {
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&(channels as i16).to_be_bytes());
        comm_body.extend_from_slice(&frames.to_be_bytes());
        comm_body.extend_from_slice(&(bits as i16).to_be_bytes());
        comm_body.extend_from_slice(&ext(rate));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes()); // offset
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes()); // blockSize
        ssnd_body.extend_from_slice(samples);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));

        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        file.extend_from_slice(&inner);
        file
    }

    fn build_aifc_file(
        channels: u16,
        frames: u32,
        bits: u16,
        rate: f64,
        compression: &[u8; 4],
        compression_name: &str,
        samples: &[u8],
    ) -> Vec<u8> {
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&(channels as i16).to_be_bytes());
        comm_body.extend_from_slice(&frames.to_be_bytes());
        comm_body.extend_from_slice(&(bits as i16).to_be_bytes());
        comm_body.extend_from_slice(&ext(rate));
        comm_body.extend_from_slice(compression);
        // pstring: length + chars + pad to even total
        comm_body.push(compression_name.len() as u8);
        comm_body.extend_from_slice(compression_name.as_bytes());
        // Pad so the pstring occupies an even number of bytes.
        if (1 + compression_name.len()) % 2 == 1 {
            comm_body.push(0);
        }

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(samples);

        // FVER chunk: 4-byte timestamp.
        let fver_body = 0xA280_5140_u32.to_be_bytes();

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFC");
        inner.extend_from_slice(&pack(b"FVER", &fver_body));
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));

        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        file.extend_from_slice(&inner);
        file
    }

    #[test]
    fn parse_minimal_aiff() {
        // 16-bit stereo PCM, 2 frames = 8 bytes (BE, two's-complement).
        let pcm: [u8; 8] = [0x00, 0x01, 0xff, 0xff, 0x12, 0x34, 0xfe, 0xdc];
        let f = build_aiff_file(2, 2, 16, 44_100.0, &pcm);
        let parsed = parse(&f).unwrap();
        assert_eq!(&parsed.form_type, b"AIFF");
        assert_eq!(parsed.common.num_channels, 2);
        assert_eq!(parsed.common.num_sample_frames, 2);
        assert_eq!(parsed.common.sample_size, 16);
        assert_eq!(parsed.common.sample_rate, 44_100.0);
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
        assert_eq!(parsed.fver_timestamp, None);
    }

    #[test]
    fn parse_minimal_aifc_none() {
        let pcm: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
        let f = build_aifc_file(1, 2, 16, 48_000.0, b"NONE", "not compressed", &pcm);
        let parsed = parse(&f).unwrap();
        assert_eq!(&parsed.form_type, b"AIFC");
        assert_eq!(parsed.common.compression_type, Some(*b"NONE"));
        assert_eq!(
            parsed.common.compression_name.as_deref(),
            Some("not compressed")
        );
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
        assert_eq!(parsed.fver_timestamp, Some(0xA280_5140));
    }

    #[test]
    fn parse_minimal_aifc_sowt() {
        // Little-endian PCM payload (sowt). The container parser
        // doesn't byteswap — that's `pcm::read_*`'s job. We just
        // confirm the compressionType made it through.
        let pcm: [u8; 4] = [0x78, 0x56, 0x34, 0x12];
        let f = build_aifc_file(1, 2, 16, 44_100.0, b"sowt", "", &pcm);
        let parsed = parse(&f).unwrap();
        assert_eq!(parsed.common.compression_type, Some(*b"sowt"));
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
    }

    #[test]
    fn rejects_not_form() {
        let mut f = build_aiff_file(1, 0, 16, 44_100.0, &[]);
        f[0] = b'X';
        let r = parse(&f);
        assert!(matches!(r, Err(AiffError::NotForm { .. })));
    }

    #[test]
    fn rejects_unknown_form_type() {
        let mut f = build_aiff_file(1, 0, 16, 44_100.0, &[]);
        f[8..12].copy_from_slice(b"WAVE");
        let r = parse(&f);
        assert!(matches!(r, Err(AiffError::UnknownFormType { .. })));
    }

    #[test]
    fn rejects_missing_comm() {
        // Just a FORM('AIFF') wrapper with no inner chunks.
        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);
        let r = parse(&f);
        assert!(matches!(r, Err(AiffError::MissingChunk("COMM"))));
    }

    #[test]
    fn rejects_missing_ssnd_when_frames_nonzero() {
        // COMM declares 10 frames but no SSND follows.
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&10_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);
        let r = parse(&f);
        assert!(matches!(r, Err(AiffError::MissingChunk("SSND"))));
    }

    #[test]
    fn allows_missing_ssnd_when_zero_frames() {
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&0_u32.to_be_bytes()); // zero frames
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);
        let parsed = parse(&f).unwrap();
        assert_eq!(parsed.common.num_sample_frames, 0);
        assert!(parsed.sound.is_none());
    }

    #[test]
    fn parses_ssnd_with_nonzero_offset() {
        // 4 sample bytes preceded by 6 alignment-pad bytes.
        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&6_u32.to_be_bytes()); // offset
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&[0u8; 6]); // alignment padding
        ssnd_body.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes()); // 2 frames
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        let snd = parsed.sound.as_ref().unwrap();
        assert_eq!(snd.offset, 6);
        assert_eq!(snd.samples, &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn truncated_file_errors() {
        assert!(matches!(parse(&[0u8; 3]), Err(AiffError::Truncated(_))));
    }

    #[test]
    fn chunks_can_appear_in_any_order() {
        // Build an AIFC where SSND comes before COMM and FVER.
        let pcm = [0x11_u8, 0x22, 0x33, 0x44];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));
        comm_body.extend_from_slice(b"NONE");
        comm_body.push(0);
        comm_body.push(0); // pad

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let fver_body = 0xA280_5140_u32.to_be_bytes();

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFC");
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"FVER", &fver_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        assert_eq!(&parsed.form_type, b"AIFC");
        assert_eq!(parsed.common.compression_type, Some(*b"NONE"));
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
        assert_eq!(parsed.fver_timestamp, Some(0xA280_5140));
    }

    /// Build a MARK chunk body containing the given marker list.
    /// Each marker: id + position + pstring (with pad-to-even).
    fn build_mark_chunk(markers: &[(i16, u32, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(markers.len() as u16).to_be_bytes());
        for (id, pos, name) in markers {
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&pos.to_be_bytes());
            body.push(name.len() as u8);
            body.extend_from_slice(name.as_bytes());
            if (1 + name.len()) % 2 == 1 {
                body.push(0);
            }
        }
        body
    }

    #[test]
    fn parses_form_with_marker_chunk() {
        // FORM(AIFF) wrapping COMM + MARK + SSND.
        let pcm = [0x00_u8, 0x01, 0x02, 0x03];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mark_body = build_mark_chunk(&[(1, 0, "begin"), (2, 1, "end")]);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"MARK", &mark_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        let marks = parsed.markers.as_ref().unwrap();
        assert_eq!(marks.markers.len(), 2);
        assert_eq!(marks.markers[0].id, 1);
        assert_eq!(marks.markers[0].position, 0);
        assert_eq!(marks.markers[0].name, "begin");
        assert_eq!(marks.markers[1].id, 2);
        assert_eq!(marks.markers[1].position, 1);
        assert_eq!(marks.markers[1].name, "end");
        // SSND must still parse alongside MARK.
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
    }

    #[test]
    fn rejects_duplicate_mark_chunks() {
        // §6.0: at most one MARK per FORM.
        let pcm = [0x00_u8, 0x01];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mark_body_a = build_mark_chunk(&[(1, 0, "first")]);
        let mark_body_b = build_mark_chunk(&[(2, 0, "second")]);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"MARK", &mark_body_a));
        inner.extend_from_slice(&pack(b"MARK", &mark_body_b));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        assert!(matches!(parse(&f), Err(AiffError::DuplicateChunk("MARK"))));
    }

    #[test]
    fn form_without_mark_chunk_has_none_markers() {
        // Re-uses build_aiff_file's tiny fixture; should produce
        // `markers: None`.
        let f = build_aiff_file(1, 1, 16, 44_100.0, &[0x00, 0x01]);
        let parsed = parse(&f).unwrap();
        assert!(parsed.markers.is_none());
    }

    #[test]
    fn aifc_with_empty_marker_list_yields_some_empty() {
        // Empty MARK chunk: numMarkers=0, no marker bodies. The
        // chunk *is* present so `markers` must be `Some` — telling
        // the caller the encoder declared markers but had none.
        let pcm = [0x12_u8, 0x34];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));
        comm_body.extend_from_slice(b"NONE");
        comm_body.push(0);
        comm_body.push(0);

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let fver_body = 0xA280_5140_u32.to_be_bytes();
        let mark_body: Vec<u8> = 0_u16.to_be_bytes().to_vec();

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFC");
        inner.extend_from_slice(&pack(b"FVER", &fver_body));
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"MARK", &mark_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        let marks = parsed.markers.as_ref().unwrap();
        assert!(marks.markers.is_empty());
    }

    /// Build a 20-byte INST chunk body.
    #[allow(clippy::too_many_arguments)]
    fn build_inst_chunk(
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
        for triple in [sustain, release] {
            v.extend_from_slice(&triple.0.to_be_bytes());
            v.extend_from_slice(&triple.1.to_be_bytes());
            v.extend_from_slice(&triple.2.to_be_bytes());
        }
        v
    }

    #[test]
    fn parses_form_with_instrument_chunk() {
        // FORM(AIFF) wrapping COMM + MARK + INST + SSND.
        let pcm = [0x00_u8, 0x01, 0x02, 0x03];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mark_body = build_mark_chunk(&[(1, 0, "loopbegin"), (2, 1, "loopend")]);
        let inst_body = build_inst_chunk(60, -3, 48, 72, 1, 127, 0, (1, 1, 2), (0, 0, 0));

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"MARK", &mark_body));
        inner.extend_from_slice(&pack(b"INST", &inst_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        let inst = parsed.instrument.as_ref().unwrap();
        assert_eq!(inst.base_note, 60);
        assert_eq!(inst.detune, -3);
        assert_eq!(inst.sustain_loop.begin_loop, 1);
        assert_eq!(inst.sustain_loop.end_loop, 2);

        // INST can resolve against the FORM's MARK chunk.
        let markers = parsed.markers.as_ref().unwrap();
        let r = inst.resolve_sustain_loop(markers).unwrap();
        assert_eq!(r.begin.name, "loopbegin");
        assert_eq!(r.end.name, "loopend");
    }

    #[test]
    fn rejects_duplicate_inst_chunks() {
        // §9: at most one INST per FORM.
        let pcm = [0x00_u8, 0x01];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let inst_body_a = build_inst_chunk(60, 0, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));
        let inst_body_b = build_inst_chunk(67, 0, 0, 127, 1, 127, 0, (0, 0, 0), (0, 0, 0));

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"INST", &inst_body_a));
        inner.extend_from_slice(&pack(b"INST", &inst_body_b));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        assert!(matches!(parse(&f), Err(AiffError::DuplicateChunk("INST"))));
    }

    #[test]
    fn form_without_inst_chunk_has_none_instrument() {
        let f = build_aiff_file(1, 1, 16, 44_100.0, &[0x00, 0x01]);
        let parsed = parse(&f).unwrap();
        assert!(parsed.instrument.is_none());
    }

    #[test]
    fn unknown_chunks_are_skipped() {
        let pcm = [0xAA_u8, 0xBB, 0xCC, 0xDD];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        // Two wild custom ckIDs — neither matches any §13.0 / §10.0 /
        // §12.0 chunk, so both must be ignored without affecting the
        // SSND parse.
        let zzzz_body = b"some-bytes";
        let qqqq_body = b"more-bytes";

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"ZZZZ", zzzz_body));
        inner.extend_from_slice(&pack(b"QQQQ", qqqq_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
        // None of the §13.0 text chunks are populated.
        assert!(parsed.name.is_none());
        assert!(parsed.author.is_none());
        assert!(parsed.copyright.is_none());
        assert!(parsed.annotations.is_empty());
    }

    #[test]
    fn parses_form_with_all_text_chunks() {
        // FORM(AIFC) wrapping COMM + NAME + AUTH + (c)  + ANNO×2 + SSND.
        let pcm = [0x12_u8, 0x34, 0x56, 0x78];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));
        comm_body.extend_from_slice(b"NONE");
        comm_body.push(0);
        comm_body.push(0); // pad

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let fver_body = 0xA280_5140_u32.to_be_bytes();

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFC");
        inner.extend_from_slice(&pack(b"FVER", &fver_body));
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"NAME", b"Helicopter take-off"));
        inner.extend_from_slice(&pack(b"AUTH", b"sound designer"));
        inner.extend_from_slice(&pack(b"(c) ", b"1991 Apple Computer, Inc."));
        inner.extend_from_slice(&pack(b"ANNO", b"first comment"));
        inner.extend_from_slice(&pack(b"ANNO", b"second comment"));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        assert_eq!(
            parsed.name.as_ref().unwrap().as_str(),
            Some("Helicopter take-off")
        );
        assert_eq!(
            parsed.author.as_ref().unwrap().as_str(),
            Some("sound designer")
        );
        assert_eq!(
            parsed.copyright.as_ref().unwrap().as_str(),
            Some("1991 Apple Computer, Inc.")
        );
        assert_eq!(parsed.annotations.len(), 2);
        assert_eq!(parsed.annotations[0].as_str(), Some("first comment"));
        assert_eq!(parsed.annotations[1].as_str(), Some("second comment"));
        // The SSND payload still parses alongside the text chunks.
        assert_eq!(parsed.sound.as_ref().unwrap().samples, &pcm);
    }

    #[test]
    fn rejects_duplicate_name_chunks() {
        // §13.0: at most one NAME per FORM.
        let pcm = [0x00_u8, 0x01];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"NAME", b"first"));
        inner.extend_from_slice(&pack(b"NAME", b"second"));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        assert!(matches!(parse(&f), Err(AiffError::DuplicateChunk("NAME"))));
    }

    #[test]
    fn rejects_duplicate_auth_chunks() {
        // §13.0: at most one AUTH per FORM.
        let pcm = [0x00_u8, 0x01];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"AUTH", b"alice"));
        inner.extend_from_slice(&pack(b"AUTH", b"bob"));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        assert!(matches!(parse(&f), Err(AiffError::DuplicateChunk("AUTH"))));
    }

    #[test]
    fn rejects_duplicate_copyright_chunks() {
        // §13.0: at most one Copyright per FORM.
        let pcm = [0x00_u8, 0x01];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"(c) ", b"1991"));
        inner.extend_from_slice(&pack(b"(c) ", b"1992"));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        assert!(matches!(parse(&f), Err(AiffError::DuplicateChunk("(c) "))));
    }

    #[test]
    fn allows_multiple_annotation_chunks() {
        // §13.0: ANNO is "any number per FORM".
        let pcm = [0x00_u8, 0x01];
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&1_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&pcm);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"ANNO", b"alpha"));
        inner.extend_from_slice(&pack(b"ANNO", b"beta"));
        inner.extend_from_slice(&pack(b"ANNO", b"gamma"));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut f = Vec::new();
        f.extend_from_slice(b"FORM");
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend_from_slice(&inner);

        let parsed = parse(&f).unwrap();
        let annos: Vec<&str> = parsed
            .annotations
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(annos, vec!["alpha", "beta", "gamma"]);
    }
}
