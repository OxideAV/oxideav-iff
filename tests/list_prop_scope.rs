//! `LIST` shared properties and nested groups (EA IFF 85 §5).
//!
//! "PROP chunks may appear in LISTs … They supply shared properties for
//! the FORMs in that LIST … Individual FORMs can override the shared
//! settings", and "shared properties are semantically equivalent to
//! copying those properties into each of the nested FORMs right after
//! their FORM type IDs". `decode` / `decode_all` (and the `iff_ilbm`
//! group demuxer built on the same walk) apply them.

use oxideav_iff::{
    EncodeOptions, IffImage, IffPixelFormat, Palette, Plane, decode, decode_all, encode,
};

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    if body.len() & 1 == 1 {
        v.push(0);
    }
    v
}

/// The `(id, padded wire bytes)` chunks of an encoded `FORM`.
fn chunks_of(form: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 12;
    while pos + 8 <= form.len() {
        let id = [form[pos], form[pos + 1], form[pos + 2], form[pos + 3]];
        let size = u32::from_be_bytes([form[pos + 4], form[pos + 5], form[pos + 6], form[pos + 7]])
            as usize;
        let end = pos + 8 + size + (size & 1);
        out.push((id, form[pos..end.min(form.len())].to_vec()));
        pos = end;
    }
    out
}

fn pick(chunks: &[([u8; 4], Vec<u8>)], ids: &[&[u8; 4]]) -> Vec<u8> {
    chunks
        .iter()
        .filter(|(id, _)| ids.contains(&id))
        .flat_map(|(_, b)| b.clone())
        .collect()
}

fn palette(seed: u8) -> Palette {
    Palette::new(
        (0..4u8)
            .map(|i| [(i * 60).wrapping_add(seed), 200 - i * 40, seed ^ i, 255])
            .collect(),
    )
}

fn picture(w: u32, h: u32, seed: u8) -> (IffImage, Vec<u8>) {
    let idx: Vec<u8> = (0..w * h)
        .map(|i| ((i + u32::from(seed)) % 4) as u8)
        .collect();
    let img = IffImage::new(
        w,
        h,
        IffPixelFormat::Pal8,
        vec![Plane::new(w as usize, idx)],
    )
    .unwrap()
    .with_palette(palette(seed));
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    (img, bytes)
}

fn group(id: &[u8; 4], contents: &[u8; 4], kids: &[Vec<u8>]) -> Vec<u8> {
    chunk(id, &[contents.to_vec(), kids.concat()].concat())
}

fn form(ft: &[u8; 4], body: &[u8]) -> Vec<u8> {
    chunk(b"FORM", &[ft.to_vec(), body.to_vec()].concat())
}

fn indices(img: &IffImage) -> &[u8] {
    &img.planes[0].data
}

#[test]
fn list_prop_supplies_the_header_and_colour_map() {
    // Two pictures with the same size and colours: BMHD + CMAP move into
    // one PROP ILBM, each FORM keeps only its BODY.
    let (a, a_bytes) = picture(8, 3, 0);
    let (b, b_bytes) = picture(8, 3, 0);
    let a_chunks = chunks_of(&a_bytes);
    let prop = chunk(
        b"PROP",
        &[b"ILBM".to_vec(), pick(&a_chunks, &[b"BMHD", b"CMAP"])].concat(),
    );
    let f1 = form(b"ILBM", &pick(&a_chunks, &[b"BODY"]));
    let f2 = form(b"ILBM", &pick(&chunks_of(&b_bytes), &[b"BODY"]));
    let list = group(b"LIST", b"ILBM", &[prop, f1, f2]);

    let frames = decode_all(&list).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(indices(&frames[0].image), indices(&a));
    assert_eq!(indices(&frames[1].image), indices(&b));
    assert_eq!(frames[1].image.palette, a.palette);
    // `decode` = the first child, with the shared properties applied.
    assert_eq!(indices(&decode(&list).unwrap()), indices(&a));
}

#[test]
fn a_forms_own_property_overrides_the_shared_one() {
    // Shared CMAP = palette(0); the second FORM carries its own CMAP.
    let (a, a_bytes) = picture(4, 2, 0);
    let (b, b_bytes) = picture(4, 2, 77);
    let a_chunks = chunks_of(&a_bytes);
    let b_chunks = chunks_of(&b_bytes);
    let prop = chunk(
        b"PROP",
        &[b"ILBM".to_vec(), pick(&a_chunks, &[b"BMHD", b"CMAP"])].concat(),
    );
    let f1 = form(b"ILBM", &pick(&a_chunks, &[b"BODY"]));
    let f2 = form(b"ILBM", &pick(&b_chunks, &[b"CMAP", b"BODY"]));
    let frames = decode_all(&group(b"LIST", b"ILBM", &[prop, f1, f2])).unwrap();
    assert_eq!(frames[0].image.palette, a.palette);
    assert_eq!(frames[1].image.palette, b.palette);
    assert_eq!(indices(&frames[1].image), indices(&b));
}

#[test]
fn props_scope_over_nested_groups_and_only_their_form_type() {
    let (a, a_bytes) = picture(6, 2, 3);
    let a_chunks = chunks_of(&a_bytes);
    let shared = pick(&a_chunks, &[b"BMHD", b"CMAP"]);
    let body = pick(&a_chunks, &[b"BODY"]);
    // The outer LIST's PROP ILBM reaches the FORM inside the nested CAT;
    // a PROP for another FORM type (8SVX) does not.
    let nested = group(b"CAT ", b"ILBM", &[form(b"ILBM", &body)]);
    let list = group(
        b"LIST",
        b"ILBM",
        &[
            chunk(
                b"PROP",
                &[b"8SVX".to_vec(), chunk(b"NAME", b"voice")].concat(),
            ),
            chunk(b"PROP", &[b"ILBM".to_vec(), shared].concat()),
            form(b"ILBM", &body),
            nested,
        ],
    );
    let frames = decode_all(&list).unwrap();
    assert_eq!(frames.len(), 2);
    for f in &frames {
        assert_eq!(indices(&f.image), indices(&a));
        assert!(f.image.chunk(b"NAME").is_none());
    }
}

#[test]
fn a_cat_without_props_is_unchanged() {
    let (a, a_bytes) = picture(5, 5, 1);
    let (b, b_bytes) = picture(3, 2, 2);
    let cat = group(b"CAT ", b"ILBM", &[a_bytes, b_bytes]);
    let frames = decode_all(&cat).unwrap();
    assert_eq!(indices(&frames[0].image), indices(&a));
    assert_eq!(indices(&frames[1].image), indices(&b));
}

#[test]
fn hostile_nesting_is_bounded() {
    // 64 nested LISTs around one picture: deeper than the walk follows,
    // so the group holds no reachable raster FORM — an error, no stack
    // exhaustion.
    let (_, a_bytes) = picture(2, 2, 0);
    let mut g = a_bytes;
    for _ in 0..64 {
        g = group(b"LIST", b"ILBM", &[g]);
    }
    assert!(decode_all(&g).is_err());
    // 8 levels are followed.
    let (a, a_bytes) = picture(2, 2, 0);
    let mut g = a_bytes;
    for _ in 0..8 {
        g = group(b"LIST", b"ILBM", &[g]);
    }
    assert_eq!(indices(&decode(&g).unwrap()), indices(&a));
}

#[test]
fn shared_property_amplification_is_bounded() {
    // One 64 KiB PROP and 2000 empty FORMs: copying the PROP into every
    // FORM would need ~128 MiB from an ~88 KiB file.
    let prop = chunk(
        b"PROP",
        &[b"ILBM".to_vec(), chunk(b"ANNO", &vec![b'x'; 65_536])].concat(),
    );
    let kids: Vec<Vec<u8>> = std::iter::once(prop)
        .chain(std::iter::repeat_n(form(b"ILBM", &[]), 2000))
        .collect();
    let list = group(b"LIST", b"ILBM", &kids);
    assert!(matches!(
        decode_all(&list),
        Err(oxideav_iff::Error::LimitExceeded(_))
    ));
}
