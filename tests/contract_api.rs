//! IMAGE_CRATE_API conformance for `oxideav-iff`: the root vocabulary
//! (`probe` / `info` / `decode*` / `encode*`), native layouts per FORM,
//! lossless round trips, limits, strictness, and byte-identity between
//! the contract path and the document-model parsers. Framework-free:
//! runs in the `--no-default-features` CI job too.

use std::time::Duration;

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, CAMG_EHB, CAMG_HAM,
};
use oxideav_iff::{
    decode, decode_all, decode_from, decode_rgb8, decode_rgba8, decode_with, encode, encode_all,
    encode_rgb8, encode_rgba8, encode_to, info, probe, AnimOp, DecodeOptions, EncodeOptions, Frame,
    IffError, IffForm, IffImage, Palette, PixelFormat,
};

fn bmhd(w: u16, h: u16, n_planes: u8, masking: Masking, key: u16) -> Bmhd {
    Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking,
        compression: Compression::ByteRun1,
        pad: 0,
        transparent_color: key,
        x_aspect: 10,
        y_aspect: 11,
        page_width: w as i16,
        page_height: h as i16,
    }
}

/// A 4-colour 8×2 picture as the document model, with RGBA already
/// quantised to the palette so the legacy encoder maps it 1:1.
fn legacy_indexed(masking: Masking, key: u16) -> IlbmImage {
    let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let indices: Vec<u8> = (0..16).map(|i| (i % 4) as u8).collect();
    let mut rgba = Vec::new();
    for (i, &idx) in indices.iter().enumerate() {
        let c = palette[idx as usize];
        let alpha = match masking {
            Masking::HasMask if i % 3 == 0 => 0,
            _ => 255,
        };
        rgba.extend_from_slice(&[c[0], c[1], c[2], alpha]);
    }
    IlbmImage {
        width: 8,
        height: 2,
        bmhd: bmhd(8, 2, 2, masking, key),
        palette,
        rgba,
        ..IlbmImage::default()
    }
}

// ───────────────────────── probe ─────────────────────────

#[test]
fn probe_is_total_and_recognises_raster_forms() {
    assert!(!probe(&[]));
    assert!(!probe(b"FORM"));
    assert!(!probe(b"FORM\0\0\0\x04ILB"));
    for ft in [
        b"ILBM", b"PBM ", b"ACBM", b"DEEP", b"RGB8", b"RGBN", b"ANIM",
    ] {
        let mut b = b"FORM\0\0\0\x04".to_vec();
        b.extend_from_slice(ft);
        assert!(probe(&b), "{ft:?}");
        let mut c = b"CAT \0\0\0\x04".to_vec();
        c.extend_from_slice(ft);
        assert!(probe(&c));
        let mut l = b"LIST\0\0\0\x04".to_vec();
        l.extend_from_slice(ft);
        assert!(probe(&l));
    }
    assert!(!probe(b"FORM\0\0\0\x048SVX"));
    assert!(!probe(b"FORM\0\0\0\x04AIFF"));
    assert!(!probe(b"RIFF\0\0\0\x04ILBM"));
}

// ───────────────────────── decode: native layouts ─────────────────────────

#[test]
fn indexed_ilbm_decodes_to_pal8_and_matches_legacy_rgba() {
    let legacy = legacy_indexed(Masking::None, 0);
    let bytes = encode_ilbm(&legacy).unwrap();
    let parsed = parse_ilbm(&bytes).unwrap();

    let i = info(&bytes).unwrap();
    assert_eq!((i.width, i.height), (8, 2));
    assert_eq!(i.format, PixelFormat::Pal8);
    assert_eq!(i.frames, 1);
    assert!(!i.has_alpha);
    assert_eq!(i.n_planes, 2);
    assert_eq!(i.compression, 1);
    assert_eq!(i.palette_len, 4);
    assert_eq!(i.form, IffForm::Ilbm);
    assert!(!i.ham && !i.ehb);
    assert!(!i.has_icc && !i.has_exif && !i.has_xmp);

    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Pal8);
    assert_eq!(img.width(), 8);
    assert_eq!(img.height(), 2);
    assert_eq!(img.n_planes, 2);
    assert_eq!(img.aspect, Some((10, 11)));
    assert_eq!(img.viewmode, None);
    let indices: Vec<u8> = (0..16).map(|i| (i % 4) as u8).collect();
    assert_eq!(img.as_bytes(), Some(indices.as_slice()));
    let pal = img.palette.as_ref().unwrap();
    assert_eq!(pal.len(), 4);
    assert_eq!(pal.entries[1], [255, 0, 0, 255]);
    assert_eq!(img.to_rgba8(), parsed.rgba, "contract RGBA == legacy RGBA");
    assert_eq!(decode_rgba8(&bytes).unwrap().data, parsed.rgba);
    let rgb: Vec<u8> = parsed
        .rgba
        .chunks(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    assert_eq!(decode_rgb8(&bytes).unwrap().data, rgb);
    assert_eq!(decode_from(std::io::Cursor::new(&bytes)).unwrap(), img);
}

#[test]
fn transparent_colour_key_rides_on_the_palette() {
    let legacy = legacy_indexed(Masking::HasTransparentColor, 2);
    let bytes = encode_ilbm(&legacy).unwrap();
    let parsed = parse_ilbm(&bytes).unwrap();
    let i = info(&bytes).unwrap();
    assert_eq!(i.format, PixelFormat::Pal8);
    assert!(i.has_alpha);
    assert_eq!(i.masking, 2);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Pal8);
    assert_eq!(img.palette.as_ref().unwrap().entries[2][3], 0);
    assert_eq!(img.palette.as_ref().unwrap().transparent_index(), Some(2));
    assert!(img.has_alpha());
    assert_eq!(img.to_rgba8(), parsed.rgba);
    // Lossless round trip keeps the key.
    let again = encode(&img, &EncodeOptions::default()).unwrap();
    assert_eq!(decode(&again).unwrap(), img);
    assert_eq!(info(&again).unwrap().masking, 2);
}

#[test]
fn mask_plane_decodes_to_rgba() {
    let legacy = legacy_indexed(Masking::HasMask, 0);
    let bytes = encode_ilbm(&legacy).unwrap();
    let parsed = parse_ilbm(&bytes).unwrap();
    let i = info(&bytes).unwrap();
    assert_eq!(i.format, PixelFormat::Rgba);
    assert!(i.has_alpha);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Rgba);
    assert!(img.palette.is_none());
    assert_eq!(img.as_bytes(), Some(parsed.rgba.as_slice()));
    assert_eq!(img.to_rgba8(), parsed.rgba);
    assert!(parsed.rgba.iter().skip(3).step_by(4).any(|&a| a == 0));
}

#[test]
fn truecolor_24_bit_decodes_to_rgb24_and_round_trips() {
    let mut rgb = Vec::new();
    for y in 0..3u8 {
        for x in 0..5u8 {
            rgb.extend_from_slice(&[x * 40, y * 90, 255 - x * 20]);
        }
    }
    let bytes = encode_rgb8(5, 3, &rgb, &EncodeOptions::default()).unwrap();
    let i = info(&bytes).unwrap();
    assert_eq!(i.format, PixelFormat::Rgb24);
    assert_eq!(i.n_planes, 24);
    assert_eq!(i.palette_len, 0);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Rgb24);
    assert_eq!(img.as_bytes(), Some(rgb.as_slice()));
    assert_eq!(img.n_planes, 24);
    // decode(encode(img)) == img for the contract image too.
    let src = IffImage::from_rgb8(5, 3, rgb.clone()).unwrap();
    let again = encode(&src, &EncodeOptions::default()).unwrap();
    let back = decode(&again).unwrap();
    assert_eq!(back.planes, src.planes);
    assert_eq!(back.format, src.format);
    // Legacy parser agrees byte-for-byte.
    let legacy = parse_ilbm(&bytes).unwrap();
    assert_eq!(legacy.rgba, img.to_rgba8());
}

#[test]
fn rgba_into_24_bit_needs_drop_alpha() {
    let rgba = vec![1, 2, 3, 0, 4, 5, 6, 255];
    let err = encode_rgba8(2, 1, &rgba, &EncodeOptions::default()).unwrap_err();
    assert!(matches!(err, IffError::Unsupported(_)), "{err}");
    let opaque = vec![1, 2, 3, 255, 4, 5, 6, 255];
    assert!(encode_rgba8(2, 1, &opaque, &EncodeOptions::default()).is_ok());
    let dropped =
        encode_rgba8(2, 1, &rgba, &EncodeOptions::default().with_drop_alpha(true)).unwrap();
    assert_eq!(decode_rgb8(&dropped).unwrap().data, vec![1, 2, 3, 4, 5, 6]);
    // With `indexed` the alpha becomes a HasMask plane.
    let masked = encode_rgba8(2, 1, &rgba, &EncodeOptions::default().with_indexed(true)).unwrap();
    let i = info(&masked).unwrap();
    assert_eq!(i.masking, 1);
    assert_eq!(i.format, PixelFormat::Rgba);
    assert_eq!(decode_rgba8(&masked).unwrap().data, rgba);
}

#[test]
fn indexed_option_quantises_rgb_deterministically() {
    // 300 distinct colours: the first 256 form the CMAP, the rest map to
    // the nearest entry.
    let mut rgb = Vec::new();
    for i in 0..300u32 {
        rgb.extend_from_slice(&[(i % 256) as u8, (i / 256) as u8 * 100, 7]);
    }
    let bytes = encode_rgb8(300, 1, &rgb, &EncodeOptions::default().with_indexed(true)).unwrap();
    let i = info(&bytes).unwrap();
    assert_eq!(i.format, PixelFormat::Pal8);
    assert_eq!(i.n_planes, 8);
    assert_eq!(i.palette_len, 256);
    let img = decode(&bytes).unwrap();
    let back = img.to_rgb8();
    assert_eq!(&back[..256 * 3], &rgb[..256 * 3], "first 256 colours exact");
    // Same input, same bytes.
    assert_eq!(
        bytes,
        encode_rgb8(300, 1, &rgb, &EncodeOptions::default().with_indexed(true)).unwrap()
    );
    // Few colours → few planes.
    let two = encode_rgb8(
        2,
        1,
        &[0, 0, 0, 9, 9, 9],
        &EncodeOptions::default().with_indexed(true),
    )
    .unwrap();
    assert_eq!(info(&two).unwrap().n_planes, 1);
}

#[test]
fn pal8_round_trip_is_lossless_across_ilbm_pbm_acbm() {
    let palette = Palette::new(vec![
        [0, 0, 0, 255],
        [255, 0, 0, 255],
        [255, 0, 0, 255], // duplicate colour: must survive index-for-index
        [0, 0, 255, 255],
        [9, 9, 9, 255],
    ]);
    let indices: Vec<u8> = (0..35).map(|i| (i % 5) as u8).collect();
    let img = IffImage::new_indexed(7, 5, indices, palette)
        .unwrap()
        .with_aspect((10u8, 11u8));
    for form in [IffForm::Ilbm, IffForm::Pbm, IffForm::Acbm] {
        for compression in [Compression::None, Compression::ByteRun1, Compression::Auto] {
            let opts = EncodeOptions::default()
                .with_form(form)
                .with_compression(compression);
            let bytes = encode(&img, &opts).unwrap();
            assert!(probe(&bytes));
            let i = info(&bytes).unwrap();
            assert_eq!(i.form, form, "{form:?}");
            assert_eq!(i.format, PixelFormat::Pal8);
            let back = decode(&bytes).unwrap();
            assert_eq!(back.planes, img.planes, "{form:?} {compression:?}");
            assert_eq!(back.palette, img.palette, "{form:?}");
            assert_eq!(back.form, form);
            assert_eq!(back.aspect, img.aspect);
            assert_eq!(
                back.n_planes,
                if form == IffForm::Pbm { 8 } else { 3 },
                "{form:?}"
            );
            // Second generation is byte-stable.
            let again = encode(&back, &opts).unwrap();
            assert_eq!(decode(&again).unwrap(), back);
        }
    }
}

#[test]
fn ehb_decodes_to_pal8_with_the_expanded_palette() {
    let palette: Vec<[u8; 3]> = (0..32u8).map(|i| [i * 8, 255 - i * 8, 128]).collect();
    let expanded = oxideav_iff::ilbm::expand_ehb_palette(&palette);
    let indices: Vec<u8> = (0..64).map(|i| i as u8).collect();
    let mut rgba = Vec::new();
    for &i in &indices {
        let c = expanded[i as usize];
        rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
    }
    let legacy = IlbmImage {
        width: 64,
        height: 1,
        bmhd: bmhd(64, 1, 6, Masking::None, 0),
        palette,
        camg: Camg { raw: CAMG_EHB },
        rgba,
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&legacy).unwrap();
    let i = info(&bytes).unwrap();
    assert!(i.ehb);
    assert_eq!(i.format, PixelFormat::Pal8);
    assert_eq!(i.viewmode, Some(CAMG_EHB));
    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Pal8);
    assert_eq!(img.palette.as_ref().unwrap().len(), 64);
    assert_eq!(img.as_bytes(), Some(indices.as_slice()));
    assert_eq!(img.to_rgba8(), parse_ilbm(&bytes).unwrap().rgba);
    assert_eq!(img.viewmode, Some(CAMG_EHB));
    // Round trip writes 6 planes + a 32-entry CMAP and reads back identical.
    let again = encode(&img, &EncodeOptions::default()).unwrap();
    assert_eq!(info(&again).unwrap().palette_len, 32);
    assert_eq!(info(&again).unwrap().n_planes, 6);
    assert_eq!(decode(&again).unwrap(), img);
}

#[test]
fn ham6_decodes_to_rgb24_and_matches_legacy() {
    // A smooth gradient the HAM encoder can only approximate; what
    // matters is that the contract path yields the legacy renderer's
    // bytes and the layout is Rgb24.
    let mut rgba = Vec::new();
    for x in 0..32u8 {
        rgba.extend_from_slice(&[x * 8, 255 - x * 8, x * 4, 255]);
    }
    let legacy = IlbmImage {
        width: 32,
        height: 1,
        bmhd: bmhd(32, 1, 6, Masking::None, 0),
        palette: vec![[0, 0, 0]; 16],
        camg: Camg { raw: CAMG_HAM },
        rgba,
        ..IlbmImage::default()
    };
    let bytes = encode_ilbm(&legacy).unwrap();
    let i = info(&bytes).unwrap();
    assert!(i.ham);
    assert_eq!(i.format, PixelFormat::Rgb24);
    assert!(!i.has_alpha);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Rgb24);
    assert_eq!(img.to_rgba8(), parse_ilbm(&bytes).unwrap().rgba);
    // Re-encoding the RGB through the HAM encoder is allowed (viewmode
    // carried on the image) and yields a HAM file again.
    let again = encode(&img, &EncodeOptions::default()).unwrap();
    assert!(info(&again).unwrap().ham);
    // But indices cannot be HAM codes.
    let pal8 = IffImage::new_indexed(1, 1, vec![0], Palette::from_rgb(&[0, 0, 0]))
        .unwrap()
        .with_viewmode(CAMG_HAM);
    assert!(matches!(
        encode(&pal8, &EncodeOptions::default()),
        Err(IffError::Unsupported(_))
    ));
}

#[test]
fn pbm_decodes_to_pal8_and_quantised_rgb_needs_indexed() {
    let img = IffImage::new_indexed(
        3,
        2,
        vec![0, 1, 2, 2, 1, 0],
        Palette::from_rgb_triples(&[[1, 1, 1], [2, 2, 2], [3, 3, 3]]),
    )
    .unwrap();
    let bytes = encode(&img, &EncodeOptions::default().with_form(IffForm::Pbm)).unwrap();
    assert_eq!(&bytes[8..12], b"PBM ");
    let back = decode(&bytes).unwrap();
    assert_eq!(back.planes, img.planes);
    assert_eq!(back.form, IffForm::Pbm);
    let rgb = IffImage::from_rgb8(1, 1, vec![5, 6, 7]).unwrap();
    assert!(matches!(
        encode(&rgb, &EncodeOptions::default().with_form(IffForm::Pbm)),
        Err(IffError::Unsupported(_))
    ));
    let quant = encode(
        &rgb,
        &EncodeOptions::default()
            .with_form(IffForm::Pbm)
            .with_indexed(true),
    )
    .unwrap();
    assert_eq!(decode_rgb8(&quant).unwrap().data, vec![5, 6, 7]);
}

// ───────────────────────── DEEP / RGB8 / RGBN ─────────────────────────

#[test]
fn deep_round_trips_rgb_and_rgba_natively() {
    let rgba: Vec<u8> = (0..4 * 6).map(|i| (i * 11) as u8).collect();
    let src = IffImage::from_rgba8(3, 2, rgba.clone()).unwrap();
    for compression in [Compression::None, Compression::ByteRun1, Compression::Auto] {
        let opts = EncodeOptions::default()
            .with_form(IffForm::Deep)
            .with_compression(compression);
        let bytes = encode(&src, &opts).unwrap();
        assert_eq!(&bytes[8..12], b"DEEP");
        let i = info(&bytes).unwrap();
        assert_eq!(i.format, PixelFormat::Rgba);
        assert!(i.has_alpha);
        assert_eq!(i.n_planes, 32);
        assert_eq!(i.frames, 1);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.format, PixelFormat::Rgba);
        assert_eq!(back.as_bytes(), Some(rgba.as_slice()));
        assert_eq!(back.form, IffForm::Deep);
    }
    let rgb = IffImage::from_rgb8(3, 2, vec![7; 18]).unwrap();
    let bytes = encode(&rgb, &EncodeOptions::default().with_form(IffForm::Deep)).unwrap();
    let i = info(&bytes).unwrap();
    assert_eq!(i.format, PixelFormat::Rgb24);
    assert_eq!(i.n_planes, 24);
    assert_eq!(decode(&bytes).unwrap().as_bytes(), Some(&[7u8; 18][..]));
    // rgb_only forces the 24-bit DPEL; non-opaque alpha then needs drop_alpha.
    let err = encode(
        &src,
        &EncodeOptions::default()
            .with_form(IffForm::Deep)
            .with_deep_rgb_only(true),
    )
    .unwrap_err();
    assert!(matches!(err, IffError::Unsupported(_)));
}

#[test]
fn deep_multi_frame_decode_all_and_encode_all() {
    let f0 = IffImage::from_rgb8(2, 2, vec![1; 12]).unwrap();
    let f1 = IffImage::from_rgb8(2, 2, vec![2; 12]).unwrap();
    let frames = vec![
        Frame::new(f0.clone(), Some(Duration::from_millis(40)), 0),
        Frame::new(f1.clone(), Some(Duration::from_millis(40)), 1),
    ];
    let bytes = encode_all(&frames, &EncodeOptions::default().with_form(IffForm::Deep)).unwrap();
    assert_eq!(info(&bytes).unwrap().frames, 2);
    let back = decode_all(&bytes).unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].image.planes, f0.planes);
    assert_eq!(back[1].image.planes, f1.planes);
    assert_eq!(back[0].delay, Some(Duration::from_millis(40)));
    assert_eq!(back[1].index, 1);
    assert_eq!(decode(&bytes).unwrap().planes, f0.planes);
}

#[test]
fn rgb8_and_rgbn_decode_and_round_trip() {
    let rgb: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x11, 0x22, 0x33];
    for form in [IffForm::Rgb8, IffForm::Rgbn] {
        let bytes = encode_rgb8(3, 1, &rgb, &EncodeOptions::default().with_form(form)).unwrap();
        assert_eq!(&bytes[8..12], &form.form_type());
        let i = info(&bytes).unwrap();
        assert_eq!(i.form, form);
        assert_eq!(i.format, PixelFormat::Rgb24);
        assert_eq!(i.compression, 4);
        assert_eq!(i.n_planes, if form == IffForm::Rgb8 { 25 } else { 13 });
        let img = decode(&bytes).unwrap();
        assert_eq!(img.format, PixelFormat::Rgb24);
        // RGBN keeps 4 bits per gun; the sample's guns are nibble-replicated
        // so both forms are exact here.
        assert_eq!(img.as_bytes(), Some(rgb.as_slice()));
        // Alpha 0 → genlock bit; brush-transparency policy reads it back.
        let rgba = vec![
            0x11, 0x22, 0x33, 0, 0x44, 0x55, 0x66, 255, 0x11, 0x22, 0x33, 255,
        ];
        let masked = encode_rgba8(3, 1, &rgba, &EncodeOptions::default().with_form(form)).unwrap();
        let opts = DecodeOptions::default()
            .with_genlock(oxideav_iff::ilbm::GenlockPolicy::BrushTransparency);
        let back = decode_with(&masked, &opts).unwrap();
        assert_eq!(back.format, PixelFormat::Rgba);
        assert_eq!(back.as_bytes(), Some(rgba.as_slice()));
    }
}

// ───────────────────────── ANIM ─────────────────────────

#[test]
fn anim_decode_all_yields_pal8_frames_with_delays_and_round_trips() {
    let palette =
        Palette::from_rgb_triples(&[[0, 0, 0], [255, 255, 255], [128, 0, 0], [0, 128, 0]]);
    let mut frames = Vec::new();
    for f in 0..3u8 {
        let indices: Vec<u8> = (0..32).map(|i| ((i + f as usize) % 4) as u8).collect();
        let img = IffImage::new_indexed(16, 2, indices, palette.clone()).unwrap();
        frames.push(Frame::new(
            img,
            Some(Duration::from_micros(2 * 1_000_000 / 60)),
            f as u32,
        ));
    }
    for op in [
        AnimOp::Op0,
        AnimOp::Op1,
        AnimOp::Op2,
        AnimOp::Op3,
        AnimOp::Op4 { long_data: false },
        AnimOp::Op5,
        AnimOp::Op7 { long_data: false },
        AnimOp::Op8 { long_data: false },
    ] {
        let bytes = encode_all(&frames, &EncodeOptions::default().with_anim_op(op)).unwrap();
        assert_eq!(&bytes[8..12], b"ANIM", "{op:?}");
        assert!(probe(&bytes));
        let i = info(&bytes).unwrap();
        assert_eq!(i.frames, 3, "{op:?}");
        assert_eq!(i.format, PixelFormat::Pal8);
        // decode = seed frame.
        let seed = decode(&bytes).unwrap();
        assert_eq!(seed.planes, frames[0].image.planes, "{op:?}");
        assert_eq!(seed.palette, frames[0].image.palette);
        let all = decode_all(&bytes).unwrap();
        assert_eq!(all.len(), 3, "{op:?}");
        for (k, fr) in all.iter().enumerate() {
            assert_eq!(fr.index, k as u32);
            assert_eq!(fr.image.format, PixelFormat::Pal8, "{op:?} frame {k}");
            assert_eq!(fr.image.planes, frames[k].image.planes, "{op:?} frame {k}");
            assert_eq!(fr.image.palette, frames[k].image.palette);
            assert_eq!(
                fr.delay,
                Some(Duration::from_micros(2 * 1_000_000 / 60)),
                "{op:?} frame {k}"
            );
        }
    }
}

// ───────────────────────── groups ─────────────────────────

#[test]
fn cat_of_pictures_decodes_first_and_all() {
    let a = IffImage::from_rgb8(1, 1, vec![1, 2, 3]).unwrap();
    let b =
        IffImage::new_indexed(2, 1, vec![0, 1], Palette::from_rgb(&[9, 9, 9, 8, 8, 8])).unwrap();
    let fa = encode(&a, &EncodeOptions::default()).unwrap();
    let fb = encode(&b, &EncodeOptions::default()).unwrap();
    let mut body = b"ILBM".to_vec();
    body.extend_from_slice(&fa);
    body.extend_from_slice(&fb);
    let mut cat = b"CAT ".to_vec();
    cat.extend_from_slice(&(body.len() as u32).to_be_bytes());
    cat.extend_from_slice(&body);
    assert!(probe(&cat));
    let i = info(&cat).unwrap();
    assert_eq!(i.frames, 2);
    assert_eq!(i.format, PixelFormat::Rgb24);
    assert_eq!(decode(&cat).unwrap().planes, a.planes);
    let all = decode_all(&cat).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].image.planes, b.planes);
    assert_eq!(all[1].index, 1);
    assert_eq!(all[0].delay, None);
}

// ───────────────────────── options ─────────────────────────

#[test]
fn limits_are_enforced_before_decoding() {
    let img = IffImage::from_rgb8(4, 4, vec![0; 48]).unwrap();
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    assert!(decode_with(&bytes, &DecodeOptions::default()).is_ok());
    for opts in [
        DecodeOptions::default().with_max_width(3u32),
        DecodeOptions::default().with_max_height(3u32),
        DecodeOptions::default().with_max_pixels(15u64),
        DecodeOptions::default().with_max_bytes(63u64),
    ] {
        assert!(matches!(
            decode_with(&bytes, &opts),
            Err(IffError::LimitExceeded(_))
        ));
    }
    assert!(decode_with(&bytes, &DecodeOptions::default().unlimited()).is_ok());
    // `info` is header-only and unaffected.
    assert!(info(&bytes).is_ok());
}

#[test]
fn strict_rejects_trailing_bytes_and_truncated_forms() {
    let img = IffImage::from_rgb8(2, 2, vec![0; 12]).unwrap();
    let mut bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let lenient = DecodeOptions::default();
    let strict = DecodeOptions::default().with_strict(true);
    assert!(decode_with(&bytes, &strict).is_ok());
    bytes.extend_from_slice(b"junk");
    assert!(decode_with(&bytes, &lenient).is_ok());
    assert!(matches!(
        decode_with(&bytes, &strict),
        Err(IffError::InvalidData(_))
    ));
    let mut truncated = encode(&img, &EncodeOptions::default()).unwrap();
    truncated.truncate(truncated.len() - 2);
    assert!(matches!(
        decode_with(&truncated, &strict),
        Err(IffError::InvalidData(_))
    ));
}

#[test]
fn encode_to_writes_the_same_bytes() {
    let img = IffImage::from_rgb8(2, 2, vec![3; 12]).unwrap();
    let mut out = Vec::new();
    encode_to(&img, &EncodeOptions::default(), &mut out).unwrap();
    assert_eq!(out, encode(&img, &EncodeOptions::default()).unwrap());
}

#[test]
fn hostile_inputs_error_without_panicking() {
    for bytes in [
        &b""[..],
        b"FORM",
        b"FORM\0\0\0\x04ILBM",
        b"FORM\xFF\xFF\xFF\xFFILBMBMHD\0\0\0\x14",
        b"FORM\0\0\0\x20ILBMBODY\xFF\xFF\xFF\xFF",
        b"FORM\0\0\0\x10DEEPDGBL\0\0\0\x02\0\0",
        b"CAT \0\0\0\x04ILBM",
        b"FORM\0\0\0\x04ANIM",
    ] {
        assert!(info(bytes).is_err());
        assert!(decode(bytes).is_err());
        assert!(decode_all(bytes).is_err());
    }
    assert!(matches!(
        decode(b"FORM\0\0\0\x048SVX"),
        Err(IffError::Unsupported(_))
    ));
}

#[test]
fn encode_rejects_oversized_and_empty_geometry() {
    let big = IffImage::from_rgb8(70_000, 1, vec![0; 70_000 * 3]).unwrap();
    assert!(matches!(
        encode(&big, &EncodeOptions::default()),
        Err(IffError::Unsupported(_))
    ));
    let empty = IffImage::from_rgb8(0, 0, vec![]).unwrap();
    assert!(matches!(
        encode(&empty, &EncodeOptions::default()),
        Err(IffError::InvalidData(_))
    ));
    assert!(matches!(
        encode_all(&[], &EncodeOptions::default()),
        Err(IffError::InvalidData(_))
    ));
}
