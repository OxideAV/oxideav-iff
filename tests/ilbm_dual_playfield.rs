//! Dual-playfield compositing (`CAMG` `DUALPF` 0x0400 / `PFBA` 0x0040).
//!
//! Source: `docs/image/iff/ahrm-dual-playfield.txt` (Amiga Hardware
//! Reference Manual ch. 3, "Forming a Dual-playfield Display") and its
//! `CAMG` mapping in `docs/image/iff/iff-chunk-gaps-2026-09.md` §7:
//! hardware planes 1/3/5 (BODY planes 0/2/4) form playfield 1, planes
//! 2/4/6 (BODY 1/3/5) playfield 2; value 000 is transparent; playfield 1
//! shows COLOR1-7, playfield 2 COLOR9-15; playfield 1 is in front unless
//! PFBA; where both are transparent the background COLOR0 shows.
//!
//! The decode fixtures are assembled chunk by chunk from raw planar
//! values, so the expected registers below come from the manual's tables,
//! not from the crate's encoder.

use oxideav_iff::ilbm::{CAMG_DUALPF, CAMG_EHB, CAMG_HAM, CAMG_HIRES, CAMG_PFBA, Camg};
use oxideav_iff::ilbm::{Masking, indices_to_planar_row};
use oxideav_iff::{
    AnimOp, EncodeOptions, Error, Frame, IffForm, IffImage, IffPixelFormat, Palette, Plane, decode,
    decode_all, decode_rgba8, encode, encode_all, info,
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

/// A 16-entry CMAP whose entries are all distinct.
fn cmap16() -> Vec<[u8; 3]> {
    (0..16u8)
        .map(|i| [i * 16, 255 - i * 8, i * 5 + 3])
        .collect()
}

/// `FORM ILBM`, uncompressed, one row per entry of `rows` (raw planar
/// pixel values: BODY plane `p` = bit `p`).
fn ilbm(rows: &[Vec<u8>], n_planes: u8, camg: Option<u32>, masking: u8, key: u16) -> Vec<u8> {
    let w = rows[0].len() as u16;
    let h = rows.len() as u16;
    let row_bytes = (usize::from(w)).div_ceil(16) * 2;
    let mut bmhd = Vec::new();
    bmhd.extend_from_slice(&w.to_be_bytes());
    bmhd.extend_from_slice(&h.to_be_bytes());
    bmhd.extend_from_slice(&[0, 0, 0, 0]);
    bmhd.extend_from_slice(&[n_planes, masking, 0, 0]);
    bmhd.extend_from_slice(&key.to_be_bytes());
    bmhd.extend_from_slice(&[1, 1]);
    bmhd.extend_from_slice(&(w as i16).to_be_bytes());
    bmhd.extend_from_slice(&(h as i16).to_be_bytes());
    let cmap: Vec<u8> = cmap16().concat();
    let mut body = Vec::new();
    for r in rows {
        for plane in indices_to_planar_row(r, n_planes, row_bytes) {
            body.extend_from_slice(&plane);
        }
    }
    let mut form = b"ILBM".to_vec();
    form.extend(chunk(b"BMHD", &bmhd));
    form.extend(chunk(b"CMAP", &cmap));
    if let Some(c) = camg {
        form.extend(chunk(b"CAMG", &c.to_be_bytes()));
    }
    form.extend(chunk(b"BODY", &body));
    chunk(b"FORM", &form)
}

/// The register the manual's tables give for a raw value, written out
/// bit by bit (independent of the crate's helper).
fn manual_register(raw: u8, pf2_front: bool) -> u8 {
    let bit = |p: u8| (raw >> p) & 1;
    // Playfield 1: planes 1, 3, 5 (BODY 0, 2, 4), plane 5 most significant.
    let pf1 = bit(0) | (bit(2) << 1) | (bit(4) << 2);
    // Playfield 2: planes 2, 4, 6 (BODY 1, 3, 5), plane 6 most significant.
    let pf2 = bit(1) | (bit(3) << 1) | (bit(5) << 2);
    let c1 = (pf1 != 0).then_some(pf1);
    let c2 = (pf2 != 0).then_some(8 + pf2);
    let (front, back) = if pf2_front { (c2, c1) } else { (c1, c2) };
    front.or(back).unwrap_or(0)
}

fn all_raw_values() -> Vec<Vec<u8>> {
    (0..4u8)
        .map(|y| (0..16u8).map(|x| y * 16 + x).collect())
        .collect()
}

fn indices(img: &IffImage) -> Vec<u8> {
    assert_eq!(img.format, IffPixelFormat::Pal8);
    img.planes[0].data.clone()
}

#[test]
fn six_planes_playfield1_in_front_follows_the_manual_tables() {
    let rows = all_raw_values();
    let img = decode(&ilbm(&rows, 6, Some(CAMG_DUALPF), 0, 0)).unwrap();
    let want: Vec<u8> = rows
        .concat()
        .iter()
        .map(|&r| manual_register(r, false))
        .collect();
    assert_eq!(indices(&img), want);
    // Spot checks straight from Tables 3-10 / 3-11.
    assert_eq!(want[0b000000], 0); // both transparent: background
    assert_eq!(want[0b000001], 1); // PF1 001 -> COLOR1
    assert_eq!(want[0b010101], 7); // PF1 111 -> COLOR7
    assert_eq!(want[0b000010], 9); // PF2 001 -> COLOR9
    assert_eq!(want[0b101010], 15); // PF2 111 -> COLOR15
    assert_eq!(want[0b000011], 1); // both opaque: PF1 in front
}

#[test]
fn pfba_puts_playfield2_in_front() {
    let rows = all_raw_values();
    let img = decode(&ilbm(&rows, 6, Some(CAMG_DUALPF | CAMG_PFBA), 0, 0)).unwrap();
    let want: Vec<u8> = rows
        .concat()
        .iter()
        .map(|&r| manual_register(r, true))
        .collect();
    assert_eq!(indices(&img), want);
    assert_eq!(want[0b000011], 9); // both opaque: PF2 in front
    assert_eq!(want[0b000001], 1); // PF2 transparent: PF1 shows through
}

#[test]
fn colour_register_8_is_never_displayed() {
    for camg in [CAMG_DUALPF, CAMG_DUALPF | CAMG_PFBA] {
        let img = decode(&ilbm(&all_raw_values(), 6, Some(camg), 0, 0)).unwrap();
        assert!(!indices(&img).contains(&8));
        assert!(indices(&img).iter().all(|&i| i < 16));
    }
}

#[test]
fn hires_two_planes_per_playfield_use_the_two_bit_tables() {
    // Table 3-12: 2-bit values -> COLOR0-3 / COLOR8-11.
    let rows: Vec<Vec<u8>> = vec![(0..16u8).collect()];
    let img = decode(&ilbm(&rows, 4, Some(CAMG_DUALPF | CAMG_HIRES), 0, 0)).unwrap();
    let got = indices(&img);
    let want: Vec<u8> = (0..16u8).map(|r| manual_register(r, false)).collect();
    assert_eq!(got, want);
    assert!(got.iter().all(|&i| i <= 3 || (9..=11).contains(&i)));
}

#[test]
fn odd_plane_counts_hand_planes_out_alternately() {
    // 3 planes: PF1 gets planes 1 and 3 (BODY 0, 2), PF2 plane 2 (BODY 1).
    let rows: Vec<Vec<u8>> = vec![(0..8u8).collect()];
    let img = decode(&ilbm(&rows, 3, Some(CAMG_DUALPF), 0, 0)).unwrap();
    // raw 6 = BODY planes 1 + 2: PF2 001 behind PF1 010 -> COLOR2.
    assert_eq!(indices(&img), vec![0, 1, 9, 1, 2, 3, 2, 3]);
    // 1 plane: playfield 1 only.
    let img = decode(&ilbm(&[vec![0, 1, 1, 0]], 1, Some(CAMG_DUALPF), 0, 0)).unwrap();
    assert_eq!(indices(&img), vec![0, 1, 1, 0]);
}

#[test]
fn rgba_render_is_the_cmap_colour_of_the_composited_register() {
    let rows = all_raw_values();
    let bytes = ilbm(&rows, 6, Some(CAMG_DUALPF | CAMG_PFBA), 0, 0);
    let rgba = decode_rgba8(&bytes).unwrap();
    let pal = cmap16();
    for (i, &r) in rows.concat().iter().enumerate() {
        let c = pal[usize::from(manual_register(r, true))];
        assert_eq!(&rgba.data[i * 4..i * 4 + 4], &[c[0], c[1], c[2], 255]);
    }
}

#[test]
fn transparent_colour_keys_the_displayed_register() {
    // Key = COLOR9: every pixel showing PF2 001 (in front, PFBA) is
    // transparent, whatever its playfield 1 value.
    let rows = vec![vec![0b000010, 0b000011, 0b000001, 0b000000]];
    let img = decode(&ilbm(&rows, 6, Some(CAMG_DUALPF | CAMG_PFBA), 2, 9)).unwrap();
    assert_eq!(indices(&img), vec![9, 9, 1, 0]);
    let pal = img.palette.as_ref().unwrap();
    assert!(pal.is_transparent(9));
    assert!(!pal.is_transparent(1));
}

#[test]
fn single_playfield_and_unsupported_combinations_keep_plain_indices() {
    let rows = all_raw_values();
    let plain: Vec<u8> = rows.concat();
    // No DUALPF: the raw value is the register.
    assert_eq!(
        indices(&decode(&ilbm(&rows, 6, Some(CAMG_PFBA | CAMG_HIRES), 0, 0)).unwrap()),
        plain
    );
    // EHB + DUALPF: EHB wins (the manual defines no combination).
    let ehb = decode(&ilbm(&rows, 6, Some(CAMG_DUALPF | CAMG_EHB), 0, 0)).unwrap();
    assert_eq!(indices(&ehb), plain);
    // HAM + DUALPF stays HAM (RGB output).
    let ham = decode(&ilbm(&rows, 6, Some(CAMG_DUALPF | CAMG_HAM), 0, 0)).unwrap();
    assert_eq!(ham.format, IffPixelFormat::Rgb24);
    // 8 planes: the AGA per-playfield banks are not in the staged
    // manual — single-playfield reading.
    let deep: Vec<Vec<u8>> = vec![(0..=255u8).collect()];
    assert_eq!(
        indices(&decode(&ilbm(&deep, 8, Some(CAMG_DUALPF), 0, 0)).unwrap()),
        deep.concat()
    );
}

#[test]
fn info_reports_the_mode() {
    let bytes = ilbm(&all_raw_values(), 6, Some(CAMG_DUALPF | CAMG_PFBA), 0, 0);
    let i = info(&bytes).unwrap();
    assert_eq!(i.format, IffPixelFormat::Pal8);
    let camg = Camg {
        raw: decode(&bytes).unwrap().viewmode.unwrap(),
    };
    assert!(camg.is_dualpf() && camg.is_pfba());
}

// ───────────────────────── encode ─────────────────────────

fn pal8(w: u32, regs: &[u8]) -> IffImage {
    let h = regs.len() as u32 / w;
    let pal = Palette::new(cmap16().iter().map(|c| [c[0], c[1], c[2], 255]).collect());
    IffImage::new(
        w,
        h,
        IffPixelFormat::Pal8,
        vec![Plane::new(w as usize, regs.to_vec())],
    )
    .unwrap()
    .with_palette(pal)
}

fn displayable() -> Vec<u8> {
    (0..16u8).filter(|&r| r != 8).collect()
}

#[test]
fn pal8_round_trips_through_the_playfield_split() {
    for camg in [CAMG_DUALPF, CAMG_DUALPF | CAMG_PFBA] {
        for form in [IffForm::Ilbm, IffForm::Acbm] {
            let img = pal8(15, &displayable());
            let opts = EncodeOptions::default()
                .with_viewmode(Some(camg))
                .with_form(form);
            let bytes = encode(&img, &opts).unwrap();
            let back = decode(&bytes).unwrap();
            assert_eq!(indices(&back), displayable(), "camg {camg:#x} {form:?}");
            assert_eq!(back.n_planes, 6);
            assert_eq!(back.viewmode, Some(camg));
        }
    }
}

#[test]
fn encoder_uses_the_fewest_planes_the_registers_need() {
    let opts = EncodeOptions::default().with_viewmode(Some(CAMG_DUALPF));
    // COLOR0, 1 (PF1 001) and 9 (PF2 001): two planes.
    let back = decode(&encode(&pal8(3, &[0, 1, 9]), &opts).unwrap()).unwrap();
    assert_eq!((back.n_planes, indices(&back)), (2, vec![0, 1, 9]));
    // COLOR3 (PF1 011 -> planes 1 and 3) and 11 (PF2 011): four planes.
    let back = decode(&encode(&pal8(2, &[3, 11]), &opts).unwrap()).unwrap();
    assert_eq!((back.n_planes, indices(&back)), (4, vec![3, 11]));
}

#[test]
fn registers_no_playfield_shows_are_refused() {
    let opts = EncodeOptions::default().with_viewmode(Some(CAMG_DUALPF));
    for bad in [8u8, 16, 200] {
        let mut regs = vec![1u8, 9, bad, 0];
        regs.truncate(4);
        let mut img = pal8(4, &regs);
        if bad >= 16 {
            let entries = (0..=bad).map(|i| [i, i, i, 255]).collect();
            img = img.with_palette(Palette::new(entries));
        }
        assert!(
            matches!(encode(&img, &opts), Err(Error::Unsupported(_))),
            "register {bad}"
        );
    }
    // Too few planes requested for the registers used.
    let few = opts.clone().with_n_planes(Some(2));
    assert!(matches!(
        encode(&pal8(1, &[7]), &few),
        Err(Error::InvalidData(_))
    ));
}

#[test]
fn transparent_key_and_mask_survive_the_split() {
    // One transparent entry (COLOR9) -> masking 2 with key 9.
    let mut img = pal8(4, &[9, 1, 15, 0]);
    let mut entries = img.palette.clone().unwrap().entries;
    entries[9][3] = 0;
    img = img.with_palette(Palette::new(entries));
    let opts = EncodeOptions::default().with_viewmode(Some(CAMG_DUALPF | CAMG_PFBA));
    let back = decode(&encode(&img, &opts).unwrap()).unwrap();
    assert_eq!(indices(&back), vec![9, 1, 15, 0]);
    assert!(back.palette.as_ref().unwrap().is_transparent(9));
    // Mask plane: decodes as Rgba with the masked pixel clear.
    let masked = encode(&img, &opts.clone().with_masking(Some(Masking::HasMask))).unwrap();
    let rgba = decode_rgba8(&masked).unwrap();
    assert_eq!(rgba.data[3], 0);
    assert_eq!(rgba.data[7], 255);
}

#[test]
fn anim_frames_round_trip_in_dual_playfield_mode() {
    // 30 px wide (row_bytes 4) so the long-data ops apply too.
    let regs_a: Vec<u8> = [displayable(), displayable()].concat();
    let regs_b: Vec<u8> = regs_a.iter().rev().copied().collect();
    let frames: Vec<Frame> = [regs_a.clone(), regs_b.clone(), regs_a.clone()]
        .into_iter()
        .enumerate()
        .map(|(i, r)| Frame::new(pal8(30, &r), None, i as u32))
        .collect();
    for op in [
        AnimOp::Op0,
        AnimOp::Op1,
        AnimOp::Op2,
        AnimOp::Op3,
        AnimOp::Op4 { long_data: false },
        AnimOp::Op4 { long_data: true },
        AnimOp::Op5,
        AnimOp::Op7 { long_data: false },
        AnimOp::Op7 { long_data: true },
        AnimOp::Op8 { long_data: false },
        AnimOp::Op8 { long_data: true },
    ] {
        let opts = EncodeOptions::default()
            .with_viewmode(Some(CAMG_DUALPF | CAMG_PFBA))
            .with_anim_op(op);
        let bytes = encode_all(&frames, &opts).unwrap();
        let back = decode_all(&bytes).unwrap();
        let got: Vec<Vec<u8>> = back.iter().map(|f| indices(&f.image)).collect();
        assert_eq!(
            got,
            vec![regs_a.clone(), regs_b.clone(), regs_a.clone()],
            "{op:?}"
        );
    }
}

#[test]
fn anim_deltas_apply_to_the_raw_planes_not_the_composite() {
    // Seed: 2 planes, every pixel raw 0b11 (PF1 001 and PF2 001 both
    // opaque); with PFBA playfield 2 is in front, so it shows COLOR9.
    let seed = ilbm(&[vec![0b11; 16]], 2, Some(CAMG_DUALPF | CAMG_PFBA), 0, 0);
    // Frame 2: op-1 XOR over the full frame flipping BODY plane 1 only.
    // The true planar state becomes 0b01 (PF1 001 alone) -> COLOR1; a
    // decoder that re-derived the state from the composite (COLOR9 =
    // raw 0b10) would land on 0b00 -> COLOR0.
    let mut anhd = [0u8; 40];
    anhd[0] = 1; // operation: XOR
    anhd[2..4].copy_from_slice(&16u16.to_be_bytes());
    anhd[4..6].copy_from_slice(&1u16.to_be_bytes());
    anhd[14..18].copy_from_slice(&1u32.to_be_bytes());
    anhd[18] = 1;
    let xor_body = [0x00, 0x00, 0xFF, 0xFF];
    let delta = chunk(
        b"FORM",
        &[
            b"ILBM".to_vec(),
            chunk(b"ANHD", &anhd),
            chunk(b"BODY", &xor_body),
        ]
        .concat(),
    );
    let anim = chunk(b"FORM", &[b"ANIM".to_vec(), seed, delta].concat());
    let frames = decode_all(&anim).unwrap();
    assert_eq!(indices(&frames[0].image), vec![9; 16]);
    assert_eq!(indices(&frames[1].image), vec![1; 16]);
}
