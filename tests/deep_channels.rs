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
