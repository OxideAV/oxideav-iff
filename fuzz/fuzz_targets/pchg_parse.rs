#![no_main]

//! Feed arbitrary fuzz-supplied bytes through `ilbm::Pchg::parse`.
//!
//! `Pchg::parse` decodes a PCHG (Palette CHanGes per scan-line) chunk
//! body. The chunk carries:
//!
//!  * a fixed 20-byte header (Compression, Flags, StartLine,
//!    LineCount, ChangedLines, MinReg, MaxReg, MaxChanges,
//!    TotalChanges),
//!  * an optional Huffman compression mode (PCHGCompHeader +
//!    serialized signed-16-bit tree + MSB-first bitstream) wrapping
//!    the LineData,
//!  * a LineMask bitmap of which scan-lines carry changes,
//!  * for each set mask bit, a change record — 12-bit
//!    SmallLineChanges packed words or 32-bit BigLineChanges 6-byte
//!    records depending on Flags.
//!
//! The chunk is the most failure-mode-dense single body the crate
//! parses: tabular header arithmetic, two compression modes, two
//! change-record variants, and per-line cumulative-state palette
//! reconstruction.
//!
//! The contract under test is purely that the call *returns*: a
//! malformed body yields `Err(oxideav_core::Error::…)`, a well-formed
//! one yields `Ok(Pchg)`, and neither path may panic,
//! integer-overflow (in a debug build), index out of bounds, or try to
//! allocate an attacker-controlled buffer larger than the input
//! actually supports.
//!
//! The return value is intentionally discarded.

use libfuzzer_sys::fuzz_target;
use oxideav_iff::ilbm::Pchg;

fuzz_target!(|data: &[u8]| {
    let _ = Pchg::parse(data);
});
