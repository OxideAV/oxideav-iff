//! Round-287: ANIM op-4 (Generalized short/long Delta mode) decode +
//! encode tests.
//!
//! Op-4's only normative wire description is the §2.2.2
//! `SetDLTAshort` reference routine in `docs/image/iff/anim.txt`.
//! Its salient properties, which these tests pin down:
//!
//! * The DLTA opens with 16 big-endian u32 pointers — 8 *data*-list
//!   pointers (one per plane) then 8 *op*-list pointers — and each
//!   pointer is measured in **16-bit words** (the routine does
//!   `WORD*`-pointer arithmetic: `data = deltaword + deltadata[i]`).
//! * Each plane's op list is a flat run of `(offset, size)` short
//!   pairs terminated by `0xFFFF`. `offset` is the **absolute** word
//!   position of the run's first row (`dest = planeptr + offset`);
//!   descending a column steps the dest pointer by `nw = row_bytes /
//!   word_size` words per row.
//! * `size > 0` (Uniq) copies `size` data words, one per consecutive
//!   row. `size < 0` (Same) copies ONE data word to `|size|`
//!   consecutive rows.
//!
//! The hand-built DLTA tests drive [`apply_op4_for_test`] directly; the
//! round-trip tests exercise [`encode_op4_body`] / [`encode_anim_op4`]
//! against the in-tree decoder.

use oxideav_iff::anim::{apply_op4_for_test, encode_anim_op4, encode_op4_body, parse_anim};
use oxideav_iff::ilbm::{Bmhd, Camg, Compression, IlbmImage, Masking};

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

/// Serialise the 16-pointer table: 8 data-list pointers (slots 0..=7)
/// then 8 op-list pointers (slots 8..=15), each a u32 BE word offset.
fn make_pointer_table(data_word_offsets: [u32; 8], op_word_offsets: [u32; 8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for off in data_word_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    for off in op_word_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    out
}

/// `ANHD.bits` for the documented short-data / vertical / RLC config.
const BITS_SHORT: u32 = 0b0000_1000 | 0b0001_0000; // RLC | vertical
/// Same but long data (bit 0 set).
const BITS_LONG: u32 = 0b1 | 0b0000_1000 | 0b0001_0000;

#[test]
fn op4_short_same_and_uniq_one_plane() {
    // 64×4 image, one plane → row_bytes = 8 → short words per row nw = 4.
    let bmhd = bmhd_for(64, 4, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 8); // short words per row nw = 4

    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0xAA; row_bytes])
        .collect();

    // Design (word units; word = 2 bytes):
    //   * column 0 (abs word 0, stride nw=4): Same op size=-2 →
    //     data word [0x11,0x22] to rows 0 and 1.
    //   * column 1 (abs word 1): Uniq op size=+2 → data words
    //     [0x33,0x44] (row0) then [0x55,0x66] (row1).
    //   * column 3 (abs word 3): Same op at row 2 (abs word 3 + 2*nw =
    //     11), size=-1 → [0x77,0x88] to row 2 only.
    //
    // op list (offset, size) pairs, terminated by 0xFFFF:
    let mut op_list: Vec<u8> = Vec::new();
    // col 0: offset 0, size -2
    op_list.extend_from_slice(&0u16.to_be_bytes());
    op_list.extend_from_slice(&(-2i16).to_be_bytes());
    // col 1: offset 1, size +2
    op_list.extend_from_slice(&1u16.to_be_bytes());
    op_list.extend_from_slice(&2i16.to_be_bytes());
    // col 3 row 2: abs word = 3 + 2*4 = 11, size -1
    op_list.extend_from_slice(&11u16.to_be_bytes());
    op_list.extend_from_slice(&(-1i16).to_be_bytes());
    op_list.extend_from_slice(&0xFFFFu16.to_be_bytes());

    // data list in op-consumption order: Same#1 word, Uniq#1, Uniq#2,
    // Same#2 word.
    let mut data_list: Vec<u8> = Vec::new();
    data_list.extend_from_slice(&[0x11, 0x22]); // col0 Same
    data_list.extend_from_slice(&[0x33, 0x44]); // col1 Uniq row0
    data_list.extend_from_slice(&[0x55, 0x66]); // col1 Uniq row1
    data_list.extend_from_slice(&[0x77, 0x88]); // col3 Same

    // Assemble. Pointers are word offsets. The 64-byte table = 32
    // words. Op list comes first, then data list.
    let op_word0 = 32u32;
    let data_word0 = op_word0 + (op_list.len() / 2) as u32;
    let mut data_ptrs = [0u32; 8];
    let mut op_ptrs = [0u32; 8];
    data_ptrs[0] = data_word0;
    op_ptrs[0] = op_word0;
    let mut delta = make_pointer_table(data_ptrs, op_ptrs);
    delta.extend_from_slice(&op_list);
    delta.extend_from_slice(&data_list);

    apply_op4_for_test(BITS_SHORT, &mut planar, &delta, &bmhd).unwrap();

    // Expected per row (bytes 0..8):
    //   row 0: col0=11,22 | col1=33,44 | col2=AA,AA | col3=AA,AA
    //   row 1: col0=11,22 | col1=55,66 | col2=AA,AA | col3=AA,AA
    //   row 2: col0=AA,AA | col1=AA,AA | col2=AA,AA | col3=77,88
    //   row 3: all AA
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
fn op4_long_data_one_plane() {
    // 64×3, one plane → row_bytes = 8 → long words per row nw = 2.
    let bmhd = bmhd_for(64, 3, 1);
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 8);

    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // col 0 (abs word 0): Same size=-3 → [DE,AD,BE,EF] rows 0..3.
    // col 1 (abs word 1): Uniq size=+2 → [12,34,56,78] row0,
    //                                    [9A,BC,DE,F0] row1.
    let mut op_list: Vec<u8> = Vec::new();
    op_list.extend_from_slice(&0u16.to_be_bytes());
    op_list.extend_from_slice(&(-3i16).to_be_bytes());
    op_list.extend_from_slice(&1u16.to_be_bytes());
    op_list.extend_from_slice(&2i16.to_be_bytes());
    op_list.extend_from_slice(&0xFFFFu16.to_be_bytes());

    let mut data_list: Vec<u8> = Vec::new();
    data_list.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // col0 Same
    data_list.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]); // col1 row0
    data_list.extend_from_slice(&[0x9A, 0xBC, 0xDE, 0xF0]); // col1 row1

    let op_word0 = 32u32;
    let data_word0 = op_word0 + (op_list.len() / 2) as u32;
    let mut data_ptrs = [0u32; 8];
    let mut op_ptrs = [0u32; 8];
    data_ptrs[0] = data_word0;
    op_ptrs[0] = op_word0;
    let mut delta = make_pointer_table(data_ptrs, op_ptrs);
    delta.extend_from_slice(&op_list);
    delta.extend_from_slice(&data_list);

    apply_op4_for_test(BITS_LONG, &mut planar, &delta, &bmhd).unwrap();

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
fn op4_zero_op_pointer_leaves_plane_untouched() {
    let bmhd = bmhd_for(32, 2, 1);
    let original: Vec<Vec<u8>> = (0..bmhd.height as usize)
        .map(|y| vec![0x10 + y as u8, 0x20 + y as u8, 0x30, 0x40])
        .collect();
    let mut planar = original.clone();
    let delta = make_pointer_table([0; 8], [0; 8]);
    apply_op4_for_test(BITS_SHORT, &mut planar, &delta, &bmhd).unwrap();
    assert_eq!(planar, original);
}

#[test]
fn op4_shared_info_list_two_planes() {
    // `ANHD.bits` bit 2 (shared info) lets all planes point at the same
    // op list. The reference routine handles this transparently because
    // it dereferences each slot independently — so an op list shared by
    // two planes applies the same vertical writes to both. Verify our
    // decoder honours a repeated op pointer.
    let bmhd = bmhd_for(32, 2, 2); // row_bytes = 4, nw(short) = 2
    let row_bytes = bmhd.row_bytes();
    assert_eq!(row_bytes, 4);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize * 2)
        .map(|_| vec![0x00; row_bytes])
        .collect();

    // One op list: col 0 Same size=-2 → word [0xEE,0xFF] to rows 0,1.
    let mut op_list: Vec<u8> = Vec::new();
    op_list.extend_from_slice(&0u16.to_be_bytes());
    op_list.extend_from_slice(&(-2i16).to_be_bytes());
    op_list.extend_from_slice(&0xFFFFu16.to_be_bytes());
    // Each plane needs its own data word in its own data list (data is
    // consumed per-plane from its data pointer). Use distinct data per
    // plane so we can tell them apart.
    let data0: Vec<u8> = vec![0xEE, 0xFF];
    let data1: Vec<u8> = vec![0x11, 0x99];

    let op_word0 = 32u32; // shared op list right after the table
    let data0_word = op_word0 + (op_list.len() / 2) as u32;
    let data1_word = data0_word + (data0.len() / 2) as u32;
    let mut data_ptrs = [0u32; 8];
    let mut op_ptrs = [0u32; 8];
    // Both planes share the same op-list pointer (bit 2 semantics).
    op_ptrs[0] = op_word0;
    op_ptrs[1] = op_word0;
    data_ptrs[0] = data0_word;
    data_ptrs[1] = data1_word;
    let mut delta = make_pointer_table(data_ptrs, op_ptrs);
    delta.extend_from_slice(&op_list);
    delta.extend_from_slice(&data0);
    delta.extend_from_slice(&data1);

    // bit 2 set = shared info list.
    apply_op4_for_test(BITS_SHORT | 0b0000_0100, &mut planar, &delta, &bmhd).unwrap();

    // Planar layout n_planes=2: row y plane p = index y*2 + p.
    assert_eq!(planar[0], vec![0xEE, 0xFF, 0x00, 0x00]); // row0 plane0
    assert_eq!(planar[1], vec![0x11, 0x99, 0x00, 0x00]); // row0 plane1
    assert_eq!(planar[2], vec![0xEE, 0xFF, 0x00, 0x00]); // row1 plane0
    assert_eq!(planar[3], vec![0x11, 0x99, 0x00, 0x00]); // row1 plane1
}

#[test]
fn op4_pointer_table_truncated_errors() {
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    let short_delta = vec![0u8; 32]; // < 64-byte table
    assert!(apply_op4_for_test(BITS_SHORT, &mut planar, &short_delta, &bmhd).is_err());
}

#[test]
fn op4_xor_mode_rejected() {
    // bit 1 set = XOR; no documented merge semantics → Unsupported.
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    let delta = make_pointer_table([0; 8], [0; 8]);
    let res = apply_op4_for_test(BITS_SHORT | 0b0000_0010, &mut planar, &delta, &bmhd);
    assert!(res.is_err(), "XOR mode must be rejected");
}

#[test]
fn op4_reserved_bits_rejected() {
    // A reserved high bit must be rejected per §2.1 ("Player code should
    // check undefined bits … to assure they are zero").
    let bmhd = bmhd_for(32, 2, 1);
    let mut planar: Vec<Vec<u8>> = (0..bmhd.height as usize).map(|_| vec![0u8; 4]).collect();
    let delta = make_pointer_table([0; 8], [0; 8]);
    let res = apply_op4_for_test(BITS_SHORT | (1 << 20), &mut planar, &delta, &bmhd);
    assert!(res.is_err(), "reserved bit must be rejected");
}

// ---- encode → decode round-trips ----

fn solid_palette() -> Vec<[u8; 3]> {
    vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]]
}

fn frame_pattern(w: u16, h: u16, seed: u8, palette: Vec<[u8; 3]>) -> IlbmImage {
    let bmhd = bmhd_for(w, h, 2);
    let pal_len = palette.len();
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let idx = (x + y + seed as usize) % pal_len;
            let p = palette[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    IlbmImage {
        width: w as u32,
        height: h as u32,
        bmhd,
        palette,
        camg: Camg::default(),
        rgba,
        ..IlbmImage::default()
    }
}

#[test]
fn op4_encode_decode_body_short_roundtrip() {
    use oxideav_iff::ilbm::indices_to_planar_row;
    // Build two synthetic planar frames directly (2 planes, 16×4).
    let bmhd = bmhd_for(16, 4, 2);
    let row_bytes = bmhd.row_bytes(); // 2
    let planes_per_row = 2usize;
    let height = 4usize;

    let mk = |f: &dyn Fn(usize, usize) -> u8| -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for y in 0..height {
            let indices: Vec<u8> = (0..16).map(|x| f(x, y)).collect();
            let rows = indices_to_planar_row(&indices, 2, row_bytes);
            for r in rows {
                out.push(r);
            }
        }
        out
    };
    let prev = mk(&|x, y| ((x + y) % 4) as u8);
    let cur = mk(&|x, y| ((x * 2 + y) % 4) as u8);

    for long_data in [false, true] {
        if long_data && row_bytes % 4 != 0 {
            // row_bytes=2 can't hold a 4-byte word — encoder should
            // reject, which we assert and skip.
            assert!(encode_op4_body(&prev, &cur, &bmhd, true).is_err());
            continue;
        }
        let dlta = encode_op4_body(&prev, &cur, &bmhd, long_data).unwrap();
        let mut state = prev.clone();
        let bits = if long_data { BITS_LONG } else { BITS_SHORT };
        apply_op4_for_test(bits, &mut state, &dlta, &bmhd).unwrap();
        assert_eq!(
            state, cur,
            "op4 encode→decode must reproduce the target frame (long_data={long_data})"
        );
        let _ = planes_per_row;
    }
}

#[test]
fn op4_full_container_roundtrip_short() {
    let pal = solid_palette();
    let frames = vec![
        frame_pattern(16, 6, 0, pal.clone()),
        frame_pattern(16, 6, 1, pal.clone()),
        frame_pattern(16, 6, 2, pal.clone()),
    ];
    let bytes = encode_anim_op4(&frames, false).unwrap();
    assert_eq!(&bytes[0..4], b"FORM");
    assert_eq!(&bytes[8..12], b"ANIM");
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3);
    assert_eq!(dec.width, 16);
    assert_eq!(dec.height, 6);
    // Every frame must match the source pixels (indexed 2-plane data
    // round-trips losslessly through the nearest-palette planar path).
    for (i, f) in dec.frames.iter().enumerate() {
        assert_eq!(
            f.rgba, frames[i].rgba,
            "frame {i} round-trips pixel-exactly through op-4"
        );
    }
}

#[test]
fn op4_full_container_roundtrip_long_4plane() {
    // 4-plane image with width 32 → row_bytes = 4, divisible by 4 so
    // long-data mode is legal. 16-colour palette.
    let palette: Vec<[u8; 3]> = (0..16)
        .map(|i| [(i * 16) as u8, (255 - i * 16) as u8, (i * 8) as u8])
        .collect();
    let bmhd = Bmhd {
        n_planes: 4,
        ..bmhd_for(32, 5, 4)
    };
    let mk = |seed: u8| -> IlbmImage {
        let mut rgba = Vec::new();
        for y in 0..5usize {
            for x in 0..32usize {
                let idx = (x + y * 3 + seed as usize) % 16;
                let p = palette[idx];
                rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
            }
        }
        IlbmImage {
            width: 32,
            height: 5,
            bmhd,
            palette: palette.clone(),
            camg: Camg::default(),
            rgba,
            ..IlbmImage::default()
        }
    };
    let frames = vec![mk(0), mk(5), mk(9)];
    let bytes = encode_anim_op4(&frames, true).unwrap();
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3);
    for (i, f) in dec.frames.iter().enumerate() {
        assert_eq!(
            f.rgba, frames[i].rgba,
            "frame {i} long-data op-4 round-trip"
        );
    }
}
