//! ILBM `DEST` (destination-merge) chunk surfacing — round-234
//! coverage. The `DEST` chunk (§2.6 of the IFF/ILBM 17-Jan-1986 doc)
//! declares how an ILBM with `depth_source = BMHD.nPlanes` bitplanes
//! should fan out into a deeper destination bitmap.
//!
//! Verifies:
//! 1. A hand-rolled FORM/ILBM/DEST byte stream parses into the
//!    expected `Dest { depth, plane_pick, plane_on_off, plane_mask }`.
//! 2. `encode_ilbm` writes the eight-byte DEST payload after GRAB
//!    (matching the spec's `BMHD [CMAP] [GRAB] [DEST] [SPRT] [CAMG]`
//!    grammar order).
//! 3. A parse → encode → parse cycle preserves every field byte-for-byte.
//! 4. `Dest::pick_count_matches_depth` flags inconsistent on-disk
//!    inputs without rejecting them at parse time (the spec frames
//!    the equality as an expectation, not a requirement).

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, Dest, IlbmImage, Masking,
};

fn make_indexed_image(w: u16, h: u16, n_planes: u8) -> IlbmImage {
    // Palette big enough for any n_planes ≤ 5 we exercise here.
    let palette: Vec<[u8; 3]> = (0..32u8)
        .map(|i| [i * 8, 255 - i * 8, (i.wrapping_mul(13))])
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
fn dest_parse_unpacks_eight_byte_payload() {
    // Hand-rolled DEST payload: depth=5, pad=0,
    // planePick   = 0b00000000_00011111 (low 5 bits set: source → planes 0..4)
    // planeOnOff  = 0b00000000_00100000 (bit 5 is the broadcast-1 default)
    // planeMask   = 0b00000000_00111111 (low 6 bits: write planes 0..5)
    let payload: [u8; 8] = [0x05, 0x00, 0x00, 0x1F, 0x00, 0x20, 0x00, 0x3F];
    let d = Dest::parse(&payload).unwrap();
    assert_eq!(d.depth, 5);
    assert_eq!(d.pad1, 0);
    assert_eq!(d.plane_pick, 0x001F);
    assert_eq!(d.plane_on_off, 0x0020);
    assert_eq!(d.plane_mask, 0x003F);
    assert!(d.pick_count_matches_depth());
    assert_eq!(d.write(), payload);
}

#[test]
fn dest_parse_rejects_short_payload() {
    let short = [0u8; 7];
    assert!(Dest::parse(&short).is_err());
}

#[test]
fn dest_pick_count_flag_catches_mismatch() {
    // depth claims 5 source planes but planePick has only 4 bits set
    // in its low 5 positions — a synthetic / malformed encoder.
    let d = Dest {
        depth: 5,
        pad1: 0,
        plane_pick: 0b0000_0000_0000_1111, // 4 bits, not 5
        plane_on_off: 0,
        plane_mask: 0x001F,
    };
    assert!(!d.pick_count_matches_depth());
}

#[test]
fn dest_pad_byte_round_trips_verbatim() {
    // Some encoders write non-zero into `pad1`; round-trip should keep
    // whatever byte was there (spec says "ignored on read").
    let payload: [u8; 8] = [0x04, 0xAB, 0x00, 0x0F, 0x00, 0x00, 0x00, 0x0F];
    let d = Dest::parse(&payload).unwrap();
    assert_eq!(d.pad1, 0xAB);
    assert_eq!(d.write(), payload);
}

// ───────────────────── integration with encode_ilbm / parse_ilbm ───────

#[test]
fn dest_chunk_round_trips_through_form_envelope() {
    let mut img = make_indexed_image(8, 4, 4);
    img.dest = Some(Dest {
        depth: 4,
        pad1: 0,
        plane_pick: 0x000F,   // low 4 bits set: source planes feed dest 0..3
        plane_on_off: 0x0010, // dest plane 4 forced to 1
        plane_mask: 0x001F,   // write dest planes 0..4
    });
    let bytes = encode_ilbm(&img).unwrap();
    // The DEST FourCC must appear in the encoded stream.
    let pos = bytes
        .windows(4)
        .position(|w| w == b"DEST")
        .expect("DEST chunk in encoded FORM");
    // 4-byte FourCC + 4-byte size + 8-byte payload.
    assert_eq!(
        &bytes[pos + 4..pos + 8],
        &8u32.to_be_bytes(),
        "DEST size field is 8 bytes"
    );
    let payload = &bytes[pos + 8..pos + 16];
    assert_eq!(payload[0], 4, "depth");
    assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 0x000F);
    assert_eq!(u16::from_be_bytes([payload[4], payload[5]]), 0x0010);
    assert_eq!(u16::from_be_bytes([payload[6], payload[7]]), 0x001F);

    let dec = parse_ilbm(&bytes).unwrap();
    assert_eq!(
        dec.dest,
        Some(Dest {
            depth: 4,
            pad1: 0,
            plane_pick: 0x000F,
            plane_on_off: 0x0010,
            plane_mask: 0x001F,
        })
    );
}

#[test]
fn dest_emitted_between_grab_and_body() {
    // Spec grammar §6 lists property order as BMHD [CMAP] [GRAB]
    // [DEST] [SPRT] [CAMG] ... BODY. The encoder pulls DEST out
    // right after GRAB; verify by FourCC offsets.
    let mut img = make_indexed_image(8, 2, 2);
    img.grab = Some(oxideav_iff::ilbm::Grab { x: 3, y: 1 });
    img.dest = Some(Dest {
        depth: 2,
        pad1: 0,
        plane_pick: 0x0003,
        plane_on_off: 0,
        plane_mask: 0x0003,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let grab_at = bytes
        .windows(4)
        .position(|w| w == b"GRAB")
        .expect("GRAB present");
    let dest_at = bytes
        .windows(4)
        .position(|w| w == b"DEST")
        .expect("DEST present");
    let body_at = bytes
        .windows(4)
        .position(|w| w == b"BODY")
        .expect("BODY present");
    assert!(grab_at < dest_at, "GRAB precedes DEST");
    assert!(dest_at < body_at, "DEST precedes BODY");
}

#[test]
fn dest_absent_by_default_no_chunk_emitted() {
    let img = make_indexed_image(4, 2, 2);
    assert!(img.dest.is_none(), "default IlbmImage has no DEST");
    let bytes = encode_ilbm(&img).unwrap();
    assert!(
        bytes.windows(4).all(|w| w != b"DEST"),
        "no DEST FourCC when image.dest is None"
    );
}

#[test]
fn dest_default_implicit_equals_identity_mapping() {
    // ILBM §2.6 wording: with no DEST, planePick = planeMask =
    // (1 << nPlanes) - 1 and planeOnOff = 0. Build a Dest matching
    // that implicit default for n_planes = 3 and confirm a
    // round-trip preserves the identity-mapping semantics.
    let mut img = make_indexed_image(4, 2, 3);
    let n = img.bmhd.n_planes as u32;
    let mask = (1u16 << n) - 1;
    img.dest = Some(Dest {
        depth: img.bmhd.n_planes,
        pad1: 0,
        plane_pick: mask,
        plane_on_off: 0,
        plane_mask: mask,
    });
    let bytes = encode_ilbm(&img).unwrap();
    let dec = parse_ilbm(&bytes).unwrap();
    let d = dec.dest.expect("DEST survived round-trip");
    assert!(d.pick_count_matches_depth());
    assert_eq!(d.plane_pick, mask);
    assert_eq!(d.plane_mask, mask);
    assert_eq!(d.plane_on_off, 0);
}
