//! `COMM` chunk parser — the Common chunk that carries the sample
//! format.
//!
//! Per `docs/audio/aiff/aiff-aifc-format.md` §2.1 (AIFF v1.3) and
//! §3.2 (AIFF-C), the Common chunk shape is:
//!
//! ```text
//! AIFF (uncompressed):
//!   ckSize          : 18
//!   numChannels     : int16
//!   numSampleFrames : uint32
//!   sampleSize      : int16
//!   sampleRate      : 10-byte 80-bit IEEE extended big-endian
//!
//! AIFF-C (compressed or uncompressed-with-compressionType):
//!   ckSize          : >= 22  (varies with compressionName length)
//!   numChannels     : int16
//!   numSampleFrames : uint32
//!   sampleSize      : int16
//!   sampleRate      : 10-byte 80-bit IEEE extended big-endian
//!   compressionType : 4 bytes (FourCC)
//!   compressionName : pstring (1 length byte + chars +
//!                              pad-to-even total)
//! ```
//!
//! The Pascal-string padding rule is "total bytes consumed including
//! the length byte must be even"; an empty name is encoded as the
//! single length byte `0x00` followed by one pad `0x00` byte
//! (2 bytes total).

use crate::aiff::error::{AiffError, Result};
use crate::aiff::extended::{decode_sample_rate, encode_sample_rate};

/// AIFF-C compressionType meaning "uncompressed, big-endian".
/// Used when an `'AIFC'` FORM carries unaltered PCM samples.
pub const COMPRESSION_NONE: [u8; 4] = *b"NONE";

/// Explicit big-endian 16-bit two's-complement PCM (synonym for
/// `NONE` for 16-bit data).
pub const COMPRESSION_TWOS: [u8; 4] = *b"twos";

/// "twos reversed" — little-endian PCM. Common on macOS audio
/// pipelines that emit AIFF-C with native little-endian samples.
pub const COMPRESSION_SOWT: [u8; 4] = *b"sowt";

/// 8-bit unsigned (offset-binary) PCM. Note the trailing space.
pub const COMPRESSION_RAW: [u8; 4] = *b"raw ";

/// 32-bit IEEE float PCM, big-endian (lower-case form).
pub const COMPRESSION_FL32: [u8; 4] = *b"fl32";

/// 32-bit IEEE float PCM, big-endian (upper-case form). Some
/// encoders emit `FL32`; semantics are identical to `fl32`.
pub const COMPRESSION_FL32_UC: [u8; 4] = *b"FL32";

/// 64-bit IEEE float PCM, big-endian (lower-case form).
pub const COMPRESSION_FL64: [u8; 4] = *b"fl64";

/// 64-bit IEEE float PCM, big-endian (upper-case form).
pub const COMPRESSION_FL64_UC: [u8; 4] = *b"FL64";

/// Parsed contents of the COMM (Common) chunk for either an AIFF
/// `'AIFF'` form or an AIFF-C `'AIFC'` form.
#[derive(Debug, Clone, PartialEq)]
pub struct CommonChunk {
    /// Number of audio channels (1=mono, 2=stereo, …). Spec uses
    /// `int16` but a negative count would be an encoder bug; we
    /// surface it via [`AiffError::InvalidValue`].
    pub num_channels: u16,
    /// Number of sample frames. A frame holds one sample per
    /// channel; total samples = `num_sample_frames * num_channels`.
    pub num_sample_frames: u32,
    /// Bits per sample, in `1..=32`. Stored as `int16` in the file
    /// but parser-rejected outside the closed range.
    pub sample_size: u16,
    /// Decoded sample rate in Hz. The on-disk encoding is 80-bit
    /// IEEE extended (`extended::decode_sample_rate`); we surface
    /// the decoded `f64` so callers don't need to round-trip.
    pub sample_rate: f64,
    /// AIFF-C compression type (`'NONE'` for uncompressed AIFF-C,
    /// `'sowt'` for little-endian PCM, `'ima4'` for IMA ADPCM, …).
    /// `None` when the COMM was an AIFF v1.3 (`'AIFF'`) form, which
    /// has no compressionType field.
    pub compression_type: Option<[u8; 4]>,
    /// AIFF-C human-readable compression name (the Pascal-string
    /// that follows compressionType). `None` for AIFF v1.3. Empty
    /// string is the canonical name for `NONE` per the spec
    /// example ("not compressed").
    pub compression_name: Option<String>,
}

impl CommonChunk {
    /// True when this COMM came from an AIFF-C form (it carries a
    /// `compression_type`).
    pub fn is_aifc(&self) -> bool {
        self.compression_type.is_some()
    }

    /// Bytes per sample frame, computed from `sample_size` and
    /// `num_channels`. `sample_size` rounds up to the next whole
    /// byte (spec: bits beyond a byte boundary are padded with zero
    /// in the low position).
    pub fn frame_bytes(&self) -> usize {
        let bytes_per_sample = self.sample_size.div_ceil(8) as usize;
        bytes_per_sample * self.num_channels as usize
    }

    /// `num_sample_frames * frame_bytes()`. The number of bytes that
    /// the uncompressed PCM payload occupies inside SSND. For AIFF-C
    /// compressed forms (`ima4`, `ulaw`, …) this is NOT the SSND
    /// payload length — the codec packing dictates that.
    pub fn pcm_payload_bytes(&self) -> u64 {
        self.num_sample_frames as u64 * self.frame_bytes() as u64
    }
}

/// Parse the COMM chunk body. `data` is the chunk's ckData (already
/// stripped of ckID/ckSize by the caller). `form_type` lets the
/// parser pick between AIFF (`b"AIFF"`, 18 bytes) and AIFF-C
/// (`b"AIFC"`, >=22 bytes + compressionName) layouts; the FORM-level
/// parser passes the formType field through.
pub fn parse_common(data: &[u8], form_type: [u8; 4]) -> Result<CommonChunk> {
    // The 18 fixed bytes are the same for both forms.
    if data.len() < 18 {
        return Err(AiffError::CommTooShort {
            form_type,
            declared: data.len() as u32,
        });
    }
    let num_channels_i = i16::from_be_bytes([data[0], data[1]]);
    if num_channels_i < 1 {
        return Err(AiffError::InvalidValue {
            what: "numChannels",
            value: num_channels_i as i64,
        });
    }
    let num_channels = num_channels_i as u16;
    let num_sample_frames = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    let sample_size_i = i16::from_be_bytes([data[6], data[7]]);
    if !(1..=32).contains(&sample_size_i) {
        return Err(AiffError::InvalidSampleSize(sample_size_i as u16));
    }
    let sample_size = sample_size_i as u16;
    let mut extended = [0u8; 10];
    extended.copy_from_slice(&data[8..18]);
    let sample_rate = decode_sample_rate(extended)?;

    let (compression_type, compression_name) = match &form_type {
        b"AIFF" => (None, None),
        b"AIFC" => {
            // AIFF-C requires the 4-byte compressionType immediately
            // after the 18 fixed bytes, then the compressionName
            // pstring.
            if data.len() < 22 {
                return Err(AiffError::CommTooShort {
                    form_type,
                    declared: data.len() as u32,
                });
            }
            let mut ct = [0u8; 4];
            ct.copy_from_slice(&data[18..22]);
            let name = parse_pstring(&data[22..])?;
            (Some(ct), Some(name))
        }
        other => return Err(AiffError::UnknownFormType { found: *other }),
    };

    Ok(CommonChunk {
        num_channels,
        num_sample_frames,
        sample_size,
        sample_rate,
        compression_type,
        compression_name,
    })
}

/// Decode a Pascal string: 1 byte length + `length` bytes content.
/// The string lives inside a chunk whose total occupation must end on
/// an even byte boundary; we accept either a single trailing pad
/// (length-byte + chars + pad) or an exact even-length encoding.
///
/// Trailing bytes beyond `length` are ignored (they're the pstring's
/// own pad byte plus, in some encoders, padding the compressionName
/// uses to align the chunk). The chunk-level pad-byte handling is
/// the chunk-walker's job — this routine just trusts whatever slice
/// it was given.
fn parse_pstring(data: &[u8]) -> Result<String> {
    if data.is_empty() {
        return Err(AiffError::Truncated("compressionName pstring length byte"));
    }
    let len = data[0] as usize;
    if 1 + len > data.len() {
        return Err(AiffError::Truncated("compressionName pstring body"));
    }
    // The name is conventionally ASCII per the AIFF-C spec table
    // ("not compressed", "alaw 2:1", …); we do not constrain to it —
    // we just lossy-decode any non-ASCII bytes into U+FFFD, which is
    // safe for the public `compressionName: String` field.
    Ok(String::from_utf8_lossy(&data[1..1 + len]).into_owned())
}

/// Serialise a [`CommonChunk`] back into the COMM chunk **body**
/// (ckData) — the bytes that follow the `'COMM'` ckID and the
/// `ckSize` length word. The chunk-level header and any whole-chunk
/// pad byte are the FORM muxer's responsibility, matching the
/// convention of the other `write_*_chunk` writers in this module.
///
/// The form is selected by [`CommonChunk::is_aifc`]:
///
/// * AIFF (`compression_type == None`) emits the fixed 18-byte body
///   (`numChannels`, `numSampleFrames`, `sampleSize`, 10-byte
///   `sampleRate`) per §2.1.
/// * AIFF-C (`compression_type == Some(..)`) appends the 4-byte
///   `compressionType` FourCC followed by the `compressionName`
///   Pascal string (1 length byte + chars), zero-padded so the
///   pstring's total length (including the length byte) is even, per
///   §3.2. `compression_name == None` is treated as an empty name
///   (the canonical `"not compressed"`-style encoding collapses to a
///   zero-length pstring).
///
/// The result round-trips through [`parse_common`] when given the
/// matching `form_type`. Returns [`AiffError::InvalidSampleRate`] when
/// `sample_rate` is not a positive finite value, so a writer cannot
/// emit a COMM the parser would reject.
pub fn write_common_chunk(common: &CommonChunk) -> Result<Vec<u8>> {
    let rate = encode_sample_rate(common.sample_rate)?;
    let mut out = Vec::with_capacity(if common.is_aifc() { 24 } else { 18 });
    // The 18 fixed bytes, shared by both forms. `numChannels` and
    // `sampleSize` are signed `int16` on disk; our fields are the
    // already-validated unsigned values, so the cast is exact for the
    // legal 1..=32 / >=1 ranges.
    out.extend_from_slice(&(common.num_channels as i16).to_be_bytes());
    out.extend_from_slice(&common.num_sample_frames.to_be_bytes());
    out.extend_from_slice(&(common.sample_size as i16).to_be_bytes());
    out.extend_from_slice(&rate);

    if let Some(ct) = common.compression_type {
        out.extend_from_slice(&ct);
        let name = common.compression_name.as_deref().unwrap_or("");
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(u8::MAX as usize);
        out.push(name_len as u8);
        out.extend_from_slice(&name_bytes[..name_len]);
        // pstring even-total rule (§3.2 / §6.0): 1 length byte +
        // name_len chars must be padded to an even count.
        if (1 + name_len) % 2 == 1 {
            out.push(0);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 10-byte 80-bit extended encoding of `rate` for fixture
    /// construction. Same routine as the test helper in
    /// `extended::tests`; kept duplicated here so each module's
    /// tests are self-contained.
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

    fn comm_18_bytes(channels: u16, frames: u32, bits: u16, rate: f64) -> Vec<u8> {
        let mut v = Vec::with_capacity(18);
        v.extend_from_slice(&(channels as i16).to_be_bytes());
        v.extend_from_slice(&frames.to_be_bytes());
        v.extend_from_slice(&(bits as i16).to_be_bytes());
        v.extend_from_slice(&ext(rate));
        v
    }

    #[test]
    fn parse_aiff_44100_stereo_16bit() {
        let data = comm_18_bytes(2, 100, 16, 44_100.0);
        let c = parse_common(&data, *b"AIFF").unwrap();
        assert_eq!(c.num_channels, 2);
        assert_eq!(c.num_sample_frames, 100);
        assert_eq!(c.sample_size, 16);
        assert_eq!(c.sample_rate, 44_100.0);
        assert_eq!(c.compression_type, None);
        assert_eq!(c.compression_name, None);
        assert!(!c.is_aifc());
        assert_eq!(c.frame_bytes(), 4); // 2 channels * 2 bytes
        assert_eq!(c.pcm_payload_bytes(), 400);
    }

    #[test]
    fn parse_aifc_none_with_empty_name() {
        let mut data = comm_18_bytes(1, 50, 16, 48_000.0);
        data.extend_from_slice(b"NONE");
        // pstring: length=0 + 1 pad byte (so total bytes consumed
        // for the pstring is 2 — even).
        data.extend_from_slice(&[0x00, 0x00]);
        let c = parse_common(&data, *b"AIFC").unwrap();
        assert_eq!(c.compression_type, Some(*b"NONE"));
        assert_eq!(c.compression_name.as_deref(), Some(""));
        assert_eq!(c.sample_rate, 48_000.0);
    }

    #[test]
    fn parse_aifc_sowt_with_name() {
        let mut data = comm_18_bytes(2, 88_200, 16, 44_100.0);
        data.extend_from_slice(b"sowt");
        // pstring: length=14, then "little endian " — 14 chars, no pad
        // needed beyond the pstring (1+14 = 15 odd; but the chunk's
        // own pad is handled by the chunk walker).
        let name = b"little endian ";
        data.push(name.len() as u8);
        data.extend_from_slice(name);
        let c = parse_common(&data, *b"AIFC").unwrap();
        assert_eq!(c.compression_type, Some(*b"sowt"));
        assert_eq!(c.compression_name.as_deref(), Some("little endian "));
        assert!(c.is_aifc());
    }

    #[test]
    fn rejects_zero_channels() {
        let data = comm_18_bytes(0, 100, 16, 44_100.0);
        assert!(matches!(
            parse_common(&data, *b"AIFF"),
            Err(AiffError::InvalidValue {
                what: "numChannels",
                value: 0
            })
        ));
    }

    #[test]
    fn rejects_zero_sample_size() {
        let data = comm_18_bytes(1, 100, 0, 44_100.0);
        assert!(matches!(
            parse_common(&data, *b"AIFF"),
            Err(AiffError::InvalidSampleSize(0))
        ));
    }

    #[test]
    fn rejects_sample_size_above_32() {
        let data = comm_18_bytes(1, 100, 33, 44_100.0);
        assert!(matches!(
            parse_common(&data, *b"AIFF"),
            Err(AiffError::InvalidSampleSize(33))
        ));
    }

    #[test]
    fn rejects_short_aiff_comm() {
        let data = vec![0u8; 17];
        assert!(matches!(
            parse_common(&data, *b"AIFF"),
            Err(AiffError::CommTooShort { .. })
        ));
    }

    #[test]
    fn rejects_short_aifc_comm() {
        let data = comm_18_bytes(1, 100, 16, 44_100.0); // 18 bytes, no compressionType
        assert!(matches!(
            parse_common(&data, *b"AIFC"),
            Err(AiffError::CommTooShort { .. })
        ));
    }

    #[test]
    fn rejects_unknown_form_type() {
        let data = comm_18_bytes(1, 100, 16, 44_100.0);
        assert!(matches!(
            parse_common(&data, *b"WAVE"),
            Err(AiffError::UnknownFormType { found: f }) if f == *b"WAVE"
        ));
    }

    #[test]
    fn frame_bytes_rounds_sample_size_up() {
        let mut data = comm_18_bytes(1, 10, 12, 44_100.0); // 12-bit mono
        data.extend_from_slice(b""); // AIFF
        let c = parse_common(&data, *b"AIFF").unwrap();
        // 12-bit samples occupy 2 bytes each, so 1ch * 2 = 2 bytes/frame.
        assert_eq!(c.frame_bytes(), 2);

        let mut data = comm_18_bytes(2, 10, 24, 48_000.0); // 24-bit stereo
        data.extend_from_slice(b"");
        let c = parse_common(&data, *b"AIFF").unwrap();
        // 24-bit samples occupy 3 bytes each, 2ch * 3 = 6 bytes/frame.
        assert_eq!(c.frame_bytes(), 6);
    }

    #[test]
    fn parse_pstring_empty() {
        // length=0 + 1 pad byte.
        let s = parse_pstring(&[0x00, 0x00]).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn parse_pstring_truncated_length() {
        assert!(matches!(parse_pstring(&[]), Err(AiffError::Truncated(_))));
    }

    #[test]
    fn parse_pstring_truncated_body() {
        // length=5 but only 3 bytes follow.
        assert!(matches!(
            parse_pstring(&[5, b'A', b'B', b'C']),
            Err(AiffError::Truncated(_))
        ));
    }

    #[test]
    fn write_aiff_comm_is_18_bytes() {
        let c = CommonChunk {
            num_channels: 2,
            num_sample_frames: 100,
            sample_size: 16,
            sample_rate: 44_100.0,
            compression_type: None,
            compression_name: None,
        };
        let body = write_common_chunk(&c).unwrap();
        assert_eq!(body.len(), 18);
        // Round-trips back through the parser as an AIFF form.
        let back = parse_common(&body, *b"AIFF").unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn write_aiff_comm_round_trips_24bit_mono() {
        let c = CommonChunk {
            num_channels: 1,
            num_sample_frames: 4096,
            sample_size: 24,
            sample_rate: 48_000.0,
            compression_type: None,
            compression_name: None,
        };
        let body = write_common_chunk(&c).unwrap();
        assert_eq!(parse_common(&body, *b"AIFF").unwrap(), c);
    }

    #[test]
    fn write_aifc_none_empty_name_round_trips() {
        let c = CommonChunk {
            num_channels: 1,
            num_sample_frames: 50,
            sample_size: 16,
            sample_rate: 48_000.0,
            compression_type: Some(*b"NONE"),
            compression_name: Some(String::new()),
        };
        let body = write_common_chunk(&c).unwrap();
        // 18 fixed + 4 FourCC + 2-byte empty pstring (len 0 + 1 pad).
        assert_eq!(body.len(), 24);
        assert_eq!(&body[18..22], b"NONE");
        assert_eq!(&body[22..24], &[0x00, 0x00]);
        assert_eq!(parse_common(&body, *b"AIFC").unwrap(), c);
    }

    #[test]
    fn write_aifc_none_name_field_is_optional() {
        // `compression_name: None` must encode like an empty name.
        let c = CommonChunk {
            num_channels: 2,
            num_sample_frames: 10,
            sample_size: 16,
            sample_rate: 44_100.0,
            compression_type: Some(*b"NONE"),
            compression_name: None,
        };
        let body = write_common_chunk(&c).unwrap();
        let back = parse_common(&body, *b"AIFC").unwrap();
        assert_eq!(back.compression_type, Some(*b"NONE"));
        assert_eq!(back.compression_name.as_deref(), Some(""));
    }

    #[test]
    fn write_aifc_sowt_named_round_trips() {
        let c = CommonChunk {
            num_channels: 2,
            num_sample_frames: 88_200,
            sample_size: 16,
            sample_rate: 44_100.0,
            compression_type: Some(*b"sowt"),
            compression_name: Some("little endian".to_string()),
        };
        let body = write_common_chunk(&c).unwrap();
        // "little endian" is 13 chars: 1 + 13 = 14 (even) → no pad.
        assert_eq!(body.len(), 18 + 4 + 1 + 13);
        assert_eq!(parse_common(&body, *b"AIFC").unwrap(), c);
    }

    #[test]
    fn write_aifc_odd_name_gets_padded() {
        // 14-char name: 1 + 14 = 15 (odd) → one pad byte appended.
        let c = CommonChunk {
            num_channels: 1,
            num_sample_frames: 1,
            sample_size: 8,
            sample_rate: 8_000.0,
            compression_type: Some(*b"alaw"),
            compression_name: Some("ALaw 2:1 alaw!".to_string()), // 14 chars
        };
        let body = write_common_chunk(&c).unwrap();
        assert_eq!(body.len(), 18 + 4 + 1 + 14 + 1);
        assert_eq!(*body.last().unwrap(), 0x00, "odd pstring must be padded");
        assert_eq!(parse_common(&body, *b"AIFC").unwrap(), c);
    }

    #[test]
    fn write_rejects_bad_sample_rate() {
        let c = CommonChunk {
            num_channels: 1,
            num_sample_frames: 1,
            sample_size: 16,
            sample_rate: 0.0,
            compression_type: None,
            compression_name: None,
        };
        assert!(matches!(
            write_common_chunk(&c),
            Err(AiffError::InvalidSampleRate)
        ));
    }
}
