//! Round-324: ANIM op-8 (Anim8 short / long Vertical Delta) decode tests.
//!
//! These tests exercise [`apply_op8_for_test`] directly: they build a
//! synthetic op-8 DLTA payload by hand, hand it to the decoder against a
//! known previous planar state, and verify the resulting planar state
//! matches the expected post-delta image.
//!
//! Op-8 wire layout summary (from `docs/image/iff/anim-op8.md`):
//!
//! * 16 big-endian u32 pointers: 8 opcode-list pointers (slots 0..=7),
//!   slots 8..=15 unused/zero. A `0` opcode pointer means the plane is
//!   unchanged. Each pointer is a byte offset from the DLTA start.
//! * Unlike op-7, opcodes and their data items are **interleaved
//!   inline** within each opcode list (the method-5 layout); there is no
//!   separate data list.
//! * Items are WORD (2 B) or LONG (4 B), selected by `ANHD.bits` bit 0.
//! * Per plane: walk columns left-to-right (§3.2 odd-long edge — a
//!   width that is an odd number of words wide gets a trailing WORD
//!   column even in long mode). Each column starts with an op-count
//!   item; `op_count = 0` means the column is unchanged.
//! * Three opcode classes (each an item-sized opcode):
//!   * **Skip** (hi bit clear, non-zero) — advance dest cursor by N
//!     rows; no data follows.
//!   * **Uniq** (hi bit set) — `op & !sign_bit` data items follow
//!     inline, one per consecutive row.
//!   * **Same** (`0` opcode + count item + one value item) — the value
//!     is written to `count` consecutive rows.
//! * "Advance one row" adds `row_bytes` (NOT the item width) to the
//!   byte address within the bitplane.

use oxideav_iff::anim::apply_op8_for_test;
use oxideav_iff::ilbm::{Bmhd, Compression, Masking};

/// Build a minimal BMHD of the given dimensions. Width 64 → row_bytes
/// = 8 so both WORD (2 B) and LONG (4 B) item modes are exercisable.
fn bmhd_for(width: u16, height: u16, n_planes: u8) -> Bmhd {
    Bmhd {
        width,
        height,
        x_origin: 0,
        y_origin: 0,
        n_planes,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: width as i16,
        page_height: height as i16,
    }
}

/// Serialise a 16-slot u32-BE pointer table into the leading 64 bytes
/// of a DLTA payload. Op-8 uses slots 0..=7 (opcode lists); slots
/// 8..=15 are zero.
fn make_pointer_table(op_offsets: [u32; 8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for off in op_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    for _ in 0..8 {
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out
}

fn push_word(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn push_long(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[test]
fn op8_short_skip_uniq_same_one_plane() {
    // 64×4 image, one plane → row_bytes = 8 → WORD cols = 4.
    let bmhd = bmhd_for(64, 4, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 8);

    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0xAA; row_bytes])
        .collect();

    // Plane 0, WORD data. Four columns:
    //   col 0: op_count=1 → Same(0x00 cnt=3 val=[0x11,0x22]) → rows 0..3.
    //   col 1: op_count=1 → Uniq(0x8002) → 2 items [0x33,0x44],[0x55,0x66]
    //          to rows 0..2.
    //   col 2: op_count=0 → unchanged.
    //   col 3: op_count=2 → Skip(0x0002) to row 2, then Uniq(0x8001) →
    //          item [0x77,0x88] at row 2.
    let mut ops: Vec<u8> = Vec::new();
    // col 0
    push_word(&mut ops, 1); // op_count
    push_word(&mut ops, 0x0000); // Same sentinel
    push_word(&mut ops, 3); // count
    push_word(&mut ops, 0x1122); // value
                                 // col 1
    push_word(&mut ops, 1); // op_count
    push_word(&mut ops, 0x8002); // Uniq cnt=2
    push_word(&mut ops, 0x3344);
    push_word(&mut ops, 0x5566);
    // col 2
    push_word(&mut ops, 0); // op_count = 0
                            // col 3
    push_word(&mut ops, 2); // op_count = 2
    push_word(&mut ops, 0x0002); // Skip 2 rows
    push_word(&mut ops, 0x8001); // Uniq cnt=1
    push_word(&mut ops, 0x7788);

    let mut op_ptrs = [0u32; 8];
    op_ptrs[0] = 64;
    let mut delta = make_pointer_table(op_ptrs);
    delta.extend_from_slice(&ops);

    apply_op8_for_test(&mut planar, &delta, &bmhd, /*long_data=*/ false).unwrap();

    assert_eq!(
        planar[0],
        vec![0x11, 0x22, 0x33, 0x44, 0xAA, 0xAA, 0xAA, 0xAA]
    );
    assert_eq!(
        planar[1],
        vec![0x11, 0x22, 0x55, 0x66, 0xAA, 0xAA, 0xAA, 0xAA]
    );
    assert_eq!(
        planar[2],
        vec![0x11, 0x22, 0xAA, 0xAA, 0xAA, 0xAA, 0x77, 0x88]
    );
    assert_eq!(
        planar[3],
        vec![0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]
    );
}

#[test]
fn op8_long_data_one_plane() {
    // 64×3, one plane, LONG data → row_bytes = 8 → 2 LONG cols (even).
    let bmhd = bmhd_for(64, 3, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 8);

    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // col 0: Same(0x03) → item 0xDEADBEEF to rows 0..3.
    // col 1: Uniq(2) → items 0x12345678, 0x9ABCDEF0 to rows 0..2.
    let mut ops: Vec<u8> = Vec::new();
    // col 0
    push_long(&mut ops, 1); // op_count
    push_long(&mut ops, 0); // Same sentinel
    push_long(&mut ops, 3); // count
    push_long(&mut ops, 0xDEAD_BEEF);
    // col 1
    push_long(&mut ops, 1); // op_count
    push_long(&mut ops, 0x8000_0002); // Uniq cnt=2
    push_long(&mut ops, 0x1234_5678);
    push_long(&mut ops, 0x9ABC_DEF0);

    let mut op_ptrs = [0u32; 8];
    op_ptrs[0] = 64;
    let mut delta = make_pointer_table(op_ptrs);
    delta.extend_from_slice(&ops);

    apply_op8_for_test(&mut planar, &delta, &bmhd, /*long_data=*/ true).unwrap();

    assert_eq!(
        planar[0],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78]
    );
    assert_eq!(
        planar[1],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x9A, 0xBC, 0xDE, 0xF0]
    );
    assert_eq!(
        planar[2],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn op8_long_odd_width_trailing_word_column() {
    // §3.2 worked example shape, shrunk: a plane 6 bytes wide (= 3 words
    // = 1.5 longs) is long-compressed as 1 LONG column + 1 trailing WORD
    // column. width = 48 → row_bytes = 6.
    let bmhd = bmhd_for(48, 2, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 6);

    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // LONG col 0 (bytes 0..4): Same(2) item 0xCAFEBABE to rows 0..2.
    // WORD col 1 (bytes 4..6): Uniq(2) items 0x1111, 0x2222 to rows 0..2.
    let mut ops: Vec<u8> = Vec::new();
    // LONG column op-count + ops (LONG items)
    push_long(&mut ops, 1); // op_count
    push_long(&mut ops, 0); // Same sentinel
    push_long(&mut ops, 2); // count
    push_long(&mut ops, 0xCAFE_BABE);
    // trailing WORD column op-count + ops (WORD items)
    push_word(&mut ops, 1); // op_count
    push_word(&mut ops, 0x8002); // Uniq cnt=2
    push_word(&mut ops, 0x1111);
    push_word(&mut ops, 0x2222);

    let mut op_ptrs = [0u32; 8];
    op_ptrs[0] = 64;
    let mut delta = make_pointer_table(op_ptrs);
    delta.extend_from_slice(&ops);

    apply_op8_for_test(&mut planar, &delta, &bmhd, /*long_data=*/ true).unwrap();

    assert_eq!(planar[0], vec![0xCA, 0xFE, 0xBA, 0xBE, 0x11, 0x11]);
    assert_eq!(planar[1], vec![0xCA, 0xFE, 0xBA, 0xBE, 0x22, 0x22]);
}

#[test]
fn op8_zero_op_pointer_leaves_plane_untouched() {
    let bmhd = bmhd_for(32, 2, 1);
    let original: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|y| vec![0x10 + y as u8, 0x20 + y as u8, 0x30, 0x40])
        .collect();
    let mut planar = original.clone();
    let delta = make_pointer_table([0; 8]);
    apply_op8_for_test(&mut planar, &delta, &bmhd, false).unwrap();
    assert_eq!(planar, original);
}

#[test]
fn op8_two_planes_independent_pointers() {
    let bmhd = bmhd_for(64, 2, 2);
    let row_bytes = bmhd.row_bytes();
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize * 2)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // Plane 0: col 0 Same item [0x11,0x22] for both rows; cols 1..3 zero.
    let mut p0: Vec<u8> = Vec::new();
    push_word(&mut p0, 1); // op_count col 0
    push_word(&mut p0, 0); // Same
    push_word(&mut p0, 2); // count
    push_word(&mut p0, 0x1122);
    push_word(&mut p0, 0); // col 1 op_count = 0
    push_word(&mut p0, 0); // col 2
    push_word(&mut p0, 0); // col 3

    // Plane 1: col 3 Same item [0x33,0x44] for both rows; cols 0..2 zero.
    let mut p1: Vec<u8> = Vec::new();
    push_word(&mut p1, 0); // col 0
    push_word(&mut p1, 0); // col 1
    push_word(&mut p1, 0); // col 2
    push_word(&mut p1, 1); // op_count col 3
    push_word(&mut p1, 0); // Same
    push_word(&mut p1, 2); // count
    push_word(&mut p1, 0x3344);

    let op0_offset = 64u32;
    let op1_offset = op0_offset + p0.len() as u32;
    let mut op_ptrs = [0u32; 8];
    op_ptrs[0] = op0_offset;
    op_ptrs[1] = op1_offset;

    let mut delta = make_pointer_table(op_ptrs);
    delta.extend_from_slice(&p0);
    delta.extend_from_slice(&p1);

    apply_op8_for_test(&mut planar, &delta, &bmhd, false).unwrap();

    let expected_plane0 = vec![0x11, 0x22, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let expected_plane1 = vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x44];
    assert_eq!(planar[0], expected_plane0); // row 0 plane 0
    assert_eq!(planar[1], expected_plane1); // row 0 plane 1
    assert_eq!(planar[2], expected_plane0); // row 1 plane 0
    assert_eq!(planar[3], expected_plane1); // row 1 plane 1
}

#[test]
fn op8_pointer_table_truncated_errors() {
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    let short_delta = vec![0u8; 32]; // short of the 64-byte table
    let res = apply_op8_for_test(&mut planar, &short_delta, &bmhd, false);
    assert!(res.is_err(), "truncated pointer table must error");
}

#[test]
fn op8_op_pointer_past_end_errors() {
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    let mut op_ptrs = [0u32; 8];
    op_ptrs[0] = 9999;
    let delta = make_pointer_table(op_ptrs);
    let res = apply_op8_for_test(&mut planar, &delta, &bmhd, false);
    assert!(res.is_err(), "op pointer past end must error");
}
