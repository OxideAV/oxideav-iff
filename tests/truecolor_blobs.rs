//! The true-colour containers carry the native chunks too: `iff_deep` /
//! `iff_tvpp` / `iff_rgb8` / `iff_rgbn` declare `ANNO` and friends as
//! `iff-*` stream blobs, and the DEEP / RGB8 / RGBN muxers write the
//! stream's blobs back ahead of the body.

use std::io::Cursor;

use oxideav_core::{
    BlobKind, CodecId, CodecParameters, ContainerRegistry, MetadataBlob, PixelFormat, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};
use oxideav_iff::{encode, DecodeOptions, EncodeOptions, IffChunk, IffForm, IffImage};

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

fn picture(form: IffForm) -> Vec<u8> {
    let rgb: Vec<u8> = (0..3 * 4 * 2).map(|i| (i * 9) as u8).collect();
    let img = IffImage::from_rgb8(4, 2, rgb)
        .unwrap()
        .with_chunk(IffChunk::text_chunk(*b"ANNO", "$VER: a test"))
        .with_chunk(IffChunk::text_chunk(*b"AUTH", "someone"));
    encode(&img, &EncodeOptions::default().with_form(form)).unwrap()
}

#[test]
fn true_colour_demuxers_declare_the_chunks_as_stream_blobs() {
    let reg = registry();
    for (form, name) in [
        (IffForm::Deep, "iff_deep"),
        (IffForm::Rgb8, "iff_rgb8"),
        (IffForm::Rgbn, "iff_rgbn"),
    ] {
        let bytes = picture(form);
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let dmx = reg
            .open_demuxer(name, rs, &oxideav_core::NullCodecResolver)
            .unwrap();
        let blobs = &dmx.streams()[0].params.blobs;
        let kinds: Vec<&str> = blobs.iter().map(|b| b.kind.as_str()).collect();
        assert_eq!(kinds, ["iff-anno", "iff-auth"], "{name}");
        assert_eq!(blobs[0].data, b"$VER: a test", "{name}");
        assert_eq!(
            oxideav_iff::blob_chunks(blobs)[1].text(),
            "someone",
            "{name}"
        );
    }
}

#[test]
fn true_colour_muxers_write_the_stream_blobs_back() {
    let reg = registry();
    for (name, form) in [
        ("iff_deep", IffForm::Deep),
        ("iff_rgb8", IffForm::Rgb8),
        ("iff_rgbn", IffForm::Rgbn),
    ] {
        let mut params = CodecParameters::video(CodecId::new("rawvideo"));
        params.width = Some(2);
        params.height = Some(1);
        params.pixel_format = Some(PixelFormat::Rgb24);
        params.blobs = vec![
            MetadataBlob::new(BlobKind::custom("iff-anno"), b"written back".to_vec()),
            MetadataBlob::new(BlobKind::EXIF, vec![0x4D, 0x4D, 0, 0x2A]),
        ];
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1),
            duration: Some(1),
            start_time: Some(0),
            params,
        };
        let path = std::env::temp_dir().join(format!(
            "oxideav-iff-{name}-blobs-{}.iff",
            std::process::id()
        ));
        {
            let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
            let mut mux = reg
                .open_muxer(name, ws, std::slice::from_ref(&stream))
                .unwrap();
            mux.write_header().unwrap();
            let pkt =
                oxideav_core::Packet::new(0, TimeBase::new(1, 1), vec![0, 16, 32, 240, 224, 208]);
            mux.write_packet(&pkt).unwrap();
            mux.write_trailer().unwrap();
        }
        let out = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let img =
            oxideav_iff::decode_with(&out, &DecodeOptions::default().with_strict(true)).unwrap();
        assert_eq!(img.form, form, "{name}");
        assert_eq!(
            img.chunk_text(b"ANNO").as_deref(),
            Some("written back"),
            "{name}"
        );
        assert_eq!(img.chunks.len(), 1, "{name}: Exif has no IFF chunk");
        assert_eq!(img.width, 2);
    }
}
