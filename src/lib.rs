//! Pure-Rust reader and writer for the Electronic Arts / Commodore
//! **IFF 85** container family ("FORM / LIST / CAT" chunked format) and
//! the picture, animation and sound FORMs that live in it.
//!
//! IFF files are big-endian chunk trees. The top-level chunk is always a
//! group chunk — `FORM`, `LIST`, or `CAT ` — whose first 4 bytes of
//! payload are a 4-character "form type" such as `ILBM` (Amiga picture),
//! `ANIM` (cel animation), `DEEP` / `RGB8` / `RGBN` (true-colour
//! pictures), `8SVX` (Amiga 8-bit sampled voice) or `AIFF` (Apple audio).
//!
//! # Standalone use (the image-crate contract)
//!
//! The crate root implements the OxideAV `IMAGE_CRATE_API`: the still
//! pictures (`ILBM` / `PBM ` / `ACBM` / `DEEP` / `RGB8` / `RGBN`), the
//! `ANIM` frame sequence and `CAT ` / `LIST` groups decode and encode
//! through [`probe`], [`info`], [`decode`] / [`decode_with`] /
//! [`decode_rgb8`] / [`decode_rgba8`] / [`decode_all`] / [`decode_from`]
//! and [`encode`] / [`encode_rgb8`] / [`encode_rgba8`] / [`encode_to`] /
//! [`encode_all`], returning plain `Vec<u8>` pixels in an [`IffImage`].
//! All of it builds with `default-features = false` and no `oxideav-core`.
//!
//! ```
//! # fn main() -> Result<(), oxideav_iff::Error> {
//! // A 2×1 two-colour picture, written as a planar FORM ILBM and read back.
//! let palette = oxideav_iff::Palette::from_rgb_triples(&[[0, 0, 0], [255, 255, 255]]);
//! let img = oxideav_iff::IffImage::new_indexed(2, 1, vec![0, 1], palette)?;
//! let bytes = oxideav_iff::encode(&img, &oxideav_iff::EncodeOptions::default())?;
//!
//! assert!(oxideav_iff::probe(&bytes));
//! let info = oxideav_iff::info(&bytes)?;
//! assert_eq!((info.width, info.height, info.n_planes), (2, 1, 1));
//! let back = oxideav_iff::decode(&bytes)?;
//! assert_eq!(back.format, oxideav_iff::PixelFormat::Pal8);
//! assert_eq!(back.to_rgba8(), vec![0, 0, 0, 255, 255, 255, 255, 255]);
//! # Ok(()) }
//! ```
//!
//! # Depth below the contract
//!
//! The document models stay available under their own names:
//! [`ilbm::parse_ilbm`] / [`ilbm::encode_ilbm`] ([`ilbm::IlbmImage`] with
//! every property chunk — `GRAB`, `DEST`, `SPRT`, `SHAM`, `PCHG`, `CRNG`,
//! `CCRT`, `DRNG`), [`ilbm::parse_acbm`], [`ilbm::parse_deep_frames`],
//! [`ilbm::parse_rgb8`] / [`ilbm::parse_rgbn`], [`ilbm::parse_tvpp`],
//! [`anim::parse_anim`] and the per-operation ANIM encoders, the `8SVX`
//! voice model in [`svx`] (framework-only) and the AIFF chunk parsers in
//! [`aiff`].
//!
//! # Framework use
//!
//! With the default-on `registry` feature the crate plugs into
//! `oxideav-core`: [`register`] installs the `ilbm` image codec
//! ([`make_decoder`] / [`make_encoder`], one whole FORM per packet, native
//! layout out) and every IFF-family container demuxer and muxer
//! (`iff_ilbm`, `iff_acbm`, `iff_rgb8`, `iff_rgbn`, `iff_deep`,
//! `iff_tvpp`, `iff_anim`, `iff_8svx`, `aiff`); `From<IffImage> for
//! VideoFrame` and [`IffImage::from_video_frame`] bridge the two layers.

pub mod aiff;
pub mod anim;
pub mod api;
pub mod chunk;
pub mod error;
pub mod ilbm;
pub mod image;
pub mod options;
#[cfg(feature = "registry")]
pub mod registry;
#[cfg(feature = "registry")]
pub mod svx;

// ---- The image-crate contract (IMAGE_CRATE_API) ----
pub use api::{
    decode, decode_all, decode_all_with, decode_from, decode_rgb8, decode_rgba8, decode_with,
    encode, encode_all, encode_rgb8, encode_rgba8, encode_to, info, probe,
};
pub use error::{Error, IffError, Result};
pub use image::{
    ColorInfo, ColorRange, Frame, IffForm, IffImage, IffPixelFormat, ImageInfo, Metadata, Palette,
    PixelFormat, Plane, RgbImage, RgbaImage,
};
pub use options::{AnimOp, DecodeOptions, EncodeOptions};

// ---- Framework integration (registry feature) ----
#[cfg(feature = "registry")]
#[doc(hidden)]
pub use registry::__oxideav_entry;
#[cfg(feature = "registry")]
pub use registry::{
    make_decoder, make_encoder, register, register_codecs, register_containers,
    register_registries, CODEC_ID_STR,
};
