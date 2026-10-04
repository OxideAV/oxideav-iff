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
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, ColorPrimaries,
    ColorSignal, ContainerRegistry, Decoder, Encoder, Frame, MatrixCoefficients, Packet,
    PixelFormat, RuntimeContext, TimeBase, TransferCharacteristics, VideoFrame, VideoPlane,
};

use crate::error::IffError;
use crate::image::{ColorInfo, ColorRange, IffImage, IffPixelFormat, Palette, Plane};
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

// ---- Frame bridge ----

fn stamp_frame_side_channels(frame: &mut VideoFrame, image: &IffImage) {
    if let (IffPixelFormat::Pal8, Some(p)) = (image.format, &image.palette) {
        frame.set_palette(p.to_rgb());
    }
    // IFF never signals colour, so the documented default is NOT stamped
    // on the frame; only a caller-supplied signal beyond it is.
    let c = image.color;
    if c != ColorInfo::iff_default() && c != ColorInfo::unspecified() {
        frame.set_color_signal(to_color_signal(&c));
    }
}

/// [`IffImage`] → `VideoFrame` with `pts` stamped: the single packed
/// plane, the palette side-channel for `Pal8` (RGB only — the
/// framework's palette record has no alpha; a transparent-colour key is
/// therefore lost on this path), and the colour-signal side-channel
/// only when the image signals more than the IFF default.
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
    /// palette side-channel (opaque entries); the frame's colour-signal
    /// side-channel, when attached, becomes `color`. The geometry is
    /// validated.
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
            img.palette = Some(Palette::from_rgb(rgb));
            img.validate()?;
        }
        if let Some(sig) = frame.color_signal() {
            img.color = from_color_signal(&sig);
        }
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
        eof: false,
    }))
}

struct IffDecoder {
    codec_id: CodecId,
    pending: Option<VideoFrame>,
    eof: bool,
}

impl Decoder for IffDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        let image = crate::decode(&packet.data)?;
        self.pending = Some(image_into_video_frame(image, packet.pts));
        Ok(())
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

/// Factory registered with the codec registry: one frame in, one
/// complete `FORM ILBM` out through [`crate::encode`] with
/// [`EncodeOptions::default`] plus `drop_alpha` (a `Pal8` frame is
/// written index-for-index as planar bitplanes; `Rgb24` / `Rgba` as
/// the 24-bit literal-RGB form, alpha dropped — the 24-bit form has no
/// alpha mechanism). Accepted `pixel_format`s: `Pal8` (palette on the
/// frame's side-channel), `Rgb24`, `Rgba`; `Bgr24` / `Bgra` are
/// swapped to RGB.
pub fn make_encoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Encoder>> {
    let mut out_params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    out_params.width = params.width;
    out_params.height = params.height;
    out_params.pixel_format = params.pixel_format;
    Ok(Box::new(IffEncoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        out_params,
        opts: EncodeOptions::default().with_drop_alpha(true),
        pending: None,
        eof: false,
    }))
}

struct IffEncoder {
    codec_id: CodecId,
    out_params: CodecParameters,
    opts: EncodeOptions,
    pending: Option<Vec<u8>>,
    eof: bool,
}

/// Bring a framework frame in a layout outside the 1:1 set (`Bgr24`,
/// `Bgra`) into an [`IffImage`]; the 1:1 layouts go through
/// [`IffImage::from_video_frame`] unchanged.
fn frame_to_image(vf: &VideoFrame, params: &CodecParameters) -> oxideav_core::Result<IffImage> {
    let format = params.pixel_format.ok_or_else(|| {
        oxideav_core::Error::invalid("IFF encoder: pixel_format missing in CodecParameters")
    })?;
    let width = params.width.ok_or_else(|| {
        oxideav_core::Error::invalid("IFF encoder: width missing in CodecParameters")
    })?;
    let height = params.height.ok_or_else(|| {
        oxideav_core::Error::invalid("IFF encoder: height missing in CodecParameters")
    })?;
    match format {
        PixelFormat::Rgba | PixelFormat::Rgb24 | PixelFormat::Pal8 => {
            Ok(IffImage::from_video_frame(vf, params)?)
        }
        PixelFormat::Bgr24 | PixelFormat::Bgra => {
            let plane = vf
                .image_planes()
                .first()
                .ok_or_else(|| oxideav_core::Error::invalid("IFF encoder: empty frame plane"))?;
            let bpp = if format == PixelFormat::Bgr24 { 3 } else { 4 };
            let tight = tighten_packed(plane, width as usize, height as usize, bpp)?;
            if bpp == 3 {
                let rgb: Vec<u8> = tight
                    .chunks_exact(3)
                    .flat_map(|c| [c[2], c[1], c[0]])
                    .collect();
                Ok(IffImage::from_rgb8(width, height, rgb)?)
            } else {
                let rgba: Vec<u8> = tight
                    .chunks_exact(4)
                    .flat_map(|c| [c[2], c[1], c[0], c[3]])
                    .collect();
                Ok(IffImage::from_rgba8(width, height, rgba)?)
            }
        }
        other => Err(oxideav_core::Error::invalid(format!(
            "IFF encoder: unsupported pixel format {other:?}"
        ))),
    }
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
        let image = frame_to_image(vf, &self.out_params)?;
        self.pending = Some(crate::encode(&image, &self.opts)?);
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
    let caps = CodecCapabilities::video("ilbm_sw")
        .with_intra_only(true)
        .with_lossless(true)
        .with_max_size(65535, 65535)
        .with_pixel_formats(vec![
            PixelFormat::Pal8,
            PixelFormat::Rgb24,
            PixelFormat::Rgba,
            PixelFormat::Bgr24,
            PixelFormat::Bgra,
        ]);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder),
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

    #[test]
    fn encoder_drops_alpha_into_24_bit_and_swaps_bgr() {
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.width = Some(2);
        params.height = Some(1);
        params.pixel_format = Some(PixelFormat::Bgra);
        let frame = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 8,
                data: vec![1, 2, 3, 0, 4, 5, 6, 128],
            }],
        };
        let mut enc = make_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(frame)).unwrap();
        let pkt = enc.receive_packet().unwrap();
        let back = crate::decode(&pkt.data).unwrap();
        assert_eq!(back.format, Pf::Rgb24);
        assert_eq!(back.n_planes, 24);
        assert_eq!(back.to_rgb8(), vec![3, 2, 1, 6, 5, 4]);
    }
}
