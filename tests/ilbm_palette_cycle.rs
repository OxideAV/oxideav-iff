//! Tests for the palette-cycling helpers introduced in round 184:
//!
//! * `Crng::cycle_step`
//! * `Ccrt::cycle_step`
//! * `Drng::cycle_step`
//! * `Pchg::palette_at_line`
//! * top-level `palette_for_line(image, y)`
//!
//! Every test sources `IlbmImage` / `Pchg` / `Crng` / `Ccrt` / `Drng`
//! from the public API only. No bit-exact fixtures are required — the
//! semantics under test are arithmetic rotations over a synthesised
//! palette.

use oxideav_iff::ilbm::{
    palette_for_line, Bmhd, Ccrt, Compression, Crng, Drng, DrngRegCell, DrngTrueCell, IlbmImage,
    Masking, Pchg, PchgChange, PchgLine,
};

/// Build a small palette of 8 distinct RGB triples — easier to track
/// during rotation than the typical `[0,0,0]` defaults.
fn mk_palette() -> Vec<[u8; 3]> {
    vec![
        [10, 11, 12],
        [20, 21, 22],
        [30, 31, 32],
        [40, 41, 42],
        [50, 51, 52],
        [60, 61, 62],
        [70, 71, 72],
        [80, 81, 82],
    ]
}

// ───────────────────── CRNG ─────────────────────

#[test]
fn crng_cycle_step_forward_one_tick() {
    let mut pal = mk_palette();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 1,
        high: 4,
    };
    // Range = [1..=4] = 4 slots: 20, 30, 40, 50.
    // After one forward tick, the value at slot 1 should be 50 (it
    // came from the wrap-around tail), slot 2 should be 20, slot 3 30,
    // slot 4 40.
    let changed = crng.cycle_step(&mut pal, 1);
    assert!(changed);
    assert_eq!(pal[0], [10, 11, 12]); // untouched
    assert_eq!(pal[1], [50, 51, 52]);
    assert_eq!(pal[2], [20, 21, 22]);
    assert_eq!(pal[3], [30, 31, 32]);
    assert_eq!(pal[4], [40, 41, 42]);
    assert_eq!(pal[5], [60, 61, 62]); // untouched
}

#[test]
fn crng_cycle_step_reverse_one_tick() {
    let mut pal = mk_palette();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE | Crng::FLAG_REVERSE,
        low: 1,
        high: 4,
    };
    let changed = crng.cycle_step(&mut pal, 1);
    assert!(changed);
    // Reverse one tick: slot 1 takes 30, slot 2 takes 40, slot 3 takes
    // 50, slot 4 takes 20.
    assert_eq!(pal[1], [30, 31, 32]);
    assert_eq!(pal[2], [40, 41, 42]);
    assert_eq!(pal[3], [50, 51, 52]);
    assert_eq!(pal[4], [20, 21, 22]);
    assert_eq!(pal[0], [10, 11, 12]); // untouched
    assert_eq!(pal[5], [60, 61, 62]); // untouched
}

#[test]
fn crng_cycle_step_full_revolution_is_identity() {
    let mut pal = mk_palette();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 1,
        high: 4,
    };
    // 4 ticks over a 4-slot range = full revolution; result equals
    // input — and `cycle_step` returns `false` since `steps % len == 0`.
    let pre = pal.clone();
    let changed = crng.cycle_step(&mut pal, 4);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn crng_cycle_step_modulo_steps() {
    // 9 forward ticks over a 4-slot range == 1 forward tick.
    let mut pal_a = mk_palette();
    let mut pal_b = mk_palette();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 1,
        high: 4,
    };
    crng.cycle_step(&mut pal_a, 9);
    crng.cycle_step(&mut pal_b, 1);
    assert_eq!(pal_a, pal_b);
}

#[test]
fn crng_cycle_step_inactive_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: 0, // FLAG_ACTIVE not set
        low: 1,
        high: 4,
    };
    let changed = crng.cycle_step(&mut pal, 3);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn crng_cycle_step_single_slot_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 3,
        high: 3,
    };
    let changed = crng.cycle_step(&mut pal, 7);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn crng_cycle_step_inverted_range_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 5,
        high: 2, // low > high — range_len() == 0
    };
    let changed = crng.cycle_step(&mut pal, 3);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn crng_cycle_step_range_past_palette_is_noop() {
    let mut pal = mk_palette(); // len 8 — valid indices 0..=7
    let pre = pal.clone();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 2,
        high: 9, // off the end
    };
    let changed = crng.cycle_step(&mut pal, 1);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn crng_cycle_step_zero_steps_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 1,
        high: 4,
    };
    let changed = crng.cycle_step(&mut pal, 0);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn crng_forward_then_reverse_returns_to_original() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let fwd = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 1,
        high: 5,
    };
    let rev = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE | Crng::FLAG_REVERSE,
        low: 1,
        high: 5,
    };
    fwd.cycle_step(&mut pal, 3);
    rev.cycle_step(&mut pal, 3);
    assert_eq!(pal, pre);
}

// ───────────────────── CCRT ─────────────────────

#[test]
fn ccrt_cycle_step_forward_one_tick() {
    let mut pal = mk_palette();
    let ccrt = Ccrt {
        direction: 1,
        start: 2,
        end: 5,
        seconds: 0,
        micros: 250_000,
        pad: 0,
    };
    let changed = ccrt.cycle_step(&mut pal, 1);
    assert!(changed);
    // Range = [2..=5] = 4 slots: 30, 40, 50, 60.
    // Forward 1 tick: slot 2 ← 60, slot 3 ← 30, slot 4 ← 40, slot 5 ← 50.
    assert_eq!(pal[2], [60, 61, 62]);
    assert_eq!(pal[3], [30, 31, 32]);
    assert_eq!(pal[4], [40, 41, 42]);
    assert_eq!(pal[5], [50, 51, 52]);
    assert_eq!(pal[1], [20, 21, 22]); // untouched
    assert_eq!(pal[6], [70, 71, 72]); // untouched
}

#[test]
fn ccrt_cycle_step_reverse_is_inverse_of_forward() {
    let mut pal_fwd = mk_palette();
    let mut pal_rev = mk_palette();
    let fwd = Ccrt {
        direction: 1,
        start: 1,
        end: 6,
        seconds: 0,
        micros: 100,
        pad: 0,
    };
    let rev = Ccrt {
        direction: -1,
        start: 1,
        end: 6,
        seconds: 0,
        micros: 100,
        pad: 0,
    };
    fwd.cycle_step(&mut pal_fwd, 2);
    rev.cycle_step(&mut pal_rev, 2);
    // Apply the inverse on top — should restore the original.
    rev.cycle_step(&mut pal_fwd, 2);
    fwd.cycle_step(&mut pal_rev, 2);
    assert_eq!(pal_fwd, mk_palette());
    assert_eq!(pal_rev, mk_palette());
}

#[test]
fn ccrt_cycle_step_direction_zero_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let ccrt = Ccrt {
        direction: 0, // inactive per is_active()
        start: 1,
        end: 5,
        seconds: 1,
        micros: 0,
        pad: 0,
    };
    let changed = ccrt.cycle_step(&mut pal, 4);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn ccrt_cycle_step_inverted_range_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let ccrt = Ccrt {
        direction: 1,
        start: 6,
        end: 2, // start > end
        seconds: 0,
        micros: 0,
        pad: 0,
    };
    let changed = ccrt.cycle_step(&mut pal, 1);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn ccrt_cycle_step_huge_step_modulos_down() {
    let mut pal_huge = mk_palette();
    let mut pal_small = mk_palette();
    let ccrt = Ccrt {
        direction: 1,
        start: 1,
        end: 4,
        seconds: 0,
        micros: 0,
        pad: 0,
    };
    // 1_000_003 mod 4 == 3
    ccrt.cycle_step(&mut pal_huge, 1_000_003);
    ccrt.cycle_step(&mut pal_small, 3);
    assert_eq!(pal_huge, pal_small);
}

// ───────────────────── DRNG ─────────────────────

#[test]
fn drng_cycle_step_rotates_contiguous_range() {
    let mut pal = mk_palette();
    let drng = Drng {
        min: 1,
        max: 4,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE,
        trues: vec![],
        regs: vec![],
    };
    let changed = drng.cycle_step(&mut pal, 1);
    assert!(changed);
    // Same shape as CRNG forward one tick.
    assert_eq!(pal[1], [50, 51, 52]);
    assert_eq!(pal[2], [20, 21, 22]);
    assert_eq!(pal[3], [30, 31, 32]);
    assert_eq!(pal[4], [40, 41, 42]);
    assert_eq!(pal[0], [10, 11, 12]);
    assert_eq!(pal[5], [60, 61, 62]);
}

#[test]
fn drng_cycle_step_inactive_is_noop() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let drng = Drng {
        min: 1,
        max: 4,
        rate: 16384,
        flags: 0,
        trues: vec![],
        regs: vec![],
    };
    let changed = drng.cycle_step(&mut pal, 3);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn drng_cycle_step_preserves_cell_lists() {
    let mut pal = mk_palette();
    let drng = Drng {
        min: 1,
        max: 4,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE | Drng::FLAG_DP_RGB,
        trues: vec![DrngTrueCell {
            cell: 2,
            r: 200,
            g: 100,
            b: 50,
        }],
        regs: vec![DrngRegCell { cell: 3, index: 0 }],
    };
    let before_trues = drng.trues.clone();
    let before_regs = drng.regs.clone();
    drng.cycle_step(&mut pal, 1);
    // Cell list isn't owned by the palette — verify we didn't somehow
    // perturb the read-only descriptor.
    assert_eq!(drng.trues, before_trues);
    assert_eq!(drng.regs, before_regs);
}

#[test]
fn drng_cycle_step_full_revolution_is_identity() {
    let mut pal = mk_palette();
    let pre = pal.clone();
    let drng = Drng {
        min: 2,
        max: 5,
        rate: 16384,
        flags: Drng::FLAG_ACTIVE,
        trues: vec![],
        regs: vec![],
    };
    let changed = drng.cycle_step(&mut pal, 4);
    assert!(!changed);
    assert_eq!(pal, pre);
}

#[test]
fn drng_cycle_step_range_past_palette_is_noop() {
    let mut pal = mk_palette(); // len 8
    let pre = pal.clone();
    let drng = Drng {
        min: 5,
        max: 12, // off the end
        rate: 16384,
        flags: Drng::FLAG_ACTIVE,
        trues: vec![],
        regs: vec![],
    };
    let changed = drng.cycle_step(&mut pal, 1);
    assert!(!changed);
    assert_eq!(pal, pre);
}

// ───────────────────── PCHG / palette_for_line ─────────────────────

fn mk_pchg() -> Pchg {
    // Lines 2 and 5 each carry one override.
    Pchg {
        raw: Vec::new(),
        lines: vec![
            PchgLine {
                line: 2,
                changes: vec![PchgChange {
                    index: 1,
                    rgb: [200, 0, 0],
                }],
            },
            PchgLine {
                line: 5,
                changes: vec![PchgChange {
                    index: 2,
                    rgb: [0, 200, 0],
                }],
            },
        ],
    }
}

fn mk_image_with_pchg(pchg: Option<Pchg>) -> IlbmImage {
    IlbmImage {
        width: 4,
        height: 8,
        bmhd: Bmhd {
            width: 4,
            height: 8,
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
            page_height: 8,
        },
        palette: mk_palette(),
        camg: Default::default(),
        form_type: *b"ILBM",
        grab: None,
        dest: None,
        sprt: None,
        sham: None,
        pchg,
        crngs: vec![],
        ccrts: vec![],
        drngs: vec![],
        rgba: vec![0u8; 4 * 8 * 4],
    }
}

#[test]
fn pchg_palette_at_line_before_first_override_is_base() {
    let pchg = mk_pchg();
    let base = mk_palette();
    let pal = pchg.palette_at_line(&base, 0);
    assert_eq!(pal, base);
    let pal = pchg.palette_at_line(&base, 1);
    assert_eq!(pal, base);
}

#[test]
fn pchg_palette_at_line_applies_overrides_in_order() {
    let pchg = mk_pchg();
    let base = mk_palette();

    // At line 2 the first override has fired.
    let pal2 = pchg.palette_at_line(&base, 2);
    assert_eq!(pal2[1], [200, 0, 0]);
    assert_eq!(pal2[2], base[2]);

    // At line 4 still only the first override has fired.
    let pal4 = pchg.palette_at_line(&base, 4);
    assert_eq!(pal4[1], [200, 0, 0]);
    assert_eq!(pal4[2], base[2]);

    // At line 5 both have fired.
    let pal5 = pchg.palette_at_line(&base, 5);
    assert_eq!(pal5[1], [200, 0, 0]);
    assert_eq!(pal5[2], [0, 200, 0]);
    // Other slots untouched.
    assert_eq!(pal5[0], base[0]);
    assert_eq!(pal5[3], base[3]);
}

#[test]
fn pchg_palette_at_line_past_image_height_is_final_state() {
    let pchg = mk_pchg();
    let base = mk_palette();
    let pal_late = pchg.palette_at_line(&base, 999);
    assert_eq!(pal_late[1], [200, 0, 0]);
    assert_eq!(pal_late[2], [0, 200, 0]);
}

#[test]
fn pchg_palette_at_line_skips_out_of_range_index() {
    let pchg = Pchg {
        raw: Vec::new(),
        lines: vec![PchgLine {
            line: 0,
            changes: vec![
                // First entry: in-range — should apply.
                PchgChange {
                    index: 0,
                    rgb: [255, 255, 255],
                },
                // Second entry: index past the base palette length —
                // skipped silently (parser-tolerant semantics).
                PchgChange {
                    index: 999,
                    rgb: [1, 2, 3],
                },
            ],
        }],
    };
    let base = mk_palette();
    let pal = pchg.palette_at_line(&base, 0);
    assert_eq!(pal[0], [255, 255, 255]);
    // The malformed override didn't grow the buffer or panic.
    assert_eq!(pal.len(), base.len());
}

#[test]
fn palette_for_line_returns_base_when_no_pchg() {
    let img = mk_image_with_pchg(None);
    for y in 0..img.height {
        assert_eq!(palette_for_line(&img, y), img.palette);
    }
}

#[test]
fn palette_for_line_walks_pchg_state_per_row() {
    let img = mk_image_with_pchg(Some(mk_pchg()));
    // Before any override: base palette.
    assert_eq!(palette_for_line(&img, 0)[1], img.palette[1]);
    // At line 2 the first override has fired.
    assert_eq!(palette_for_line(&img, 2)[1], [200, 0, 0]);
    // Last row: both overrides applied.
    let last = palette_for_line(&img, img.height - 1);
    assert_eq!(last[1], [200, 0, 0]);
    assert_eq!(last[2], [0, 200, 0]);
}

#[test]
fn palette_for_line_is_pure_does_not_mutate_image() {
    let img = mk_image_with_pchg(Some(mk_pchg()));
    let pre_palette = img.palette.clone();
    let _ = palette_for_line(&img, 7);
    let _ = palette_for_line(&img, 0);
    let _ = palette_for_line(&img, 4);
    // The image's stored palette is untouched after repeated calls.
    assert_eq!(img.palette, pre_palette);
}

// ───────────────────── End-to-end interaction ─────────────────────

#[test]
fn cycle_then_pchg_resolve_compose_cleanly() {
    // Build an image with both PCHG (line 3: rewrite slot 1 to red) and
    // a CRNG that cycles slots 4..=6. Demonstrate that consumers can
    // compose them in either order — `palette_for_line(...)` gives the
    // pre-cycle baseline at row `y`, then `crng.cycle_step` rotates a
    // sub-range without interfering with the PCHG-rewritten slots.
    let pchg = Pchg {
        raw: Vec::new(),
        lines: vec![PchgLine {
            line: 3,
            changes: vec![PchgChange {
                index: 1,
                rgb: [255, 0, 0],
            }],
        }],
    };
    let img = mk_image_with_pchg(Some(pchg));
    let crng = Crng {
        pad1: 0,
        rate: 16384,
        flags: Crng::FLAG_ACTIVE,
        low: 4,
        high: 6,
    };

    // At scanline 4 the PCHG override has applied (slot 1 = red).
    let mut pal = palette_for_line(&img, 4);
    assert_eq!(pal[1], [255, 0, 0]);
    // Then we rotate slots 4..=6 by one tick.
    assert!(crng.cycle_step(&mut pal, 1));
    // CRNG range was 3 slots: 50, 60, 70. Forward 1 tick: slot 4 ← 70,
    // slot 5 ← 50, slot 6 ← 60.
    assert_eq!(pal[4], [70, 71, 72]);
    assert_eq!(pal[5], [50, 51, 52]);
    assert_eq!(pal[6], [60, 61, 62]);
    // Slot 1 (red, from PCHG) untouched by the rotation.
    assert_eq!(pal[1], [255, 0, 0]);
}
