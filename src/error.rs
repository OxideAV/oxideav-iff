//! Crate-local error type used by `oxideav-iff`'s standalone (no
//! `oxideav-core`) public API.
//!
//! When the `registry` feature is enabled, [`IffError`] gains a
//! `From<IffError> for oxideav_core::Error` impl (defined in
//! `crate::registry`) so the trait-side surface (`Demuxer` / `Muxer` /
//! `Decoder` / `Encoder`) can keep returning `oxideav_core::Result<T>`
//! while the underlying parse / render / encode functions stay
//! framework-free.

use core::fmt;

/// `Result` alias scoped to `oxideav-iff`. Standalone (no
/// `oxideav-core`) callers see this; framework callers convert via the
/// gated `From<IffError> for oxideav_core::Error` impl.
pub type Result<T> = core::result::Result<T, IffError>;

/// The contract name for [`IffError`].
pub type Error = IffError;

/// Error variants returned by `oxideav-iff`'s standalone API.
///
/// The variants mirror the subset of `oxideav_core::Error` the IFF
/// walkers can hit plus the two the image-crate contract requires
/// (`LimitExceeded`, `Io`). Framework-specific errors
/// (`FormatNotFound`, `CodecNotFound`, `Eof`, `NeedMore`) originate in
/// the registry adapters, which already link `oxideav-core`.
#[derive(Debug)]
#[non_exhaustive]
pub enum IffError {
    /// The byte stream is malformed (a chunk runs past its FORM, a
    /// `BMHD` is short, a ByteRun1 row does not fill its plane, a
    /// mandatory chunk is missing, …), or a caller-assembled image is
    /// inconsistent (plane too short, palette index out of range, …).
    InvalidData(String),
    /// The byte stream uses a feature this crate does not implement
    /// (a DEEP `HUFFMAN` body, a HAM viewport on the chunky `PBM `
    /// form, an ANIM operation outside 0–8, …), or the encoder was
    /// asked for something the format cannot represent.
    Unsupported(String),
    /// A [`crate::DecodeOptions`] limit (dimensions / pixels / bytes)
    /// would be exceeded; nothing was allocated.
    LimitExceeded(String),
    /// A read / write on a caller-supplied stream failed
    /// ([`crate::decode_from`] / [`crate::encode_to`]).
    Io(std::io::Error),
}

impl IffError {
    /// Construct an [`IffError::InvalidData`] from a stringy message.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Construct an [`IffError::Unsupported`] from a stringy message.
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }

    /// Construct an [`IffError::LimitExceeded`] from a stringy message.
    pub fn limit(msg: impl Into<String>) -> Self {
        Self::LimitExceeded(msg.into())
    }
}

impl From<std::io::Error> for IffError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl fmt::Display for IffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidData(s) => write!(f, "invalid data: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::LimitExceeded(s) => write!(f, "limit exceeded: {s}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for IffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_source() {
        let e = IffError::invalid("x");
        assert_eq!(e.to_string(), "invalid data: x");
        assert!(std::error::Error::source(&e).is_none());
        let io: IffError = std::io::Error::other("boom").into();
        assert!(matches!(io, IffError::Io(_)));
        assert!(std::error::Error::source(&io).is_some());
        assert_eq!(IffError::unsupported("y").to_string(), "unsupported: y");
        assert_eq!(IffError::limit("z").to_string(), "limit exceeded: z");
    }
}
