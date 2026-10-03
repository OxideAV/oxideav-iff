//! The standalone image types: the shapes every `oxideav-<format>`
//! image crate shares (`IMAGE_CRATE_API`), specialised for the IFF
//! raster FORMs.
//!
//! * [`IffImage`] — the native-layout image [`crate::decode`] returns
//!   and [`crate::encode`] consumes: dimensions, a [`PixelFormat`] tag,
//!   one packed [`Plane`], [`ColorInfo`], [`Metadata`], an optional
//!   [`Palette`] (`Pal8` pictures) plus the IFF extras — which
//!   [`IffForm`] the pixels came from (or should be written as), the
//!   `BMHD` plane count, the `CAMG` viewmode and the pixel aspect.
//! * [`RgbImage`] / [`RgbaImage`] — the tightly packed 8-bit raw paths
//!   ([`crate::decode_rgb8`] / [`crate::decode_rgba8`],
//!   [`IffImage::to_rgb8`] / [`IffImage::to_rgba8`]).
//! * [`ImageInfo`] — what [`crate::info`] reads from the property chunks
//!   (`BMHD` / `CMAP` / `CAMG` / `DGBL` / `DPEL`) without touching a
//!   pixel.
//! * [`Frame`] — one picture of a `FORM ANIM` / multi-`DBOD` `FORM DEEP`
//!   / `CAT` / `LIST` for [`crate::decode_all`].
//!
//! Defined here (rather than reusing `oxideav_core::VideoFrame`) so the
//! crate builds with the default `registry` feature off — i.e. without
//! depending on `oxideav-core` at all. With `registry` on,
//! `crate::registry` adds the `From<IffImage> for VideoFrame`
//! conversion and its inverse so the framework `Decoder` / `Encoder`
//! are thin adapters over the same functions.

use std::time::Duration;

use crate::error::{IffError, Result};

/// The pixel layouts an IFF picture decodes to or encodes from. Variant
/// names mirror `oxideav_core::PixelFormat` exactly.
///
/// * `Pal8` — one palette index per pixel plus [`Palette`]: every
///   single-palette planar `ILBM` / `ACBM` and chunky `PBM ` picture
///   (1–8 bitplanes, including Extra-Half-Brite, whose 64-entry table
///   is the expanded palette). A transparent-colour key becomes alpha 0
///   on that palette entry.
/// * `Rgb24` — HAM6 / HAM8 (the hold-and-modify state machine has no
///   indexed representation), per-line palettes (`SHAM` / `PCHG`), the
///   24-bit literal-RGB `ILBM`, `DEEP` without an alpha component,
///   and `RGB8` / `RGBN` pictures whose genlock bit never fires.
/// * `Rgba` — pictures with per-pixel alpha: a `HasMask` mask plane,
///   `mskLasso` seed-fill transparency, `DEEP` with an alpha
///   component, and `RGB8` / `RGBN` under a genlock policy that clears
///   pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IffPixelFormat {
    /// 8-bit palette indices, one byte per pixel; see [`IffImage::palette`].
    Pal8,
    /// Packed 8-bit RGB, 3 bytes per pixel.
    Rgb24,
    /// Packed 8-bit RGBA, 4 bytes per pixel.
    Rgba,
}

/// The contract name for [`IffPixelFormat`].
pub type PixelFormat = IffPixelFormat;

impl IffPixelFormat {
    /// Bytes per pixel of the packed plane.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba => 4,
            Self::Rgb24 => 3,
            Self::Pal8 => 1,
        }
    }

    /// `true` for the layout that carries a per-pixel alpha channel.
    pub fn has_alpha(self) -> bool {
        matches!(self, Self::Rgba)
    }
}

/// Which IFF raster FORM a picture came from, or should be written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum IffForm {
    /// `FORM ILBM` — planar bitplanes, row-interleaved `BODY`
    /// (indexed 1–8 planes, EHB, HAM6 / HAM8, or 24-bit literal RGB).
    #[default]
    Ilbm,
    /// `FORM PBM ` — chunky 8-bit-per-pixel `BODY` (DPaint II / Brilliance).
    Pbm,
    /// `FORM ACBM` — plane-contiguous, uncompressed `ABIT` (AmigaBASIC).
    Acbm,
    /// `FORM DEEP` — chunky deep-pixel raster (`DGBL` / `DPEL` / `DBOD`).
    Deep,
    /// `FORM RGB8` — Turbo Silver 24-bit genlock-RLE true colour.
    Rgb8,
    /// `FORM RGBN` — Turbo Silver 12-bit genlock-RLE true colour.
    Rgbn,
}

impl IffForm {
    /// The four-character form type as written on disk.
    pub fn form_type(self) -> [u8; 4] {
        match self {
            Self::Ilbm => *b"ILBM",
            Self::Pbm => *b"PBM ",
            Self::Acbm => *b"ACBM",
            Self::Deep => *b"DEEP",
            Self::Rgb8 => *b"RGB8",
            Self::Rgbn => *b"RGBN",
        }
    }

    /// The [`IffForm`] for an on-disk form type, `None` for anything
    /// that is not one of the still-image raster FORMs this crate reads.
    pub fn from_form_type(ft: &[u8; 4]) -> Option<Self> {
        Some(match ft {
            b"ILBM" => Self::Ilbm,
            b"PBM " => Self::Pbm,
            b"ACBM" => Self::Acbm,
            b"DEEP" => Self::Deep,
            b"RGB8" => Self::Rgb8,
            b"RGBN" => Self::Rgbn,
            _ => return None,
        })
    }
}

/// One packed image plane: `stride` bytes per row, rows top-to-bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plane {
    /// Bytes from the start of one row to the start of the next.
    pub stride: usize,
    /// At least `stride * (height - 1) + width * bytes_per_pixel` bytes.
    pub data: Vec<u8>,
}

impl Plane {
    /// A plane from its stride and row-major bytes.
    pub fn new(stride: usize, data: Vec<u8>) -> Self {
        Self { stride, data }
    }
}

/// Sample range of the colour signal (H.273 `video_full_range_flag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ColorRange {
    /// The file does not say.
    #[default]
    Unspecified,
    /// Limited ("video", 16–235) range.
    Limited,
    /// Full (0–255) range.
    Full,
}

/// Colour signalling: range plus the H.273 code points for primaries,
/// transfer characteristics and matrix coefficients.
///
/// IFF raster FORMs carry no colorimetry at all — `CAMG` is a display
/// mode word, not a colour description — so every decoded picture gets
/// the documented convention [`ColorInfo::iff_default`]: full range, RGB
/// (identity matrix), primaries and transfer unspecified (`2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ColorInfo {
    /// Sample range.
    pub range: ColorRange,
    /// H.273 `ColourPrimaries` code point (`2` = unspecified).
    pub primaries: u8,
    /// H.273 `TransferCharacteristics` code point (`2` = unspecified).
    pub transfer: u8,
    /// H.273 `MatrixCoefficients` code point (`0` = identity / RGB).
    pub matrix: u8,
}

impl ColorInfo {
    /// H.273 "unspecified" code point.
    pub const UNSPECIFIED: u8 = 2;
    /// H.273 identity (RGB) matrix.
    pub const MATRIX_IDENTITY: u8 = 0;
    /// H.273 BT.709 primaries.
    pub const PRIMARIES_BT709: u8 = 1;
    /// H.273 sRGB / IEC 61966-2-1 transfer.
    pub const TRANSFER_SRGB: u8 = 13;

    /// Explicit code points.
    pub const fn new(range: ColorRange, primaries: u8, transfer: u8, matrix: u8) -> Self {
        Self {
            range,
            primaries,
            transfer,
            matrix,
        }
    }

    /// Everything unspecified.
    pub const fn unspecified() -> Self {
        Self::new(
            ColorRange::Unspecified,
            Self::UNSPECIFIED,
            Self::UNSPECIFIED,
            Self::UNSPECIFIED,
        )
    }

    /// The convention this crate stamps on every decoded picture: full
    /// range RGB with unspecified primaries and transfer. IFF has no
    /// colour signalling, so this is a documented default, not a
    /// file-derived value (and the registry adapter does not stamp it
    /// on frames).
    pub const fn iff_default() -> Self {
        Self::new(
            ColorRange::Full,
            Self::UNSPECIFIED,
            Self::UNSPECIFIED,
            Self::MATRIX_IDENTITY,
        )
    }

    /// Full-range sRGB (BT.709 primaries, sRGB transfer, identity matrix).
    pub const fn srgb() -> Self {
        Self::new(
            ColorRange::Full,
            Self::PRIMARIES_BT709,
            Self::TRANSFER_SRGB,
            Self::MATRIX_IDENTITY,
        )
    }

    /// Builder: replace the range.
    pub fn with_range(mut self, range: ColorRange) -> Self {
        self.range = range;
        self
    }

    /// Builder: replace the primaries code point.
    pub fn with_primaries(mut self, primaries: u8) -> Self {
        self.primaries = primaries;
        self
    }

    /// Builder: replace the transfer code point.
    pub fn with_transfer(mut self, transfer: u8) -> Self {
        self.transfer = transfer;
        self
    }

    /// Builder: replace the matrix code point.
    pub fn with_matrix(mut self, matrix: u8) -> Self {
        self.matrix = matrix;
        self
    }

    /// `true` when both primaries and transfer are specified.
    pub fn is_specified(&self) -> bool {
        self.primaries != Self::UNSPECIFIED && self.transfer != Self::UNSPECIFIED
    }
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self::iff_default()
    }
}

/// Embedded metadata blobs. IFF raster FORMs define none of these (no
/// ICC / Exif / XMP chunk is registered for `ILBM`, `DEEP`, `RGB8`,
/// `RGBN`), so decoded pictures carry an empty record; the fields exist
/// for shape parity with the other image crates.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Metadata {
    /// ICC profile bytes.
    pub icc: Option<Vec<u8>>,
    /// Exif payload (TIFF header onwards).
    pub exif: Option<Vec<u8>>,
    /// XMP packet.
    pub xmp: Option<Vec<u8>>,
    /// Encoding gamma as an exponent (PNG `gAMA` semantics).
    pub gamma: Option<f32>,
}

impl Metadata {
    /// An empty record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: attach an ICC profile.
    pub fn with_icc(mut self, icc: impl Into<Option<Vec<u8>>>) -> Self {
        self.icc = icc.into();
        self
    }

    /// Builder: attach an Exif payload.
    pub fn with_exif(mut self, exif: impl Into<Option<Vec<u8>>>) -> Self {
        self.exif = exif.into();
        self
    }

    /// Builder: attach an XMP packet.
    pub fn with_xmp(mut self, xmp: impl Into<Option<Vec<u8>>>) -> Self {
        self.xmp = xmp.into();
        self
    }

    /// Builder: set the encoding gamma.
    pub fn with_gamma(mut self, gamma: impl Into<Option<f32>>) -> Self {
        self.gamma = gamma.into();
        self
    }

    /// `true` when no field is set.
    pub fn is_empty(&self) -> bool {
        self.icc.is_none() && self.exif.is_none() && self.xmp.is_none() && self.gamma.is_none()
    }
}

/// An RGBA palette: the `CMAP` triples (EHB-expanded to 64 entries when
/// the viewmode says so) with alpha 255, except a `HasTransparentColor`
/// key whose entry carries alpha 0.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Palette {
    /// RGBA entries, index order.
    pub entries: Vec<[u8; 4]>,
}

impl Palette {
    /// A palette from RGBA entries.
    pub fn new(entries: Vec<[u8; 4]>) -> Self {
        Self { entries }
    }

    /// A palette from packed RGB bytes (alpha 255).
    pub fn from_rgb(rgb: &[u8]) -> Self {
        let entries = rgb
            .chunks_exact(3)
            .map(|e| [e[0], e[1], e[2], 255])
            .collect();
        Self { entries }
    }

    /// A palette from RGB triples (alpha 255).
    pub fn from_rgb_triples(rgb: &[[u8; 3]]) -> Self {
        Self {
            entries: rgb.iter().map(|e| [e[0], e[1], e[2], 255]).collect(),
        }
    }

    /// Entry count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `index`, if any.
    pub fn get(&self, index: u8) -> Option<[u8; 4]> {
        self.entries.get(usize::from(index)).copied()
    }

    /// Packed RGB bytes (alpha dropped).
    pub fn to_rgb(&self) -> Vec<u8> {
        self.entries
            .iter()
            .flat_map(|e| [e[0], e[1], e[2]])
            .collect()
    }

    /// RGB triples (alpha dropped) — the `CMAP` payload shape.
    pub fn to_rgb_triples(&self) -> Vec<[u8; 3]> {
        self.entries.iter().map(|e| [e[0], e[1], e[2]]).collect()
    }

    /// `true` when any entry is not fully opaque.
    pub fn has_alpha(&self) -> bool {
        self.entries.iter().any(|e| e[3] != 255)
    }

    /// The first entry whose alpha is 0 — the transparent-colour key an
    /// `ILBM` writer records as `BMHD.transparentColor`.
    pub fn transparent_index(&self) -> Option<u16> {
        self.entries
            .iter()
            .position(|e| e[3] == 0)
            .map(|i| i as u16)
    }
}

/// A decoded (or to-be-encoded) IFF picture in its native layout.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct IffImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Native layout of the single packed plane.
    pub format: PixelFormat,
    /// Exactly one packed plane.
    pub planes: Vec<Plane>,
    /// Colour signalling ([`ColorInfo::iff_default`] on decode).
    pub color: ColorInfo,
    /// Embedded metadata (always empty on decode; IFF defines none).
    pub metadata: Metadata,
    /// The palette of a `Pal8` picture; `None` for RGB layouts.
    pub palette: Option<Palette>,
    /// Which FORM the picture came from, or the one [`crate::encode`]
    /// writes when [`crate::EncodeOptions::form`] is `None`.
    pub form: IffForm,
    /// `BMHD.nPlanes` of the source (`1..=8` indexed, `24` literal RGB,
    /// `13` / `25` for `RGBN` / `RGB8`), or the total `DPEL` bit depth
    /// of a `DEEP` picture. `0` for caller-built images, meaning "let
    /// the encoder derive it".
    pub n_planes: u8,
    /// The `CAMG` viewmode longword when the file carried one (HAM /
    /// EHB / LACE / HIRES … bits, see [`crate::ilbm::Camg`]).
    pub viewmode: Option<u32>,
    /// `BMHD` pixel aspect `(x, y)` when the file carried a non-zero
    /// pair.
    pub aspect: Option<(u8, u8)>,
}

impl IffImage {
    /// Build an image from its parts, validating the plane geometry
    /// (exactly one plane whose stride and length cover
    /// `width × height` at the layout's bytes per pixel). Colour is
    /// [`ColorInfo::iff_default`], metadata empty, no palette.
    pub fn new(width: u32, height: u32, format: PixelFormat, planes: Vec<Plane>) -> Result<Self> {
        let img = Self::unchecked(width, height, format, planes);
        img.validate_planes()?;
        Ok(img)
    }

    /// Build a `Pal8` image from tightly packed indices and a palette;
    /// validates geometry and that every index is covered.
    pub fn new_indexed(
        width: u32,
        height: u32,
        indices: Vec<u8>,
        palette: Palette,
    ) -> Result<Self> {
        let img = Self::unchecked(
            width,
            height,
            PixelFormat::Pal8,
            vec![Plane::new(width as usize, indices)],
        )
        .with_palette(palette);
        img.validate()?;
        Ok(img)
    }

    pub(crate) fn unchecked(
        width: u32,
        height: u32,
        format: PixelFormat,
        planes: Vec<Plane>,
    ) -> Self {
        Self {
            width,
            height,
            format,
            planes,
            color: ColorInfo::iff_default(),
            metadata: Metadata::default(),
            palette: None,
            form: IffForm::Ilbm,
            n_planes: 0,
            viewmode: None,
            aspect: None,
        }
    }

    /// Build a packed image (stride = `width × bytes_per_pixel`).
    pub fn packed(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Self> {
        let stride = (width as usize)
            .checked_mul(format.bytes_per_pixel())
            .ok_or_else(|| IffError::invalid("IFF: row size overflows"))?;
        Self::new(width, height, format, vec![Plane::new(stride, data)])
    }

    /// Packed `Rgb24`, `3 × width` bytes per row.
    pub fn from_rgb8(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        Self::packed(width, height, PixelFormat::Rgb24, data)
    }

    /// Packed `Rgba`, `4 × width` bytes per row.
    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        Self::packed(width, height, PixelFormat::Rgba, data)
    }

    /// Builder: replace the colour signalling.
    pub fn with_color(mut self, color: ColorInfo) -> Self {
        self.color = color;
        self
    }

    /// Builder: replace the metadata.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Builder: attach (or clear) the palette.
    pub fn with_palette(mut self, palette: impl Into<Option<Palette>>) -> Self {
        self.palette = palette.into();
        self
    }

    /// Builder: set the FORM the encoder writes by default.
    pub fn with_form(mut self, form: IffForm) -> Self {
        self.form = form;
        self
    }

    /// Builder: set the `BMHD` plane count.
    pub fn with_n_planes(mut self, n_planes: u8) -> Self {
        self.n_planes = n_planes;
        self
    }

    /// Builder: set (or clear) the `CAMG` viewmode.
    pub fn with_viewmode(mut self, viewmode: impl Into<Option<u32>>) -> Self {
        self.viewmode = viewmode.into();
        self
    }

    /// Builder: set (or clear) the pixel aspect.
    pub fn with_aspect(mut self, aspect: impl Into<Option<(u8, u8)>>) -> Self {
        self.aspect = aspect.into();
        self
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Native layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Bytes per pixel of the native layout.
    pub fn bytes_per_pixel(&self) -> usize {
        self.format.bytes_per_pixel()
    }

    /// Stride of the single plane (0 when there is none).
    pub fn stride(&self) -> usize {
        self.planes.first().map(|p| p.stride).unwrap_or(0)
    }

    /// The single packed plane's bytes (`Some` for every IFF layout).
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.planes.first().map(|p| p.data.as_slice())
    }

    /// The plane bytes, or an empty slice when there is no plane.
    pub fn data(&self) -> &[u8] {
        self.as_bytes().unwrap_or(&[])
    }

    /// Mutable plane bytes.
    pub fn data_mut(&mut self) -> &mut [u8] {
        match self.planes.first_mut() {
            Some(p) => p.data.as_mut_slice(),
            None => &mut [],
        }
    }

    /// The planes concatenated in order (one plane for every IFF layout).
    pub fn into_raw(self) -> Vec<u8> {
        let mut planes = self.planes.into_iter();
        let mut out = planes.next().map(|p| p.data).unwrap_or_default();
        for p in planes {
            out.extend_from_slice(&p.data);
        }
        out
    }

    /// `true` when the layout or the palette carries alpha.
    pub fn has_alpha(&self) -> bool {
        self.format.has_alpha() || self.palette.as_ref().is_some_and(Palette::has_alpha)
    }

    pub(crate) fn validate_planes(&self) -> Result<()> {
        if self.planes.len() != 1 {
            return Err(IffError::invalid(format!(
                "IFF: expected exactly one packed plane, got {}",
                self.planes.len()
            )));
        }
        let plane = &self.planes[0];
        let row = (self.width as usize)
            .checked_mul(self.bytes_per_pixel())
            .ok_or_else(|| IffError::invalid("IFF: row size overflows"))?;
        if plane.stride < row {
            return Err(IffError::invalid(format!(
                "IFF: stride {} shorter than a {}-pixel row of {} bytes",
                plane.stride, self.width, row
            )));
        }
        let need = if self.height == 0 {
            0
        } else {
            plane
                .stride
                .checked_mul(self.height as usize - 1)
                .and_then(|n| n.checked_add(row))
                .ok_or_else(|| IffError::invalid("IFF: plane size overflows"))?
        };
        if plane.data.len() < need {
            return Err(IffError::invalid(format!(
                "IFF: plane holds {} bytes, {}x{} at stride {} needs {}",
                plane.data.len(),
                self.width,
                self.height,
                plane.stride,
                need
            )));
        }
        Ok(())
    }

    /// Full validation: plane geometry, and for `Pal8` a non-empty
    /// palette of at most 256 entries that covers every index used.
    pub fn validate(&self) -> Result<()> {
        self.validate_planes()?;
        if self.format == PixelFormat::Pal8 {
            let pal = self
                .palette
                .as_ref()
                .ok_or_else(|| IffError::invalid("IFF: Pal8 image without a palette"))?;
            if pal.is_empty() {
                return Err(IffError::invalid("IFF: Pal8 image with an empty palette"));
            }
            if pal.len() > 256 {
                return Err(IffError::invalid(format!(
                    "IFF: palette has {} entries; an 8-bit index addresses at most 256",
                    pal.len()
                )));
            }
            let n = pal.len();
            let w = self.width as usize;
            let stride = self.stride();
            let data = self.data();
            for y in 0..self.height as usize {
                let row = &data[y * stride..y * stride + w];
                if let Some(&bad) = row.iter().find(|&&i| usize::from(i) >= n) {
                    return Err(IffError::invalid(format!(
                        "IFF: palette index {bad} out of range (palette has {n} entries)"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Tightly packed RGBA8 (`4 × width` bytes per row). Palette
    /// expanded (uncovered indices become opaque black), `Rgb24` gets
    /// alpha 255. Exact for every layout this crate decodes.
    pub fn to_rgba8(&self) -> Vec<u8> {
        self.convert(true)
    }

    /// Tightly packed RGB8 (`3 × width` bytes per row); alpha dropped.
    pub fn to_rgb8(&self) -> Vec<u8> {
        self.convert(false)
    }

    /// [`Self::to_rgba8`] after [`Self::validate`], for caller-built images.
    pub fn try_to_rgba8(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(self.convert(true))
    }

    /// [`Self::to_rgb8`] after [`Self::validate`], for caller-built images.
    pub fn try_to_rgb8(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(self.convert(false))
    }

    fn convert(&self, alpha: bool) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let out_bpp = if alpha { 4 } else { 3 };
        let mut out = vec![0u8; w * h * out_bpp];
        if alpha {
            out.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
        }
        if w == 0 || h == 0 {
            return out;
        }
        let bpp = self.bytes_per_pixel();
        let stride = self.stride();
        let src = self.data();
        let row_bytes = w * bpp;

        // Palette lookup table: 256 RGBA cells; entries the palette
        // does not cover are opaque black, matching the ILBM renderer's
        // out-of-range rule.
        let lut: [[u8; 4]; 256] = match (&self.palette, self.format) {
            (Some(p), PixelFormat::Pal8) => {
                let mut lut = [[0, 0, 0, 255u8]; 256];
                for (slot, e) in lut.iter_mut().zip(p.entries.iter()) {
                    *slot = *e;
                }
                lut
            }
            _ => [[0, 0, 0, 255u8]; 256],
        };

        for y in 0..h {
            let Some(row) = src.get(y * stride..y * stride + row_bytes) else {
                break;
            };
            let dst = &mut out[y * w * out_bpp..(y + 1) * w * out_bpp];
            match self.format {
                PixelFormat::Rgba => {
                    for (s, d) in row.chunks_exact(4).zip(dst.chunks_exact_mut(out_bpp)) {
                        d.copy_from_slice(&s[..out_bpp]);
                    }
                }
                PixelFormat::Rgb24 => {
                    for (s, d) in row.chunks_exact(3).zip(dst.chunks_exact_mut(out_bpp)) {
                        d[..3].copy_from_slice(s);
                    }
                }
                PixelFormat::Pal8 => {
                    for (&i, d) in row.iter().zip(dst.chunks_exact_mut(out_bpp)) {
                        d.copy_from_slice(&lut[usize::from(i)][..out_bpp]);
                    }
                }
            }
        }
        out
    }
}

/// Tightly packed 8-bit RGB, 3 bytes per pixel, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `3 × width × height` bytes.
    pub data: Vec<u8>,
}

impl RgbImage {
    /// Wrap packed RGB bytes.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    /// The pixel bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consume into the pixel bytes.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }

    /// Bytes per row.
    pub fn stride(&self) -> usize {
        self.width as usize * 3
    }
}

/// Tightly packed 8-bit RGBA, 4 bytes per pixel, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `4 × width × height` bytes.
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// Wrap packed RGBA bytes.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    /// The pixel bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consume into the pixel bytes.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }

    /// Bytes per row.
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

/// What [`crate::info`] learns from the property chunks without
/// decoding a pixel.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The layout [`crate::decode`] will return.
    pub format: PixelFormat,
    /// Picture count: `1` for a still, the frame count of a `FORM ANIM`
    /// or multi-`DBOD` `FORM DEEP`, the child count of a `CAT` / `LIST`.
    pub frames: u32,
    /// `true` when the decoded picture carries alpha (mask plane,
    /// lasso, transparent-colour key, `DEEP` alpha component, genlock).
    pub has_alpha: bool,
    /// Colour signalling (always [`ColorInfo::iff_default`]).
    pub color: ColorInfo,
    /// Always `false`: IFF defines no ICC chunk.
    pub has_icc: bool,
    /// Always `false`: IFF defines no Exif chunk.
    pub has_exif: bool,
    /// Always `false`: IFF defines no XMP chunk.
    pub has_xmp: bool,
    /// The raster FORM (for `ANIM`, the seed frame's; for `CAT` /
    /// `LIST`, the first child's).
    pub form: IffForm,
    /// `BMHD.nPlanes` (or the total `DPEL` bit depth of a `DEEP`).
    pub n_planes: u8,
    /// `BMHD.compression` byte (`0` none, `1` ByteRun1, `4` Turbo
    /// Silver RLE) or the `DGBL` compression code.
    pub compression: u8,
    /// `BMHD.masking` byte (`0` none, `1` mask plane, `2` transparent
    /// colour, `3` lasso); `0` for `DEEP`.
    pub masking: u8,
    /// The `CAMG` viewmode when present.
    pub viewmode: Option<u32>,
    /// `CMAP` entry count (before EHB expansion).
    pub palette_len: u32,
    /// `true` when the picture decodes through the HAM6 / HAM8 state
    /// machine (explicit `CAMG` HAM, or the assumed-HAM6 reading of a
    /// 6-plane picture with no usable `CAMG`).
    pub ham: bool,
    /// `true` when the picture uses Extra-Half-Brite.
    pub ehb: bool,
}

impl ImageInfo {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        frames: u32,
        has_alpha: bool,
        form: IffForm,
    ) -> Self {
        Self {
            width,
            height,
            format,
            frames,
            has_alpha,
            color: ColorInfo::iff_default(),
            has_icc: false,
            has_exif: false,
            has_xmp: false,
            form,
            n_planes: 0,
            compression: 0,
            masking: 0,
            viewmode: None,
            palette_len: 0,
            ham: false,
            ehb: false,
        }
    }
}

/// One picture of a multi-image file ([`crate::decode_all`]).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Frame {
    /// The decoded picture.
    pub image: IffImage,
    /// How long the picture is shown before the next: the ANIM timeline
    /// duration (the following frame's `ANHD.reltime` in 1/60 s
    /// jiffies, see [`crate::anim::AnimImage::playback`]) or the `DEEP`
    /// `DCHG` frame delay. `None` for stills and `CAT` / `LIST` children.
    pub delay: Option<Duration>,
    /// Position in the file (frame number, `DBOD` number, child number).
    pub index: u32,
}

impl Frame {
    /// A frame from its parts.
    pub fn new(image: IffImage, delay: Option<Duration>, index: u32) -> Self {
        Self {
            image,
            delay,
            index,
        }
    }

    /// Builder: set the delay.
    pub fn with_delay(mut self, delay: impl Into<Option<Duration>>) -> Self {
        self.delay = delay.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_validate_geometry() {
        assert!(IffImage::from_rgb8(2, 2, vec![0; 12]).is_ok());
        assert!(matches!(
            IffImage::from_rgb8(2, 2, vec![0; 11]),
            Err(IffError::InvalidData(_))
        ));
        assert!(matches!(
            IffImage::from_rgba8(2, 2, vec![0; 12]),
            Err(IffError::InvalidData(_))
        ));
        assert!(matches!(
            IffImage::new(1, 1, PixelFormat::Rgb24, vec![]),
            Err(IffError::InvalidData(_))
        ));
        let pal = Palette::from_rgb_triples(&[[1, 2, 3]]);
        assert!(matches!(
            IffImage::new_indexed(2, 1, vec![0, 1], pal.clone()),
            Err(IffError::InvalidData(_))
        ));
        assert!(IffImage::new_indexed(2, 1, vec![0, 0], pal).is_ok());
    }

    #[test]
    fn conversions_are_exact() {
        let pal = Palette::new(vec![[10, 20, 30, 255], [1, 2, 3, 0]]);
        let img = IffImage::new_indexed(2, 1, vec![0, 1], pal).unwrap();
        assert_eq!(img.to_rgba8(), vec![10, 20, 30, 255, 1, 2, 3, 0]);
        assert_eq!(img.to_rgb8(), vec![10, 20, 30, 1, 2, 3]);
        assert!(img.has_alpha());
        assert_eq!(img.palette.as_ref().unwrap().transparent_index(), Some(1));

        let rgb = IffImage::from_rgb8(1, 1, vec![9, 8, 7]).unwrap();
        assert_eq!(rgb.to_rgba8(), vec![9, 8, 7, 255]);
        assert_eq!(rgb.into_raw(), vec![9, 8, 7]);

        let rgba = IffImage::from_rgba8(1, 1, vec![9, 8, 7, 6]).unwrap();
        assert_eq!(rgba.to_rgb8(), vec![9, 8, 7]);
        assert_eq!(rgba.as_bytes(), Some(&[9u8, 8, 7, 6][..]));
    }

    #[test]
    fn form_types_round_trip() {
        for f in [
            IffForm::Ilbm,
            IffForm::Pbm,
            IffForm::Acbm,
            IffForm::Deep,
            IffForm::Rgb8,
            IffForm::Rgbn,
        ] {
            assert_eq!(IffForm::from_form_type(&f.form_type()), Some(f));
        }
        assert_eq!(IffForm::from_form_type(b"8SVX"), None);
    }
}
