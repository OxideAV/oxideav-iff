//! Format-native metadata on the contract surface: the EA IFF 85 text
//! chunks (`ANNO` / `AUTH` / `(c) ` / `NAME` / `TEXT`) and the `DPI ` /
//! `GRAB` / `DEST` properties ride `IffImage::chunks` verbatim, `info`
//! reports them, and every encoder writes them back ahead of the body
//! with the FORM size patched (strict decode holds). Framework-free.

use oxideav_iff::{
    decode, decode_all, decode_with, encode, encode_all, info, DecodeOptions, EncodeOptions, Frame,
    IffChunk, IffForm, IffImage, Palette, PixelFormat, CARRIED_CHUNKS,
};

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() & 1 == 1 {
        out.push(0);
    }
    out
}

/// A 2×1 one-plane ILBM with every carried chunk, one unknown chunk,
/// and a `CAMG`, hand-assembled.
fn file_with_chunks() -> Vec<u8> {
    let mut bmhd = Vec::new();
    bmhd.extend_from_slice(&2u16.to_be_bytes()); // w
    bmhd.extend_from_slice(&1u16.to_be_bytes()); // h
    bmhd.extend_from_slice(&[0, 0, 0, 0]); // x, y
    bmhd.push(1); // nPlanes
    bmhd.push(0); // masking
    bmhd.push(0); // compression none
    bmhd.push(0); // pad
    bmhd.extend_from_slice(&0u16.to_be_bytes()); // transparentColor
    bmhd.extend_from_slice(&[10, 11]); // aspect
    bmhd.extend_from_slice(&2i16.to_be_bytes());
    bmhd.extend_from_slice(&1i16.to_be_bytes());
    let mut body = Vec::new();
    body.extend_from_slice(&chunk(b"BMHD", &bmhd));
    body.extend_from_slice(&chunk(b"ANNO", b"made by a test\0"));
    body.extend_from_slice(&chunk(b"CMAP", &[0, 0, 0, 255, 255, 255]));
    body.extend_from_slice(&chunk(b"AUTH", b"author"));
    body.extend_from_slice(&chunk(b"(c) ", b"(c) 2026 nobody"));
    body.extend_from_slice(&chunk(b"NAME", b"odd")); // odd length → pad byte
    body.extend_from_slice(&chunk(b"TEXT", b"line 1\nline 2"));
    body.extend_from_slice(&chunk(b"DPI ", &[0x00, 0xF0, 0x01, 0x2C]));
    body.extend_from_slice(&chunk(b"GRAB", &[0, 1, 0, 1]));
    body.extend_from_slice(&chunk(b"DEST", &[1, 0, 0, 1, 0, 1, 0, 0]));
    body.extend_from_slice(&chunk(b"XYZW", b"not carried"));
    body.extend_from_slice(&chunk(b"CAMG", &0x8000u32.to_be_bytes()));
    body.extend_from_slice(&chunk(b"BODY", &[0b0100_0000, 0])); // row: 0, 1
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(b"ILBM");
    out.extend_from_slice(&body);
    out
}

#[test]
fn decode_carries_the_native_chunks_in_file_order() {
    let bytes = file_with_chunks();
    let img = decode(&bytes).unwrap();
    let ids: Vec<[u8; 4]> = img.chunks.iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        [*b"ANNO", *b"AUTH", *b"(c) ", *b"NAME", *b"TEXT", *b"DPI ", *b"GRAB", *b"DEST"]
    );
    assert!(ids.iter().all(IffChunk::is_carried));
    assert!(!IffChunk::is_carried(b"XYZW"));
    assert_eq!(img.chunk_text(b"ANNO").as_deref(), Some("made by a test"));
    assert_eq!(img.chunk_text(b"TEXT").as_deref(), Some("line 1\nline 2"));
    assert_eq!(img.chunk_text(b"NAME").as_deref(), Some("odd"));
    assert_eq!(
        img.chunk(b"DPI ").unwrap().data,
        vec![0x00, 0xF0, 0x01, 0x2C]
    );
    assert_eq!(img.chunk(b"XYZW"), None);
    assert_eq!(img.viewmode, Some(0x8000));
    assert_eq!(img.aspect, Some((10, 11)));
    assert_eq!(img.format, PixelFormat::Pal8);
    assert_eq!(img.data(), &[0, 1]);

    let i = info(&bytes).unwrap();
    assert!(i.has_text);
    assert_eq!(i.chunks, ids);
    assert!(!i.has_icc && !i.has_exif && !i.has_xmp);
    assert!(img.metadata.is_empty(), "no ICC / Exif / XMP in IFF");
}

#[test]
fn every_encoder_writes_the_chunks_back_and_the_form_size_stays_exact() {
    let src = decode(&file_with_chunks()).unwrap();
    let strict = DecodeOptions::default().with_strict(true);
    for form in [IffForm::Ilbm, IffForm::Pbm, IffForm::Acbm] {
        let bytes = encode(&src, &EncodeOptions::default().with_form(form)).unwrap();
        let back = decode_with(&bytes, &strict).unwrap_or_else(|e| panic!("{form:?}: {e}"));
        assert_eq!(back.chunks, src.chunks, "{form:?}");
        assert_eq!(back.planes, src.planes, "{form:?}");
        assert_eq!(back.viewmode, src.viewmode, "{form:?}");
        assert_eq!(back.aspect, src.aspect, "{form:?}");
        // The chunks sit ahead of the body chunk.
        let body = if form == IffForm::Acbm {
            b"ABIT"
        } else {
            b"BODY"
        };
        let at = |id: &[u8; 4]| bytes.windows(4).position(|w| w == id).unwrap();
        assert!(at(b"ANNO") < at(body) && at(b"DEST") < at(body), "{form:?}");
    }
    for form in [IffForm::Deep, IffForm::Rgb8, IffForm::Rgbn] {
        let rgb = IffImage::from_rgb8(2, 1, src.to_rgb8())
            .unwrap()
            .with_chunks(src.chunks.clone());
        let bytes = encode(&rgb, &EncodeOptions::default().with_form(form)).unwrap();
        let back = decode_with(&bytes, &strict).unwrap_or_else(|e| panic!("{form:?}: {e}"));
        assert_eq!(back.chunks, src.chunks, "{form:?}");
        assert_eq!(back.to_rgb8(), src.to_rgb8(), "{form:?}");
    }
}

#[test]
fn caller_built_chunks_round_trip_and_text_chunks_are_latin1() {
    let pal = Palette::from_rgb_triples(&[[0, 0, 0], [255, 255, 255]]);
    let img = IffImage::new_indexed(2, 1, vec![0, 1], pal)
        .unwrap()
        .with_chunk(IffChunk::text_chunk(*b"ANNO", "Größe — é"))
        .with_chunk(IffChunk::new(*b"DPI ", [0, 72, 0, 72]))
        .with_chunk(IffChunk::text_chunk(*b"(c) ", "2026"));
    assert_eq!(img.chunks[0].text(), "Größe ? é");
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let back = decode_with(&bytes, &DecodeOptions::default().with_strict(true)).unwrap();
    assert_eq!(back.chunks, img.chunks);
    assert_eq!(back.chunk_text(b"(c) ").as_deref(), Some("2026"));
    let i = info(&bytes).unwrap();
    assert!(i.has_text);
    assert_eq!(i.chunks, vec![*b"ANNO", *b"DPI ", *b"(c) "]);
}

#[test]
fn anim_and_deep_sequences_carry_the_first_frames_chunks() {
    let pal = Palette::from_rgb_triples(&[[0, 0, 0], [255, 255, 255]]);
    let mk = |bits: Vec<u8>, anno: &str| {
        IffImage::new_indexed(4, 1, bits, pal.clone())
            .unwrap()
            .with_chunk(IffChunk::text_chunk(*b"ANNO", anno))
    };
    let frames = vec![
        Frame::new(mk(vec![0, 1, 0, 1], "first"), None, 0),
        Frame::new(mk(vec![1, 1, 0, 0], "second"), None, 1),
    ];
    let anim = encode_all(&frames, &EncodeOptions::default()).unwrap();
    let strict = DecodeOptions::default().with_strict(true);
    assert_eq!(
        decode_with(&anim, &strict)
            .unwrap()
            .chunk_text(b"ANNO")
            .as_deref(),
        Some("first")
    );
    let all = decode_all(&anim).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].image.chunk_text(b"ANNO").as_deref(), Some("first"));
    // Delta frames carry no chunks of their own.
    assert!(all[1].image.chunks.is_empty());
    assert_eq!(all[1].image.data(), &[1, 1, 0, 0]);

    let deep = encode_all(&frames, &EncodeOptions::default().with_form(IffForm::Deep)).unwrap();
    let all = decode_all(&deep).unwrap();
    assert_eq!(all.len(), 2);
    for f in &all {
        assert_eq!(f.image.chunk_text(b"ANNO").as_deref(), Some("first"));
    }
}

#[test]
fn blob_kind_names_map_both_ways() {
    let names: Vec<String> = CARRIED_CHUNKS
        .iter()
        .map(IffChunk::blob_kind_name_of)
        .collect();
    assert_eq!(
        names,
        [
            "iff-anno",
            "iff-auth",
            "iff-copyright",
            "iff-name",
            "iff-text",
            "iff-dpi",
            "iff-grab",
            "iff-dest"
        ]
    );
    for (id, name) in CARRIED_CHUNKS.iter().zip(&names) {
        assert_eq!(IffChunk::id_for_blob_kind_name(name), Some(*id));
    }
    assert_eq!(IffChunk::id_for_blob_kind_name("icc"), None);
    assert_eq!(IffChunk::id_for_blob_kind_name("iff-camg"), None);
    assert_eq!(IffChunk::new(*b"(c) ", b"x".to_vec()).id_str(), "(c) ");
}
