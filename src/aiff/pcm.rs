//! PCM sample readers for the uncompressed AIFF-C `compressionType`
//! flavours.
//!
//! `docs/audio/aiff/aiff-aifc-format.md` §3.3 lists the standard
//! AIFF-C compression FourCCs. This module implements the
//! "uncompressed-but-still-flagged" subset:
//!
//! | FourCC | Layout                                                  |
//! |--------|---------------------------------------------------------|
//! | `NONE` / `twos` | big-endian two's-complement, `sampleSize` bits |
//! | `sowt`          | little-endian two's-complement, `sampleSize` bits |
//! | `raw `          | 8-bit unsigned (offset-binary); sampleSize MUST be 8 |
//! | `fl32` / `FL32` | 32-bit IEEE float, big-endian                  |
//! | `fl64` / `FL64` | 64-bit IEEE float, big-endian                  |
//!
//! Each reader returns a planar `Vec<Vec<T>>` where the outer index
//! is the channel and the inner is the sample frame, matching the
//! shape `oxideav-core::AudioFrame` expects. The on-wire payload is
//! frame-interleaved (channel 0 sample of frame 0, channel 1 sample
//! of frame 0, …) so the readers deinterleave.
//!
//! All readers expand `sampleSize` < 32 to `i32` left-justified per
//! AIFF spec §2.2 (high bits significant; low pad bits zero), so a
//! caller doesn't have to remember the original bit depth. Float
//! flavours return their native precision.

use crate::aiff::common::{CommonChunk, COMPRESSION_NONE};
use crate::aiff::error::{AiffError, Result};

/// PCM sample data deinterleaved into per-channel planes.
///
/// The variant captures the on-wire numeric format. Integer flavours
/// always promote to `i32` left-justified (the high bits of each
/// sample carry the value, low padding bits are zero) so callers don't
/// have to keep track of `sample_size`. Float flavours carry their
/// native f32 / f64 precision.
#[derive(Debug, Clone, PartialEq)]
pub enum PcmSamples {
    /// One signed-i32 plane per channel. Produced for
    /// `NONE` / `twos` / `sowt` / `raw `.
    I32(Vec<Vec<i32>>),
    /// One `f32` plane per channel. Produced for `fl32` / `FL32`.
    F32(Vec<Vec<f32>>),
    /// One `f64` plane per channel. Produced for `fl64` / `FL64`.
    F64(Vec<Vec<f64>>),
}

impl PcmSamples {
    /// Number of channels (outer dimension).
    pub fn channels(&self) -> usize {
        match self {
            PcmSamples::I32(v) => v.len(),
            PcmSamples::F32(v) => v.len(),
            PcmSamples::F64(v) => v.len(),
        }
    }

    /// Number of sample frames per channel. Same across all planes.
    pub fn frames(&self) -> usize {
        match self {
            PcmSamples::I32(v) => v.first().map(|p| p.len()).unwrap_or(0),
            PcmSamples::F32(v) => v.first().map(|p| p.len()).unwrap_or(0),
            PcmSamples::F64(v) => v.first().map(|p| p.len()).unwrap_or(0),
        }
    }
}

/// Decode SSND PCM payload bytes into per-channel sample planes.
///
/// `samples` is the post-offset SSND payload (already past `offset`
/// padding by `form::parse`). `common` carries the format
/// description: an `AIFF` (v1.3) COMM is treated as big-endian
/// two's-complement (`NONE` / `twos`). For `AIFF-C` COMMs, the
/// `compression_type` field selects the layout.
///
/// Returns [`AiffError::UnsupportedPcmCompression`] for FourCCs the
/// caller should route through a sibling codec crate
/// (`ima4` → oxideav-adpcm, `ulaw`/`alaw` → oxideav-g711, …).
pub fn decode_pcm(common: &CommonChunk, samples: &[u8]) -> Result<PcmSamples> {
    // AIFF v1.3 has no compressionType; treat as `NONE`.
    let ct = common.compression_type.unwrap_or(COMPRESSION_NONE);
    match &ct {
        b"NONE" | b"twos" => decode_int_be(common, samples),
        b"sowt" => decode_int_le(common, samples),
        b"raw " => decode_u8_unsigned(common, samples),
        b"fl32" | b"FL32" => decode_f32_be(common, samples),
        b"fl64" | b"FL64" => decode_f64_be(common, samples),
        _ => Err(AiffError::UnsupportedPcmCompression(ct)),
    }
}

/// True when the given compressionType is a PCM flavour this crate
/// knows how to decode. Convenient for callers that want to fall back
/// to a sibling codec crate rather than calling [`decode_pcm`] and
/// matching on the error.
pub fn is_pcm_compression(compression_type: [u8; 4]) -> bool {
    matches!(
        &compression_type,
        b"NONE" | b"twos" | b"sowt" | b"raw " | b"fl32" | b"FL32" | b"fl64" | b"FL64"
    )
}

/// Helper: validate that `payload` divides evenly into
/// `numChannels * bytes_per_sample` blocks. Returns the implied
/// `frames_in_payload`. Falls back to `common.num_sample_frames` if
/// the SSND payload is at least as long as that count (extra bytes
/// are silently truncated, as some encoders pad SSND).
fn frame_count(common: &CommonChunk, payload: &[u8], bytes_per_sample: usize) -> Result<usize> {
    let frame_size = bytes_per_sample * common.num_channels as usize;
    if frame_size == 0 {
        return Err(AiffError::InvalidValue {
            what: "frame_size",
            value: 0,
        });
    }
    let declared = common.num_sample_frames as usize;
    let needed = declared
        .checked_mul(frame_size)
        .ok_or(AiffError::InvalidValue {
            what: "numSampleFrames",
            value: common.num_sample_frames as i64,
        })?;
    if payload.len() >= needed {
        // Trust the declared count.
        return Ok(declared);
    }
    // Otherwise, derive the count from the payload itself.
    if payload.len() % frame_size != 0 {
        return Err(AiffError::SsndUnaligned {
            payload: payload.len(),
            frame_size,
        });
    }
    Ok(payload.len() / frame_size)
}

/// Read a single big-endian signed sample of `bits` bits from
/// `buf`, returning it left-justified into i32 (high bits significant,
/// low pad bits zero). `bits` is the COMM `sampleSize`; on-disk the
/// sample occupies `ceil(bits / 8)` bytes.
fn read_be_sample(buf: &[u8], bits: u16) -> i32 {
    let bytes = bits.div_ceil(8) as usize;
    debug_assert!(bytes <= 4);
    let mut acc = 0i32;
    for &b in &buf[..bytes] {
        acc = (acc << 8) | b as i32;
    }
    // Sign-extend from the top of the `bytes*8`-bit word, then
    // left-justify into 32 bits so the caller doesn't need bit-depth
    // knowledge.
    let total_bits = bytes * 8;
    let sign_shift = 32 - total_bits as i32;
    let signed = (acc << sign_shift) >> sign_shift; // sign-extend
                                                    // Left-justify so the sample's MSB occupies bit 31.
    let left_shift = (8 - (bits as i32 % 8)) % 8; // pad bits inside the byte
    let _ = left_shift; // reserved for the in-byte sub-bit case
    signed << (32 - total_bits as i32)
}

/// Same as `read_be_sample` but reads little-endian (sowt).
fn read_le_sample(buf: &[u8], bits: u16) -> i32 {
    let bytes = bits.div_ceil(8) as usize;
    debug_assert!(bytes <= 4);
    let mut acc = 0i32;
    for (i, &b) in buf.iter().enumerate().take(bytes) {
        acc |= (b as i32) << (i * 8);
    }
    let total_bits = bytes * 8;
    let sign_shift = 32 - total_bits as i32;
    let signed = (acc << sign_shift) >> sign_shift;
    signed << (32 - total_bits as i32)
}

fn decode_int_be(common: &CommonChunk, payload: &[u8]) -> Result<PcmSamples> {
    let bytes_per_sample = common.sample_size.div_ceil(8) as usize;
    let frames = frame_count(common, payload, bytes_per_sample)?;
    let n_ch = common.num_channels as usize;
    let mut planes = vec![Vec::with_capacity(frames); n_ch];
    let frame_size = bytes_per_sample * n_ch;
    for f in 0..frames {
        let frame = &payload[f * frame_size..f * frame_size + frame_size];
        for (ch, plane) in planes.iter_mut().enumerate().take(n_ch) {
            let off = ch * bytes_per_sample;
            let sample = read_be_sample(&frame[off..off + bytes_per_sample], common.sample_size);
            plane.push(sample);
        }
    }
    Ok(PcmSamples::I32(planes))
}

fn decode_int_le(common: &CommonChunk, payload: &[u8]) -> Result<PcmSamples> {
    let bytes_per_sample = common.sample_size.div_ceil(8) as usize;
    let frames = frame_count(common, payload, bytes_per_sample)?;
    let n_ch = common.num_channels as usize;
    let mut planes = vec![Vec::with_capacity(frames); n_ch];
    let frame_size = bytes_per_sample * n_ch;
    for f in 0..frames {
        let frame = &payload[f * frame_size..f * frame_size + frame_size];
        for (ch, plane) in planes.iter_mut().enumerate().take(n_ch) {
            let off = ch * bytes_per_sample;
            let sample = read_le_sample(&frame[off..off + bytes_per_sample], common.sample_size);
            plane.push(sample);
        }
    }
    Ok(PcmSamples::I32(planes))
}

/// `raw ` is 8-bit unsigned with bias 128 (offset-binary). The
/// canonical decoded value is `(byte as i32 - 128) << 24`, putting it
/// in the same left-justified i32 range as the BE / LE readers.
fn decode_u8_unsigned(common: &CommonChunk, payload: &[u8]) -> Result<PcmSamples> {
    if common.sample_size != 8 {
        return Err(AiffError::InvalidValue {
            what: "sampleSize for `raw ` compressionType",
            value: common.sample_size as i64,
        });
    }
    let bytes_per_sample = 1usize;
    let frames = frame_count(common, payload, bytes_per_sample)?;
    let n_ch = common.num_channels as usize;
    let mut planes = vec![Vec::with_capacity(frames); n_ch];
    let frame_size = n_ch;
    for f in 0..frames {
        let frame = &payload[f * frame_size..f * frame_size + frame_size];
        for (ch, plane) in planes.iter_mut().enumerate().take(n_ch) {
            let unsigned = frame[ch] as i32;
            let centred = (unsigned - 128) << 24; // left-justify
            plane.push(centred);
        }
    }
    Ok(PcmSamples::I32(planes))
}

fn decode_f32_be(common: &CommonChunk, payload: &[u8]) -> Result<PcmSamples> {
    if common.sample_size != 32 {
        return Err(AiffError::InvalidValue {
            what: "sampleSize for `fl32` compressionType",
            value: common.sample_size as i64,
        });
    }
    let bytes_per_sample = 4usize;
    let frames = frame_count(common, payload, bytes_per_sample)?;
    let n_ch = common.num_channels as usize;
    let mut planes = vec![Vec::with_capacity(frames); n_ch];
    let frame_size = bytes_per_sample * n_ch;
    for f in 0..frames {
        let frame = &payload[f * frame_size..f * frame_size + frame_size];
        for (ch, plane) in planes.iter_mut().enumerate().take(n_ch) {
            let off = ch * bytes_per_sample;
            let bits =
                u32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            plane.push(f32::from_bits(bits));
        }
    }
    Ok(PcmSamples::F32(planes))
}

fn decode_f64_be(common: &CommonChunk, payload: &[u8]) -> Result<PcmSamples> {
    if common.sample_size != 64 && common.sample_size != 32 {
        // The spec says `sampleSize` is the uncompressed bit depth;
        // for fl64 it should be 64. Some files set it to 32 (the
        // "AIFC source bit depth was 32-bit float"); accept both.
        return Err(AiffError::InvalidValue {
            what: "sampleSize for `fl64` compressionType",
            value: common.sample_size as i64,
        });
    }
    let bytes_per_sample = 8usize;
    let frames = frame_count(common, payload, bytes_per_sample)?;
    let n_ch = common.num_channels as usize;
    let mut planes = vec![Vec::with_capacity(frames); n_ch];
    let frame_size = bytes_per_sample * n_ch;
    for f in 0..frames {
        let frame = &payload[f * frame_size..f * frame_size + frame_size];
        for (ch, plane) in planes.iter_mut().enumerate().take(n_ch) {
            let off = ch * bytes_per_sample;
            let bits = u64::from_be_bytes([
                frame[off],
                frame[off + 1],
                frame[off + 2],
                frame[off + 3],
                frame[off + 4],
                frame[off + 5],
                frame[off + 6],
                frame[off + 7],
            ]);
            plane.push(f64::from_bits(bits));
        }
    }
    Ok(PcmSamples::F64(planes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common(channels: u16, frames: u32, bits: u16, ct: Option<[u8; 4]>) -> CommonChunk {
        CommonChunk {
            num_channels: channels,
            num_sample_frames: frames,
            sample_size: bits,
            sample_rate: 44_100.0,
            compression_type: ct,
            compression_name: ct.map(|_| String::new()),
        }
    }

    #[test]
    fn none_16bit_stereo_be() {
        // 2 frames of 16-bit stereo: ch0_f0=0x0001, ch1_f0=-1,
        // ch0_f1=0x7fff, ch1_f1=-0x8000.
        let payload: [u8; 8] = [0x00, 0x01, 0xff, 0xff, 0x7f, 0xff, 0x80, 0x00];
        let c = common(2, 2, 16, Some(*b"NONE"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => {
                assert_eq!(p[0], vec![0x0001_0000, 0x7fff_0000]);
                assert_eq!(p[1], vec![-0x0001_0000, -0x8000_0000]);
            }
            _ => panic!("expected I32"),
        }
    }

    #[test]
    fn twos_is_synonym_for_none() {
        let payload: [u8; 4] = [0x00, 0x01, 0xff, 0xff];
        let c_none = common(1, 2, 16, Some(*b"NONE"));
        let c_twos = common(1, 2, 16, Some(*b"twos"));
        let a = decode_pcm(&c_none, &payload).unwrap();
        let b = decode_pcm(&c_twos, &payload).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn sowt_swaps_byte_order() {
        // Little-endian payload for the same logical samples as the
        // big-endian test above.
        let payload: [u8; 8] = [0x01, 0x00, 0xff, 0xff, 0xff, 0x7f, 0x00, 0x80];
        let c = common(2, 2, 16, Some(*b"sowt"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => {
                assert_eq!(p[0], vec![0x0001_0000, 0x7fff_0000]);
                assert_eq!(p[1], vec![-0x0001_0000, -0x8000_0000]);
            }
            _ => panic!("expected I32"),
        }
    }

    #[test]
    fn raw_is_unsigned_offset_binary() {
        // mid (silence) = 0x80; full positive = 0xff -> +127;
        // full negative = 0x00 -> -128.
        let payload: [u8; 3] = [0x80, 0xff, 0x00];
        let c = common(1, 3, 8, Some(*b"raw "));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => {
                assert_eq!(p[0], vec![0, 127 << 24, -128 << 24]);
            }
            _ => panic!("expected I32"),
        }
    }

    #[test]
    fn fl32_round_trip() {
        // 2 mono float samples: 1.0 and -0.5.
        let mut payload = Vec::new();
        payload.extend_from_slice(&1.0_f32.to_be_bytes());
        payload.extend_from_slice(&(-0.5_f32).to_be_bytes());
        let c = common(1, 2, 32, Some(*b"fl32"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::F32(p) => {
                assert_eq!(p[0], vec![1.0, -0.5]);
            }
            _ => panic!("expected F32"),
        }
    }

    #[test]
    fn fl32_uppercase_variant_also_works() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0.25_f32.to_be_bytes());
        let c = common(1, 1, 32, Some(*b"FL32"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::F32(p) => assert_eq!(p[0], vec![0.25]),
            _ => panic!("expected F32"),
        }
    }

    #[test]
    fn fl64_round_trip() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1.0_f64.to_be_bytes());
        payload.extend_from_slice(&(-0.25_f64).to_be_bytes());
        let c = common(1, 2, 64, Some(*b"fl64"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::F64(p) => assert_eq!(p[0], vec![1.0, -0.25]),
            _ => panic!("expected F64"),
        }
    }

    #[test]
    fn unsupported_compression_errors() {
        let c = common(1, 1, 16, Some(*b"ima4"));
        let payload = vec![0u8; 64];
        let r = decode_pcm(&c, &payload);
        assert!(matches!(
            r,
            Err(AiffError::UnsupportedPcmCompression(ct)) if ct == *b"ima4"
        ));
    }

    #[test]
    fn aiff_v1_3_treated_as_none() {
        // No compressionType (AIFF v1.3 form).
        let payload: [u8; 4] = [0x00, 0x01, 0xff, 0xff];
        let c = common(1, 2, 16, None);
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => {
                assert_eq!(p[0], vec![0x0001_0000, -0x0001_0000]);
            }
            _ => panic!("expected I32"),
        }
    }

    #[test]
    fn frames_truncate_to_payload_when_short() {
        // COMM declares 4 frames but only 2 frames of payload follow.
        let payload: [u8; 4] = [0x00, 0x10, 0xff, 0xf0];
        let c = common(1, 4, 16, Some(*b"NONE"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => assert_eq!(p[0].len(), 2),
            _ => panic!("expected I32"),
        }
    }

    #[test]
    fn unaligned_short_payload_errors() {
        // 16-bit stereo expects 4 bytes/frame; payload is 3 bytes.
        let payload = [0x00_u8, 0x01, 0xff];
        let c = common(2, 2, 16, Some(*b"NONE"));
        let r = decode_pcm(&c, &payload);
        assert!(matches!(r, Err(AiffError::SsndUnaligned { .. })));
    }

    #[test]
    fn raw_rejects_non_8bit_sample_size() {
        let c = common(1, 1, 16, Some(*b"raw "));
        let r = decode_pcm(&c, &[0u8, 0]);
        assert!(matches!(r, Err(AiffError::InvalidValue { .. })));
    }

    #[test]
    fn is_pcm_compression_matrix() {
        for ct in [
            b"NONE", b"twos", b"sowt", b"raw ", b"fl32", b"FL32", b"fl64", b"FL64",
        ] {
            assert!(is_pcm_compression(*ct));
        }
        for ct in [b"ima4", b"alaw", b"ulaw", b"GSM ", b"G722", b"MAC3"] {
            assert!(!is_pcm_compression(*ct));
        }
    }

    #[test]
    fn channels_and_frames_accessors() {
        let pcm = PcmSamples::I32(vec![vec![1, 2, 3], vec![4, 5, 6]]);
        assert_eq!(pcm.channels(), 2);
        assert_eq!(pcm.frames(), 3);

        let pcm = PcmSamples::F32(vec![vec![]]);
        assert_eq!(pcm.channels(), 1);
        assert_eq!(pcm.frames(), 0);
    }

    #[test]
    fn eightbit_be_round_trips_through_padding() {
        // 8-bit mono: each sample sign-extended to i32 then
        // left-justified, so 0x7f -> +127<<24, 0x80 -> -128<<24.
        let payload = [0x7f_u8, 0x80, 0x00];
        let c = common(1, 3, 8, Some(*b"NONE"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => {
                assert_eq!(p[0], vec![127 << 24, -128 << 24, 0]);
            }
            _ => panic!("expected I32"),
        }
    }

    #[test]
    fn twenty_four_bit_be_round_trip() {
        // 24-bit mono frames: 0x010203 = 66051, 0xFEFDFC sign-extends
        // to -66052.
        let payload = [0x01_u8, 0x02, 0x03, 0xfe, 0xfd, 0xfc];
        let c = common(1, 2, 24, Some(*b"NONE"));
        let pcm = decode_pcm(&c, &payload).unwrap();
        match pcm {
            PcmSamples::I32(p) => {
                assert_eq!(p[0][0], 0x0102_0300);
                assert_eq!(p[0][1], 0xfefd_fc00u32 as i32);
            }
            _ => panic!("expected I32"),
        }
    }
}
