//! PCHG Huffman-compressed LineData (`Compression == 1`).
//!
//! The compressed form is: 20-byte PCHGHeader, 8-byte PCHGCompHeader
//! (`u32 CompInfoSize`, `u32 OriginalDataSize`), the serialized tree
//! (big-endian signed 16-bit nodes), then the MSB-first bitstream.
//! Decoding walks from the **end** of the node array: a `1` bit
//! inspects the current node (non-negative = leaf symbol in the low
//! byte, negative = byte offset to follow, halved into node units); a
//! `0` bit steps to the previous node and emits it if it carries the
//! `0x0100` leaf tag.

use oxideav_iff::ilbm::{Pchg, PchgChange, PchgKind, PchgLine, PCHG_COMP_HUFFMAN};

fn line(l: u32, changes: &[(u16, [u8; 3])]) -> PchgLine {
    PchgLine {
        line: l,
        changes: changes
            .iter()
            .map(|&(index, rgb)| PchgChange::new(index, rgb))
            .collect(),
    }
}

/// 20-byte PCHG header with Huffman compression flagged.
fn header(flags: u16, start_line: i16, line_count: u16, hints: [u16; 4], total: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    raw.extend_from_slice(&PCHG_COMP_HUFFMAN.to_be_bytes());
    raw.extend_from_slice(&flags.to_be_bytes());
    raw.extend_from_slice(&start_line.to_be_bytes());
    raw.extend_from_slice(&line_count.to_be_bytes());
    for h in hints {
        raw.extend_from_slice(&h.to_be_bytes());
    }
    raw.extend_from_slice(&total.to_be_bytes());
    raw
}

#[test]
fn hand_built_tree_decodes_per_spec_walk() {
    // LineData to reconstruct (Small, LineCount = 1):
    //   mask 80 00 00 00, record 01 00 (counts), word 10 F0
    // i.e. bytes {0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x10, 0xF0}.
    //
    // Hand-laid tree (7 nodes, root in the last slot):
    //   idx 0: 0x0110  left-leaf marker for 0x10
    //   idx 1: 0x00F0  internal: right branch = leaf 0xF0
    //   idx 2: 0x0101  left-leaf marker for 0x01
    //   idx 3: 0xFFFC  internal: right branch = link -4 bytes → idx 1
    //   idx 4: 0x0080  internal: right branch = leaf 0x80
    //   idx 5: 0x0100  left-leaf marker for 0x00
    //   idx 6: 0xFFFC  root: right branch = link -4 bytes → idx 4
    //
    // Codes: 0x00 = 0, 0x80 = 11, 0x01 = 100, 0x10 = 1010, 0xF0 = 1011.
    let tree: [u16; 7] = [0x0110, 0x00F0, 0x0101, 0xFFFC, 0x0080, 0x0100, 0xFFFC];
    // 0x80 0x00 0x00 0x00 0x01 0x00 0x10 0xF0 →
    // 11 0 0 0 100 0 1010 1011 = 1100 0100 0101 0101 1(000...)
    let stream: [u8; 3] = [0xC4, 0x55, 0x80];

    let mut raw = header(1, 0, 1, [1, 1, 1, 1], 1);
    raw.extend_from_slice(&(tree.len() as u32 * 2).to_be_bytes()); // CompInfoSize
    raw.extend_from_slice(&8u32.to_be_bytes()); // OriginalDataSize
    for n in tree {
        raw.extend_from_slice(&n.to_be_bytes());
    }
    raw.extend_from_slice(&stream);

    let pchg = Pchg::parse(&raw).unwrap();
    assert!(pchg.header().unwrap().is_compressed());
    assert_eq!(pchg.lines.len(), 1);
    assert_eq!(pchg.lines[0].line, 0);
    assert_eq!(
        pchg.lines[0].changes,
        vec![PchgChange::new(1, [0, 0xFF, 0])]
    );
}

#[test]
fn degenerate_single_symbol_tree_decodes() {
    // LineData = 4 zero bytes (a LineCount-32 mask with no set bits).
    // Tree: one slot, 0x0100 | 0x00 — every `1` bit emits 0x00.
    let mut raw = header(1, 0, 32, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&2u32.to_be_bytes()); // CompInfoSize
    raw.extend_from_slice(&4u32.to_be_bytes()); // OriginalDataSize
    raw.extend_from_slice(&[0x01, 0x00]); // the lone node
    raw.push(0xF0); // four `1` bits
    let pchg = Pchg::parse(&raw).unwrap();
    assert!(pchg.lines.is_empty());
}

#[test]
fn huffman_roundtrip_small() {
    let lines = vec![
        line(0, &[(1, [0x00, 0xFF, 0x00]), (17, [0x11, 0x22, 0x33])]),
        line(9, &[(2, [0xAA, 0xBB, 0xCC])]),
        line(63, &[(31, [0xFF, 0xFF, 0xFF])]),
    ];
    let src = Pchg::from_lines(lines.clone(), PchgKind::Small);
    let compressed = src.encode_huffman(PchgKind::Small);
    let back = Pchg::parse(&compressed).unwrap();
    assert!(back.header().unwrap().is_compressed());
    assert_eq!(back.lines, src.lines);
    assert_eq!(back.lines, lines);
}

#[test]
fn huffman_roundtrip_big_with_alpha() {
    let mut ch = PchgChange::new(300, [0x12, 0x34, 0x56]);
    ch.alpha = Some(0x77);
    let lines = vec![
        PchgLine {
            line: 2,
            changes: vec![ch, PchgChange::new(5, [0xAB, 0xCD, 0xEF])],
        },
        line(200, &[(0, [0x01, 0x02, 0x03])]),
    ];
    let src = Pchg::from_lines(lines, PchgKind::Big);
    let back = Pchg::parse(&src.encode_huffman(PchgKind::Big)).unwrap();
    assert_eq!(back.lines, src.lines);
    assert_eq!(back.lines[0].changes[0].alpha, Some(0x77));
    assert_eq!(back.lines[0].changes[1].alpha, Some(0xFF));
}

#[test]
fn huffman_roundtrip_wide_symbol_spread() {
    // Many distinct byte values → a deep, multi-link tree.
    let lines: Vec<PchgLine> = (0..96u32)
        .map(|i| {
            line(
                i * 2,
                &[(
                    (i % 256) as u16,
                    [
                        (i * 37 % 256) as u8,
                        (i * 91 % 256) as u8,
                        (i * 53 % 256) as u8,
                    ],
                )],
            )
        })
        .collect();
    let src = Pchg::from_lines(lines, PchgKind::Big);
    let back = Pchg::parse(&src.encode_huffman(PchgKind::Big)).unwrap();
    assert_eq!(back.lines, src.lines);
}

#[test]
fn huffman_and_uncompressed_decode_identically() {
    let lines = vec![
        line(1, &[(4, [0x44, 0x44, 0x44])]),
        line(3, &[(6, [0x66, 0x66, 0x66]), (7, [0x77, 0x77, 0x77])]),
    ];
    let src = Pchg::from_lines(lines, PchgKind::Small);
    let plain = Pchg::parse(&src.encode(PchgKind::Small)).unwrap();
    let packed = Pchg::parse(&src.encode_huffman(PchgKind::Small)).unwrap();
    assert_eq!(plain.lines, packed.lines);
}

#[test]
fn empty_change_list_compresses_to_header_only_stream() {
    let src = Pchg::from_lines(Vec::new(), PchgKind::Small);
    let bytes = src.encode_huffman(PchgKind::Small);
    let back = Pchg::parse(&bytes).unwrap();
    assert!(back.lines.is_empty());
}

// ───────────────────── malformed compressed payloads ─────────────────────

#[test]
fn missing_comp_header_is_rejected() {
    let mut raw = header(1, 0, 1, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&[0, 0, 0]); // 3 of the 8 PCHGCompHeader bytes
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn comp_info_overrunning_chunk_is_rejected() {
    let mut raw = header(1, 0, 1, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&1000u32.to_be_bytes()); // CompInfoSize
    raw.extend_from_slice(&8u32.to_be_bytes()); // OriginalDataSize
    raw.extend_from_slice(&[0; 8]); // far less than 1000 bytes
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn odd_comp_info_size_is_rejected() {
    let mut raw = header(1, 0, 1, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&3u32.to_be_bytes());
    raw.extend_from_slice(&8u32.to_be_bytes());
    raw.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF]);
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn oversized_original_data_size_is_rejected() {
    // Claims 4 GiB-ish output from a 1-byte stream: unsatisfiable.
    let mut raw = header(1, 0, 1, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&2u32.to_be_bytes());
    raw.extend_from_slice(&0xFFFF_0000u32.to_be_bytes());
    raw.extend_from_slice(&[0x01, 0x00]);
    raw.push(0xFF);
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn exhausted_bitstream_is_rejected() {
    // Single-leaf tree, 8 stream bits, but 9 bytes claimed... the
    // capacity guard uses bits, so claim 8 bytes from 4 bits instead.
    let mut raw = header(1, 0, 32, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&2u32.to_be_bytes());
    raw.extend_from_slice(&8u32.to_be_bytes()); // wants 8 bytes
    raw.extend_from_slice(&[0x01, 0x00]);
    raw.push(0b1111_0000); // stream: only emits on `1` bits → 4 bytes
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn left_walk_out_of_tree_is_rejected() {
    // Single-node tree and a leading `0` bit: p is already at slot 0.
    let mut raw = header(1, 0, 32, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&2u32.to_be_bytes());
    raw.extend_from_slice(&1u32.to_be_bytes());
    raw.extend_from_slice(&[0x01, 0x00]);
    raw.push(0b0000_0000);
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn link_walk_out_of_tree_is_rejected() {
    // Root node holds a huge negative link.
    let mut raw = header(1, 0, 32, [0, 0, 0, 0], 0);
    raw.extend_from_slice(&2u32.to_be_bytes());
    raw.extend_from_slice(&1u32.to_be_bytes());
    raw.extend_from_slice(&0x8000u16.to_be_bytes()); // -32768
    raw.push(0b1000_0000);
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn unknown_compression_mode_is_rejected() {
    let mut raw = header(1, 0, 0, [0, 0, 0, 0], 0);
    raw[0..2].copy_from_slice(&2u16.to_be_bytes()); // Compression = 2
    assert!(Pchg::parse(&raw).is_err());
}

#[test]
fn compressed_raw_bytes_roundtrip_verbatim() {
    let src = Pchg::from_lines(vec![line(0, &[(1, [0x22, 0x44, 0x66])])], PchgKind::Big);
    let bytes = src.encode_huffman(PchgKind::Big);
    let back = Pchg::parse(&bytes).unwrap();
    assert_eq!(back.raw, bytes, "wire bytes preserved for round-trip");
}
