//! Decode-side limits ([`DecodeOptions`]) and encode-side behaviour
//! ([`EncodeOptions`]) for the `IMAGE_CRATE_API` root functions.

use crate::error::{IffError, Result};
use crate::ilbm::{Compression, GenlockPolicy, Masking};
use crate::image::IffForm;

/// Limits and strictness for [`crate::decode_with`] (and the functions
/// built on it). `None` means unlimited; the defaults are finite.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Reject pictures wider than this.
    pub max_width: Option<u32>,
    /// Reject pictures taller than this.
    pub max_height: Option<u32>,
    /// Reject pictures with more than this many pixels.
    pub max_pixels: Option<u64>,
    /// Reject pictures whose decoded RGBA working buffer
    /// (`width × height × 4`) would exceed this many bytes.
    pub max_bytes: Option<u64>,
    /// Reject what the lenient reader tolerates: a `FORM` size that
    /// runs past the end of the buffer (truncated file) and trailing
    /// bytes after the `FORM`.
    pub strict: bool,
    /// How the `RGB8` / `RGBN` genlock bit is interpreted
    /// (default: ignore it and keep the coded colour).
    pub genlock: GenlockPolicy,
    /// The 16-word signed delta table a TVDC-compressed `FORM DEEP`
    /// needs; the FORM does not carry it (§1.5 of the staged DEEP
    /// notes), so a TVDC picture decodes only when the caller supplies
    /// the table here.
    pub tvdc_table: Option<[i16; 16]>,
}

impl DecodeOptions {
    /// Default `max_bytes`: 1 GiB of decoded RGBA.
    pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set `max_width` (`None` = unlimited).
    pub fn with_max_width(mut self, max_width: impl Into<Option<u32>>) -> Self {
        self.max_width = max_width.into();
        self
    }

    /// Builder: set `max_height` (`None` = unlimited).
    pub fn with_max_height(mut self, max_height: impl Into<Option<u32>>) -> Self {
        self.max_height = max_height.into();
        self
    }

    /// Builder: set `max_pixels` (`None` = unlimited).
    pub fn with_max_pixels(mut self, max_pixels: impl Into<Option<u64>>) -> Self {
        self.max_pixels = max_pixels.into();
        self
    }

    /// Builder: set `max_bytes` (`None` = unlimited).
    pub fn with_max_bytes(mut self, max_bytes: impl Into<Option<u64>>) -> Self {
        self.max_bytes = max_bytes.into();
        self
    }

    /// Builder: set `strict`.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Builder: set the genlock policy.
    pub fn with_genlock(mut self, genlock: GenlockPolicy) -> Self {
        self.genlock = genlock;
        self
    }

    /// Builder: supply the TVDC delta table.
    pub fn with_tvdc_table(mut self, table: impl Into<Option<[i16; 16]>>) -> Self {
        self.tvdc_table = table.into();
        self
    }

    /// Builder: lift every limit.
    pub fn unlimited(mut self) -> Self {
        self.max_width = None;
        self.max_height = None;
        self.max_pixels = None;
        self.max_bytes = None;
        self
    }

    /// Check a header's geometry against the limits before anything is
    /// allocated. `bytes` is the decoded working-buffer size.
    pub(crate) fn check(&self, width: u32, height: u32, bytes: u64) -> Result<()> {
        if let Some(m) = self.max_width {
            if width > m {
                return Err(IffError::limit(format!(
                    "IFF: width {width} exceeds max_width {m}"
                )));
            }
        }
        if let Some(m) = self.max_height {
            if height > m {
                return Err(IffError::limit(format!(
                    "IFF: height {height} exceeds max_height {m}"
                )));
            }
        }
        let pixels = u64::from(width) * u64::from(height);
        if let Some(m) = self.max_pixels {
            if pixels > m {
                return Err(IffError::limit(format!(
                    "IFF: {pixels} pixels exceed max_pixels {m}"
                )));
            }
        }
        if let Some(m) = self.max_bytes {
            if bytes > m {
                return Err(IffError::limit(format!(
                    "IFF: decoded buffer of {bytes} bytes exceeds max_bytes {m}"
                )));
            }
        }
        Ok(())
    }
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_width: None,
            max_height: None,
            max_pixels: None,
            max_bytes: Some(Self::DEFAULT_MAX_BYTES),
            strict: false,
            genlock: GenlockPolicy::default(),
            tvdc_table: None,
        }
    }
}

/// The `ANHD.operation` [`crate::encode_all`] writes for frames `1..N`
/// of a `FORM ANIM`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AnimOp {
    /// Op-0 — full literal `BODY` per frame.
    Op0,
    /// Op-1 — XOR ILBM mode (full-frame rectangle, all planes).
    Op1,
    /// Op-2 — Long Delta mode.
    Op2,
    /// Op-3 — Short Delta mode.
    Op3,
    /// Op-4 — Generalized short / long Delta mode.
    Op4 {
        /// `true` writes 32-bit items (`ANHD.bits` bit 0).
        long_data: bool,
    },
    /// Op-5 — Byte Vertical Delta (the DeluxePaint workhorse).
    #[default]
    Op5,
    /// Op-7 — Short / Long Vertical Delta.
    Op7 {
        /// `true` writes 32-bit items.
        long_data: bool,
    },
    /// Op-8 — Anim8 short / long Vertical Delta.
    Op8 {
        /// `true` writes 32-bit items.
        long_data: bool,
    },
}

/// Encoder behaviour for [`crate::encode`] and the functions built on it.
/// Behaviour variants are fields; defaults write what a DeluxePaint-era
/// reader expects.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// Which FORM to write. `None` uses [`crate::IffImage::form`]
    /// (`Ilbm` for caller-built images).
    pub form: Option<IffForm>,
    /// `BODY` / `DBOD` compression: [`Compression::ByteRun1`] (default)
    /// or [`Compression::None`]; [`Compression::Auto`] writes whichever
    /// is shorter. `ACBM` is always uncompressed; `RGB8` / `RGBN` always
    /// use their own run-length coding.
    pub compression: Compression,
    /// RGB input into an `ILBM` / `ACBM` / `PBM `: `false` (default)
    /// writes the 24-bit literal-RGB `ILBM`; `true` quantises to at most
    /// 256 colours (the first 256 distinct RGB triples in scan order form
    /// the `CMAP`, later colours map to the nearest entry by squared RGB
    /// distance) and writes planar indexed bitplanes. `Pal8` input is
    /// always written index-for-index.
    pub indexed: bool,
    /// Bitplane count for indexed output (`1..=8`). `None` uses the
    /// image's `n_planes` when it covers the palette, else the smallest
    /// count that does.
    pub n_planes: Option<u8>,
    /// `CAMG` viewmode to write. `None` uses the image's; `Some(0)`
    /// writes no `CAMG`. Setting `CAMG_HAM` / `CAMG_EHB` selects the
    /// HAM / EHB encoders for RGB input.
    pub viewmode: Option<u32>,
    /// `BMHD.masking`. `None` derives it: a palette entry with alpha 0
    /// becomes `HasTransparentColor`, an `Rgba` input with any
    /// non-opaque pixel going to an indexed form becomes `HasMask`
    /// (alpha < 128 is masked out), otherwise `None`.
    pub masking: Option<Masking>,
    /// Allow an `Rgba` input to lose its alpha where the target has no
    /// alpha mechanism (24-bit `ILBM`, `RGB8` / `RGBN`, `DEEP` with
    /// `rgb_only`). Without it such an input is `Error::Unsupported`.
    pub drop_alpha: bool,
    /// `FORM DEEP`: write an RGB 8:8:8 `DPEL` even for `Rgba` input
    /// (needs `drop_alpha` when any pixel is not opaque).
    pub deep_rgb_only: bool,
    /// The delta operation [`crate::encode_all`] writes (default op-5).
    pub anim_op: AnimOp,
    /// `BMHD` pixel aspect. `None` uses the image's, falling back to 1:1.
    pub aspect: Option<(u8, u8)>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            form: None,
            compression: Compression::ByteRun1,
            indexed: false,
            n_planes: None,
            viewmode: None,
            masking: None,
            drop_alpha: false,
            deep_rgb_only: false,
            anim_op: AnimOp::default(),
            aspect: None,
        }
    }
}

impl EncodeOptions {
    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: pick the FORM.
    pub fn with_form(mut self, form: impl Into<Option<IffForm>>) -> Self {
        self.form = form.into();
        self
    }

    /// Builder: pick the body compression.
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Builder: quantise RGB input to indexed bitplanes.
    pub fn with_indexed(mut self, indexed: bool) -> Self {
        self.indexed = indexed;
        self
    }

    /// Builder: fix the bitplane count.
    pub fn with_n_planes(mut self, n_planes: impl Into<Option<u8>>) -> Self {
        self.n_planes = n_planes.into();
        self
    }

    /// Builder: set the `CAMG` viewmode.
    pub fn with_viewmode(mut self, viewmode: impl Into<Option<u32>>) -> Self {
        self.viewmode = viewmode.into();
        self
    }

    /// Builder: fix the masking mode.
    pub fn with_masking(mut self, masking: impl Into<Option<Masking>>) -> Self {
        self.masking = masking.into();
        self
    }

    /// Builder: allow alpha to be dropped.
    pub fn with_drop_alpha(mut self, drop_alpha: bool) -> Self {
        self.drop_alpha = drop_alpha;
        self
    }

    /// Builder: force an RGB-only `DEEP` layout.
    pub fn with_deep_rgb_only(mut self, rgb_only: bool) -> Self {
        self.deep_rgb_only = rgb_only;
        self
    }

    /// Builder: pick the ANIM delta operation.
    pub fn with_anim_op(mut self, op: AnimOp) -> Self {
        self.anim_op = op;
        self
    }

    /// Builder: set the pixel aspect.
    pub fn with_aspect(mut self, aspect: impl Into<Option<(u8, u8)>>) -> Self {
        self.aspect = aspect.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_fire_in_order() {
        let o = DecodeOptions::default()
            .with_max_width(10u32)
            .with_max_height(10u32)
            .with_max_pixels(50u64)
            .with_max_bytes(100u64);
        assert!(o.check(5, 5, 75).is_ok());
        assert!(matches!(o.check(11, 1, 1), Err(IffError::LimitExceeded(_))));
        assert!(matches!(o.check(1, 11, 1), Err(IffError::LimitExceeded(_))));
        assert!(matches!(o.check(8, 8, 1), Err(IffError::LimitExceeded(_))));
        assert!(matches!(
            o.check(5, 5, 101),
            Err(IffError::LimitExceeded(_))
        ));
        assert!(o.unlimited().check(u32::MAX, u32::MAX, u64::MAX).is_ok());
    }

    #[test]
    fn encode_defaults() {
        let o = EncodeOptions::default();
        assert_eq!(o.form, None);
        assert_eq!(o.compression, Compression::ByteRun1);
        assert!(!o.indexed);
        assert_eq!(o.n_planes, None);
        assert_eq!(o.anim_op, AnimOp::Op5);
        assert!(!o.drop_alpha);
    }
}
