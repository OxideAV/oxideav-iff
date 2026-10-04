//! `iff_ilbm` demuxer — native stream layout (image-crate contract, round
//! 470 fleet sweep).
//!
//! The demuxer declares an `ilbm` codec stream whose `pixel_format` is the
//! picture's own layout (`Pal8` with the `CMAP` as RGB triples in
//! `extradata`, `Rgb24` for a 24-bit ILBM, `Rgba` for a masked picture)
//! and emits the whole `FORM` as one keyframe packet; the registered
//! `ilbm` decoder turns it into the native frame with the palette
//! side-channel. The `iff_ilbm` muxer writes such packets verbatim, so a
//! demux → mux round trip is byte-exact.

use std::io::Cursor;

use oxideav_core::{
    CodecId, ContainerRegistry, Error, Frame, MediaType, PixelFormat, ReadSeek, WriteSeek,
};
use oxideav_iff::{
    encode, make_decoder, EncodeOptions, IffForm, IffImage, IffPixelFormat, Palette, Plane,
    CODEC_ID_STR,
};

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

fn demux(bytes: &[u8]) -> (Box<dyn oxideav_core::Demuxer>, oxideav_core::Packet) {
    let reg = registry();
    let mut cur = Cursor::new(bytes.to_vec());
    assert_eq!(reg.probe_input(&mut cur, None).unwrap(), "iff_ilbm");
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    let mut dmx = reg
        .open_demuxer("iff_ilbm", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    let pkt = dmx.next_packet().unwrap();
    assert!(pkt.flags.keyframe);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    (dmx, pkt)
}

fn pal4() -> Palette {
    Palette::new(vec![
        [0, 0, 0, 255],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
    ])
}

fn indexed_file() -> (IffImage, Vec<u8>) {
    let indices: Vec<u8> = (0..24u8).map(|i| i % 4).collect();
    let img = IffImage::new(6, 4, IffPixelFormat::Pal8, vec![Plane::new(6, indices)])
        .unwrap()
        .with_palette(pal4());
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    (img, bytes)
}

#[test]
fn ilbm_demuxer_declares_pal8_with_palette_extradata_and_emits_the_form() {
    let (img, bytes) = indexed_file();
    let (dmx, pkt) = demux(&bytes);
    let s = &dmx.streams()[0];
    assert_eq!(s.params.codec_id, CodecId::new(CODEC_ID_STR));
    assert_eq!(s.params.media_type, MediaType::Video);
    assert_eq!((s.params.width, s.params.height), (Some(6), Some(4)));
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Pal8));
    assert_eq!(s.params.extradata, pal4().to_rgb());
    // The packet is the FORM itself, not decoded samples.
    assert_eq!(pkt.data, bytes);

    // Registered decoder: indexed samples + palette side-channel.
    let mut dec = make_decoder(&s.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("expected a video frame");
    };
    assert_eq!(v.image_plane_count(), 1);
    assert_eq!(v.planes[0].stride, 6);
    assert_eq!(v.planes[0].data, img.planes[0].data);
    assert_eq!(v.palette(), Some(&pal4().to_rgb()[..]));
    assert!(v.color_signal().is_none(), "IFF carries no colour signal");
}

#[test]
fn ilbm_demuxer_declares_rgb24_for_a_24bit_ilbm() {
    let rgb: Vec<u8> = (0..3 * 5 * 2).map(|i| (i * 7) as u8).collect();
    let img = IffImage::from_rgb8(5, 2, rgb.clone()).unwrap();
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let (dmx, pkt) = demux(&bytes);
    let s = &dmx.streams()[0];
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Rgb24));
    assert!(s.params.extradata.is_empty());

    let mut dec = make_decoder(&s.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("expected a video frame");
    };
    assert_eq!(v.planes[0].data, rgb);
    assert!(v.palette().is_none());
}

#[test]
fn ilbm_demuxer_declares_rgba_for_a_masked_picture() {
    // A transparent palette entry becomes `HasTransparentColor`; the
    // contract decode keeps the picture indexed (alpha rides the palette).
    // A mask plane on an RGB input (`Rgba` into an indexed form) is what
    // produces an `Rgba` decode — exercise it through the `Deep` form,
    // whose DPEL carries alpha natively.
    let rgba = vec![1, 2, 3, 255, 4, 5, 6, 0];
    let img = IffImage::from_rgba8(2, 1, rgba.clone()).unwrap();
    let bytes = encode(&img, &EncodeOptions::default().with_form(IffForm::Deep)).unwrap();
    let reg = registry();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = reg
        .open_demuxer("iff_deep", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    assert_eq!(
        dmx.streams()[0].params.pixel_format,
        Some(PixelFormat::Rgba)
    );
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, rgba);
}

#[test]
fn ilbm_muxer_writes_ilbm_packets_verbatim() {
    let (_img, bytes) = indexed_file();
    let (dmx, pkt) = demux(&bytes);
    let reg = registry();
    let path = std::env::temp_dir().join(format!(
        "oxideav-iff-ilbm-native-{}.lbm",
        std::process::id()
    ));
    {
        let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
        let mut mux = reg.open_muxer("iff_ilbm", ws, dmx.streams()).unwrap();
        mux.write_header().unwrap();
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let out = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(out, bytes);
}

#[test]
fn ilbm_muxer_rejects_a_non_form_ilbm_packet() {
    let (_img, bytes) = indexed_file();
    let (dmx, mut pkt) = demux(&bytes);
    pkt.data = vec![0; 16];
    let reg = registry();
    let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mux = reg.open_muxer("iff_ilbm", ws, dmx.streams()).unwrap();
    mux.write_header().unwrap();
    mux.write_packet(&pkt).unwrap();
    assert!(mux.write_trailer().is_err());
}
