//! CAMG — full ViewMode / DisplayID bitfield surface.
//!
//! Covers the `ViewPort.Modes` flag accessors, the 32-bit
//! ModeID/DisplayID form (monitor key in the high word ORed with a
//! mode key in the low word), the legacy junk-bit stripping mask, the
//! format-bit extraction used by raster decoders, and the classic
//! "bad CAMG" heuristic.

use oxideav_iff::ilbm::{
    encode_ilbm, parse_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking, A2024_MONITOR_ID,
    CAMG_DOUBLESCAN, CAMG_DUALPF, CAMG_EHB, CAMG_EXTENDED_MODE, CAMG_FORMAT_MASK,
    CAMG_GENLOCK_AUDIO, CAMG_GENLOCK_VIDEO, CAMG_HAM, CAMG_HIRES, CAMG_JUNK_MASK, CAMG_LACE,
    CAMG_PFBA, CAMG_SPRITES, CAMG_SUPERHIRES, CAMG_VP_HIDE, DBLPAL_MONITOR_ID,
    EXTRAHALFBRITELACE_KEY, HAMLACE_KEY, HIRESLACE_KEY, MONITOR_ID_MASK, PAL_MONITOR_ID,
    SUPER72HIRESDBL_KEY, SUPER_KEY, VGAHAM_KEY, VGAPRODUCT_KEY, VGA_MONITOR_ID,
};

fn camg(raw: u32) -> Camg {
    Camg { raw }
}

#[test]
fn every_viewmode_flag_has_an_accessor() {
    assert!(camg(CAMG_HAM).is_ham());
    assert!(camg(CAMG_EHB).is_ehb());
    assert!(camg(CAMG_LACE).is_lace());
    assert!(camg(CAMG_HIRES).is_hires());
    assert!(camg(CAMG_SUPERHIRES).is_superhires());
    assert!(camg(CAMG_DOUBLESCAN).is_doublescan());
    assert!(camg(CAMG_DUALPF).is_dualpf());
    assert!(camg(CAMG_PFBA).is_pfba());
    assert!(camg(CAMG_GENLOCK_VIDEO).is_genlock_video());
    assert!(camg(CAMG_GENLOCK_AUDIO).is_genlock_audio());
    assert!(camg(CAMG_EXTENDED_MODE).is_extended_mode());
    assert!(camg(CAMG_VP_HIDE).is_vp_hide());
    assert!(camg(CAMG_SPRITES).is_sprites());
    // Zero has none of them.
    let z = camg(0);
    assert!(
        !z.is_ham()
            && !z.is_ehb()
            && !z.is_lace()
            && !z.is_hires()
            && !z.is_superhires()
            && !z.is_doublescan()
            && !z.is_dualpf()
            && !z.is_pfba()
            && !z.is_genlock_video()
            && !z.is_genlock_audio()
            && !z.is_extended_mode()
            && !z.is_vp_hide()
            && !z.is_sprites()
    );
}

#[test]
fn mode_id_is_monitor_or_mode_key() {
    // Building a specific mode ORs a monitor ID with a base key.
    let c = camg(PAL_MONITOR_ID | HAMLACE_KEY);
    assert!(c.is_mode_id());
    assert_eq!(c.monitor_id(), PAL_MONITOR_ID);
    assert_eq!(c.monitor_name(), Some("PAL"));
    assert!(c.is_ham());
    assert!(c.is_lace());
    assert!(!c.is_hires());
    assert_eq!(c.format_bits(), CAMG_HAM | CAMG_LACE);
}

#[test]
fn named_monitor_qualified_keys_decode() {
    // VGAHAM: VGA monitor, HAM + LACE format bits folded into the key.
    let c = camg(VGAHAM_KEY);
    assert_eq!(c.monitor_id(), VGA_MONITOR_ID);
    assert_eq!(c.monitor_name(), Some("VGA"));
    assert!(c.is_ham());
    assert!(c.is_lace());

    // VGA productivity: super-hires class (HIRES | SUPERHIRES | LACE).
    let p = camg(VGAPRODUCT_KEY);
    assert_eq!(p.monitor_name(), Some("VGA"));
    assert!(p.is_hires());
    assert!(p.is_superhires());
    assert!(p.is_lace());

    // SUPER72 hires scan-doubled: DBL modes carry the 0x0008 bit.
    let s = camg(SUPER72HIRESDBL_KEY);
    assert_eq!(s.monitor_name(), Some("SUPER72"));
    assert!(s.is_hires());
    assert!(s.is_doublescan());

    // A2024 and DBLPAL are recognised monitors.
    assert_eq!(camg(A2024_MONITOR_ID).monitor_name(), Some("A2024"));
    assert_eq!(camg(DBLPAL_MONITOR_ID).monitor_name(), Some("DBLPAL"));
}

#[test]
fn base_mode_keys_fold_format_bits() {
    assert!(camg(SUPER_KEY).is_hires());
    assert!(camg(SUPER_KEY).is_superhires());
    let e = camg(EXTRAHALFBRITELACE_KEY);
    assert!(e.is_ehb());
    assert!(e.is_lace());
    assert!(!e.is_mode_id(), "base keys alone have no monitor part");
}

#[test]
fn view_mode_strips_junk_and_monitor_bits() {
    let raw = PAL_MONITOR_ID
        | CAMG_HIRES
        | CAMG_LACE
        | CAMG_SPRITES
        | CAMG_GENLOCK_AUDIO
        | CAMG_EXTENDED_MODE
        | CAMG_VP_HIDE
        | CAMG_GENLOCK_VIDEO;
    let c = camg(raw);
    assert_eq!(c.view_mode(), CAMG_HIRES | CAMG_LACE);
    // The junk mask is exactly the five non-format bits.
    assert_eq!(
        CAMG_JUNK_MASK,
        CAMG_EXTENDED_MODE | CAMG_SPRITES | CAMG_VP_HIDE | CAMG_GENLOCK_AUDIO | CAMG_GENLOCK_VIDEO
    );
    assert_eq!(CAMG_JUNK_MASK, 0x7102);
    assert_eq!(CAMG_FORMAT_MASK, 0x8CE4);
}

#[test]
fn legacy_viewmode_is_not_a_mode_id() {
    let c = camg(CAMG_HIRES | CAMG_LACE | CAMG_EXTENDED_MODE);
    assert!(!c.is_mode_id());
    assert_eq!(c.monitor_name(), None);
    // EXTENDED_MODE leaks into MONITOR_ID_MASK by design — which is
    // why monitor_id() is only meaningful on a real ModeID.
    assert_eq!(c.monitor_id(), CAMG_EXTENDED_MODE & MONITOR_ID_MASK);
}

#[test]
fn unknown_monitor_id_has_no_name() {
    let c = camg(0x00F0_1000 | HIRESLACE_KEY);
    assert!(c.is_mode_id());
    assert_eq!(c.monitor_name(), None);
    assert!(c.is_hires());
}

#[test]
fn bad_camg_heuristic() {
    // Junk-only values look broken regardless of depth.
    assert!(camg(CAMG_SPRITES).looks_bad_for_planes(5));
    assert!(camg(CAMG_GENLOCK_VIDEO | CAMG_VP_HIDE).looks_bad_for_planes(1));
    assert!(camg(CAMG_EXTENDED_MODE).looks_bad_for_planes(2));
    // Zero is suspicious only for deeper-than-5-plane images.
    assert!(camg(0).looks_bad_for_planes(6));
    assert!(camg(0).looks_bad_for_planes(8));
    assert!(!camg(0).looks_bad_for_planes(5));
    // Real format bits are trusted.
    assert!(!camg(CAMG_HAM).looks_bad_for_planes(6));
    assert!(!camg(CAMG_EHB | CAMG_LACE).looks_bad_for_planes(6));
    assert!(!camg(PAL_MONITOR_ID | HAMLACE_KEY).looks_bad_for_planes(6));
}

#[test]
fn display_id_camg_roundtrips_through_encode() {
    // A DisplayID-form CAMG (PAL monitor, HIRES + LACE) on a plain
    // indexed image survives encode → parse bit-exact and its format
    // accessors keep working on the decoded image.
    let w = 8u16;
    let h = 2u16;
    let palette = vec![[0u8, 0, 0], [0xFF, 0xFF, 0xFF]];
    let rgba: Vec<u8> = (0..(w as usize * h as usize))
        .flat_map(|i| {
            let c = palette[i % 2];
            [c[0], c[1], c[2], 0xFF]
        })
        .collect();
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: Bmhd {
            width: w,
            height: h,
            x_origin: 0,
            y_origin: 0,
            n_planes: 1,
            masking: Masking::None,
            compression: Compression::None,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: w as i16,
            page_height: h as i16,
        },
        camg: Camg {
            raw: PAL_MONITOR_ID | HIRESLACE_KEY,
        },
        palette,
        rgba,
        ..IlbmImage::default()
    };
    let decoded = parse_ilbm(&encode_ilbm(&img).unwrap()).unwrap();
    assert_eq!(decoded.camg.raw, PAL_MONITOR_ID | HIRESLACE_KEY);
    assert!(decoded.camg.is_mode_id());
    assert_eq!(decoded.camg.monitor_name(), Some("PAL"));
    assert!(decoded.camg.is_hires());
    assert!(decoded.camg.is_lace());
    assert!(!decoded.camg.looks_bad_for_planes(decoded.bmhd.n_planes));
}

#[test]
fn ham_bit_in_mode_id_form_drives_ham_decode() {
    // A HAM6 image whose CAMG is the full DisplayID form (monitor key
    // present) must still decode through the HAM path, because the
    // mode key folds the HAM format bit into the low word.
    let w = 16u16;
    let h = 2u16;
    let palette: Vec<[u8; 3]> = (0..16u8).map(|i| [i * 0x11; 3]).collect();
    // A gradient the HAM encoder can chase.
    let rgba: Vec<u8> = (0..(w as usize * h as usize))
        .flat_map(|i| {
            let v = ((i % w as usize) * 255 / (w as usize - 1)) as u8;
            [v, v, v, 0xFF]
        })
        .collect();
    let img = IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd: Bmhd {
            width: w,
            height: h,
            x_origin: 0,
            y_origin: 0,
            n_planes: 6,
            masking: Masking::None,
            compression: Compression::None,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: w as i16,
            page_height: h as i16,
        },
        camg: Camg {
            raw: PAL_MONITOR_ID | HAMLACE_KEY,
        },
        palette,
        rgba: rgba.clone(),
        ..IlbmImage::default()
    };
    let decoded = parse_ilbm(&encode_ilbm(&img).unwrap()).unwrap();
    assert_eq!(decoded.camg.raw, PAL_MONITOR_ID | HAMLACE_KEY);
    assert!(decoded.camg.is_ham());
    // HAM6 reconstruction within the usual +/-16-per-gun tolerance.
    for (ours, orig) in decoded.rgba.chunks_exact(4).zip(rgba.chunks_exact(4)) {
        for c in 0..3 {
            assert!(
                (i16::from(ours[c]) - i16::from(orig[c])).abs() <= 16,
                "HAM6 channel drifted: {ours:?} vs {orig:?}"
            );
        }
    }
}
