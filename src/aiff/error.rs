//! Crate-local error type.
//!
//! The public surface uses [`AiffError`] directly even when the
//! `registry` feature is on. Mapping into `oxideav_core::Error`
//! happens at the demuxer trait boundary in `demuxer.rs`, so a
//! free-standing caller (with `default-features = false`) still
//! gets the structured variants below without any framework
//! dependency.

use core::fmt;

/// Errors produced by the AIFF / AIFF-C crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AiffError {
    /// A parser ran out of input before a field finished.
    ///
    /// The associated string names which structure was being parsed
    /// (`"FORM header"`, `"COMM chunk"`, `"SSND chunk header"`, …).
    Truncated(&'static str),

    /// The outermost chunk was not `FORM`.
    NotForm {
        /// The four bytes that were found in place of `FORM`.
        found: [u8; 4],
    },

    /// The form type was neither `AIFF` (uncompressed) nor `AIFC`
    /// (compressed). Both forms are defined by Apple's AIFF v1.3
    /// (1989) and AIFF-C (1991) specs.
    UnknownFormType {
        /// The four bytes found in the `formType` field.
        found: [u8; 4],
    },

    /// A chunk's declared `ckSize` exceeds the surrounding container.
    OversizedChunk {
        /// FourCC of the chunk whose declared length was too large.
        id: [u8; 4],
        /// Declared length.
        declared: u32,
        /// Bytes still available in the container.
        available: u32,
    },

    /// A required chunk was missing from the FORM.
    MissingChunk(&'static str),

    /// `COMM`'s `ckSize` was below the spec's minimum (18 for AIFF,
    /// 22 for AIFF-C before the compressionName pstring).
    CommTooShort {
        /// Form type the parser had selected when it found the short
        /// COMM (`*b"AIFF"` or `*b"AIFC"`).
        form_type: [u8; 4],
        /// Declared `ckSize`.
        declared: u32,
    },

    /// A field had a value the spec doesn't define.
    InvalidValue {
        /// Identifier of the field (e.g. `"numChannels"`).
        what: &'static str,
        /// The unexpected value, formatted as decimal.
        value: i64,
    },

    /// The 80-bit IEEE extended sample rate decoded to NaN, infinity,
    /// or a non-positive value.
    InvalidSampleRate,

    /// `sampleSize` (bits per sample) was outside the closed range
    /// `1..=32`.
    InvalidSampleSize(u16),

    /// A `compressionType` FourCC the caller asked about isn't one
    /// the PCM readers know how to handle. The caller may still
    /// route this through an external decoder (e.g. an ADPCM crate
    /// for `ima4` or a G.711 crate for `ulaw` / `alaw`).
    UnsupportedPcmCompression([u8; 4]),

    /// SSND payload bytes don't divide evenly into frames of the
    /// declared `frame_size_bytes`.
    SsndUnaligned {
        /// Bytes available in the SSND payload after `offset`.
        payload: usize,
        /// Bytes per sample frame implied by `numChannels` *
        /// `sampleSize`.
        frame_size: usize,
    },
}

impl fmt::Display for AiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(what) => write!(f, "truncated input while parsing {what}"),
            Self::NotForm { found } => write!(
                f,
                "outermost chunk is {:?}, expected `FORM`",
                fourcc_lossy(found)
            ),
            Self::UnknownFormType { found } => write!(
                f,
                "FORM type {:?} is neither `AIFF` nor `AIFC`",
                fourcc_lossy(found)
            ),
            Self::OversizedChunk {
                id,
                declared,
                available,
            } => write!(
                f,
                "chunk {:?} declared {declared} bytes but only {available} are available",
                fourcc_lossy(id)
            ),
            Self::MissingChunk(what) => write!(f, "FORM is missing required chunk `{what}`"),
            Self::CommTooShort {
                form_type,
                declared,
            } => write!(
                f,
                "COMM ckSize={declared} is below the minimum for form type {:?}",
                fourcc_lossy(form_type)
            ),
            Self::InvalidValue { what, value } => {
                write!(f, "invalid value {value} for field `{what}`")
            }
            Self::InvalidSampleRate => f.write_str("sampleRate decoded to NaN, infinity, or <= 0"),
            Self::InvalidSampleSize(b) => write!(f, "sampleSize={b} is outside 1..=32"),
            Self::UnsupportedPcmCompression(c) => write!(
                f,
                "compressionType {:?} is not a PCM flavour this crate handles",
                fourcc_lossy(c)
            ),
            Self::SsndUnaligned {
                payload,
                frame_size,
            } => write!(
                f,
                "SSND payload of {payload} bytes is not a multiple of frame_size={frame_size}"
            ),
        }
    }
}

impl std::error::Error for AiffError {}

/// Convenience `Result` alias used throughout the crate.
pub type Result<T> = core::result::Result<T, AiffError>;

/// Render a FourCC as a printable string, replacing non-ASCII bytes
/// with `?`. Pure formatting helper.
fn fourcc_lossy(b: &[u8; 4]) -> String {
    let mut s = String::with_capacity(4);
    for &c in b {
        if (0x20..=0x7e).contains(&c) {
            s.push(c as char);
        } else {
            s.push('?');
        }
    }
    s
}
