//! HAM6-vs-EHB when `CAMG` is missing or unusable.
//!
//! The platform vendor's ViewMode addendum states the default twice
//! in the same words: "If no CAMG chunk is present, and image is 6
//! planes deep, assume HAM and you'll probably be right." Reader
//! policy pinned here: consider only the old ViewMode HAM/EHB bits;
//! exactly one set → honour it; both set (not a legal display mode —
//! the signature of a garbage longword), junk-only values, or an
//! absent chunk → depth default (6 planes → HAM6, anything else →
//! plain indexed). The CMAP-entry-count heuristic is subordinate: it
//! only *flags* ambiguity, never overrides the vendor default.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, FormatResolution, IlbmImage, InferredFormat,
    Masking, CAMG_EHB, CAMG_GENLOCK_VIDEO, CAMG_HAM, CAMG_VP_HIDE,
};

fn bmhd(w: u16, h: u16, planes: u8) -> Bmhd {
    Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes: planes,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    }
}

/// Remove the 12-byte CAMG chunk from an encoded FORM.
fn strip_camg(mut form: Vec<u8>) -> Vec<u8> {
    let at = form
        .windows(4)
        .position(|w| w == b"CAMG")
        .expect("form has a CAMG chunk");
    form.drain(at..at + 12);
    let new_size = (form.len() - 8) as u32;
    form[4..8].copy_from_slice(&new_size.to_be_bytes());
    form
}

/// Overwrite the CAMG chunk's longword in an encoded FORM.
fn replace_camg(mut form: Vec<u8>, raw: u32) -> Vec<u8> {
    let at = form
        .windows(4)
        .position(|w| w == b"CAMG")
        .expect("form has a CAMG chunk");
    form[at + 8..at + 12].copy_from_slice(&raw.to_be_bytes());
    form
}

/// A 16×2 HAM6 test image (16-grey CMAP, per-pixel HAM ops).
fn ham6_image() -> IlbmImage {
    let palette: Vec<[u8; 3]> = (0..16u8).map(|i| [i * 17, i * 17, i * 17]).collect();
    let mut rgba = Vec::with_capacity(16 * 2 * 4);
    for _y in 0..2 {
        for x in 0..16u8 {
            rgba.extend_from_slice(&[x * 17, x * 17, x * 17, 0xFF]);
        }
    }
    IlbmImage {
        width: 16,
        height: 2,
        bmhd: bmhd(16, 2, 6),
        palette,
        camg: Camg { raw: CAMG_HAM },
        rgba,
        ..IlbmImage::default()
    }
}

#[test]
fn six_planes_without_camg_assume_ham6() {
    let form = encode_ilbm(&ham6_image()).unwrap();
    let reference = parse_ilbm(&form).unwrap();
    let inferred = parse_ilbm(&strip_camg(form)).unwrap();
    assert_eq!(
        inferred.rgba, reference.rgba,
        "the CAMG-less decode must equal the explicit HAM6 decode"
    );
    assert_eq!(
        inferred.format_resolution(),
        FormatResolution {
            format: InferredFormat::Ham,
            inferred: true,
            ambiguous_ehb: false,
        }
    );
}

#[test]
fn six_planes_with_junk_only_camg_assume_ham6() {
    // Pre-2.0 writers stored uninitialised longwords; a value with
    // only non-format bits set is unusable, same as absent.
    let form = encode_ilbm(&ham6_image()).unwrap();
    let reference = parse_ilbm(&form).unwrap();
    let junk = parse_ilbm(&replace_camg(form, CAMG_GENLOCK_VIDEO | CAMG_VP_HIDE)).unwrap();
    assert_eq!(junk.rgba, reference.rgba);
    assert!(junk.format_resolution().inferred);
    assert_eq!(junk.format_resolution().format, InferredFormat::Ham);
}

#[test]
fn ham_and_ehb_both_set_is_garbage_and_falls_to_the_depth_default() {
    // Both set is not a legal display mode. On 6 planes the depth
    // default is HAM6 — previously such a file was EHB-expanded *and*
    // HAM-rendered at once.
    let form = encode_ilbm(&ham6_image()).unwrap();
    let reference = parse_ilbm(&form).unwrap();
    let both = parse_ilbm(&replace_camg(form, CAMG_HAM | CAMG_EHB)).unwrap();
    assert_eq!(both.rgba, reference.rgba);
    assert!(both.format_resolution().inferred);
}

#[test]
fn five_planes_without_camg_stay_indexed() {
    let palette: Vec<[u8; 3]> = (0..32u8).map(|i| [i * 8, 0, 255 - i * 8]).collect();
    let mut rgba = Vec::with_capacity(8 * 4 * 4);
    for y in 0..4usize {
        for x in 0..8usize {
            let p = palette[(y * 8 + x) % 32];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let img = IlbmImage {
        width: 8,
        height: 4,
        bmhd: bmhd(8, 4, 5),
        palette: palette.clone(),
        rgba: rgba.clone(),
        ..IlbmImage::default()
    };
    let decoded = parse_ilbm(&encode_ilbm(&img).unwrap()).unwrap();
    assert_eq!(decoded.rgba, rgba, "plain indexed round-trip");
    let res = decoded.format_resolution();
    assert_eq!(res.format, InferredFormat::Indexed);
    assert!(res.inferred, "no CAMG: the indexed default is an inference");
}

#[test]
fn eight_planes_without_camg_stay_indexed_not_ham8() {
    // The vendor default names depth 6 only; 7/8 planes are a deep
    // planar image, not a modified mode.
    let palette: Vec<[u8; 3]> = (0..=255u8).map(|i| [i, i, i]).collect();
    let mut rgba = Vec::with_capacity(16 * 2 * 4);
    for y in 0..2usize {
        for x in 0..16usize {
            let g = (y * 16 + x) as u8 * 8;
            rgba.extend_from_slice(&[g, g, g, 0xFF]);
        }
    }
    let img = IlbmImage {
        width: 16,
        height: 2,
        bmhd: bmhd(16, 2, 8),
        palette,
        rgba: rgba.clone(),
        ..IlbmImage::default()
    };
    let decoded = parse_ilbm(&encode_ilbm(&img).unwrap()).unwrap();
    assert_eq!(decoded.rgba, rgba);
    assert_eq!(decoded.format_resolution().format, InferredFormat::Indexed);
}

#[test]
fn cmap_count_heuristic_only_flags_ambiguity() {
    // Depth 6, unusable CAMG: >16 CMAP entries → still HAM (vendor
    // default), but flagged so a caller can offer an EHB override.
    let ambiguous = Camg::default().resolve_planar_format(6, 32);
    assert_eq!(ambiguous.format, InferredFormat::Ham);
    assert!(ambiguous.inferred && ambiguous.ambiguous_ehb);

    // ≤16 entries agrees with the default — strong evidence, no flag.
    let clear = Camg::default().resolve_planar_format(6, 16);
    assert_eq!(clear.format, InferredFormat::Ham);
    assert!(clear.inferred && !clear.ambiguous_ehb);

    // An explicit EHB flag is honoured and never ambiguous.
    let ehb = Camg { raw: CAMG_EHB }.resolve_planar_format(6, 32);
    assert_eq!(
        ehb,
        FormatResolution {
            format: InferredFormat::Ehb,
            inferred: false,
            ambiguous_ehb: false,
        }
    );

    // An explicit HAM flag is honoured as stated, not inferred.
    let ham = Camg { raw: CAMG_HAM }.resolve_planar_format(6, 16);
    assert!(!ham.inferred);
    assert_eq!(ham.format, InferredFormat::Ham);
}
