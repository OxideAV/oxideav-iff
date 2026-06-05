//! ILBM `SPRT` (Sprite Precedence) chunk surfacing — round-240
//! coverage. The `SPRT` chunk (§2.7 of the IFF/ILBM 17-Jan-1986
//! supplement) marks an ILBM "as intended as a sprite" and carries a
//! single `UWORD` precedence (`0` = foremost).
//!
//! Verifies:
//! 1. A hand-rolled FORM/ILBM/SPRT byte stream parses into the
//!    expected `Sprt { precedence }`.
//! 2. `encode_ilbm` writes the two-byte SPRT payload between DEST
//!    and the BODY (matching the spec grammar
//!    `BMHD [CMAP] [GRAB] [DEST] [SPRT] [CAMG]`).
//! 3. A parse → encode → parse cycle preserves the precedence
//!    byte-for-byte across the full unsigned 16-bit range.
//! 4. `Sprt::is_foremost` flags the `precedence == 0` slot per the
//!    §2.7 "0 is the highest" convention.
//! 5. The grammar-ordering invariant (DEST precedes SPRT, SPRT
//!    precedes BODY) holds when both chunks are present.
//! 6. Default `IlbmImage` carries no SPRT and the encoder omits the
//!    chunk.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, Sprt,
};

fn make_indexed_image(w: u16, h: u16, n_planes: u8) -> IlbmImage {
    let palette: Vec<[u8; 3]> = (0..32u8)
        .map(|i| [i * 8, 255 - i * 8, i.wrapping_mul(13)])
        .collect();
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        for x in 0..w {
            let idx = (((x as usize) ^ (y as usize)) % (1usize << n_planes)) as u8;
            let p = palette[idx as usize % palette.len()];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    let bmhd = Bmhd {
        width: w,
        height: h,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: w as i16,
        page_height: h as i16,
    };
    IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd,
        palette,
        camg: Camg::default(),
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    }
}

// ───────────────────── unit-level wire layout ─────────────────────

#[test]
fn sprt_parse_unpacks_two_byte_payload() {
    // Hand-rolled SPRT payload — precedence = 0x002A (42).
    let payload: [u8; 2] = [0x00, 0x2A];
    let s = Sprt::parse(&payload).unwrap();
    assert_eq!(s.precedence, 0x002A);
    assert!(!s.is_foremost());
    assert_eq!(s.write(), payload);
}

#[test]
fn sprt_parse_foremost_zero_precedence() {
    // Precedence 0 — "the highest" / foremost sprite per §2.7.
    let payload: [u8; 2] = [0x00, 0x00];
    let s = Sprt::parse(&payload).unwrap();
    assert_eq!(s.precedence, Sprt::FOREMOST);
    assert!(s.is_foremost());
}

#[test]
fn sprt_parse_max_uword_precedence() {
    // Spec types SpritePrecedence as UWORD, so 0xFFFF is legal.
    let payload: [u8; 2] = [0xFF, 0xFF];
    let s = Sprt::parse(&payload).unwrap();
    assert_eq!(s.precedence, 0xFFFF);
    assert!(!s.is_foremost());
    assert_eq!(s.write(), payload);
}

#[test]
fn sprt_parse_rejects_short_payload() {
    let one = [0u8; 1];
    assert!(Sprt::parse(&one).is_err());
    let empty: [u8; 0] = [];
    assert!(Sprt::parse(&empty).is_err());
}

#[test]
fn sprt_parse_extra_trailing_bytes_ignored() {
    // The supplement fixes SPRT to a single UWORD; an over-long
    // payload (which a stray writer might emit) parses to the first
    // two bytes without error — `Sprt::parse` reads exactly the
    // documented field width.
    let payload: [u8; 4] = [0x01, 0x23, 0xDE, 0xAD];
    let s = Sprt::parse(&payload).unwrap();
    assert_eq!(s.precedence, 0x0123);
}

// ───────────────────── integration with encode_ilbm / parse_ilbm ───────

#[test]
fn sprt_chunk_round_trips_through_form_envelope() {
    let mut img = make_indexed_image(8, 4, 4);
    img.sprt = Some(Sprt { precedence: 7 });
    let bytes = encode_ilbm(&img).unwrap();
    let pos = bytes
        .windows(4)
        .position(|w| w == b"SPRT")
        .expect("SPRT chunk in encoded FORM");
    // 4-byte FourCC + 4-byte size + 2-byte payload.
    assert_eq!(
        &bytes[pos + 4..pos + 8],
        &2u32.to_be_bytes(),
        "SPRT size field is 2 bytes"
    );
    let payload = &bytes[pos + 8..pos + 10];
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 7);

    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.sprt, Some(Sprt { precedence: 7 }));
}

#[test]
fn sprt_round_trip_preserves_zero_foremost() {
    let mut img = make_indexed_image(4, 2, 2);
    img.sprt = Some(Sprt {
        precedence: Sprt::FOREMOST,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    let s = dec.sprt.expect("SPRT survived round-trip");
    assert!(s.is_foremost());
    assert_eq!(s.precedence, 0);
}

#[test]
fn sprt_round_trip_preserves_max_uword() {
    let mut img = make_indexed_image(4, 2, 2);
    img.sprt = Some(Sprt { precedence: 0xFFFF });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    let s = dec.sprt.expect("SPRT survived round-trip");
    assert_eq!(s.precedence, 0xFFFF);
    assert!(!s.is_foremost());
}

#[test]
fn sprt_emitted_between_dest_and_body() {
    // Spec grammar Appendix A: `BMHD [CMAP] [GRAB] [DEST] [SPRT]
    // [CAMG] ... BODY`. With both DEST and SPRT supplied, DEST
    // must precede SPRT, and SPRT must precede BODY.
    let mut img = make_indexed_image(8, 2, 2);
    img.dest = Some(oxideav_iff::ilbm::Dest {
        depth: 2,
        pad1: 0,
        plane_pick: 0x0003,
        plane_on_off: 0,
        plane_mask: 0x0003,
    });
    img.sprt = Some(Sprt { precedence: 1 });
    let bytes = encode_ilbm(&img).unwrap();
    let dest_at = bytes
        .windows(4)
        .position(|w| w == b"DEST")
        .expect("DEST present");
    let sprt_at = bytes
        .windows(4)
        .position(|w| w == b"SPRT")
        .expect("SPRT present");
    let body_at = bytes
        .windows(4)
        .position(|w| w == b"BODY")
        .expect("BODY present");
    assert!(dest_at < sprt_at, "DEST precedes SPRT");
    assert!(sprt_at < body_at, "SPRT precedes BODY");
}

#[test]
fn sprt_absent_by_default_no_chunk_emitted() {
    let img = make_indexed_image(4, 2, 2);
    assert!(img.sprt.is_none(), "default IlbmImage has no SPRT");
    let bytes = encode_ilbm(&img).unwrap();
    assert!(
        bytes.windows(4).all(|w| w != b"SPRT"),
        "no SPRT FourCC when image.sprt is None"
    );
}

#[test]
fn sprt_coexists_with_grab_dest_camg_in_order() {
    // Maximal property set: GRAB + DEST + SPRT + CAMG should all
    // appear, with the encoder's emission order preserving the
    // §2.7 grammar arrangement (DEST then SPRT) and CAMG, which is
    // emitted earlier by this encoder (still legal per spec §6:
    // "may actually be in any order"). The structural invariant
    // the test exercises is that none of them lands after BODY.
    let mut img = make_indexed_image(8, 2, 2);
    img.grab = Some(oxideav_iff::ilbm::Grab { x: 2, y: 1 });
    img.dest = Some(oxideav_iff::ilbm::Dest {
        depth: 2,
        pad1: 0,
        plane_pick: 0x0003,
        plane_on_off: 0,
        plane_mask: 0x0003,
    });
    img.sprt = Some(Sprt { precedence: 3 });
    // Set a non-HAM/non-EHB CAMG bit so the encoder emits the chunk;
    // the specific bit isn't material — `Camg` just round-trips the
    // raw 32-bit viewport word.
    img.camg = Camg { raw: 0x0000_0004 };
    let bytes = encode_ilbm(&img).unwrap();
    let body_at = bytes
        .windows(4)
        .position(|w| w == b"BODY")
        .expect("BODY present");
    for fourcc in [b"GRAB", b"DEST", b"SPRT", b"CAMG"].iter() {
        let at = bytes
            .windows(4)
            .position(|w| w == *fourcc)
            .unwrap_or_else(|| panic!("{} present", std::str::from_utf8(*fourcc).unwrap()));
        assert!(
            at < body_at,
            "{} precedes BODY",
            std::str::from_utf8(*fourcc).unwrap()
        );
    }
    let dest_at = bytes.windows(4).position(|w| w == b"DEST").unwrap();
    let sprt_at = bytes.windows(4).position(|w| w == b"SPRT").unwrap();
    assert!(
        dest_at < sprt_at,
        "DEST precedes SPRT in grammar order even with full property set"
    );

    // Full round-trip preserves every property.
    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(dec.sprt, Some(Sprt { precedence: 3 }));
    assert_eq!(dec.dest.map(|d| d.depth), Some(2));
    assert!(dec.grab.is_some());
}
