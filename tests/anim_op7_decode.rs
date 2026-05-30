//! Round-192: ANIM op-7 (Short / Long Vertical Delta) decode tests.
//!
//! These tests exercise [`apply_op7_for_test`] directly: they build a
//! synthetic op-7 DLTA payload by hand, hand it to the decoder against
//! a known previous planar state, and verify the resulting planar
//! state matches the expected post-delta image.
//!
//! Op-7 wire layout summary (from `docs/image/iff/anim.txt` §"DLTA
//! Chunk Format for method 7"):
//!
//! * 16 big-endian u32 pointers: 8 opcode-list pointers followed by 8
//!   data-list pointers (one pair per plane). A `0` pointer means the
//!   plane is unchanged.
//! * Per plane: walk columns left-to-right, `column_count =
//!   row_bytes / data_size`. Each column starts with an `op_count`
//!   byte; `op_count = 0` means the column is unchanged.
//! * Three opcode classes (per the spec):
//!   * **Skip** (hi bit clear, non-zero) — advance dest cursor by N
//!     rows; no data consumed.
//!   * **Uniq** (hi bit set) — `byte & 0x7F` data items copied
//!     literally from the data list, one per consecutive row.
//!   * **Same** (`0x00` opcode + count byte) — one data item copied
//!     `count` times to consecutive rows.
//! * "Advance one row" adds `row_bytes` (NOT `data_size`) to the
//!   byte address within the bitplane.

use oxideav_iff::anim::apply_op7_for_test;
use oxideav_iff::ilbm::{Bmhd, Compression, Masking};

/// Build a minimal 1-plane BMHD of the given dimensions. The width
/// is taken as the bitplane row width; for the op-7 tests we pick
/// 64 (→ `row_bytes = 8`) so both short (2 B) and long (4 B) data
/// modes can be exercised against the same image shape.
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

/// Serialise a 64-bit pointer table (8 opcode + 8 data) into the
/// leading 64 bytes of a DLTA payload.
fn make_pointer_table(op_offsets: [u32; 8], data_offsets: [u32; 8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for off in op_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    for off in data_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    out
}

#[test]
fn op7_short_skip_uniq_same_one_plane() {
    // 64×4 image, one plane → row_bytes = 8 → short cols = 4.
    let bmhd = bmhd_for(64, 4, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 8);

    // Start the planar state filled with 0xAA — we want to verify the
    // ops actually overwrite the correct bytes.
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0xAA; row_bytes])
        .collect();

    // Design: plane 0, short data (data_size = 2). We'll touch all 4
    // columns and exercise Skip, Same and Uniq:
    //   * col 0: op_count = 1 → Same op (0x00 0x02) → write data item
    //     [0x11, 0x22] to rows 0..2.   Row 2..3 remain 0xAA.
    //   * col 1: op_count = 1 → Uniq op 0x82 (cnt = 2) → write data
    //     items [0x33,0x44] then [0x55,0x66] to rows 0..2.
    //   * col 2: op_count = 0 → unchanged.
    //   * col 3: op_count = 2 → Skip op 0x02 (advance to row 2),
    //                              then Uniq 0x81 (cnt = 1) → write
    //                              [0x77, 0x88] at row 2.
    //
    // Opcode-list bytes (concatenated for the single plane):
    //   col0: op_count=1, op_byte=0x00, count=0x02
    //   col1: op_count=1, op_byte=0x82
    //   col2: op_count=0
    //   col3: op_count=2, op_byte=0x02, op_byte=0x81
    let opcodes: Vec<u8> = vec![
        // col 0
        1, 0x00, 0x02, // col 1
        1, 0x82, // col 2
        0,    // col 3
        2, 0x02, 0x81,
    ];

    // Data items in the order opcodes consume them (Same col0 first,
    // then Uniq col1 first/second, then Uniq col3 first).
    let data_items: Vec<u8> = vec![
        0x11, 0x22, // col 0 Same
        0x33, 0x44, // col 1 Uniq #1
        0x55, 0x66, // col 1 Uniq #2
        0x77, 0x88, // col 3 Uniq #1
    ];

    // Assemble DLTA: 64-byte pointer table, then opcodes, then data.
    let op_offset = 64u32;
    let data_offset = op_offset + opcodes.len() as u32;
    let mut op_ptrs = [0u32; 8];
    let mut data_ptrs = [0u32; 8];
    op_ptrs[0] = op_offset;
    data_ptrs[0] = data_offset;
    let mut delta = make_pointer_table(op_ptrs, data_ptrs);
    delta.extend_from_slice(&opcodes);
    delta.extend_from_slice(&data_items);

    apply_op7_for_test(&mut planar, &delta, &bmhd, /*long_data=*/ false).unwrap();

    // Expected state per row (bytes 0..8 of plane 0):
    //   row 0: col0=0x11,0x22 | col1=0x33,0x44 | col2=0xAA,0xAA | col3=0xAA,0xAA
    //   row 1: col0=0x11,0x22 | col1=0x55,0x66 | col2=0xAA,0xAA | col3=0xAA,0xAA
    //   row 2: col0=0xAA,0xAA | col1=0xAA,0xAA | col2=0xAA,0xAA | col3=0x77,0x88
    //   row 3: col0=0xAA,0xAA | col1=0xAA,0xAA | col2=0xAA,0xAA | col3=0xAA,0xAA
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
        vec![0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0x77, 0x88]
    );
    assert_eq!(
        planar[3],
        vec![0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]
    );
}

#[test]
fn op7_long_data_one_plane() {
    // Same shape but long data items (4 B). row_bytes = 8 → 2 cols.
    let bmhd = bmhd_for(64, 3, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 8);

    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // col 0: Same(0x03) → item [0xDE, 0xAD, 0xBE, 0xEF] to rows 0..3.
    // col 1: Uniq(0x82) → items [0x12,0x34,0x56,0x78] then
    //                            [0x9A,0xBC,0xDE,0xF0] to rows 0..2.
    //   (row 2 of col 1 keeps the 0x00 fill.)
    let opcodes: Vec<u8> = vec![1, 0x00, 0x03, 1, 0x82];
    let data_items: Vec<u8> = vec![
        0xDE, 0xAD, 0xBE, 0xEF, // col 0 Same
        0x12, 0x34, 0x56, 0x78, // col 1 Uniq #1
        0x9A, 0xBC, 0xDE, 0xF0, // col 1 Uniq #2
    ];

    let op_offset = 64u32;
    let data_offset = op_offset + opcodes.len() as u32;
    let mut op_ptrs = [0u32; 8];
    let mut data_ptrs = [0u32; 8];
    op_ptrs[0] = op_offset;
    data_ptrs[0] = data_offset;
    let mut delta = make_pointer_table(op_ptrs, data_ptrs);
    delta.extend_from_slice(&opcodes);
    delta.extend_from_slice(&data_items);

    apply_op7_for_test(&mut planar, &delta, &bmhd, /*long_data=*/ true).unwrap();

    assert_eq!(
        planar[0],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78]
    );
    assert_eq!(
        planar[1],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x9A, 0xBC, 0xDE, 0xF0]
    );
    // row 2 of col 1 wasn't touched by op-7; col 0 row 2 = Same item.
    assert_eq!(
        planar[2],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn op7_zero_op_pointer_leaves_plane_untouched() {
    // 1 plane, all-zero pointer table → planar state must be unchanged.
    let bmhd = bmhd_for(32, 2, 1);
    let original: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|y| vec![0x10 + y as u8, 0x20 + y as u8, 0x30, 0x40])
        .collect();
    let mut planar = original.clone();
    let delta = make_pointer_table([0; 8], [0; 8]);
    apply_op7_for_test(&mut planar, &delta, &bmhd, false).unwrap();
    assert_eq!(planar, original);
}

#[test]
fn op7_pointer_table_truncated_errors() {
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    // Only 32 bytes — short of the 64-byte op-7 pointer table.
    let short_delta = vec![0u8; 32];
    let res = apply_op7_for_test(&mut planar, &short_delta, &bmhd, false);
    assert!(res.is_err(), "truncated pointer table must error");
}

#[test]
fn op7_op_pointer_past_end_errors() {
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    let mut op_ptrs = [0u32; 8];
    op_ptrs[0] = 9999;
    let delta = make_pointer_table(op_ptrs, [0; 8]);
    let res = apply_op7_for_test(&mut planar, &delta, &bmhd, false);
    assert!(
        res.is_err(),
        "op pointer past end of DLTA payload must error"
    );
}

#[test]
fn op7_two_planes_independent_pointers() {
    // 2 planes, each touched at a single distinct column. Verifies the
    // pointer-table lookup picks the right (op, data) pair per plane
    // and doesn't leak state between planes.
    let bmhd = bmhd_for(64, 2, 2);
    let row_bytes = bmhd.row_bytes();
    // planes_per_row = 2 (no mask plane), rows interleave plane bytes.
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize * 2)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // Plane 0: col 0 Same item [0x11, 0x22] for both rows.
    // Plane 1: col 3 Same item [0x33, 0x44] for both rows.
    let plane0_ops: Vec<u8> = vec![1, 0x00, 0x02]; // col 0: Same(2). cols 1..3: op_count=0.
    let plane0_ops_full: Vec<u8> = [plane0_ops.as_slice(), &[0u8, 0u8, 0u8]].concat();
    let plane0_data: Vec<u8> = vec![0x11, 0x22];

    let plane1_ops: Vec<u8> = vec![0u8, 0u8, 0u8, 1, 0x00, 0x02];
    let plane1_data: Vec<u8> = vec![0x33, 0x44];

    let op0_offset = 64u32;
    let op1_offset = op0_offset + plane0_ops_full.len() as u32;
    let data0_offset = op1_offset + plane1_ops.len() as u32;
    let data1_offset = data0_offset + plane0_data.len() as u32;

    let mut op_ptrs = [0u32; 8];
    let mut data_ptrs = [0u32; 8];
    op_ptrs[0] = op0_offset;
    op_ptrs[1] = op1_offset;
    data_ptrs[0] = data0_offset;
    data_ptrs[1] = data1_offset;

    let mut delta = make_pointer_table(op_ptrs, data_ptrs);
    delta.extend_from_slice(&plane0_ops_full);
    delta.extend_from_slice(&plane1_ops);
    delta.extend_from_slice(&plane0_data);
    delta.extend_from_slice(&plane1_data);

    apply_op7_for_test(&mut planar, &delta, &bmhd, false).unwrap();

    // Planar layout for n_planes=2, no mask: row y plane p = index y*2+p.
    // Plane 0, both rows: col 0 = 0x11, 0x22; rest 0x00.
    // Plane 1, both rows: col 3 = 0x33, 0x44; rest 0x00.
    let expected_plane0 = vec![0x11, 0x22, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let expected_plane1 = vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x44];
    assert_eq!(planar[0], expected_plane0); // row 0 plane 0
    assert_eq!(planar[1], expected_plane1); // row 0 plane 1
    assert_eq!(planar[2], expected_plane0); // row 1 plane 0
    assert_eq!(planar[3], expected_plane1); // row 1 plane 1
}
