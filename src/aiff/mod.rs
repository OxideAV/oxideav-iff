//! AIFF / AIFF-C (AIFC) container support — Apple's audio FORM types.
//!
//! AIFF is an application of EA IFF 85, the same chunked-file standard
//! used by the sibling modules in this crate (`svx`, `ilbm`, `anim`):
//! an outermost `FORM` chunk holds a `COMM` (format) chunk, an `SSND`
//! (sample data) chunk, and any number of optional metadata chunks.
//! It is the big-endian Macintosh counterpart to Microsoft's
//! little-endian RIFF/WAVE.
//!
//! This module covers the two form types defined by Apple:
//!
//! * **`FORM/AIFF`** — the original uncompressed format (Apple AIFF
//!   v1.3, 1989). 80-bit IEEE-extended sample rate, big-endian
//!   two's-complement PCM samples.
//! * **`FORM/AIFC`** — the compressed-capable extension (Apple
//!   AIFF-C draft 8/26/91). Adds a `FVER` chunk and extends `COMM`
//!   with a `compressionType` FourCC + Pascal-string compression
//!   name.
//!
//! The clean-room layout summary the parser is built against is in
//! `docs/audio/aiff/aiff-aifc-format.md`, derived from the staged
//! Apple specs (`aiff-1.3.pdf`, `aiff-c.pdf`, `aiff-c.txt`).
//!
//! ## Surface
//!
//! * The EA IFF 85 chunk walker [`chunk::ChunkIter`] — ckID + ckSize
//!   header with odd-size pad-byte handling, slice-based zero-copy
//!   iterator (distinct from the stream-based walker in the parent
//!   `oxideav_iff::chunk` module used by svx/ilbm/anim).
//! * 80-bit IEEE 754 extended-precision sample-rate decode **and
//!   encode** ([`extended::decode_sample_rate`] /
//!   [`extended::decode_extended`] /
//!   [`extended::encode_sample_rate`] / [`extended::encode_extended`]).
//! * `COMM` parser ([`common::parse_common`]) **and writer**
//!   ([`common::write_common_chunk`]) for both AIFF and AIFF-C forms,
//!   including the AIFF-C `compressionType` + Pascal-string
//!   compression name.
//! * `FVER` + `SSND` + the top-level FORM walker ([`form::parse`]).
//! * PCM compression-flavour readers ([`pcm::decode_pcm`]) for the
//!   uncompressed AIFF-C `compressionType` FourCCs **`NONE`**,
//!   **`twos`**, **`sowt`**, **`raw `**, **`fl32`**/`FL32`, and
//!   **`fl64`**/`FL64`. Integer flavours promote to left-justified
//!   `i32`; float flavours stay in their native precision.
//!
//! Codec-bearing AIFF-C `compressionType` FourCCs (`ima4`, `ulaw`,
//! `alaw`, `MAC3`, `MAC6`, `GSM `, `G722`, `G726`, `G728`, `QDMC`,
//! `QDM2`, `Qclp`, …) are recognised in the chunk parser but routed
//! through sibling codec crates (`oxideav-adpcm` for `ima4`,
//! `oxideav-g711` for `ulaw` / `alaw`, etc.) — they are NOT decoded
//! here. [`pcm::decode_pcm`] returns
//! [`error::AiffError::UnsupportedPcmCompression`] for those so
//! callers can dispatch cleanly.
//!
//! The §13.0 text chunks (`NAME`, `AUTH`, `(c) `, `ANNO`) are parsed
//! into [`TextChunk`]s and surfaced through [`Form::name`],
//! [`Form::author`], [`Form::copyright`], and [`Form::annotations`].
//! Per §13.0, `NAME` / `AUTH` / `(c) ` are at-most-one-per-FORM
//! (duplicates raise [`AiffError::DuplicateChunk`]) while `ANNO` is
//! unconstrained and accumulated in document order, mirroring how
//! §10.0 MIDI and §12.0 APPL handle the "any-number-per-FORM" rule.
//! The text body is preserved byte-for-byte ("neither a pstring nor a
//! C string"); UTF-8 lossy decoding is available as a
//! [`TextChunk::as_string_lossy`] convenience.
//!
//! The `MARK` (marker) chunk is parsed into a structured
//! [`MarkerChunk`] (id / sample-frame position / pstring name per
//! marker) and surfaced through [`Form::markers`]. Multiple MARK
//! chunks inside the same FORM are rejected per §6.0 of the spec.
//! Encoders can also build a `MARK` chunk body via
//! [`write_marker_chunk`] (preserving document order and the
//! pad-to-even pstring discipline).
//!
//! The `INST` (instrument) chunk is parsed into [`InstrumentChunk`]
//! and surfaced through [`Form::instrument`]. The decoded fields
//! cover sampler playback parameters (baseNote, detune, low/high
//! note + velocity ranges, gain) and the two `Loop` substructures
//! (sustainLoop, releaseLoop). The accompanying
//! [`InstrumentChunk::resolve_sustain_loop`] /
//! [`InstrumentChunk::resolve_release_loop`] helpers join the loop
//! endpoints against the FORM's [`MarkerChunk`] and apply §9's
//! "begin position must be less than the end position" rule so
//! callers can ask "what does the spec say to actually play?". At
//! most one `INST` chunk per FORM is permitted; a second one is
//! rejected as [`AiffError::DuplicateChunk`]. The exact 20-byte
//! ckData body can also be produced via [`write_instrument_chunk`]
//! for write-side encoders.
//!
//! The `COMT` (comments) chunk is parsed into [`CommentsChunk`] —
//! a list of `(timestamp, marker, text)` triples — and surfaced
//! through [`Form::comments`]. Each comment optionally links to a
//! `MARK` entry; [`Comment::resolve_marker`] joins the linkage
//! against a supplied [`MarkerChunk`]. At most one `COMT` per FORM
//! per §7.0; duplicates are rejected as [`AiffError::DuplicateChunk`].
//!
//! The `AESD` (audio recording) chunk is parsed into [`AesdChunk`]
//! preserving the 24-byte AES channel-status block verbatim and
//! exposing the byte-0 bits-2..=4 recording-emphasis field through
//! [`AesdChunk::emphasis`]. Surfaced through [`Form::aesd`]; at most
//! one AESD per FORM per §11.0.
//!
//! The `APPL` (application-specific) chunks — §12.0 explicitly
//! permits any number per FORM — are parsed into
//! [`ApplicationChunk`]s and surfaced through [`Form::applications`]
//! in document order. The `pdos` / `stoc` dialects decode their
//! leading Pascal-string application name via
//! [`ApplicationChunk::application_name`] while Macintosh
//! application signatures (any other FourCC) carry raw bytes.
//!
//! The `MIDI` (MIDI Data) chunks — §10.0 permits any number per FORM
//! — are parsed into [`MidiDataChunk`]s and surfaced through
//! [`Form::midi`] in document order. The body is the raw MIDI byte
//! stream the spec describes as opaque to the AIFF layer; a full
//! Standard MIDI File / event-level decode is the job of the
//! `oxideav-midi` sibling crate. Lightweight observers
//! ([`MidiDataChunk::len`], [`MidiDataChunk::is_empty`],
//! [`MidiDataChunk::is_sysex`]) cover the common "is this a SysEx
//! patch dump or something else?" classification without re-parsing.
//!
//! The `SAXL` (Sound Accelerator) chunks — §8.0 / Appendix D permits
//! any number per FORM — are parsed into [`SaxelChunk`]s and surfaced
//! through [`Form::saxels`] in document order. Each contained
//! [`Saxel`] pairs a `MarkerId` with a compression-type-specific
//! `data` payload that primes a decompressor's internal state so
//! playback can begin at a random marker without startup artifacts.
//! Appendix D ¶ "Caution" / §8.0 ¶ "Under Construction" emphasises
//! the mechanism remained a "rough proposal" so the parser preserves
//! `data` verbatim rather than interpreting it against any specific
//! algorithm; [`Saxel::resolve_marker`] joins the saxel's id against
//! a [`MarkerChunk`] for callers that want the §6.0 lookup.
//!
//! The §14 chunk-precedence rules are exposed through the
//! [`ChunkClass`] enum (a precedence-ranked enumeration of every
//! §3.1, §4..§13 chunk class) and the [`Form::precedence_order`] /
//! [`Form::highest_precedence_class`] helpers. §14 ¶ "Information
//! in the Common Chunk always takes precedence over conflicting
//! information in any other chunk" maps to
//! `ChunkClass::Common.higher_precedence_than(other)`; the §14
//! worked example "the loop points in the Instrument Chunk take
//! precedence over conflicting loop points found in the MIDI Data
//! Chunk" maps to
//! `ChunkClass::Instrument.higher_precedence_than(ChunkClass::MidiData)`.

pub mod aesd;
pub mod appl;
pub mod chunk;
pub mod comment;
pub mod common;
pub mod error;
pub mod extended;
pub mod form;
pub mod instrument;
pub mod marker;
pub mod midi;
pub mod pcm;
pub mod precedence;
pub mod saxel;
pub mod text;

pub mod demuxer;

pub use aesd::{parse_aesd_chunk, write_aesd_chunk, AesdChunk, Emphasis};
pub use appl::{parse_appl_chunk, write_appl_chunk, ApplicationChunk, ApplicationDialect};
pub use chunk::{Chunk, ChunkIter};
pub use comment::{parse_comments_chunk, write_comments_chunk, Comment, CommentsChunk};
pub use common::{
    parse_common, write_common_chunk, CommonChunk, COMPRESSION_FL32, COMPRESSION_FL32_UC,
    COMPRESSION_FL64, COMPRESSION_FL64_UC, COMPRESSION_NONE, COMPRESSION_RAW, COMPRESSION_SOWT,
    COMPRESSION_TWOS,
};
pub use error::{AiffError, Result};
pub use extended::{decode_extended, decode_sample_rate, encode_extended, encode_sample_rate};
pub use form::{parse, Form, SoundData};
pub use instrument::{
    parse_instrument_chunk, write_instrument_chunk, InstrumentChunk, Loop, PlayMode, ResolvedLoop,
};
pub use marker::{parse_marker_chunk, write_marker_chunk, Marker, MarkerChunk};
pub use midi::{parse_midi_chunk, write_midi_chunk, MidiDataChunk};
pub use pcm::{decode_pcm, is_pcm_compression, PcmSamples};
pub use precedence::ChunkClass;
pub use saxel::{parse_saxel_chunk, write_saxel_chunk, Saxel, SaxelChunk};
pub use text::{parse_text_chunk, write_text_chunk, TextChunk, TextKind};

pub use demuxer::{make_demuxer, register, AiffDemuxer, FORMAT_NAME};

/// Codec id string under which the demuxer factory installs itself
/// in `oxideav-core`'s `ContainerRegistry` (`"aiff"`).
pub const CODEC_ID_STR: &str = "aiff";
