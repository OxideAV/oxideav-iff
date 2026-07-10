//! PCHG parser robustness — deterministic in-test fuzzing.
//!
//! `Pchg::parse` is the most failure-mode-dense body in the crate:
//! header arithmetic, a LineMask bitmap, two record encodings, and a
//! Huffman expander whose tree is attacker-controlled. These tests
//! drive it with deterministic pseudo-random inputs across three
//! strategies (pure noise, plausible-header noise, and mutations of
//! valid chunks) asserting the only outcomes are `Ok` or a clean
//! `Err` — no panic, no overflow, no runaway allocation — plus a
//! randomised encode/parse round-trip property for both kinds and
//! both compression modes.

use oxideav_iff::ilbm::{Pchg, PchgChange, PchgKind, PchgLine};

/// Tiny deterministic xorshift64* PRNG so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 32) as u8
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn pure_noise_never_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..20_000 {
        let len = rng.below(160) as usize;
        let buf: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let _ = Pchg::parse(&buf);
    }
}

#[test]
fn plausible_headers_with_noise_tails_never_panic() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..20_000 {
        let mut buf = Vec::new();
        // Compression 0..=2 (2 exercises the unknown-mode rejection).
        buf.extend_from_slice(&((rng.below(3)) as u16).to_be_bytes());
        // Flags: bias toward the meaningful low bits.
        buf.extend_from_slice(&((rng.next() as u16) & 0x0007).to_be_bytes());
        // StartLine: small signed values.
        buf.extend_from_slice(&((rng.next() as i16) % 100).to_be_bytes());
        // LineCount: up to a few hundred.
        buf.extend_from_slice(&((rng.below(400)) as u16).to_be_bytes());
        // Hints: noise (parse is hint-tolerant).
        for _ in 0..4 {
            buf.extend_from_slice(&(rng.next() as u16).to_be_bytes());
        }
        buf.extend_from_slice(&(rng.next() as u32).to_be_bytes());
        // Tail: noise, sometimes shaped like a comp header.
        let tail = rng.below(200) as usize;
        for _ in 0..tail {
            buf.push(rng.byte());
        }
        let _ = Pchg::parse(&buf);
    }
}

#[test]
fn mutated_valid_chunks_never_panic() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut with_alpha = PchgChange::new(7, [1, 2, 3]);
    with_alpha.alpha = Some(0x80);
    let src = Pchg::from_lines(
        vec![
            PchgLine {
                line: 0,
                changes: vec![PchgChange::new(1, [0x11, 0x22, 0x33]), with_alpha],
            },
            PchgLine {
                line: 40,
                changes: vec![PchgChange::new(20, [0xAA, 0xBB, 0xCC])],
            },
        ],
        PchgKind::Big,
    );
    let seeds = [
        src.encode(PchgKind::Small),
        src.encode(PchgKind::Big),
        src.encode_huffman(PchgKind::Small),
        src.encode_huffman(PchgKind::Big),
    ];
    for _ in 0..20_000 {
        let mut buf = seeds[rng.below(4) as usize].clone();
        // 1..=4 random byte stomps.
        for _ in 0..=rng.below(4) {
            let at = rng.below(buf.len() as u64) as usize;
            buf[at] = rng.byte();
        }
        // Occasionally truncate or extend.
        match rng.below(4) {
            0 => {
                let keep = rng.below(buf.len() as u64 + 1) as usize;
                buf.truncate(keep);
            }
            1 => {
                for _ in 0..rng.below(16) {
                    buf.push(rng.byte());
                }
            }
            _ => {}
        }
        let _ = Pchg::parse(&buf);
    }
}

#[test]
fn random_change_lists_roundtrip_both_kinds_and_compressions() {
    let mut rng = Rng(0xFEED_FACE_0BAD_F00D);
    for _ in 0..400 {
        let n_lines = 1 + rng.below(12) as u32;
        let mut used = std::collections::BTreeSet::new();
        let mut lines = Vec::new();
        for _ in 0..n_lines {
            let l = rng.below(300) as u32;
            if !used.insert(l) {
                continue; // one entry per line for exact comparison
            }
            let n_changes = 1 + rng.below(6) as usize;
            let mut seen_regs = std::collections::BTreeSet::new();
            let mut changes = Vec::new();
            for _ in 0..n_changes {
                let big = rng.below(2) == 0;
                let reg = if big {
                    rng.below(256) as u16
                } else {
                    rng.below(32) as u16
                };
                if !seen_regs.insert(reg) {
                    continue;
                }
                changes.push(PchgChange::new(reg, [rng.byte(), rng.byte(), rng.byte()]));
            }
            if !changes.is_empty() {
                lines.push(PchgLine { line: l, changes });
            }
        }

        for kind in [PchgKind::Small, PchgKind::Big] {
            // Normalise through from_lines (Small quantises / clamps).
            let src = Pchg::from_lines(lines.clone(), kind);
            let plain = Pchg::parse(&src.encode(kind)).expect("plain reparse");
            assert_eq!(plain.lines, src.lines, "uncompressed {kind:?}");
            let packed = Pchg::parse(&src.encode_huffman(kind)).expect("huffman reparse");
            assert_eq!(packed.lines, src.lines, "huffman {kind:?}");
        }
    }
}
