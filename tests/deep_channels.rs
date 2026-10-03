//! Round-373 coverage: DEEP per-component channel extraction + the
//! `Dpel` layout-query helpers.
//!
//! A DEEP pixel (§1.2) packs its components consecutively, MSB-first,
//! padded to a byte boundary. An RGBA collapse keeps only RED/GREEN/BLUE
//! and (when present) ALPHA/OPACITY, dropping every other component a
//! `DPEL` may describe — ZBUFFER, MASK, key channels, BLACK, etc.
//! `ilbm::extract_deep_channel` pulls any one named component out of an
//! *uncompressed* chunky DBOD body into a row-major `Vec<u8>` plane
//! (scaled to 8 bits), and the `Dpel` accessors (`has_component`,
//! `bit_depth_of`, `bit_offset_of`, `has_alpha`) describe the layout.
//!
//! Spec reference: `docs/image/iff/iff-truecolor-chunks.md` §1.2.

use oxideav_iff::ilbm::{extract_deep_channel, DeepCType, Dpel, DpelElement};

fn dpel(elems: &[(DeepCType, u16)]) -> Dpel {
    Dpel {
        elements: elems
            .iter()
            .map(|&(c_type, c_bit_depth)| DpelElement {
                c_type,
                c_bit_depth,
            })
            .collect(),
    }
}

#[test]
fn dpel_query_helpers_report_layout() {
    // RGBA 8:8:8:8 — 4 bytes/pixel.
    let d = dpel(&[
        (DeepCType::Red, 8),
        (DeepCType::Green, 8),
        (DeepCType::Blue, 8),
        (DeepCType::Alpha, 8),
    ]);
    assert_eq!(d.total_bits(), 32);
    assert_eq!(d.pixel_bytes(), 4);
    assert!(d.has_component(DeepCType::Red));
    assert!(d.has_component(DeepCType::Alpha));
    assert!(!d.has_component(DeepCType::ZBuffer));
    assert_eq!(d.bit_depth_of(DeepCType::Green), Some(8));
    assert_eq!(d.bit_depth_of(DeepCType::ZBuffer), None);
    // Components are MSB-first in storage order.
    assert_eq!(d.bit_offset_of(DeepCType::Red), Some(0));
    assert_eq!(d.bit_offset_of(DeepCType::Green), Some(8));
    assert_eq!(d.bit_offset_of(DeepCType::Blue), Some(16));
    assert_eq!(d.bit_offset_of(DeepCType::Alpha), Some(24));
    assert!(d.has_alpha());
}

#[test]
fn has_alpha_only_for_alpha_or_opacity() {
    assert!(dpel(&[(DeepCType::Red, 8), (DeepCType::Opacity, 8)]).has_alpha());
    // MASK / key channels are not treated as alpha (undocumented semantics).
    assert!(!dpel(&[(DeepCType::Red, 8), (DeepCType::Mask, 8)]).has_alpha());
    assert!(!dpel(&[(DeepCType::Red, 8), (DeepCType::LinearKey, 8)]).has_alpha());
    assert!(!dpel(&[(DeepCType::Red, 8), (DeepCType::BinaryKey, 8)]).has_alpha());
    assert!(!dpel(&[(DeepCType::Red, 8)]).has_alpha());
}

#[test]
fn extract_rgb_channels_from_chunky_body() {
    // 2x2 RGB888 chunky stream, 3 bytes/pixel.
    let d = dpel(&[
        (DeepCType::Red, 8),
        (DeepCType::Green, 8),
        (DeepCType::Blue, 8),
    ]);
    #[rustfmt::skip]
    let body: Vec<u8> = vec![
        10, 20, 30,   40, 50, 60,
        70, 80, 90,   100, 110, 120,
    ];
    let red = extract_deep_channel(&d, 2, 2, &body, DeepCType::Red)
        .unwrap()
        .unwrap();
    assert_eq!(red, vec![10, 40, 70, 100]);
    let green = extract_deep_channel(&d, 2, 2, &body, DeepCType::Green)
        .unwrap()
        .unwrap();
    assert_eq!(green, vec![20, 50, 80, 110]);
    let blue = extract_deep_channel(&d, 2, 2, &body, DeepCType::Blue)
        .unwrap()
        .unwrap();
    assert_eq!(blue, vec![30, 60, 90, 120]);
}

#[test]
fn extract_zbuffer_channel_dropped_by_rgba_collapse() {
    // RGB + a 16-bit ZBUFFER per pixel — 5 bytes/pixel.
    let d = dpel(&[
        (DeepCType::Red, 8),
        (DeepCType::Green, 8),
        (DeepCType::Blue, 8),
        (DeepCType::ZBuffer, 16),
    ]);
    assert_eq!(d.pixel_bytes(), 5);
    // Two pixels. ZBUFFER values 0x1234 and 0xABCD (big-endian within pixel).
    #[rustfmt::skip]
    let body: Vec<u8> = vec![
        1, 2, 3, 0x12, 0x34,
        4, 5, 6, 0xAB, 0xCD,
    ];
    let z = extract_deep_channel(&d, 2, 1, &body, DeepCType::ZBuffer)
        .unwrap()
        .unwrap();
    // 16-bit scaled to 8 bits = top byte (bit replication of a 16-bit value
    // takes the high 8 bits): 0x1234 -> 0x12, 0xABCD -> 0xAB.
    assert_eq!(z, vec![0x12, 0xAB]);
}

#[test]
fn extract_absent_component_returns_none() {
    let d = dpel(&[
        (DeepCType::Red, 8),
        (DeepCType::Green, 8),
        (DeepCType::Blue, 8),
    ]);
    let body = vec![0u8; 3];
    let got = extract_deep_channel(&d, 1, 1, &body, DeepCType::ZBuffer).unwrap();
    assert!(got.is_none());
}

#[test]
fn extract_mask_channel_4bit_scaled() {
    // RGB + 4-bit MASK; pixel = 3*8 + 4 = 28 bits -> padded to 4 bytes.
    let d = dpel(&[
        (DeepCType::Red, 8),
        (DeepCType::Green, 8),
        (DeepCType::Blue, 8),
        (DeepCType::Mask, 4),
    ]);
    assert_eq!(d.total_bits(), 28);
    assert_eq!(d.pixel_bytes(), 4);
    assert_eq!(d.bit_offset_of(DeepCType::Mask), Some(24));
    // One pixel: R=0xAA G=0xBB B=0xCC, MASK nibble = 0xF in the high nibble
    // of the 4th byte (padding fills the low nibble with 0).
    let body: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xF0];
    let mask = extract_deep_channel(&d, 1, 1, &body, DeepCType::Mask)
        .unwrap()
        .unwrap();
    // 4-bit 0xF scaled to 8 bits via bit replication = 0xFF.
    assert_eq!(mask, vec![0xFF]);
}

#[test]
fn extract_rejects_short_body() {
    let d = dpel(&[
        (DeepCType::Red, 8),
        (DeepCType::Green, 8),
        (DeepCType::Blue, 8),
    ]);
    // Needs 2*1*3 = 6 bytes; give 5.
    let body = vec![0u8; 5];
    assert!(extract_deep_channel(&d, 2, 1, &body, DeepCType::Red).is_err());
}

// ───────────── round-469 fuzz regressions (deep_decode target) ─────────────

/// A `DPEL` component deeper than 16 bits (here `cBitDepth = 0x23de`)
/// used to shift a `u16` by `depth - 8` in the 8-bit scaler — a debug
/// overflow panic. Deeper components contribute their top 16 bits.
#[test]
fn deep_component_depth_beyond_16_bits_does_not_panic() {
    let bytes: [u8; 78] = [
        0x46, 0x4f, 0x52, 0x4d, 0xff, 0xdf, 0xde, 0xff, 0x44, 0x45, 0x45, 0x50, 0x44, 0x42, 0x4f,
        0x44, 0x00, 0x00, 0x00, 0x0d, 0xb2, 0x81, 0xb9, 0x00, 0x04, 0xb0, 0x00, 0xb9, 0x00, 0x00,
        0x00, 0x00, 0x45, 0x50, 0x44, 0x50, 0x45, 0x4c, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x09, 0x00, 0x23, 0xde, 0xff, 0xb7, 0xba, 0xb2, 0xaf, 0x44, 0x47, 0x42, 0x4c,
        0x00, 0x00, 0x00, 0x0d, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x84, 0xff, 0xff,
    ];
    // Must return (Ok or Err), never panic.
    let _ = oxideav_iff::ilbm::parse_deep_frames(&bytes);
    let _ = oxideav_iff::decode(&bytes);
}

/// A `DPEL` with zero components defeated the TVDC 15x expansion guard
/// (0 component bytes always fit) and the RGBA canvas was then sized
/// from the DGBL dimensions alone — a 5 GiB allocation. Rejected now.
#[test]
fn deep_tvdc_zero_component_dpel_is_rejected_before_allocating() {
    let bytes: [u8; 78] = [
        0x46, 0x4f, 0x52, 0x4d, 0xff, 0xdf, 0xde, 0xff, 0x44, 0x45, 0x45, 0x50, 0x44, 0x42, 0x4f,
        0x44, 0x00, 0x00, 0x00, 0x0d, 0x4e, 0x46, 0x46, 0x4f, 0x52, 0x4d, 0xff, 0xdf, 0xde, 0xff,
        0x44, 0x45, 0x45, 0x50, 0x44, 0x50, 0x45, 0x4c, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xb7, 0xba, 0xb2, 0xaf, 0x44, 0x47, 0x42, 0x4c,
        0x00, 0x00, 0x00, 0x0d, 0x4e, 0x00, 0xf9, 0xe9, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x84,
        0xff, 0xdf, 0xff,
    ];
    let table: [i16; 16] = [
        0, 1, -1, 2, -2, 4, -4, 8, -8, 16, -16, 32, -32, 64, -64, 128,
    ];
    let err = oxideav_iff::ilbm::parse_deep_frames_with_tvdc_table(&bytes, &table).unwrap_err();
    assert!(
        matches!(err, oxideav_iff::IffError::InvalidData(_)),
        "{err}"
    );
    let empty = oxideav_iff::ilbm::Dpel { elements: vec![] };
    assert!(oxideav_iff::ilbm::assemble_deep_tvdc(&empty, 40000, 40000, &table, &[0; 16]).is_err());
}
