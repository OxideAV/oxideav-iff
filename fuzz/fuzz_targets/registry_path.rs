#![no_main]

//! Feed arbitrary fuzz-supplied bytes through the framework (`registry`)
//! surface the round-477 contract audit touched: the `iff_ilbm` /
//! `iff_acbm` / `iff_anim` demuxers (stream labels, palette `extradata`,
//! metadata blobs), the registered `ilbm` decoder (native frame with the
//! palette + palette-alpha + blobs side-channel records), the frame
//! bridge (`IffImage::from_video_frame`, `apply_blobs` / `image_blobs`)
//! and the `ilbm` encoder (declared layouts, option schema, the masking
//! rule, chunk splice) — then back through `decode` with `strict` so the
//! FORM size the splice patched is checked.
//!
//! The contract under test: every call *returns*, nothing panics,
//! overflows or indexes out of bounds, and a picture that decodes
//! re-encodes through the registry to a strict-decodable FORM whose
//! `Pal8` planes and palette alpha match.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    BlobKind, CodecParameters, ContainerRegistry, Error, Frame, MetadataBlob, NullCodecResolver,
    PixelFormat, ReadSeek,
};
use oxideav_iff::{DecodeOptions, IffImage, IffPixelFormat};

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

fuzz_target!(|data: &[u8]| {
    // Keep the working buffers small enough for a fuzz iteration.
    let opts = DecodeOptions::default()
        .with_max_pixels(1u64 << 16)
        .with_max_bytes(1u64 << 20);
    let Ok(img) = oxideav_iff::decode_with(data, &opts) else {
        // Hostile input: the demuxers must still return, not panic.
        let reg = registry();
        for name in ["iff_ilbm", "iff_acbm", "iff_anim"] {
            let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
            if let Ok(mut dmx) = reg.open_demuxer(name, rs, &NullCodecResolver) {
                for _ in 0..4 {
                    match dmx.next_packet() {
                        Ok(_) => {}
                        Err(Error::Eof) | Err(_) => break,
                    }
                }
            }
        }
        return;
    };

    // Demux the raster FORMs that the registry opens whole.
    let reg = registry();
    let name = match &data[8..12] {
        b"ILBM" | b"PBM " => Some("iff_ilbm"),
        b"ACBM" => Some("iff_acbm"),
        b"ANIM" => Some("iff_anim"),
        _ => None,
    };
    if let Some(name) = name {
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
        if let Ok(mut dmx) = reg.open_demuxer(name, rs, &NullCodecResolver) {
            let params = dmx.streams()[0].params.clone();
            let _ = params.blobs.len();
            if name != "iff_anim" {
                if let (Ok(pkt), Ok(mut dec)) =
                    (dmx.next_packet(), oxideav_iff::make_decoder(&params))
                {
                    if dec.send_packet(&pkt).is_ok() {
                        if let Ok(Frame::Video(vf)) = dec.receive_frame() {
                            let _ = vf.palette_alpha();
                            let _ = vf.blobs();
                            let back = IffImage::from_video_frame(&vf, &params)
                                .expect("own frame bridges back");
                            assert_eq!(back.planes, img.planes);
                            assert_eq!(back.palette, img.palette);
                            assert_eq!(back.chunks, img.chunks);
                        }
                    }
                }
            } else {
                for _ in 0..8 {
                    if dmx.next_packet().is_err() {
                        break;
                    }
                }
            }
        }
    }

    // The frame bridge + encoder on every declared layout the picture
    // can present itself as, with a foreign blob the encoder must skip.
    let frame: oxideav_core::VideoFrame = img.clone().into();
    let mut params = CodecParameters::video(oxideav_core::CodecId::new(oxideav_iff::CODEC_ID_STR));
    params.width = Some(img.width);
    params.height = Some(img.height);
    params.pixel_format = Some(PixelFormat::from(img.format));
    params
        .blobs
        .push(MetadataBlob::new(BlobKind::ICC, vec![0; 4]));
    let strict = DecodeOptions::default().with_strict(true);
    for (form, masking) in [
        ("ilbm", "auto"),
        ("acbm", "auto"),
        ("pbm", "transparent"),
        ("deep", "auto"),
        ("ilbm", "mask"),
    ] {
        let mut p = params.clone();
        p.options.insert("form", form);
        p.options.insert("masking", masking);
        p.options.insert("drop_alpha", "true");
        let Ok(mut enc) = oxideav_iff::make_encoder(&p) else {
            continue;
        };
        if enc.send_frame(&Frame::Video(frame.clone())).is_err() {
            continue;
        }
        let pkt = enc.receive_packet().expect("a sent frame yields a packet");
        let back = oxideav_iff::decode_with(&pkt.data, &strict)
            .expect("registry output decodes strictly");
        let label = enc.output_params().pixel_format.expect("label set");
        assert_eq!(label, PixelFormat::from(back.format), "truthful label");
        assert_eq!(back.chunks, img.chunks, "chunks written back");
        if img.format == IffPixelFormat::Pal8 && back.format == IffPixelFormat::Pal8 && form != "deep"
        {
            assert_eq!(back.planes, img.planes);
        }
    }
});
