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
    CODEC_ID_STR, EncodeOptions, IffForm, IffImage, IffPixelFormat, Palette, Plane, encode,
    make_decoder,
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

#[test]
fn transparent_colour_key_arrives_as_the_palette_alpha_record() {
    // BMHD masking 2 on disk: the demuxer declares Pal8 (the stream
    // palette has no alpha side), the registered decoder attaches the
    // palette-alpha record — that entry 0, every other entry 255 — so a
    // framework consumer sees the key instead of losing it.
    let pal = Palette::new(vec![[0, 0, 0, 255], [255, 0, 0, 0], [0, 0, 255, 255]]);
    let img = IffImage::new_indexed(3, 1, vec![0, 1, 2], pal).unwrap();
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    assert_eq!(oxideav_iff::info(&bytes).unwrap().masking, 2);
    let (dmx, pkt) = demux(&bytes);
    let s = &dmx.streams()[0];
    assert_eq!(s.params.pixel_format, Some(PixelFormat::Pal8));
    assert_eq!(s.params.extradata, vec![0, 0, 0, 255, 0, 0, 0, 0, 255]);

    let mut dec = make_decoder(&s.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("expected a video frame");
    };
    assert_eq!(v.palette_alpha(), Some(&[255u8, 0, 255][..]));
    assert_eq!(v.palette_rgba(1), Some([255, 0, 0, 0]));
    assert_eq!(v.palette_rgba(0), Some([0, 0, 0, 255]));
    // Mask plane (masking 1) keeps composing Rgba on the registry path.
    let masked = encode(
        &img,
        &EncodeOptions::default().with_masking(oxideav_iff::ilbm::Masking::HasMask),
    )
    .unwrap();
    assert_eq!(oxideav_iff::info(&masked).unwrap().masking, 1);
    let (dmx, _pkt) = demux(&masked);
    assert_eq!(
        dmx.streams()[0].params.pixel_format,
        Some(PixelFormat::Rgba)
    );
}

#[test]
fn registry_encoder_maps_the_alpha_record_back_to_a_masking_mode() {
    use oxideav_core::{VideoFrame, VideoPlane};
    let mut params = oxideav_core::CodecParameters::video(CodecId::new(CODEC_ID_STR));
    params.width = Some(4);
    params.height = Some(1);
    params.pixel_format = Some(PixelFormat::Pal8);
    let frame = |alpha: Vec<u8>| {
        let mut f = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 4,
                data: vec![0, 1, 2, 3],
            }],
        };
        f.set_palette(vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3]);
        f.set_palette_alpha(alpha);
        f
    };
    let encode_via_registry = |f: VideoFrame| {
        let mut enc = oxideav_iff::make_encoder(&params).unwrap();
        enc.send_frame(&Frame::Video(f)).unwrap();
        enc.receive_packet().unwrap().data
    };
    // One transparent entry → transparent colour (masking 2), Pal8 back.
    let one = encode_via_registry(frame(vec![255, 255, 0, 255]));
    let info = oxideav_iff::info(&one).unwrap();
    assert_eq!((info.masking, info.format), (2, IffPixelFormat::Pal8));
    assert_eq!(
        oxideav_iff::decode(&one).unwrap().palette.unwrap().entries[2],
        [2, 2, 2, 0]
    );
    // Several (alpha below 128) → mask plane (masking 1), Rgba back.
    let two = encode_via_registry(frame(vec![255, 127, 0, 255]));
    let info = oxideav_iff::info(&two).unwrap();
    assert_eq!((info.masking, info.format), (1, IffPixelFormat::Rgba));
    assert_eq!(
        oxideav_iff::decode(&two).unwrap().to_rgba8(),
        vec![0, 0, 0, 255, 1, 1, 1, 0, 2, 2, 2, 0, 3, 3, 3, 255]
    );
    // No record → masking 0.
    let none = encode_via_registry(frame(Vec::new()));
    assert_eq!(oxideav_iff::info(&none).unwrap().masking, 0);
}

#[test]
fn native_chunks_ride_the_registry_as_blobs_and_come_back() {
    use oxideav_core::{BlobKind, MetadataBlob};
    use oxideav_iff::{BLOB_KIND_ASPECT, BLOB_KIND_CAMG, IffChunk};
    let pal = Palette::from_rgb_triples(&[[0, 0, 0], [255, 255, 255]]);
    let img = IffImage::new_indexed(2, 1, vec![0, 1], pal)
        .unwrap()
        .with_chunk(IffChunk::text_chunk(*b"ANNO", "hello"))
        .with_chunk(IffChunk::new(*b"DPI ", [0, 72, 0, 96]))
        .with_viewmode(0x0000_8000u32)
        .with_aspect((10, 11));
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let (dmx, pkt) = demux(&bytes);
    let s = &dmx.streams()[0];
    // Stream level: one blob per carried chunk + CAMG + aspect.
    let kinds: Vec<&str> = s.params.blobs.iter().map(|b| b.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["iff-anno", "iff-dpi", BLOB_KIND_CAMG, BLOB_KIND_ASPECT]
    );
    assert_eq!(s.params.blobs[0].data, b"hello");
    assert_eq!(s.params.blobs[2].data, 0x8000u32.to_be_bytes());
    assert_eq!(s.params.blobs[3].data, [10, 11]);
    // Frame level: the decoder attaches the same set.
    let mut dec = make_decoder(&s.params).unwrap();
    dec.send_packet(&pkt).unwrap();
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("expected a video frame");
    };
    assert_eq!(v.blobs(), s.params.blobs);
    assert_eq!(
        v.blob(&BlobKind::custom("iff-anno")).map(|b| b.data),
        Some(b"hello".to_vec())
    );
    // Back through the frame bridge and the encoder: chunks, CAMG and
    // aspect are written again; a frame blob outranks the stream's.
    let back = IffImage::from_video_frame(&v, &s.params).unwrap();
    assert_eq!(back.chunks, img.chunks);
    assert_eq!((back.viewmode, back.aspect), (Some(0x8000), Some((10, 11))));

    let mut params = s.params.clone();
    params
        .blobs
        .push(MetadataBlob::new(BlobKind::ICC, vec![1, 2, 3])); // no IFF chunk
    let mut frame = v.clone();
    frame.set_blobs(vec![MetadataBlob::new(
        BlobKind::custom("iff-anno"),
        b"frame wins".to_vec(),
    )]);
    let mut enc = oxideav_iff::make_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(frame)).unwrap();
    let out = enc.receive_packet().unwrap().data;
    let re = oxideav_iff::decode_with(
        &out,
        &oxideav_iff::DecodeOptions::default().with_strict(true),
    )
    .unwrap();
    assert_eq!(re.chunk_text(b"ANNO").as_deref(), Some("frame wins"));
    assert_eq!(
        re.chunk(b"DPI ").map(|c| c.data.clone()),
        Some(vec![0, 72, 0, 96])
    );
    assert_eq!((re.viewmode, re.aspect), (Some(0x8000), Some((10, 11))));
    assert_eq!(oxideav_iff::info(&out).unwrap().chunks.len(), 2);

    // The `viewmode` option outranks the CAMG blob.
    let mut params = s.params.clone();
    params.options.insert("viewmode", "4");
    let mut enc = oxideav_iff::make_encoder(&params).unwrap();
    enc.send_frame(&Frame::Video(v.clone())).unwrap();
    let out = enc.receive_packet().unwrap().data;
    assert_eq!(oxideav_iff::decode(&out).unwrap().viewmode, Some(4));
}

#[test]
fn rawvideo_muxer_writes_the_stream_blobs_as_chunks() {
    use oxideav_core::{BlobKind, CodecParameters, MetadataBlob, StreamInfo, TimeBase};
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.width = Some(2);
    params.height = Some(1);
    params.pixel_format = Some(PixelFormat::Rgb24);
    params.blobs = vec![
        MetadataBlob::new(BlobKind::custom("iff-auth"), b"muxer test".to_vec()),
        MetadataBlob::new(BlobKind::custom("iff-aspect"), vec![5, 6]),
        MetadataBlob::new(BlobKind::XMP, b"<x/>".to_vec()),
    ];
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: Some(1),
        start_time: Some(0),
        params,
    };
    let reg = registry();
    let path =
        std::env::temp_dir().join(format!("oxideav-iff-ilbm-blobs-{}.lbm", std::process::id()));
    {
        let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
        let mut mux = reg
            .open_muxer("iff_ilbm", ws, std::slice::from_ref(&stream))
            .unwrap();
        mux.write_header().unwrap();
        let pkt = oxideav_core::Packet::new(0, TimeBase::new(1, 1), vec![1, 2, 3, 4, 5, 6]);
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let out = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let img = oxideav_iff::decode_with(
        &out,
        &oxideav_iff::DecodeOptions::default().with_strict(true),
    )
    .unwrap();
    assert_eq!(img.chunk_text(b"AUTH").as_deref(), Some("muxer test"));
    assert_eq!(img.aspect, Some((5, 6)));
    assert_eq!(img.chunks.len(), 1, "XMP has no IFF chunk");
    assert_eq!(img.to_rgb8(), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn a_form_shorter_than_its_type_is_an_error_not_a_panic() {
    // Found by the registry_path fuzz target: `FORM` with size 0..3
    // underflowed the body length in the demuxers' open paths.
    let reg = registry();
    for (name, form) in [
        ("iff_ilbm", b"PBM "),
        ("iff_ilbm", b"ILBM"),
        ("iff_acbm", b"ACBM"),
        ("iff_anim", b"ANIM"),
    ] {
        for size in 0u32..4 {
            let mut bytes = b"FORM".to_vec();
            bytes.extend_from_slice(&size.to_be_bytes());
            bytes.extend_from_slice(form);
            let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
            assert!(
                matches!(
                    reg.open_demuxer(name, rs, &oxideav_core::NullCodecResolver),
                    Err(Error::InvalidData(_))
                ),
                "{name} size {size}"
            );
        }
    }
}
