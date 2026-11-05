//! ANIM master colormap (anim.txt §1.1 / §1.3): "A master colormap should
//! be kept for the entire ANIM which would be initially set from the CMAP
//! chunk in the initial ILBM … If a CMAP chunk appears in one of the
//! frames, then this master colormap is updated and the new colormap
//! applies to all frames until the occurrance of another CMAP chunk."

use oxideav_iff::anim::parse_anim;
use oxideav_iff::{EncodeOptions, IffImage, IffPixelFormat, Palette, Plane, decode_all, encode};

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    if body.len() & 1 == 1 {
        v.push(0);
    }
    v
}

fn body_of(form: &[u8]) -> Vec<u8> {
    let mut pos = 12;
    while pos + 8 <= form.len() {
        let size = u32::from_be_bytes(form[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if &form[pos..pos + 4] == b"BODY" {
            return form[pos + 8..pos + 8 + size].to_vec();
        }
        pos += 8 + size + (size & 1);
    }
    panic!("no BODY")
}

/// An op-0 (full BODY) delta frame, optionally carrying a CMAP.
fn op0_frame(body: &[u8], cmap: Option<&[[u8; 3]]>) -> Vec<u8> {
    let mut anhd = [0u8; 40];
    anhd[2..4].copy_from_slice(&4u16.to_be_bytes());
    anhd[4..6].copy_from_slice(&2u16.to_be_bytes());
    anhd[14..18].copy_from_slice(&1u32.to_be_bytes());
    anhd[18] = 1;
    let mut inner = b"ILBM".to_vec();
    inner.extend(chunk(b"ANHD", &anhd));
    if let Some(c) = cmap {
        inner.extend(chunk(b"CMAP", &c.concat()));
    }
    inner.extend(chunk(b"BODY", body));
    chunk(b"FORM", &inner)
}

#[test]
fn a_delta_frame_cmap_replaces_the_master_colormap_from_then_on() {
    let first = [[10, 20, 30], [200, 0, 0], [0, 200, 0], [0, 0, 200]];
    let second = [[1, 2, 3], [4, 5, 6], [7, 8, 9], [250, 251, 252]];
    let idx: Vec<u8> = vec![0, 1, 2, 3, 3, 2, 1, 0];
    let img = IffImage::new(4, 2, IffPixelFormat::Pal8, vec![Plane::new(4, idx.clone())])
        .unwrap()
        .with_palette(Palette::from_rgb_triples(&first));
    let seed = encode(
        &img,
        &EncodeOptions::default().with_compression(oxideav_iff::ilbm::Compression::None),
    )
    .unwrap();
    let body = body_of(&seed);
    let anim = chunk(
        b"FORM",
        &[
            b"ANIM".to_vec(),
            seed,
            op0_frame(&body, None),
            op0_frame(&body, Some(&second)),
            op0_frame(&body, None),
        ]
        .concat(),
    );

    let frames = decode_all(&anim).unwrap();
    let pal = |i: usize| frames[i].image.palette.as_ref().unwrap().to_rgb_triples();
    assert_eq!(pal(0), first.to_vec());
    assert_eq!(pal(1), first.to_vec());
    assert_eq!(pal(2), second.to_vec());
    assert_eq!(pal(3), second.to_vec(), "the new colormap persists");
    for f in &frames {
        assert_eq!(f.image.planes[0].data, idx);
    }

    // The composited RGBA frames (what iff_anim emits) follow suit.
    let parsed = parse_anim(&anim).unwrap();
    let px = |i: usize| parsed.frames[i].rgba[4..7].to_vec(); // pixel 1 = index 1
    assert_eq!(px(1), vec![200, 0, 0]);
    assert_eq!(px(2), vec![4, 5, 6]);
    assert_eq!(px(3), vec![4, 5, 6]);
}
