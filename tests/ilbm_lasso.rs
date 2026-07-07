//! `mskLasso` (BMHD masking == 3) seed-fill transparency.
//!
//! ilbm.txt §BMHD: "The value mskLasso indicates the reader may construct
//! a mask by lassoing the image as in MacPaint. To do this, put a 1 pixel
//! border of transparentColor around the image rectangle. Then do a seed
//! fill from this border. Filled pixels are to be transparent."
//!
//! The distinguishing property versus `mskHasTransparentColor` (a plain
//! colour key) is that an *enclosed* pocket of the transparent colour —
//! one the border fill can't reach — stays opaque. These tests hand-build
//! a 5×5 one-bitplane ILBM with a ring of `transparentColor` around a
//! solid box that in turn encloses a single transparent-colour pixel.

use oxideav_iff::ilbm::parse_ilbm;

/// Append an IFF chunk (`id`, big-endian size, body, odd-length pad byte).
fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// 5×5, 1 bitplane. Index layout (0 = transparent colour):
/// ```text
///   0 0 0 0 0
///   0 1 1 1 0
///   0 1 0 1 0   <- centre (2,2) is an *enclosed* 0
///   0 1 1 1 0
///   0 0 0 0 0
/// ```
/// BODY rows are word-padded (row_bytes = 2), MSB-first.
fn build_ilbm(masking: u8) -> Vec<u8> {
    let mut bmhd = Vec::new();
    bmhd.extend_from_slice(&5u16.to_be_bytes()); // width
    bmhd.extend_from_slice(&5u16.to_be_bytes()); // height
    bmhd.extend_from_slice(&0i16.to_be_bytes()); // x
    bmhd.extend_from_slice(&0i16.to_be_bytes()); // y
    bmhd.push(1); // nPlanes
    bmhd.push(masking); // masking
    bmhd.push(0); // compression (none)
    bmhd.push(0); // pad
    bmhd.extend_from_slice(&0u16.to_be_bytes()); // transparentColor = 0
    bmhd.push(1); // xAspect
    bmhd.push(1); // yAspect
    bmhd.extend_from_slice(&5i16.to_be_bytes()); // pageWidth
    bmhd.extend_from_slice(&5i16.to_be_bytes()); // pageHeight

    // Two-entry palette: index 0 and index 1 have distinct colours.
    let cmap = [10u8, 20, 30, 200, 100, 50];

    // Planar BODY, 2 bytes/row, bit (7-x) of byte 0 set where index == 1.
    let rows: [u8; 5] = [
        0b0000_0000, // 0 0 0 0 0
        0b0111_0000, // 0 1 1 1 0
        0b0101_0000, // 0 1 0 1 0
        0b0111_0000, // 0 1 1 1 0
        0b0000_0000, // 0 0 0 0 0
    ];
    let mut body = Vec::new();
    for r in rows {
        body.push(r);
        body.push(0);
    }

    let mut inner = Vec::new();
    inner.extend_from_slice(b"ILBM");
    push_chunk(&mut inner, b"BMHD", &bmhd);
    push_chunk(&mut inner, b"CMAP", &cmap);
    push_chunk(&mut inner, b"BODY", &body);

    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    out.extend_from_slice(&inner);
    out
}

fn alpha_at(img: &oxideav_iff::ilbm::IlbmImage, x: u32, y: u32) -> u8 {
    let i = ((y * img.width + x) * 4 + 3) as usize;
    img.rgba[i]
}

#[test]
fn lasso_border_region_is_transparent() {
    let img = parse_ilbm(&build_ilbm(3)).unwrap();
    assert_eq!(img.width, 5);
    assert_eq!(img.height, 5);
    // Border ring of the transparent colour is reachable from the edge.
    for &(x, y) in &[(0u32, 0u32), (4, 0), (0, 4), (4, 4), (2, 0), (0, 2)] {
        assert_eq!(alpha_at(&img, x, y), 0x00, "border 0-pixel ({x},{y})");
    }
}

#[test]
fn lasso_box_pixels_stay_opaque() {
    let img = parse_ilbm(&build_ilbm(3)).unwrap();
    // Every index-1 pixel of the solid box is opaque.
    for &(x, y) in &[(1u32, 1u32), (2, 1), (3, 1), (1, 2), (3, 2), (1, 3), (3, 3)] {
        assert_eq!(alpha_at(&img, x, y), 0xFF, "box 1-pixel ({x},{y})");
    }
}

#[test]
fn lasso_enclosed_transparent_pixel_stays_opaque() {
    // The whole point of a lasso: the centre 0-pixel is enclosed by the
    // box and unreachable from the border, so it is NOT transparent.
    let img = parse_ilbm(&build_ilbm(3)).unwrap();
    assert_eq!(
        alpha_at(&img, 2, 2),
        0xFF,
        "enclosed transparent-colour pixel must stay opaque under lasso"
    );
    // Its colour is still the palette entry for index 0.
    let i = ((2 * img.width + 2) * 4) as usize;
    assert_eq!(&img.rgba[i..i + 3], &[10, 20, 30]);
}

/// Chunky `FORM PBM ` (8-bit-per-pixel) sibling of `build_ilbm`; same 5×5
/// index layout, BODY is one byte per pixel word-padded to `stride = 6`.
fn build_pbm(masking: u8) -> Vec<u8> {
    let mut bmhd = Vec::new();
    bmhd.extend_from_slice(&5u16.to_be_bytes()); // width
    bmhd.extend_from_slice(&5u16.to_be_bytes()); // height
    bmhd.extend_from_slice(&0i16.to_be_bytes());
    bmhd.extend_from_slice(&0i16.to_be_bytes());
    bmhd.push(8); // nPlanes — PBM is 8 bits/pixel
    bmhd.push(masking);
    bmhd.push(0); // compression none
    bmhd.push(0);
    bmhd.extend_from_slice(&0u16.to_be_bytes()); // transparentColor = 0
    bmhd.push(1);
    bmhd.push(1);
    bmhd.extend_from_slice(&5i16.to_be_bytes());
    bmhd.extend_from_slice(&5i16.to_be_bytes());

    let cmap = [10u8, 20, 30, 200, 100, 50];

    let idx: [[u8; 5]; 5] = [
        [0, 0, 0, 0, 0],
        [0, 1, 1, 1, 0],
        [0, 1, 0, 1, 0],
        [0, 1, 1, 1, 0],
        [0, 0, 0, 0, 0],
    ];
    let mut body = Vec::new();
    for row in idx {
        for v in row {
            body.push(v);
        }
        body.push(0); // stride padding to even (6 bytes/row)
    }

    let mut inner = Vec::new();
    inner.extend_from_slice(b"PBM ");
    push_chunk(&mut inner, b"BMHD", &bmhd);
    push_chunk(&mut inner, b"CMAP", &cmap);
    push_chunk(&mut inner, b"BODY", &body);

    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    out.extend_from_slice(&inner);
    out
}

#[test]
fn pbm_lasso_matches_planar_semantics() {
    let img = parse_ilbm(&build_pbm(3)).unwrap();
    assert_eq!(&img.form_type, b"PBM ");
    // Border 0-pixels transparent, box opaque, enclosed centre opaque.
    assert_eq!(alpha_at(&img, 0, 0), 0x00);
    assert_eq!(alpha_at(&img, 1, 1), 0xFF);
    assert_eq!(
        alpha_at(&img, 2, 2),
        0xFF,
        "enclosed pixel opaque under lasso"
    );
    // Colour key variant makes the enclosed pixel transparent.
    let keyed = parse_ilbm(&build_pbm(2)).unwrap();
    assert_eq!(alpha_at(&keyed, 2, 2), 0x00);
}

#[test]
fn plain_transparent_color_keys_the_enclosed_pixel() {
    // Contrast: with mskHasTransparentColor (== 2) the enclosed pixel IS
    // keyed transparent, since there is no connectivity constraint.
    let img = parse_ilbm(&build_ilbm(2)).unwrap();
    assert_eq!(
        alpha_at(&img, 2, 2),
        0x00,
        "colour key makes every transparent-colour pixel transparent"
    );
}
