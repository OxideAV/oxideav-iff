//! PCHG (Palette CHanGe) — typed-header accessors.
//!
//! Covers the [`Pchg::header`] / [`Pchg::kind`] /
//! [`Pchg::derive_header_hints`] / [`Pchg::header_matches_payload`]
//! surface. The PCHG spec defines a 20-byte fixed-layout header in
//! front of every LineData stream:
//!
//! ```text
//! u16 Compression
//! u16 Flags          (bit 0 = 12-bit, bit 1 = 32-bit, bit 2 = alpha)
//! i16 StartLine
//! u16 LineCount
//! u16 ChangedLines   (hint: number of lines with a change record)
//! u16 MinReg         (hint: smallest Register touched)
//! u16 MaxReg         (hint: largest Register touched)
//! u16 MaxChanges     (hint: longest per-line change list)
//! u32 TotalChanges   (hint: sum of per-line ChangeCounts)
//! ```
//!
//! followed by the LineMask bitmap (`((LineCount + 31) / 32) * 4`
//! bytes, MSB-first) and one change record per set mask bit. These
//! tests build hand-rolled PCHG bodies, parse them, and check both the
//! typed-field round-trip and the `derive_header_hints` /
//! `header_matches_payload` re-derivation invariants.

use oxideav_iff::ilbm::{Pchg, PchgChange, PchgHeader, PchgKind, PchgLine};

// 20-byte PCHG header for the Small format with one line of one
// change touching register 1 (packed word 0x10F0: reg 1, RGB444
// 0x0F0 → 8-bit 0x00 0xFF 0x00).
fn small_one_change_body() -> Vec<u8> {
    let mut raw = Vec::new();
    // Compression = 0 (uncompressed).
    raw.extend_from_slice(&0u16.to_be_bytes());
    // Flags = 1 (12-bit / Small).
    raw.extend_from_slice(&1u16.to_be_bytes());
    // StartLine = 0.
    raw.extend_from_slice(&0i16.to_be_bytes());
    // LineCount = 2.
    raw.extend_from_slice(&2u16.to_be_bytes());
    // ChangedLines = 1, MinReg = 1, MaxReg = 1, MaxChanges = 1,
    // TotalChanges = 1.
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&1u32.to_be_bytes());
    // LineMask (one longword for 2 lines): line 0 clear, line 1 set.
    raw.extend_from_slice(&[0x40, 0x00, 0x00, 0x00]);
    // Line 1 record: ChangeCount16 = 1, ChangeCount32 = 0, then the
    // packed word (1 << 12) | (0x0 << 8) | (0xF << 4) | 0x0.
    raw.push(1);
    raw.push(0);
    raw.extend_from_slice(&0x10F0u16.to_be_bytes());
    raw
}

// Big-format header with one line of two changes touching registers
// 3 and 7 (6-byte records, on-disk component order A, R, B, G).
fn big_two_changes_body() -> Vec<u8> {
    let mut raw = Vec::new();
    // Compression = 0, Flags = 2 (32-bit / Big).
    raw.extend_from_slice(&0u16.to_be_bytes());
    raw.extend_from_slice(&2u16.to_be_bytes());
    // StartLine = 5, LineCount = 1.
    raw.extend_from_slice(&5i16.to_be_bytes());
    raw.extend_from_slice(&1u16.to_be_bytes());
    // ChangedLines = 1, MinReg = 3, MaxReg = 7, MaxChanges = 2,
    // TotalChanges = 2.
    raw.extend_from_slice(&1u16.to_be_bytes());
    raw.extend_from_slice(&3u16.to_be_bytes());
    raw.extend_from_slice(&7u16.to_be_bytes());
    raw.extend_from_slice(&2u16.to_be_bytes());
    raw.extend_from_slice(&2u32.to_be_bytes());
    // LineMask (one longword for 1 line): line set.
    raw.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
    // Line record (= scanline 5): ChangeCount = 2 as u16.
    raw.extend_from_slice(&2u16.to_be_bytes());
    // Register 3 → RGB(0x11, 0x22, 0x33): bytes A, R, B, G.
    raw.extend_from_slice(&3u16.to_be_bytes());
    raw.extend_from_slice(&[0x00, 0x11, 0x33, 0x22]);
    // Register 7 → RGB(0x44, 0x55, 0x66).
    raw.extend_from_slice(&7u16.to_be_bytes());
    raw.extend_from_slice(&[0x00, 0x44, 0x66, 0x55]);
    raw
}

#[test]
fn small_header_round_trips_via_typed_accessor() {
    let raw = small_one_change_body();
    let pchg = Pchg::parse(&raw).unwrap();
    let h = pchg
        .header()
        .expect("parser-produced Pchg always has header");
    assert_eq!(
        h,
        PchgHeader {
            compression: 0,
            flags: 1,
            start_line: 0,
            line_count: 2,
            changed_lines: 1,
            min_reg: 1,
            max_reg: 1,
            max_changes: 1,
            total_changes: 1,
        }
    );
}

#[test]
fn small_kind_helper_reports_small() {
    let pchg = Pchg::parse(&small_one_change_body()).unwrap();
    assert_eq!(pchg.kind(), Some(PchgKind::Small));
    let h = pchg.header().unwrap();
    assert_eq!(h.kind(), PchgKind::Small);
    assert!(!h.is_compressed());
}

#[test]
fn big_header_round_trips_via_typed_accessor() {
    let raw = big_two_changes_body();
    let pchg = Pchg::parse(&raw).unwrap();
    let h = pchg.header().unwrap();
    assert_eq!(h.compression, 0);
    assert_eq!(h.flags, 2);
    assert_eq!(h.start_line, 5);
    assert_eq!(h.line_count, 1);
    assert_eq!(h.changed_lines, 1);
    assert_eq!(h.min_reg, 3);
    assert_eq!(h.max_reg, 7);
    assert_eq!(h.max_changes, 2);
    assert_eq!(h.total_changes, 2);
    assert_eq!(h.kind(), PchgKind::Big);
    assert_eq!(pchg.kind(), Some(PchgKind::Big));
}

#[test]
fn small_payload_decodes_change_record() {
    let pchg = Pchg::parse(&small_one_change_body()).unwrap();
    assert_eq!(pchg.lines.len(), 1);
    assert_eq!(pchg.lines[0].line, 1);
    assert_eq!(pchg.lines[0].changes.len(), 1);
    assert_eq!(pchg.lines[0].changes[0].index, 1);
    assert_eq!(pchg.lines[0].changes[0].rgb, [0, 0xFF, 0]);
}

#[test]
fn big_payload_decodes_change_records() {
    let pchg = Pchg::parse(&big_two_changes_body()).unwrap();
    assert_eq!(pchg.lines.len(), 1);
    assert_eq!(pchg.lines[0].line, 5);
    assert_eq!(pchg.lines[0].changes.len(), 2);
    assert_eq!(pchg.lines[0].changes[0].index, 3);
    assert_eq!(pchg.lines[0].changes[0].rgb, [0x11, 0x22, 0x33]);
    assert_eq!(pchg.lines[0].changes[1].index, 7);
    assert_eq!(pchg.lines[0].changes[1].rgb, [0x44, 0x55, 0x66]);
}

#[test]
fn derive_header_hints_recomputes_canonical_values_small() {
    let pchg = Pchg::parse(&small_one_change_body()).unwrap();
    let (changed, min_reg, max_reg, max_changes, total) = pchg.derive_header_hints();
    assert_eq!(changed, 1);
    assert_eq!(min_reg, 1);
    assert_eq!(max_reg, 1);
    assert_eq!(max_changes, 1);
    assert_eq!(total, 1);
}

#[test]
fn derive_header_hints_recomputes_canonical_values_big() {
    let pchg = Pchg::parse(&big_two_changes_body()).unwrap();
    let (changed, min_reg, max_reg, max_changes, total) = pchg.derive_header_hints();
    assert_eq!(changed, 1);
    assert_eq!(min_reg, 3);
    assert_eq!(max_reg, 7);
    assert_eq!(max_changes, 2);
    assert_eq!(total, 2);
}

#[test]
fn derive_header_hints_handles_empty_pchg() {
    // Empty PCHG: header says LineCount = 0, no per-line records.
    let mut raw = Vec::new();
    raw.extend_from_slice(&0u16.to_be_bytes()); // Compression = 0
    raw.extend_from_slice(&1u16.to_be_bytes()); // Flags = Small
    raw.extend_from_slice(&0i16.to_be_bytes()); // StartLine = 0
    raw.extend_from_slice(&0u16.to_be_bytes()); // LineCount = 0
    raw.extend_from_slice(&0u16.to_be_bytes()); // ChangedLines = 0
    raw.extend_from_slice(&0u16.to_be_bytes()); // MinReg = 0
    raw.extend_from_slice(&0u16.to_be_bytes()); // MaxReg = 0
    raw.extend_from_slice(&0u16.to_be_bytes()); // MaxChanges = 0
    raw.extend_from_slice(&0u32.to_be_bytes()); // TotalChanges = 0
    let pchg = Pchg::parse(&raw).unwrap();
    let (changed, min_reg, max_reg, max_changes, total) = pchg.derive_header_hints();
    assert_eq!(changed, 0);
    assert_eq!(min_reg, 0, "MinReg defaults to 0 for empty PCHG");
    assert_eq!(max_reg, 0, "MaxReg defaults to 0 for empty PCHG");
    assert_eq!(max_changes, 0);
    assert_eq!(total, 0);
}

#[test]
fn header_matches_payload_true_for_small_round_trip() {
    let pchg = Pchg::parse(&small_one_change_body()).unwrap();
    assert!(pchg.header_matches_payload());
}

#[test]
fn header_matches_payload_true_for_big_round_trip() {
    let pchg = Pchg::parse(&big_two_changes_body()).unwrap();
    assert!(pchg.header_matches_payload());
}

#[test]
fn header_matches_payload_false_when_hint_is_wrong() {
    // Construct a Big PCHG whose TotalChanges hint is deliberately
    // stale (header says 5 but payload only has 2).
    let mut raw = big_two_changes_body();
    // Overwrite TotalChanges (offset 16) with the wrong value 5.
    raw[16..20].copy_from_slice(&5u32.to_be_bytes());
    let pchg = Pchg::parse(&raw).unwrap();
    let h = pchg.header().unwrap();
    assert_eq!(h.total_changes, 5);
    let (_, _, _, _, derived) = pchg.derive_header_hints();
    assert_eq!(derived, 2);
    assert!(
        !pchg.header_matches_payload(),
        "stale TotalChanges hint is caught"
    );
}

#[test]
fn header_matches_payload_false_when_min_reg_drifts() {
    // ChangedLines / MaxChanges / TotalChanges agree, but MinReg
    // hint claims register 0 even though the only change touches
    // register 1.
    let mut raw = small_one_change_body();
    // MinReg is at offset 10..12.
    raw[10..12].copy_from_slice(&0u16.to_be_bytes());
    let pchg = Pchg::parse(&raw).unwrap();
    let h = pchg.header().unwrap();
    assert_eq!(h.min_reg, 0);
    let (_, derived_min, _, _, _) = pchg.derive_header_hints();
    assert_eq!(derived_min, 1);
    assert!(
        !pchg.header_matches_payload(),
        "stale MinReg hint is caught"
    );
}

#[test]
fn header_helper_returns_none_for_handcrafted_short_raw() {
    // A `Pchg` built outside the parser with a short raw buffer
    // can't surface a header.
    let pchg = Pchg {
        raw: vec![0u8; 10],
        lines: vec![PchgLine {
            line: 0,
            changes: vec![PchgChange::new(0, [0; 3])],
        }],
    };
    assert_eq!(pchg.header(), None);
    assert_eq!(pchg.kind(), None);
    assert!(
        !pchg.header_matches_payload(),
        "an absent header never matches a payload"
    );
}

#[test]
fn kind_decodes_default_zero_flags_as_small() {
    // Neither format bit set defaults to Small (non-Option accessor).
    let mut raw = Vec::new();
    raw.extend_from_slice(&0u16.to_be_bytes()); // Compression
    raw.extend_from_slice(&0u16.to_be_bytes()); // Flags = 0
    raw.extend_from_slice(&0i16.to_be_bytes()); // StartLine
    raw.extend_from_slice(&0u16.to_be_bytes()); // LineCount
    raw.extend_from_slice(&0u16.to_be_bytes());
    raw.extend_from_slice(&0u16.to_be_bytes());
    raw.extend_from_slice(&0u16.to_be_bytes());
    raw.extend_from_slice(&0u16.to_be_bytes());
    raw.extend_from_slice(&0u32.to_be_bytes());
    let pchg = Pchg::parse(&raw).unwrap();
    assert_eq!(pchg.kind(), Some(PchgKind::Small));
}

#[test]
fn is_compressed_true_when_compression_is_one() {
    let mut raw = small_one_change_body();
    raw[0..2].copy_from_slice(&1u16.to_be_bytes());
    // We hand-craft the header so the body is no longer self-consistent
    // for derive_header_hints; we only check the typed flag here.
    let pchg = Pchg { raw, lines: vec![] };
    let h = pchg.header().unwrap();
    assert_eq!(h.compression, 1);
    assert!(h.is_compressed());
}
