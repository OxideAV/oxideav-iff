//! Cross-form integration tests for the generic top-level group probe.
//!
//! The four EA IFF 85 / Apple AIFF forms this crate handles (`FORM
//! 8SVX`, `FORM ILBM`, `FORM ANIM`, `FORM AIFF`) all hand their
//! per-form parsers a fresh outer-FORM envelope. The probe primitive
//! introduced in [`oxideav_iff::chunk::probe_top_level_group`] is the
//! shared front-half — these tests exercise it against bytes produced
//! by each form's own writer (or a minimal hand-rolled equivalent
//! when the form has no encoder yet) so a regression in the probe
//! immediately shows up here regardless of which form it broke.

use oxideav_iff::chunk::{probe_top_level_group, read_top_level_group, GroupKind};
use oxideav_iff::ilbm::{encode_ilbm, Bmhd, Camg, Compression, IlbmImage, Masking};

// ───────────────────── helpers ─────────────────────

fn tiny_ilbm() -> IlbmImage {
    let pal: Vec<[u8; 3]> = (0..4u8)
        .map(|i| [i * 64, 255 - i * 64, i.wrapping_mul(33)])
        .collect();
    let bmhd = Bmhd {
        width: 4,
        height: 2,
        x_origin: 0,
        y_origin: 0,
        n_planes: 2,
        masking: Masking::None,
        compression: Compression::None,
        pad: 0,
        transparent_color: 0,
        x_aspect: 1,
        y_aspect: 1,
        page_width: 4,
        page_height: 2,
    };
    let mut rgba = Vec::with_capacity(4 * 2 * 4);
    for y in 0..2u32 {
        for x in 0..4u32 {
            let idx = ((x ^ y) & 0x3) as usize;
            let p = pal[idx];
            rgba.extend_from_slice(&[p[0], p[1], p[2], 0xFF]);
        }
    }
    IlbmImage {
        width: 4,
        height: 2,
        bmhd,
        palette: pal,
        camg: Camg::default(),
        rgba,
        form_type: *b"ILBM",
        ..IlbmImage::default()
    }
}

/// Build a minimal `FORM 8SVX { VHDR, BODY }` file the way `svx.rs`
/// tests do — this keeps the probe-side test independent of the 8SVX
/// encoder's compression-mode defaults.
fn tiny_8svx() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    let size_at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(b"8SVX");
    // VHDR (20 bytes): oneShotHiSamples=4, repeat=0, samplesPerHiCycle=0,
    // samplesPerSec=8000, ctOctave=1, sCompression=0, volume=0x10000.
    out.extend_from_slice(b"VHDR");
    out.extend_from_slice(&20u32.to_be_bytes());
    out.extend_from_slice(&4u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&8000u16.to_be_bytes());
    out.push(1);
    out.push(0);
    out.extend_from_slice(&0x10000u32.to_be_bytes());
    // BODY: 4 PCM-S8 samples.
    out.extend_from_slice(b"BODY");
    out.extend_from_slice(&4u32.to_be_bytes());
    out.extend_from_slice(&[1, 2, 3, 4]);
    // Patch outer FORM size: total - 8 (outer chunk header).
    let total = out.len();
    let size = (total - 8) as u32;
    out[size_at..size_at + 4].copy_from_slice(&size.to_be_bytes());
    out
}

/// Build the smallest legal `FORM AIFF { COMM, SSND }` shell: one
/// channel, two 8-bit frames, sample rate 8000 Hz. The probe only
/// looks at the outer envelope, so the COMM/SSND payloads can stay
/// minimal.
fn tiny_aiff() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    let size_at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(b"AIFF");
    // COMM (18 bytes): numChannels=1, numSampleFrames=2, sampleSize=8,
    // sampleRate=80-bit IEEE 754 extended for 8000.0 Hz.
    out.extend_from_slice(b"COMM");
    out.extend_from_slice(&18u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&8u16.to_be_bytes());
    // 80-bit IEEE 754 extended for 8000.0 — sign=0, exponent=0x400B,
    // significand (explicit-leading-bit) = 0xFA00_0000_0000_0000.
    out.extend_from_slice(&[0x40, 0x0B, 0xFA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    // SSND (8-byte payload header + 2 sample bytes): offset=0, blockSize=0.
    out.extend_from_slice(b"SSND");
    out.extend_from_slice(&10u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00]);
    let total = out.len();
    let size = (total - 8) as u32;
    out[size_at..size_at + 4].copy_from_slice(&size.to_be_bytes());
    out
}

// ───────────────────── ILBM ─────────────────────

#[test]
fn probe_recognises_ilbm_envelope_from_encode_ilbm() {
    let bytes = encode_ilbm(&tiny_ilbm()).unwrap();
    let g = probe_top_level_group(&bytes).unwrap().unwrap();
    assert_eq!(g.kind, GroupKind::Form);
    assert_eq!(&g.inner_type, b"ILBM");
    // declared_total_len matches the actual envelope length to the
    // byte — ILBM emits an even-sized FORM (no trailing pad).
    assert_eq!(g.declared_total_len() as usize, bytes.len());
}

// ───────────────────── 8SVX ─────────────────────

#[test]
fn probe_recognises_8svx_envelope() {
    let bytes = tiny_8svx();
    let g = probe_top_level_group(&bytes).unwrap().unwrap();
    assert_eq!(g.kind, GroupKind::Form);
    assert_eq!(&g.inner_type, b"8SVX");
    assert_eq!(g.declared_total_len() as usize, bytes.len());
}

// ───────────────────── ANIM ─────────────────────

#[test]
fn probe_recognises_anim_envelope_from_encode_anim_op0() {
    // op-0 = "frames stored as full ILBMs, no delta compression" —
    // the only ANIM op that does not depend on chunk-internal entropy
    // tables, which keeps the test fixture small.
    let frames = vec![tiny_ilbm(), tiny_ilbm()];
    let bytes = oxideav_iff::anim::encode_anim_op0(&frames).unwrap();
    let g = probe_top_level_group(&bytes).unwrap().unwrap();
    assert_eq!(g.kind, GroupKind::Form);
    assert_eq!(&g.inner_type, b"ANIM");
    assert_eq!(g.declared_total_len() as usize, bytes.len());
}

// ───────────────────── AIFF ─────────────────────

#[test]
fn probe_recognises_aiff_envelope() {
    let bytes = tiny_aiff();
    let g = probe_top_level_group(&bytes).unwrap().unwrap();
    assert_eq!(g.kind, GroupKind::Form);
    assert_eq!(&g.inner_type, b"AIFF");
    assert_eq!(g.declared_total_len() as usize, bytes.len());
}

// ───────────────────── stream variant ─────────────────────

#[test]
fn read_top_level_group_consumes_exactly_12_bytes() {
    // The streaming variant must leave the cursor positioned right at
    // the first nested chunk so the caller can hand the rest of the
    // FORM straight to a per-form walker — duplicates of that contract
    // live in svx/ilbm/anim/aiff, this is the one place we verify it
    // against the primitive itself.
    let bytes = encode_ilbm(&tiny_ilbm()).unwrap();
    let mut cur = std::io::Cursor::new(&bytes[..]);
    let g = read_top_level_group(&mut cur).unwrap().unwrap();
    assert_eq!(cur.position(), 12);
    // The next 4 bytes should be a well-formed chunk ID (BMHD for
    // every encode_ilbm output, but we only check the printable-ASCII
    // shape so the test stays decoupled from encode-ordering choices).
    let pos = cur.position() as usize;
    let next_id = &bytes[pos..pos + 4];
    assert!(next_id.iter().all(|b| b.is_ascii_graphic() || *b == b' '));
    // Match against the wire envelope to confirm the round-trip is
    // self-consistent.
    let probed = probe_top_level_group(&bytes).unwrap().unwrap();
    assert_eq!(probed, g);
}
