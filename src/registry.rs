//! `oxideav-core` integration layer for `oxideav-iff`.
//!
//! Gated behind the default-on `registry` feature so image-library
//! consumers can depend on `oxideav-iff` with `default-features = false`
//! and skip the `oxideav-core` dependency entirely.
//!
//! The module exposes:
//! * [`register`] (the fleet `RuntimeContext` entry point, also what the
//!   `oxideav_core::register!` macro dispatches), [`register_codecs`] /
//!   [`register_containers`] / [`register_registries`] for callers
//!   holding the sub-registries.
//! * [`make_decoder`] / [`make_encoder`] — the factories of the `ilbm`
//!   image codec: one packet holding a whole IFF raster `FORM` in, one
//!   native-layout frame out (and back). They are thin adapters over
//!   [`crate::decode`] / [`crate::encode`] (one implementation).
//! * The frame bridge: `From<IffImage> for VideoFrame`,
//!   [`IffImage::from_video_frame`] and
//!   `TryFrom<(&VideoFrame, &CodecParameters)>`, plus the 1:1
//!   [`IffPixelFormat`] ↔ `oxideav_core::PixelFormat` name mapping.
//! * The `From<IffError> for oxideav_core::Error` conversion every
//!   framework trait impl in this crate relies on.
//!
//! The container demuxers (`iff_ilbm` / `iff_acbm` / `iff_rgb8` /
//! `iff_rgbn` / `iff_deep` / `iff_tvpp` / `iff_anim` / `iff_8svx` /
//! `aiff`) and muxers live next to the parsers they wrap
//! (`ilbm::framework`, `anim::framework`, `svx`, `aiff::demuxer`).
//! `iff_ilbm` / `iff_acbm` declare an `ilbm` codec stream in the
//! picture's native layout (`Pal8` + palette `extradata`, `Rgb24`,
//! `Rgba`) and hand the whole `FORM` to [`make_decoder`]; the DEEP /
//! TVPP / RGB8 / RGBN demuxers emit decoded `rawvideo` packets in
//! `Rgb24` or `Rgba` per the picture; `iff_anim` emits composited `Rgba`
//! frames; `iff_8svx` / `aiff` emit PCM.

use oxideav_core::{
    parse_options, BlobKind, CodecCapabilities, CodecId, CodecInfo, CodecOptionsStruct,
    CodecParameters, CodecRegistry, ColorPrimaries, ColorSignal, ContainerRegistry, Decoder,
    Encoder, Frame, MatrixCoefficients, MetadataBlob, OptionField, OptionKind, OptionValue, Packet,
    PixelFormat, RuntimeContext, TimeBase, TransferCharacteristics, VideoFrame, VideoPlane,
};

use crate::error::IffError;
use crate::ilbm::{Camg, Compression, Masking};
use crate::image::{
    ColorInfo, ColorRange, IffChunk, IffForm, IffImage, IffPixelFormat, Palette, Plane,
};
use crate::options::EncodeOptions;

/// Codec id of the IFF raster image codec (`make_decoder` /
/// `make_encoder`): a packet is one complete `FORM ILBM` / `PBM ` /
/// `ACBM` / `DEEP` / `RGB8` / `RGBN` (or the seed frame of an `ANIM`).
pub const CODEC_ID_STR: &str = "ilbm";

/// Convert an [`IffError`] into the framework-shared `oxideav_core::Error`
/// so trait impls in this crate can use `?` on errors returned by the
/// framework-free parse / encode functions.
impl From<IffError> for oxideav_core::Error {
    fn from(e: IffError) -> Self {
        match e {
            IffError::InvalidData(s) => oxideav_core::Error::InvalidData(s),
            IffError::Unsupported(s) => oxideav_core::Error::Unsupported(s),
            IffError::LimitExceeded(s) => oxideav_core::Error::InvalidData(s),
            IffError::Io(e) => oxideav_core::Error::Io(e),
        }
    }
}

// ---- Pixel-format and colour mapping (1:1 by name) ----

/// The 1:1 name mapping from the framework enum to [`IffPixelFormat`].
pub fn from_core_pixel_format(pf: PixelFormat) -> oxideav_core::Result<IffPixelFormat> {
    Ok(match pf {
        PixelFormat::Rgba => IffPixelFormat::Rgba,
        PixelFormat::Rgb24 => IffPixelFormat::Rgb24,
        PixelFormat::Pal8 => IffPixelFormat::Pal8,
        other => {
            return Err(oxideav_core::Error::unsupported(format!(
                "IFF: pixel format {other:?} not supported"
            )))
        }
    })
}

/// The 1:1 name mapping from [`IffPixelFormat`] to the framework enum.
pub fn to_core_pixel_format(pf: IffPixelFormat) -> PixelFormat {
    match pf {
        IffPixelFormat::Rgba => PixelFormat::Rgba,
        IffPixelFormat::Rgb24 => PixelFormat::Rgb24,
        IffPixelFormat::Pal8 => PixelFormat::Pal8,
    }
}

impl From<IffPixelFormat> for PixelFormat {
    fn from(pf: IffPixelFormat) -> Self {
        to_core_pixel_format(pf)
    }
}

impl TryFrom<PixelFormat> for IffPixelFormat {
    type Error = oxideav_core::Error;
    fn try_from(pf: PixelFormat) -> oxideav_core::Result<Self> {
        from_core_pixel_format(pf)
    }
}

/// [`ColorInfo`] as the framework's [`ColorSignal`] (code points map
/// 1:1; `Unspecified` range stays unspecified).
pub fn to_color_signal(c: &ColorInfo) -> ColorSignal {
    let range = match c.range {
        ColorRange::Unspecified => oxideav_core::ColorRange::Unspecified,
        ColorRange::Limited => oxideav_core::ColorRange::Limited,
        ColorRange::Full => oxideav_core::ColorRange::Full,
    };
    ColorSignal::new(
        range,
        ColorPrimaries(c.primaries),
        TransferCharacteristics(c.transfer),
        MatrixCoefficients(c.matrix),
    )
}

/// The inverse of [`to_color_signal`].
pub fn from_color_signal(s: &ColorSignal) -> ColorInfo {
    let range = match s.range {
        oxideav_core::ColorRange::Limited => ColorRange::Limited,
        oxideav_core::ColorRange::Full => ColorRange::Full,
        _ => ColorRange::Unspecified,
    };
    ColorInfo::new(range, s.primaries.0, s.transfer.0, s.matrix.0)
}

// ---- Metadata blobs ----

/// Blob kind of the `CAMG` viewmode longword (`IffImage::viewmode`),
/// big-endian `u32` payload.
pub const BLOB_KIND_CAMG: &str = "iff-camg";
/// Blob kind of the `BMHD` pixel-aspect pair (`IffImage::aspect`),
/// payload `[x_aspect, y_aspect]`.
pub const BLOB_KIND_ASPECT: &str = "iff-aspect";

/// The metadata blobs an [`IffImage`] travels with on the registry
/// path (core 0.1.40 `MetadataBlob`s, all of custom kinds — IFF defines
/// no ICC / Exif / XMP chunk): one blob per carried chunk
/// ([`IffImage::chunks`]) of kind [`IffChunk::blob_kind_name`]
/// (`iff-anno`, `iff-auth`, `iff-copyright`, `iff-name`, `iff-text`,
/// `iff-dpi`, `iff-grab`, `iff-dest`; payload = the chunk body), plus
/// [`BLOB_KIND_CAMG`] for `viewmode` and [`BLOB_KIND_ASPECT`] for
/// `aspect` when set. The `ilbm` decoder attaches them to every frame
/// and the demuxers declare them on the stream.
pub fn image_blobs(image: &IffImage) -> Vec<MetadataBlob> {
    let mut out: Vec<MetadataBlob> = image
        .chunks
        .iter()
        .map(|c| MetadataBlob::new(BlobKind::custom(c.blob_kind_name()), c.data.clone()))
        .collect();
    if let Some(v) = image.viewmode {
        out.push(MetadataBlob::new(
            BlobKind::custom(BLOB_KIND_CAMG),
            v.to_be_bytes().to_vec(),
        ));
    }
    if let Some((x, y)) = image.aspect {
        out.push(MetadataBlob::new(
            BlobKind::custom(BLOB_KIND_ASPECT),
            vec![x, y],
        ));
    }
    out
}

/// The inverse of [`image_blobs`]: apply `blobs` to `image` — carried
/// chunks replace [`IffImage::chunks`] (in blob order), [`BLOB_KIND_CAMG`]
/// sets `viewmode` (a 4-byte payload), [`BLOB_KIND_ASPECT`] sets
/// `aspect` (2 bytes). Blobs of any other kind are ignored: IFF has no
/// chunk for an ICC profile, Exif or XMP. A malformed payload is
/// ignored too.
pub fn apply_blobs(image: &mut IffImage, blobs: &[MetadataBlob]) {
    let mut chunks = Vec::new();
    for b in blobs {
        let name = b.kind.as_str();
        if name == BLOB_KIND_CAMG {
            if let Ok(raw) = <[u8; 4]>::try_from(b.data.as_slice()) {
                image.viewmode = Some(u32::from_be_bytes(raw));
            }
        } else if name == BLOB_KIND_ASPECT {
            if let [x, y] = b.data.as_slice() {
                image.aspect = Some((*x, *y));
            }
        } else if let Some(id) = IffChunk::id_for_blob_kind_name(name) {
            chunks.push(IffChunk::new(id, b.data.clone()));
        }
    }
    if !chunks.is_empty() {
        image.chunks = chunks;
    }
}

/// Stream blobs with the frame's blobs laid over them: a frame blob of
/// some kind replaces every stream blob of that kind (the framework's
/// precedence rule), so one picture's own `ANNO` wins over the
/// stream's.
pub fn merged_blobs(stream: &[MetadataBlob], frame: &[MetadataBlob]) -> Vec<MetadataBlob> {
    let mut out: Vec<MetadataBlob> = stream
        .iter()
        .filter(|s| !frame.iter().any(|f| f.kind == s.kind))
        .cloned()
        .collect();
    out.extend_from_slice(frame);
    out
}

// ---- Frame bridge ----

fn stamp_frame_side_channels(frame: &mut VideoFrame, image: &IffImage) {
    frame.set_blobs(image_blobs(image));
    if let (IffPixelFormat::Pal8, Some(p)) = (image.format, &image.palette) {
        frame.set_palette(p.to_rgb());
        // The palette-alpha record (core 0.1.40) carries a transparent
        // colour key: that entry 0, every other entry 255. An opaque
        // palette attaches no record, so such a picture reads as opaque.
        if p.has_alpha() {
            frame.set_palette_alpha(p.alpha_bytes());
        }
    }
    // IFF never signals colour, so the documented default is NOT stamped
    // on the frame; only a caller-supplied signal beyond it is.
    let c = image.color;
    if c != ColorInfo::iff_default() && c != ColorInfo::unspecified() {
        frame.set_color_signal(to_color_signal(&c));
    }
}

/// [`IffImage`] → `VideoFrame` with `pts` stamped: the single packed
/// plane, the palette side-channel for `Pal8` plus the palette-alpha
/// record when the palette is not fully opaque (a `BMHD` transparent
/// colour key arrives as that entry's alpha `0`, every other entry
/// `255`), the metadata-blobs record ([`image_blobs`]: carried chunks,
/// `CAMG`, aspect), and the colour-signal side-channel only when the
/// image signals more than the IFF default.
pub fn image_into_video_frame(mut image: IffImage, pts: Option<i64>) -> VideoFrame {
    let stride = image.stride();
    let data = if image.planes.is_empty() {
        Vec::new()
    } else {
        std::mem::take(&mut image.planes[0].data)
    };
    let mut frame = VideoFrame {
        pts,
        planes: vec![VideoPlane { stride, data }],
    };
    stamp_frame_side_channels(&mut frame, &image);
    frame
}

impl From<IffImage> for VideoFrame {
    /// The pixel plane (`pts` `None`) plus the side-channels; see
    /// [`image_into_video_frame`].
    fn from(image: IffImage) -> Self {
        image_into_video_frame(image, None)
    }
}

impl From<&IffImage> for VideoFrame {
    fn from(image: &IffImage) -> Self {
        image_into_video_frame(image.clone(), None)
    }
}

impl IffImage {
    /// Rebuild an image from a framework frame and the stream parameters
    /// that describe it (`width`, `height` required; `pixel_format`
    /// defaults to `Rgba`). For `Pal8` the palette comes from the frame's
    /// palette side-channel, its alpha from the palette-alpha record
    /// (opaque when absent); the frame's colour-signal side-channel,
    /// when attached, becomes `color`; the stream's `params.blobs` with
    /// the frame's blobs laid over them become `chunks` / `viewmode` /
    /// `aspect` ([`apply_blobs`]). The geometry is validated.
    pub fn from_video_frame(frame: &VideoFrame, params: &CodecParameters) -> crate::Result<Self> {
        let width = params
            .width
            .ok_or_else(|| IffError::invalid("IFF: missing width"))?;
        let height = params
            .height
            .ok_or_else(|| IffError::invalid("IFF: missing height"))?;
        let pix = from_core_pixel_format(params.pixel_format.unwrap_or(PixelFormat::Rgba))
            .map_err(|e| IffError::unsupported(e.to_string()))?;
        let plane = frame
            .image_planes()
            .first()
            .ok_or_else(|| IffError::invalid("IFF: frame has no planes"))?;
        let mut img = IffImage::new(
            width,
            height,
            pix,
            vec![Plane::new(plane.stride, plane.data.clone())],
        )?;
        if pix == IffPixelFormat::Pal8 {
            let rgb = frame.palette().ok_or_else(|| {
                IffError::invalid(
                    "IFF: Pal8 frame carries no palette side-channel \
                     (attach one via VideoFrame::set_palette)",
                )
            })?;
            if rgb.is_empty() || rgb.len() % 3 != 0 {
                return Err(IffError::invalid(format!(
                    "IFF: palette side-channel must be packed RGB triplets, got {} bytes",
                    rgb.len()
                )));
            }
            let mut palette = Palette::from_rgb(rgb);
            // Palette alpha (core 0.1.40): the record may be shorter than
            // the palette; uncovered entries stay opaque.
            if let Some(alpha) = frame.palette_alpha() {
                for (e, &a) in palette.entries.iter_mut().zip(alpha) {
                    e[3] = a;
                }
            }
            img.palette = Some(palette);
            img.validate()?;
        }
        if let Some(sig) = frame.color_signal() {
            img.color = from_color_signal(&sig);
        }
        apply_blobs(&mut img, &merged_blobs(&params.blobs, &frame.blobs()));
        Ok(img)
    }
}

impl TryFrom<(&VideoFrame, &CodecParameters)> for IffImage {
    type Error = IffError;
    fn try_from((frame, params): (&VideoFrame, &CodecParameters)) -> crate::Result<Self> {
        IffImage::from_video_frame(frame, params)
    }
}

// ---- Decoder trait impl + factory ----

/// Factory registered with the codec registry. Consumes one packet per
/// whole IFF raster `FORM` and produces one frame in the **native
/// layout**: `Pal8` with the file's palette on the frame's palette
/// side-channel for single-palette planar / chunky pictures, `Rgb24`
/// for HAM / per-line-palette / 24-bit / opaque `DEEP` pictures, `Rgba`
/// where the file carries per-pixel alpha — what [`crate::decode`]
/// returns. `params.pixel_format` is not consulted; a consumer that
/// wants packed RGBA converts downstream (or calls
/// [`crate::decode_rgba8`] on the packet bytes).
pub fn make_decoder(_params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(IffDecoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        pending: None,
        emitted: None,
        eof: false,
    }))
}

struct IffDecoder {
    codec_id: CodecId,
    pending: Option<VideoFrame>,
    /// The layout of the last decoded picture — what
    /// [`Decoder::output_pixel_format`] reports.
    emitted: Option<PixelFormat>,
    eof: bool,
}

impl Decoder for IffDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        let image = crate::decode(&packet.data)?;
        self.emitted = Some(to_core_pixel_format(image.format));
        self.pending = Some(image_into_video_frame(image, packet.pts));
        Ok(())
    }
    /// The layout of the last picture decoded (`None` before the first
    /// packet): the file decides it, not `params.pixel_format`.
    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.emitted
    }
    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        match self.pending.take() {
            Some(f) => Ok(Frame::Video(f)),
            None => {
                if self.eof {
                    Err(oxideav_core::Error::Eof)
                } else {
                    Err(oxideav_core::Error::NeedMore)
                }
            }
        }
    }
    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.eof = true;
        Ok(())
    }
}

// ---- Encoder trait impl + factory ----

/// The `ilbm` encoder's option schema (`CodecParameters::options`,
/// declared through `CodecInfo::encoder_options`). The codec is
/// lossless and declares no `quality`.
///
/// | key | kind | default | meaning |
/// |---|---|---|---|
/// | `form` | `ilbm` / `pbm` / `acbm` / `deep` / `rgb8` / `rgbn` | `ilbm` | the FORM written ([`EncodeOptions::form`]) |
/// | `compression` | `byterun1` / `none` / `auto` | `byterun1` | `BMHD.compression` (`auto` = the shorter of the two) |
/// | `masking` | `auto` / `none` / `mask` / `transparent` | `auto` | `BMHD.masking`; `auto` derives it from the palette alpha / pixel alpha (see [`crate::encode`]) |
/// | `indexed` | bool | `false` | quantise RGB input with more than 256 colours to a `CMAP` (first 256 distinct colours, nearest match after); without it such input is refused |
/// | `n_planes` | u32 | `0` | bitplane count for indexed output (`0` = the smallest that covers the palette) |
/// | `viewmode` | u32 | `0` | `CAMG` longword (`0` = none; HAM / EHB bits select those encoders for RGB input) |
/// | `drop_alpha` | bool | `false` | let a non-opaque `Rgba` frame lose its alpha where the target has no alpha mechanism (24-bit `ILBM`, `RGB8` / `RGBN`, `DEEP` with `deep_rgb_only`) |
/// | `deep_rgb_only` | bool | `false` | `DEEP`: write an RGB 8:8:8 `DPEL` even for `Rgba` input |
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct IlbmEncoderOptions {
    /// The FORM written.
    pub form: IffForm,
    /// `BMHD.compression`.
    pub compression: Compression,
    /// `BMHD.masking`; `None` derives it.
    pub masking: Option<Masking>,
    /// Quantise RGB input with more than 256 colours.
    pub indexed: bool,
    /// Bitplane count for indexed output; `None` derives it.
    pub n_planes: Option<u8>,
    /// `CAMG` longword; `None` writes none.
    pub viewmode: Option<u32>,
    /// Allow a non-opaque `Rgba` frame to lose its alpha.
    pub drop_alpha: bool,
    /// `DEEP`: RGB-only `DPEL`.
    pub deep_rgb_only: bool,
}

impl Default for IlbmEncoderOptions {
    fn default() -> Self {
        Self {
            form: IffForm::Ilbm,
            compression: Compression::ByteRun1,
            masking: None,
            indexed: false,
            n_planes: None,
            viewmode: None,
            drop_alpha: false,
            deep_rgb_only: false,
        }
    }
}

impl IlbmEncoderOptions {
    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// The standalone [`EncodeOptions`] these settings map onto.
    pub fn to_encode_options(&self) -> EncodeOptions {
        EncodeOptions::default()
            .with_form(self.form)
            .with_compression(self.compression)
            .with_masking(self.masking)
            .with_indexed(self.indexed)
            .with_n_planes(self.n_planes)
            .with_viewmode(self.viewmode)
            .with_drop_alpha(self.drop_alpha)
            .with_deep_rgb_only(self.deep_rgb_only)
    }
}

impl CodecOptionsStruct for IlbmEncoderOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "form",
            kind: OptionKind::Enum(&["ilbm", "pbm", "acbm", "deep", "rgb8", "rgbn"]),
            default: OptionValue::String(String::new()),
            help: "FORM written: ilbm (default), pbm, acbm, deep, rgb8, rgbn",
        },
        OptionField {
            name: "compression",
            kind: OptionKind::Enum(&["byterun1", "none", "auto"]),
            default: OptionValue::String(String::new()),
            help: "BMHD.compression: byterun1 (default), none, auto (the shorter)",
        },
        OptionField {
            name: "masking",
            kind: OptionKind::Enum(&["auto", "none", "mask", "transparent"]),
            default: OptionValue::String(String::new()),
            help: "BMHD.masking: auto (from the alpha, default), none, mask (plane), transparent (colour key)",
        },
        OptionField {
            name: "indexed",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Quantise RGB input with more than 256 colours to a CMAP (lossy; refused without it)",
        },
        OptionField {
            name: "n_planes",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "Bitplanes for indexed output, 1..=8 (0 = smallest that covers the palette)",
        },
        OptionField {
            name: "viewmode",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "CAMG longword (0 = none; HAM / EHB bits select those encoders for RGB input)",
        },
        OptionField {
            name: "drop_alpha",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Let a non-opaque Rgba frame lose its alpha where the target carries none",
        },
        OptionField {
            name: "deep_rgb_only",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "DEEP: write an RGB 8:8:8 DPEL even for Rgba input",
        },
    ];

    fn apply(&mut self, key: &str, value: &OptionValue) -> oxideav_core::Result<()> {
        match key {
            "form" => {
                self.form = match value.as_str()? {
                    "ilbm" => IffForm::Ilbm,
                    "pbm" => IffForm::Pbm,
                    "acbm" => IffForm::Acbm,
                    "deep" => IffForm::Deep,
                    "rgb8" => IffForm::Rgb8,
                    "rgbn" => IffForm::Rgbn,
                    other => {
                        return Err(oxideav_core::Error::invalid(format!(
                            "ilbm encoder: unknown form {other:?}"
                        )))
                    }
                }
            }
            "compression" => {
                self.compression = match value.as_str()? {
                    "byterun1" => Compression::ByteRun1,
                    "none" => Compression::None,
                    "auto" => Compression::Auto,
                    other => {
                        return Err(oxideav_core::Error::invalid(format!(
                            "ilbm encoder: unknown compression {other:?}"
                        )))
                    }
                }
            }
            "masking" => {
                self.masking = match value.as_str()? {
                    "auto" => None,
                    "none" => Some(Masking::None),
                    "mask" => Some(Masking::HasMask),
                    "transparent" => Some(Masking::HasTransparentColor),
                    other => {
                        return Err(oxideav_core::Error::invalid(format!(
                            "ilbm encoder: unknown masking {other:?}"
                        )))
                    }
                }
            }
            "indexed" => self.indexed = value.as_bool()?,
            "n_planes" => {
                let n = value.as_u32()?;
                self.n_planes = match n {
                    0 => None,
                    1..=8 => Some(n as u8),
                    _ => {
                        return Err(oxideav_core::Error::invalid(format!(
                            "ilbm encoder: n_planes {n} outside 1..=8"
                        )))
                    }
                };
            }
            "viewmode" => {
                let v = value.as_u32()?;
                self.viewmode = (v != 0).then_some(v);
            }
            "drop_alpha" => self.drop_alpha = value.as_bool()?,
            "deep_rgb_only" => self.deep_rgb_only = value.as_bool()?,
            _ => unreachable!("guarded by SCHEMA"),
        }
        Ok(())
    }
}

/// The input layouts the `ilbm` encoder accepts
/// (`CodecCapabilities::accepted_pixel_formats`), in the gateway's
/// ladder order — colour before grey. Anything else is a typed
/// `Unsupported` refusal; see [`make_encoder`] for what each becomes.
pub const ENCODER_PIXEL_FORMATS: [PixelFormat; 6] = [
    PixelFormat::Pal8,
    PixelFormat::Rgb24,
    PixelFormat::Rgba,
    PixelFormat::Bgr24,
    PixelFormat::Bgra,
    PixelFormat::Gray8,
];

/// The layout a file written from `input` with `opts` decodes to —
/// the encoder's output label before the first frame (refined to the
/// actual file's layout after it, see [`Encoder::output_params`]).
fn output_label(input: PixelFormat, opts: &IlbmEncoderOptions) -> Option<PixelFormat> {
    Some(match (opts.form, input) {
        (IffForm::Deep, PixelFormat::Rgba | PixelFormat::Bgra) if !opts.deep_rgb_only => {
            PixelFormat::Rgba
        }
        (IffForm::Deep | IffForm::Rgb8 | IffForm::Rgbn, _) => PixelFormat::Rgb24,
        (_, PixelFormat::Pal8 | PixelFormat::Gray8) => match opts.masking {
            Some(Masking::HasMask) => PixelFormat::Rgba,
            _ => PixelFormat::Pal8,
        },
        (_, PixelFormat::Rgba | PixelFormat::Bgra) if !opts.drop_alpha => PixelFormat::Rgba,
        (_, PixelFormat::Rgba | PixelFormat::Bgra | PixelFormat::Rgb24 | PixelFormat::Bgr24) => {
            if opts.indexed || opts.viewmode.is_some_and(|v| Camg { raw: v }.is_ehb()) {
                PixelFormat::Pal8
            } else {
                PixelFormat::Rgb24
            }
        }
        _ => return None,
    })
}

/// Factory registered with the codec registry: one frame in, one
/// complete raster `FORM` out through [`crate::encode`] with the
/// [`IlbmEncoderOptions`] parsed from `params.options` (default: a
/// `FORM ILBM`, ByteRun1, masking derived). The accepted input layouts
/// ([`ENCODER_PIXEL_FORMATS`]) and what each becomes:
///
/// * `Pal8` (palette on the frame's side-channel, alpha on the
///   palette-alpha record) — index-for-index bitplanes; the alpha
///   record maps to a masking mode by the palette rule
///   (one transparent entry → transparent colour, several → mask
///   plane). Lossless.
/// * `Gray8` — a `Pal8` picture over the 256-entry grey ramp
///   (8 bitplanes). Lossless.
/// * `Rgb24` / `Bgr24` — the 24-bit literal-RGB `ILBM` (`Bgr24`
///   byte-swapped). Lossless.
/// * `Rgba` / `Bgra` — opaque: as `Rgb24`. Non-opaque: the format's
///   alpha mechanism, a mask plane (alpha below 128 masked out) over an
///   exact `CMAP` when the picture has at most 256 distinct colours;
///   more colours are refused (`Unsupported`) unless `indexed`
///   (quantise, lossy) or `drop_alpha` (24-bit, alpha discarded) is
///   set, or `form` is `deep` (RGBA `DPEL`), `rgb8` / `rgbn` (genlock).
/// * Anything else — `Unsupported`; the caller converts upstream.
///
/// `output_params().pixel_format` is the layout the written file
/// decodes to (`Pal8` + palette `extradata`, `Rgb24`, `Rgba`), refined
/// after the first frame from the file itself.
///
/// Metadata: the stream's `params.blobs` with the frame's blobs laid
/// over them ([`merged_blobs`]) are written back — carried chunks
/// (`iff-anno` … `iff-dest`) ahead of the body, `iff-camg` as `CAMG`
/// (the `viewmode` option outranks it), `iff-aspect` into `BMHD`;
/// blobs of other kinds (ICC / Exif / XMP) have no IFF chunk and are
/// not written ([`apply_blobs`]).
pub fn make_encoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Encoder>> {
    let opts: IlbmEncoderOptions = parse_options(&params.options)?;
    if let Some(pf) = params.pixel_format {
        if !ENCODER_PIXEL_FORMATS.contains(&pf) {
            return Err(oxideav_core::Error::unsupported(format!(
                "IFF encoder: {pf:?} is not an accepted input layout \
                 (Pal8, Rgb24, Rgba, Bgr24, Bgra, Gray8); convert upstream"
            )));
        }
    }
    let mut out_params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    out_params.width = params.width;
    out_params.height = params.height;
    out_params.pixel_format = params
        .pixel_format
        .and_then(|pf| output_label(pf, &opts))
        .or(params.pixel_format);
    Ok(Box::new(IffEncoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        input_format: params.pixel_format,
        stream_blobs: params.blobs.clone(),
        out_params,
        opts,
        pending: None,
        eof: false,
    }))
}

struct IffEncoder {
    codec_id: CodecId,
    input_format: Option<PixelFormat>,
    /// The stream's blobs (`params.blobs` at construction), the base a
    /// frame's own blobs are laid over.
    stream_blobs: Vec<MetadataBlob>,
    out_params: CodecParameters,
    opts: IlbmEncoderOptions,
    pending: Option<Vec<u8>>,
    eof: bool,
}

/// The number of distinct RGB triples in packed RGBA, stopping at
/// `limit + 1`.
fn distinct_colours(rgba: &[u8], limit: usize) -> usize {
    let mut seen: Vec<[u8; 3]> = Vec::new();
    for px in rgba.chunks_exact(4) {
        let t = [px[0], px[1], px[2]];
        if !seen.contains(&t) {
            seen.push(t);
            if seen.len() > limit {
                break;
            }
        }
    }
    seen.len()
}

/// Bring a framework frame into an [`IffImage`] in the layout the
/// encoder codes it as, and the [`EncodeOptions`] for it: the 1:1
/// layouts go through [`IffImage::from_video_frame`]; `Bgr24` / `Bgra`
/// are byte-swapped; `Gray8` becomes `Pal8` over the grey ramp; an
/// opaque `Rgba` drops to `Rgb24`; a non-opaque `Rgba` into a planar
/// form takes the mask-plane path when its colours fit a `CMAP`.
fn frame_to_image(
    vf: &VideoFrame,
    params: &CodecParameters,
    opts: &IlbmEncoderOptions,
) -> oxideav_core::Result<(IffImage, EncodeOptions)> {
    let format = params.pixel_format.ok_or_else(|| {
        oxideav_core::Error::invalid("IFF encoder: pixel_format missing in CodecParameters")
    })?;
    let width = params.width.ok_or_else(|| {
        oxideav_core::Error::invalid("IFF encoder: width missing in CodecParameters")
    })?;
    let height = params.height.ok_or_else(|| {
        oxideav_core::Error::invalid("IFF encoder: height missing in CodecParameters")
    })?;
    let mut enc = opts.to_encode_options();
    let image =
        match format {
            PixelFormat::Pal8 | PixelFormat::Rgb24 | PixelFormat::Rgba => {
                IffImage::from_video_frame(vf, params)?
            }
            PixelFormat::Bgr24 | PixelFormat::Bgra => {
                let plane = vf.image_planes().first().ok_or_else(|| {
                    oxideav_core::Error::invalid("IFF encoder: empty frame plane")
                })?;
                let bpp = if format == PixelFormat::Bgr24 { 3 } else { 4 };
                let tight = tighten_packed(plane, width as usize, height as usize, bpp)?;
                if bpp == 3 {
                    let rgb: Vec<u8> = tight
                        .chunks_exact(3)
                        .flat_map(|c| [c[2], c[1], c[0]])
                        .collect();
                    IffImage::from_rgb8(width, height, rgb)?
                } else {
                    let rgba: Vec<u8> = tight
                        .chunks_exact(4)
                        .flat_map(|c| [c[2], c[1], c[0], c[3]])
                        .collect();
                    IffImage::from_rgba8(width, height, rgba)?
                }
            }
            PixelFormat::Gray8 => {
                let plane = vf.image_planes().first().ok_or_else(|| {
                    oxideav_core::Error::invalid("IFF encoder: empty frame plane")
                })?;
                let tight = tighten_packed(plane, width as usize, height as usize, 1)?;
                let ramp: Vec<[u8; 3]> = (0..=255u8).map(|v| [v, v, v]).collect();
                IffImage::new_indexed(width, height, tight, Palette::from_rgb_triples(&ramp))?
            }
            other => {
                return Err(oxideav_core::Error::unsupported(format!(
                    "IFF encoder: {other:?} is not an accepted input layout \
                 (Pal8, Rgb24, Rgba, Bgr24, Bgra, Gray8); convert upstream"
                )))
            }
        };
    let mut image = image;
    apply_blobs(&mut image, &merged_blobs(&params.blobs, &vf.blobs()));
    // An explicit `viewmode` option outranks the frame's `CAMG` blob.
    if opts.viewmode.is_some() {
        image.viewmode = None;
    }
    let image = if image.format == IffPixelFormat::Rgba {
        let opaque = image.data().iter().skip(3).step_by(4).all(|&a| a == 255);
        if opaque {
            // Alpha carries nothing: the 24-bit (or RGB DPEL / genlock)
            // form is exact.
            let mut rgb = IffImage::from_rgb8(width, height, image.to_rgb8())?;
            rgb.chunks = image.chunks;
            rgb.viewmode = image.viewmode;
            rgb.aspect = image.aspect;
            rgb
        } else {
            let planar = matches!(enc.form, Some(IffForm::Ilbm | IffForm::Pbm | IffForm::Acbm));
            let ham_or_ehb = enc
                .viewmode
                .is_some_and(|v| Camg { raw: v }.is_ham() || Camg { raw: v }.is_ehb());
            if planar && !enc.indexed && !enc.drop_alpha && !ham_or_ehb {
                // The format's alpha mechanism is a mask plane over a
                // CMAP: exact when the colours fit one, refused (not
                // quantised silently) otherwise.
                if distinct_colours(image.data(), 256) <= 256 {
                    enc.indexed = true;
                } else {
                    return Err(oxideav_core::Error::unsupported(
                        "IFF encoder: a non-opaque Rgba picture with more than 256 colours has \
                         no exact ILBM form (the mask plane rides a CMAP); set indexed=true to \
                         quantise, drop_alpha=true to write 24-bit without alpha, or form=deep",
                    ));
                }
            }
            image
        }
    } else {
        image
    };
    Ok((image, enc))
}

impl Encoder for IffEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }
    fn send_frame(&mut self, frame: &Frame) -> oxideav_core::Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            _ => {
                return Err(oxideav_core::Error::invalid(
                    "IFF encoder: expected video frame",
                ))
            }
        };
        let mut in_params = self.out_params.clone();
        in_params.pixel_format = self.input_format;
        in_params.blobs.clone_from(&self.stream_blobs);
        let (image, enc) = frame_to_image(vf, &in_params, &self.opts)?;
        let bytes = crate::encode(&image, &enc)?;
        // Truthful label: what the written FORM decodes to.
        if let Ok(info) = crate::info(&bytes) {
            self.out_params.pixel_format = Some(to_core_pixel_format(info.format));
            self.out_params.extradata = if info.format == IffPixelFormat::Pal8 {
                crate::decode(&bytes)
                    .ok()
                    .and_then(|i| i.palette.map(|p| p.to_rgb()))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
        }
        self.pending = Some(bytes);
        Ok(())
    }
    fn receive_packet(&mut self) -> oxideav_core::Result<Packet> {
        match self.pending.take() {
            Some(bytes) => {
                let mut pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
                pkt.flags.keyframe = true;
                Ok(pkt)
            }
            None => {
                if self.eof {
                    Err(oxideav_core::Error::Eof)
                } else {
                    Err(oxideav_core::Error::NeedMore)
                }
            }
        }
    }
    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.eof = true;
        Ok(())
    }
}

fn tighten_packed(
    plane: &VideoPlane,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
) -> oxideav_core::Result<Vec<u8>> {
    let want = width * bytes_per_pixel;
    if plane.stride < want {
        return Err(oxideav_core::Error::invalid(format!(
            "IFF encoder: plane stride {} smaller than width × bytes-per-pixel {}",
            plane.stride, want
        )));
    }
    if plane.data.len() < plane.stride * height {
        return Err(oxideav_core::Error::invalid(
            "IFF encoder: plane data shorter than stride × height",
        ));
    }
    let mut tight = Vec::with_capacity(want * height);
    for y in 0..height {
        let off = y * plane.stride;
        tight.extend_from_slice(&plane.data[off..off + want]);
    }
    Ok(tight)
}

// ---- Registration ----

/// Register the `ilbm` image codec into the supplied [`CodecRegistry`].
pub fn register_codecs(reg: &mut CodecRegistry) {
    // Lossless, intra-only; the accepted input layouts are the contract
    // the gateway's ladder reads (anything else is refused by type);
    // no `quality` is declared.
    let caps = CodecCapabilities::video("ilbm_sw")
        .with_intra_only(true)
        .with_lossless(true)
        .with_max_size(65535, 65535)
        .with_pixel_formats(ENCODER_PIXEL_FORMATS.to_vec());
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .encoder_options::<IlbmEncoderOptions>(),
    );
}

/// Register every IFF-family container demuxer / muxer / extension /
/// probe (`iff_8svx`, `iff_ilbm`, `iff_acbm`, `iff_rgb8`, `iff_rgbn`,
/// `iff_deep`, `iff_tvpp`, `iff_anim`, `aiff`) into the supplied
/// [`ContainerRegistry`].
pub fn register_containers(reg: &mut ContainerRegistry) {
    crate::svx::register(reg);
    crate::ilbm::register(reg);
    crate::anim::register(reg);
    // aiff's sibling-form helper is `register_containers` (the public
    // `aiff::register` takes a full `RuntimeContext` because it was the
    // standalone-crate entry point; here we want only the container half).
    crate::aiff::demuxer::register_containers(reg);
}

/// Combined registration for callers holding the two sub-registries
/// rather than a [`RuntimeContext`].
pub fn register_registries(codecs: &mut CodecRegistry, containers: &mut ContainerRegistry) {
    register_codecs(codecs);
    register_containers(containers);
}

/// Unified registration entry point — installs the `ilbm` image codec
/// into the codec sub-registry and every IFF-family container into the
/// container sub-registry of the supplied [`RuntimeContext`]. This is
/// the form `oxideav_meta::register_all` dispatches via the
/// [`oxideav_core::register!`] macro.
pub fn register(ctx: &mut RuntimeContext) {
    register_registries(&mut ctx.codecs, &mut ctx.containers);
}

oxideav_core::register!("iff", register);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::IffPixelFormat as Pf;

    #[test]
    fn register_via_runtime_context_installs_everything() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(
            ctx.containers.container_for_extension("8svx"),
            Some("iff_8svx")
        );
        assert_eq!(
            ctx.containers.container_for_extension("ilbm"),
            Some("iff_ilbm")
        );
        assert_eq!(ctx.containers.container_for_extension("aiff"), Some("aiff"));
        assert_eq!(ctx.containers.container_for_extension("aifc"), Some("aiff"));
        assert!(ctx.codecs.has_decoder(&CodecId::new(CODEC_ID_STR)));
        assert!(ctx.codecs.has_encoder(&CodecId::new(CODEC_ID_STR)));
    }

    #[test]
    fn pixel_formats_map_by_name() {
        for (ours, theirs) in [
            (Pf::Pal8, PixelFormat::Pal8),
            (Pf::Rgb24, PixelFormat::Rgb24),
            (Pf::Rgba, PixelFormat::Rgba),
        ] {
            assert_eq!(PixelFormat::from(ours), theirs);
            assert_eq!(Pf::try_from(theirs).unwrap(), ours);
        }
        assert!(Pf::try_from(PixelFormat::Yuv420P).is_err());
    }

    #[test]
    fn frame_bridge_round_trips_pal8_with_palette() {
        let pal = Palette::from_rgb_triples(&[[1, 2, 3], [4, 5, 6]]);
        let img = IffImage::new_indexed(2, 2, vec![0, 1, 1, 0], pal).unwrap();
        let frame: VideoFrame = img.clone().into();
        assert_eq!(frame.palette(), Some(&[1u8, 2, 3, 4, 5, 6][..]));
        assert!(frame.color_signal().is_none(), "IFF default is not stamped");
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.width = Some(2);
        params.height = Some(2);
        params.pixel_format = Some(PixelFormat::Pal8);
        let back = IffImage::try_from((&frame, &params)).unwrap();
        assert_eq!(back.planes, img.planes);
        assert_eq!(back.palette, img.palette);
        assert_eq!(back.format, Pf::Pal8);
    }

    #[test]
    fn codec_round_trip_through_registry_factories() {
        let pal = Palette::from_rgb_triples(&[[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]]);
        let img = IffImage::new_indexed(3, 2, vec![0, 1, 2, 3, 2, 1], pal).unwrap();
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.width = Some(3);
        params.height = Some(2);
        params.pixel_format = Some(PixelFormat::Pal8);
        let mut enc = make_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(img.clone().into())).unwrap();
        let pkt = enc.receive_packet().unwrap();
        assert!(crate::probe(&pkt.data));

        let mut dec = make_decoder(&params).unwrap();
        dec.send_packet(&pkt).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video frame expected");
        };
        let back = IffImage::from_video_frame(&vf, &params).unwrap();
        assert_eq!(back.planes, img.planes);
        assert_eq!(back.palette, img.palette);
        dec.flush().unwrap();
        assert!(matches!(dec.receive_frame(), Err(oxideav_core::Error::Eof)));
    }

    fn pal8_params(w: u32, h: u32) -> CodecParameters {
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(PixelFormat::Pal8);
        params
    }

    fn encode_frame(params: &CodecParameters, frame: VideoFrame) -> Packet {
        let mut enc = make_encoder(params).unwrap();
        enc.send_frame(&Frame::Video(frame)).unwrap();
        enc.receive_packet().unwrap()
    }

    fn decode_packet(params: &CodecParameters, pkt: &Packet) -> VideoFrame {
        let mut dec = make_decoder(params).unwrap();
        dec.send_packet(pkt).unwrap();
        let Frame::Video(vf) = dec.receive_frame().unwrap() else {
            panic!("video frame expected");
        };
        vf
    }

    #[test]
    fn frame_bridge_carries_the_palette_alpha_record() {
        // A transparent colour key: that entry 0, every other 255.
        let pal = Palette::new(vec![[1, 2, 3, 255], [4, 5, 6, 0], [7, 8, 9, 255]]);
        let img = IffImage::new_indexed(3, 1, vec![0, 1, 2], pal.clone()).unwrap();
        let frame: VideoFrame = img.clone().into();
        assert_eq!(frame.palette(), Some(&[1u8, 2, 3, 4, 5, 6, 7, 8, 9][..]));
        assert_eq!(frame.palette_alpha(), Some(&[255u8, 0, 255][..]));
        assert_eq!(frame.palette_rgba(1), Some([4, 5, 6, 0]));
        let back = IffImage::from_video_frame(&frame, &pal8_params(3, 1)).unwrap();
        assert_eq!(back.palette, Some(pal));
        assert!(back.has_alpha());

        // An opaque palette attaches no record.
        let opaque = IffImage::new_indexed(1, 1, vec![0], Palette::from_rgb(&[9, 9, 9])).unwrap();
        let frame: VideoFrame = opaque.into();
        assert!(frame.palette_alpha().is_none());
        assert_eq!(frame.palette_rgba(0), Some([9, 9, 9, 255]));
    }

    #[test]
    fn a_shorter_alpha_record_leaves_uncovered_entries_opaque() {
        let mut frame = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 2,
                data: vec![0, 1],
            }],
        };
        frame.set_palette(vec![1, 1, 1, 2, 2, 2]);
        frame.set_palette_alpha(vec![0]);
        let back = IffImage::from_video_frame(&frame, &pal8_params(2, 1)).unwrap();
        assert_eq!(
            back.palette.unwrap().entries,
            vec![[1, 1, 1, 0], [2, 2, 2, 255]]
        );
    }

    #[test]
    fn transparent_key_survives_the_codec_round_trip() {
        // One transparent entry → BMHD masking 2 (transparent colour),
        // the picture stays Pal8 with the alpha record on the way back.
        let pal = Palette::new(vec![[0, 0, 0, 255], [255, 0, 0, 0], [0, 255, 0, 255]]);
        let img = IffImage::new_indexed(3, 2, vec![0, 1, 2, 2, 1, 0], pal.clone()).unwrap();
        let params = pal8_params(3, 2);
        let pkt = encode_frame(&params, img.clone().into());
        let info = crate::info(&pkt.data).unwrap();
        assert_eq!(info.masking, 2);
        assert_eq!(info.format, Pf::Pal8);
        assert!(info.has_alpha);
        let vf = decode_packet(&params, &pkt);
        assert_eq!(vf.palette_alpha(), Some(&[255u8, 0, 255][..]));
        let back = IffImage::from_video_frame(&vf, &params).unwrap();
        assert_eq!(back.planes, img.planes);
        assert_eq!(back.palette, Some(pal));
    }

    #[test]
    fn several_transparent_entries_write_a_mask_plane() {
        // Two transparent entries cannot share one colour key: the
        // encoder writes a mask plane (masking 1); the file decodes as
        // Rgba with those pixels at alpha 0 and the rest opaque.
        let pal = Palette::new(vec![[0, 0, 0, 255], [255, 0, 0, 0], [0, 255, 0, 100]]);
        let img = IffImage::new_indexed(3, 1, vec![0, 1, 2], pal).unwrap();
        let params = pal8_params(3, 1);
        let pkt = encode_frame(&params, img.into());
        let info = crate::info(&pkt.data).unwrap();
        assert_eq!(info.masking, 1);
        assert_eq!(info.format, Pf::Rgba);
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!(back.format, Pf::Rgba);
        assert_eq!(
            back.to_rgba8(),
            vec![0, 0, 0, 255, 255, 0, 0, 0, 0, 255, 0, 0]
        );
    }

    fn bgra_frame() -> VideoFrame {
        VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 8,
                data: vec![1, 2, 3, 0, 4, 5, 6, 128],
            }],
        }
    }

    fn video_params(pf: PixelFormat, w: u32, h: u32) -> CodecParameters {
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.width = Some(w);
        params.height = Some(h);
        params.pixel_format = Some(pf);
        params
    }

    #[test]
    fn non_opaque_bgra_takes_the_mask_plane_and_swaps_bgr() {
        // Two colours, one transparent pixel: the format's alpha
        // mechanism is a mask plane over an exact CMAP; the label says
        // what the file decodes to.
        let params = video_params(PixelFormat::Bgra, 2, 1);
        let mut enc = make_encoder(&params).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Rgba));
        enc.send_frame(&Frame::Video(bgra_frame())).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Rgba));
        let pkt = enc.receive_packet().unwrap();
        let info = crate::info(&pkt.data).unwrap();
        assert_eq!((info.masking, info.n_planes), (1, 1));
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!(back.format, Pf::Rgba);
        assert_eq!(back.to_rgba8(), vec![3, 2, 1, 0, 6, 5, 4, 255]);
    }

    #[test]
    fn drop_alpha_option_writes_24_bit_from_bgra() {
        let mut params = video_params(PixelFormat::Bgra, 2, 1);
        params.options.insert("drop_alpha", "true");
        let mut enc = make_encoder(&params).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Rgb24));
        enc.send_frame(&Frame::Video(bgra_frame())).unwrap();
        let pkt = enc.receive_packet().unwrap();
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!(back.format, Pf::Rgb24);
        assert_eq!(back.n_planes, 24);
        assert_eq!(back.to_rgb8(), vec![3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn opaque_rgba_writes_the_exact_24_bit_form() {
        let params = video_params(PixelFormat::Rgba, 2, 1);
        let frame = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 8,
                data: vec![1, 2, 3, 255, 4, 5, 6, 255],
            }],
        };
        let mut enc = make_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(frame)).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Rgb24));
        let pkt = enc.receive_packet().unwrap();
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!((back.format, back.n_planes), (Pf::Rgb24, 24));
        assert_eq!(back.to_rgb8(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn rgba_with_too_many_colours_is_refused_not_quantised() {
        // 300 distinct colours and one transparent pixel: no exact ILBM.
        let w = 300u32;
        let mut data = Vec::with_capacity(w as usize * 4);
        for i in 0..w {
            data.extend_from_slice(&[(i & 0xFF) as u8, (i >> 8) as u8, 7, 255]);
        }
        data[3] = 0;
        let frame = || VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: w as usize * 4,
                data: data.clone(),
            }],
        };
        let params = video_params(PixelFormat::Rgba, w, 1);
        let mut enc = make_encoder(&params).unwrap();
        assert!(matches!(
            enc.send_frame(&Frame::Video(frame())),
            Err(oxideav_core::Error::Unsupported(_))
        ));
        // `indexed` quantises (explicit, lossy) and keeps the mask plane.
        let mut p = params.clone();
        p.options.insert("indexed", "true");
        let mut enc = make_encoder(&p).unwrap();
        enc.send_frame(&Frame::Video(frame())).unwrap();
        let pkt = enc.receive_packet().unwrap();
        let info = crate::info(&pkt.data).unwrap();
        assert_eq!((info.masking, info.format, info.n_planes), (1, Pf::Rgba, 8));
        assert_eq!(crate::decode(&pkt.data).unwrap().to_rgba8()[3], 0);
        // `form=deep` carries RGBA natively and exactly.
        let mut p = params.clone();
        p.options.insert("form", "deep");
        let mut enc = make_encoder(&p).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Rgba));
        enc.send_frame(&Frame::Video(frame())).unwrap();
        let pkt = enc.receive_packet().unwrap();
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!(back.form, IffForm::Deep);
        assert_eq!(back.to_rgba8(), data);
    }

    #[test]
    fn gray8_writes_an_exact_grey_palette_picture() {
        let params = video_params(PixelFormat::Gray8, 3, 2);
        let frame = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 4,
                data: vec![0, 128, 255, 9, 7, 200, 1, 9],
            }],
        };
        let mut enc = make_encoder(&params).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Pal8));
        enc.send_frame(&Frame::Video(frame)).unwrap();
        let out = enc.output_params();
        assert_eq!(out.pixel_format, Some(PixelFormat::Pal8));
        assert_eq!(out.extradata.len(), 256 * 3);
        assert_eq!(&out.extradata[3 * 200..3 * 201], &[200, 200, 200]);
        let pkt = enc.receive_packet().unwrap();
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!((back.format, back.n_planes), (Pf::Pal8, 8));
        assert_eq!(back.data(), &[0, 128, 255, 7, 200, 1]);
        assert_eq!(
            back.to_rgb8(),
            vec![0, 0, 0, 128, 128, 128, 255, 255, 255, 7, 7, 7, 200, 200, 200, 1, 1, 1]
        );
    }

    #[test]
    fn undeclared_layouts_are_refused_by_type_at_construction() {
        for pf in [
            PixelFormat::Yuv420P,
            PixelFormat::Rgb48Le,
            PixelFormat::Gray16Le,
        ] {
            let params = video_params(pf, 2, 2);
            assert!(
                matches!(
                    make_encoder(&params),
                    Err(oxideav_core::Error::Unsupported(_))
                ),
                "{pf:?} must be refused"
            );
        }
    }

    #[test]
    fn encoder_declares_its_layouts_and_schema_without_quality() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new(CODEC_ID_STR);
        let caps = &ctx.codecs.implementations(&id)[0].caps;
        assert_eq!(caps.accepted_pixel_formats, ENCODER_PIXEL_FORMATS.to_vec());
        assert!(caps.lossless && !caps.lossy);
        let schema = ctx
            .codecs
            .encoder_options_schema(&id)
            .expect("schema declared");
        let names: Vec<&str> = schema.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "form",
                "compression",
                "masking",
                "indexed",
                "n_planes",
                "viewmode",
                "drop_alpha",
                "deep_rgb_only"
            ]
        );
        assert!(!names.contains(&"quality"));
    }

    #[test]
    fn options_select_form_compression_planes_and_masking() {
        let pal = Palette::new(vec![[0, 0, 0, 255], [9, 9, 9, 0]]);
        let img = IffImage::new_indexed(2, 1, vec![0, 1], pal).unwrap();
        let base = pal8_params(2, 1);

        let mut p = base.clone();
        p.options.insert("form", "pbm");
        p.options.insert("compression", "none");
        let pkt = encode_frame(&p, img.clone().into());
        let info = crate::info(&pkt.data).unwrap();
        assert_eq!(
            (info.form, info.compression, info.masking),
            (IffForm::Pbm, 0, 2)
        );

        let mut p = base.clone();
        p.options.insert("form", "acbm");
        p.options.insert("n_planes", "5");
        p.options.insert("masking", "mask");
        let mut enc = make_encoder(&p).unwrap();
        assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Rgba));
        enc.send_frame(&Frame::Video(img.clone().into())).unwrap();
        let pkt = enc.receive_packet().unwrap();
        let info = crate::info(&pkt.data).unwrap();
        assert_eq!(
            (info.form, info.n_planes, info.masking, info.format),
            (IffForm::Acbm, 5, 1, Pf::Rgba)
        );

        let mut p = base.clone();
        p.options.insert("masking", "none");
        let pkt = encode_frame(&p, img.clone().into());
        assert_eq!(crate::info(&pkt.data).unwrap().masking, 0);

        for (k, v) in [("n_planes", "9"), ("form", "jpeg"), ("quality", "90")] {
            let mut p = base.clone();
            p.options.insert(k, v);
            assert!(
                matches!(make_encoder(&p), Err(oxideav_core::Error::InvalidData(_))),
                "{k}={v} must be rejected"
            );
        }
    }
}
