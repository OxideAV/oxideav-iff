//! FORM 8SVX — Amiga 8-bit sampled voice audio.
//!
//! Layout: `FORM` group chunk → 4-byte `8SVX` form type → children:
//! - `VHDR` (20 bytes): voice header (one-shot/repeat sample counts,
//!   samples per high-cycle, samples per second, octave count, compression
//!   code, 16.16 volume) — typed as [`VoiceHeader`].
//! - optional `CHAN` (mono left/right routing or stereo —
//!   [`ChannelAssignment`]), `PAN` (mono stereo-position — [`Pan`]),
//!   `ATAK` / `RLSE` volume envelopes ([`Envelope`] of [`EgPoint`]s;
//!   multiple chunks of each build a full ADSR), `SEQN` waveform
//!   segment sequencing ([`Seqn`]), `FADE` fade-out trigger ([`Fade`]),
//!   and the `NAME` / `ANNO` / `AUTH` / `(c) ` / `CHRS` text chunks.
//! - `BODY`: raw signed 8-bit samples (or Fibonacci-delta compressed),
//!   `ctOctave` waveforms back to back (highest octave first, each
//!   following octave twice as long as the previous one).
//!
//! Two read/write surfaces exist: the streaming container pair
//! (demuxer + [`SvxMuxer`]) exposes the voice as a `pcm_s8` audio
//! stream, and the structural pair [`parse_voice`] / [`encode_voice`]
//! surfaces the complete typed chunk tree ([`Voice`]) with per-octave
//! sample vectors for callers that need the instrument structure
//! (octaves, loop split, envelopes, sequencing) rather than a flat
//! playback stream.
//!
//! We expose an 8SVX file as a single audio stream with codec id
//! `pcm_s8`. Two compression modes are supported:
//!
//! * `sCompression = 0` — raw signed 8-bit PCM.
//! * `sCompression = 1` — Fibonacci-delta (lossy). Each channel's
//!   compressed stream starts with a 1-byte pad, then a 1-byte initial
//!   sample, then 4-bit delta indices packed two-per-byte high-nibble
//!   first. The decoded delta table is
//!   `[-34, -21, -13, -8, -5, -3, -2, -1, 0, 1, 2, 3, 5, 8, 13, 21]`
//!   (16 entries). The task prompt listed a 17-entry variant; we use the
//!   16-entry table from the Amiga ROM Kernel Manual / AmigaOS wiki
//!   because the nibble is only 4 bits wide and 16 codes are all that
//!   can actually be addressed. See `FIB_DELTA_TABLE` below.
//!
//! Channel layout: `CHAN` payload is 4 bytes BE. We recognise `2`
//! (LEFT, mono) and `6` (LEFT|RIGHT, stereo). Stereo BODY layout is
//! **concatenated halves** — left channel in full, then right channel in
//! full (the common convention cited by the AmigaOS wiki and sampling
//! software). For Fibonacci-compressed stereo each half carries its own
//! pad + initial-sample header, so the two channels can be decoded
//! independently.

use std::io::{Read, Seek, SeekFrom, Write};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, MediaType, Packet, Result, SampleFormat,
    StreamInfo, TimeBase,
};
use oxideav_core::{ContainerRegistry, Demuxer, Muxer, ReadSeek, WriteSeek};

use crate::chunk::{
    read_body, read_chunk_header, read_form_type, skip_chunk_body, ChunkHeader, GROUP_FORM,
};

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("iff_8svx", open);
    reg.register_muxer("iff_8svx", open_muxer);
    reg.register_extension("8svx", "iff_8svx");
    reg.register_extension("iff", "iff_8svx");
    reg.register_probe("iff_8svx", probe);
}

/// `FORM....8SVX` — IFF group chunk with the 8SVX form type.
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 12 && &p.buf[0..4] == b"FORM" && &p.buf[8..12] == b"8SVX" {
        100
    } else {
        0
    }
}

// --- Compression + channel types -----------------------------------------

/// 8SVX `sCompression` values we support end-to-end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// No compression; BODY is raw signed 8-bit PCM.
    #[default]
    None,
    /// Fibonacci-delta compression (`sCompression = 1`). Each channel's
    /// compressed byte stream begins with a pad byte and an initial
    /// signed 8-bit sample, followed by 4-bit delta nibbles (high nibble
    /// first).
    Fibonacci,
}

impl Compression {
    fn to_vhdr_byte(self) -> u8 {
        match self {
            Compression::None => 0,
            Compression::Fibonacci => 1,
        }
    }

    fn from_vhdr_byte(b: u8) -> Result<Self> {
        match b {
            0 => Ok(Compression::None),
            1 => Ok(Compression::Fibonacci),
            other => Err(Error::unsupported(format!(
                "8SVX: compression {} not implemented (0=none, 1=Fibonacci)",
                other
            ))),
        }
    }
}

/// Channel layout accepted by the muxer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Channels {
    /// Single-channel voice; no `CHAN` chunk needed (LEFT implied).
    #[default]
    Mono,
    /// Two channels stored back-to-back in BODY (LEFT then RIGHT).
    Stereo,
}

impl Channels {
    fn count(self) -> u16 {
        match self {
            Channels::Mono => 1,
            Channels::Stereo => 2,
        }
    }

    /// `CHAN` chunk payload: LEFT (2), RIGHT (4), STEREO (LEFT|RIGHT = 6).
    fn chan_value(self) -> u32 {
        match self {
            Channels::Mono => 2,
            Channels::Stereo => 6,
        }
    }
}

// --- Fibonacci-delta codec -----------------------------------------------

/// Standard Amiga 8SVX Fibonacci-delta table (16 entries). The 4-bit
/// nibble selector indexes directly into this array. We deliberately use
/// the 16-entry version from the Amiga ROM Kernel Manual / AmigaOS wiki
/// rather than the 17-entry variant sometimes cited — a 4-bit code only
/// covers codes 0..15 so the 17th value (`34`) is unreachable.
#[doc(hidden)] // internal — exposed for tests/fuzz; not part of the stable API
pub const FIB_DELTA_TABLE: [i32; 16] =
    [-34, -21, -13, -8, -5, -3, -2, -1, 0, 1, 2, 3, 5, 8, 13, 21];

/// Pick the nibble (0..=15) whose delta most closely approaches
/// `target - prev` and return `(nibble, new_prev)` where `new_prev` is
/// clamped to [-128, 127] — matching what the decoder will reconstruct.
fn fib_pick_nibble(prev: i32, target: i32) -> (u8, i32) {
    let mut best_idx = 0u8;
    let mut best_err = i64::MAX;
    let mut best_next = prev;
    for (i, delta) in FIB_DELTA_TABLE.iter().enumerate() {
        let next = (prev + delta).clamp(-128, 127);
        let err = (next as i64 - target as i64).abs();
        if err < best_err {
            best_err = err;
            best_idx = i as u8;
            best_next = next;
        }
    }
    (best_idx, best_next)
}

/// Encode one mono channel's worth of `i8` samples to the Fibonacci-delta
/// byte stream: `[pad=0x00, initial_sample_u8, packed_nibbles..]`. Two
/// deltas share one byte, high nibble first. If the sample count after
/// the initial is odd the final low nibble is padded with index 8 (delta
/// 0) so the result is always a whole number of bytes.
#[doc(hidden)] // internal — exposed for tests/fuzz; not part of the stable API
pub fn fibonacci_encode_channel(samples: &[i8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0u8); // pad byte
    if samples.is_empty() {
        out.push(0u8);
        return out;
    }
    let initial = samples[0];
    out.push(initial as u8);

    let mut prev = initial as i32;
    let deltas = &samples[1..];
    let mut i = 0;
    while i < deltas.len() {
        let (hi_idx, next_hi) = fib_pick_nibble(prev, deltas[i] as i32);
        prev = next_hi;
        let (lo_idx, next_lo) = if i + 1 < deltas.len() {
            let (idx, np) = fib_pick_nibble(prev, deltas[i + 1] as i32);
            (idx, np)
        } else {
            // Pad with zero-delta (index 8) so the stream stays whole bytes.
            (8u8, prev)
        };
        prev = next_lo;
        out.push((hi_idx << 4) | (lo_idx & 0x0F));
        i += 2;
    }
    out
}

/// Decode one mono channel's Fibonacci-delta byte stream. Returns the
/// reconstructed `i8` samples, including the stored initial value. The
/// caller is responsible for knowing how many samples the channel should
/// produce (typically `VHDR.oneShotHiSamples`); callers can truncate the
/// output.
#[doc(hidden)] // internal — exposed for tests/fuzz; not part of the stable API
pub fn fibonacci_decode_channel(body: &[u8]) -> Result<Vec<i8>> {
    if body.len() < 2 {
        return Err(Error::invalid(
            "8SVX Fibonacci BODY: need at least pad + initial byte",
        ));
    }
    // body[0] is the pad byte (ignored — typically 0).
    let initial = body[1] as i8;
    let mut out = Vec::with_capacity(2 * (body.len() - 2) + 1);
    out.push(initial);
    let mut prev = initial as i32;
    for &byte in &body[2..] {
        let hi = ((byte >> 4) & 0x0F) as usize;
        let lo = (byte & 0x0F) as usize;
        prev = (prev + FIB_DELTA_TABLE[hi]).clamp(-128, 127);
        out.push(prev as i8);
        prev = (prev + FIB_DELTA_TABLE[lo]).clamp(-128, 127);
        out.push(prev as i8);
    }
    Ok(out)
}

/// Decode a whole BODY: takes the compression mode, channel count, and
/// expected per-channel frame count. Returns interleaved `pcm_s8` bytes
/// (as produced by the demuxer: L0 R0 L1 R1 …).
///
/// A stereo BODY is concatenated halves — the LEFT channel **in full**
/// (all of its octaves), then the RIGHT channel in full — so the split
/// point is `body.len() / 2`, not the frame count: a voice whose header
/// announces fewer frames than the body carries must still split the
/// channels where the wire actually puts them.
fn decode_body(
    body: &[u8],
    compression: Compression,
    channels: u16,
    frames_per_channel: usize,
) -> Result<Vec<u8>> {
    match compression {
        Compression::None => {
            // Raw PCM: mono is already interleaved-by-definition; for
            // stereo we need to convert concatenated halves (L…L R…R)
            // into interleaved (L R L R …).
            if channels <= 1 {
                let take = frames_per_channel.min(body.len());
                return Ok(body[..take].to_vec());
            }
            let half = body.len() / 2;
            let take = frames_per_channel.min(half);
            let (left, right) = body.split_at(half);
            let mut out = Vec::with_capacity(2 * take);
            for i in 0..take {
                out.push(left[i]);
                out.push(right[i]);
            }
            Ok(out)
        }
        Compression::Fibonacci => {
            if channels <= 1 {
                let samples = fibonacci_decode_channel(body)?;
                let take = frames_per_channel.min(samples.len());
                Ok(samples[..take].iter().map(|&s| s as u8).collect())
            } else {
                // Stereo: the two halves are equal length.
                if body.len() % 2 != 0 {
                    return Err(Error::invalid(
                        "8SVX Fibonacci stereo BODY: odd length can't split into equal halves",
                    ));
                }
                let half = body.len() / 2;
                let left = fibonacci_decode_channel(&body[..half])?;
                let right = fibonacci_decode_channel(&body[half..])?;
                let take = frames_per_channel.min(left.len()).min(right.len());
                let mut out = Vec::with_capacity(2 * take);
                for i in 0..take {
                    out.push(left[i] as u8);
                    out.push(right[i] as u8);
                }
                Ok(out)
            }
        }
    }
}

// --- VHDR — Voice8Header ---------------------------------------------------

/// The 20-byte `VHDR` Voice8Header — the mandatory 8SVX voice header.
///
/// A voice holds waveform data for one or more octaves. The one-shot
/// part is played once and the repeat part is looped; the sum of
/// [`one_shot_hi_samples`](Self::one_shot_hi_samples) and
/// [`repeat_hi_samples`](Self::repeat_hi_samples) is the full length of
/// the **highest** octave waveform, and each following octave waveform
/// is twice as long as the previous one (`ctOctave` octaves total,
/// highest octave stored first in BODY).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceHeader {
    /// Length (in per-channel samples) of the highest-octave one-shot
    /// part — played once.
    pub one_shot_hi_samples: u32,
    /// Length (in per-channel samples) of the highest-octave repeat
    /// part — looped after the one-shot part.
    pub repeat_hi_samples: u32,
    /// Samples per cycle in the highest octave (the waveform period);
    /// `0` when unknown / not applicable.
    pub samples_per_hi_cycle: u32,
    /// Playback sample rate in Hz.
    pub samples_per_sec: u16,
    /// Number of octave waveforms stored in BODY (highest first). `0`
    /// is treated like `1` (a single octave) by the readers here.
    pub ct_octave: u8,
    /// `sCompression` byte: `0` = none, `1` = Fibonacci-delta. Decoded
    /// via [`VoiceHeader::compression`].
    pub compression_byte: u8,
    /// Playback volume, 16.16 fixed point: `0` = silent, `0x1_0000` =
    /// maximum.
    pub volume: u32,
}

/// Maximum-volume constant for the 16.16 fixed-point `VHDR.volume` /
/// `EGPoint.dest` fields (`0x1_0000` = 1.0).
pub const UNITY_VOLUME: u32 = 0x0001_0000;

impl VoiceHeader {
    /// Parse the 20-byte `VHDR` chunk body.
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 20 {
            return Err(Error::invalid("8SVX VHDR: need 20 bytes"));
        }
        Ok(VoiceHeader {
            one_shot_hi_samples: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
            repeat_hi_samples: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
            samples_per_hi_cycle: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
            samples_per_sec: u16::from_be_bytes([body[12], body[13]]),
            ct_octave: body[14],
            compression_byte: body[15],
            volume: u32::from_be_bytes([body[16], body[17], body[18], body[19]]),
        })
    }

    /// Serialise back to the 20-byte `VHDR` chunk body.
    pub fn write(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[0..4].copy_from_slice(&self.one_shot_hi_samples.to_be_bytes());
        out[4..8].copy_from_slice(&self.repeat_hi_samples.to_be_bytes());
        out[8..12].copy_from_slice(&self.samples_per_hi_cycle.to_be_bytes());
        out[12..14].copy_from_slice(&self.samples_per_sec.to_be_bytes());
        out[14] = self.ct_octave;
        out[15] = self.compression_byte;
        out[16..20].copy_from_slice(&self.volume.to_be_bytes());
        out
    }

    /// The decoded [`Compression`] mode, or `Error::unsupported` for a
    /// code other than 0 / 1.
    pub fn compression(&self) -> Result<Compression> {
        Compression::from_vhdr_byte(self.compression_byte)
    }

    /// Full length of the **highest**-octave waveform in per-channel
    /// samples: `oneShotHiSamples + repeatHiSamples`.
    pub fn hi_octave_samples(&self) -> u64 {
        self.one_shot_hi_samples as u64 + self.repeat_hi_samples as u64
    }

    /// Length of octave `k` (0 = highest) in per-channel samples. Each
    /// following octave waveform is twice as long as the previous one.
    /// `None` on shift overflow or when `k >= ct_octave.max(1)`.
    pub fn octave_samples(&self, k: u8) -> Option<u64> {
        if k as u32 >= self.ct_octave.max(1) as u32 {
            return None;
        }
        self.hi_octave_samples().checked_shl(k as u32)
    }

    /// Length of octave `k`'s **one-shot** part in per-channel
    /// samples. The whole octave waveform doubles per octave, and its
    /// one-shot and repeat constituents double with it (the only split
    /// consistent with `one_shot + repeat` summing to the octave
    /// length at every octave). Same bounds/overflow behaviour as
    /// [`octave_samples`](Self::octave_samples).
    pub fn one_shot_samples(&self, k: u8) -> Option<u64> {
        if k as u32 >= self.ct_octave.max(1) as u32 {
            return None;
        }
        (self.one_shot_hi_samples as u64).checked_shl(k as u32)
    }

    /// Length of octave `k`'s looped **repeat** part in per-channel
    /// samples — the counterpart of
    /// [`one_shot_samples`](Self::one_shot_samples).
    pub fn repeat_samples(&self, k: u8) -> Option<u64> {
        if k as u32 >= self.ct_octave.max(1) as u32 {
            return None;
        }
        (self.repeat_hi_samples as u64).checked_shl(k as u32)
    }

    /// Total per-channel samples across all `ct_octave` octaves:
    /// `hi * (2^ct - 1)`. `ct_octave == 0` is treated as one octave.
    /// Returns `None` when the doubling series overflows `u64` (a
    /// forged header — no real BODY can back it).
    pub fn total_samples_per_channel(&self) -> Option<u64> {
        let ct = self.ct_octave.max(1) as u32;
        let hi = self.hi_octave_samples();
        if hi == 0 {
            return Some(0);
        }
        // hi * (2^ct - 1) with overflow checks; ct can be up to 255 on
        // a hostile header, so go through u128 and reject > u64::MAX.
        if ct > 64 {
            return None;
        }
        let factor: u128 = (1u128 << ct) - 1;
        let total = hi as u128 * factor;
        u64::try_from(total).ok()
    }

    /// `true` when the voice has a looped repeat part
    /// (`repeatHiSamples > 0`).
    pub fn has_loop(&self) -> bool {
        self.repeat_hi_samples > 0
    }

    /// Playback volume as a float (`1.0` = maximum).
    pub fn volume_f32(&self) -> f32 {
        self.volume as f32 / UNITY_VOLUME as f32
    }

    /// Fundamental frequency of the highest octave in Hz —
    /// `samplesPerSec / samplesPerHiCycle` — or `None` when either
    /// field is zero.
    pub fn hi_cycle_frequency_hz(&self) -> Option<f64> {
        if self.samples_per_sec == 0 || self.samples_per_hi_cycle == 0 {
            return None;
        }
        Some(self.samples_per_sec as f64 / self.samples_per_hi_cycle as f64)
    }
}

// --- CHAN — channel assignment --------------------------------------------

/// The `CHAN` chunk payload — a 4-byte big-endian `sampleType` that
/// either routes a mono voice to one speaker or declares the BODY
/// stereo (two concatenated channel halves, LEFT first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelAssignment {
    /// `2` — play the mono sample on the left speaker.
    Left,
    /// `4` — play the mono sample on the right speaker.
    Right,
    /// `6` (`LEFT | RIGHT`) — the BODY contains two channels, left
    /// channel in full first, then the right channel in full.
    Stereo,
}

impl ChannelAssignment {
    /// Decode the wire value (`2` / `4` / `6`).
    pub fn from_raw(v: u32) -> Result<Self> {
        match v {
            2 => Ok(ChannelAssignment::Left),
            4 => Ok(ChannelAssignment::Right),
            6 => Ok(ChannelAssignment::Stereo),
            other => Err(Error::invalid(format!(
                "8SVX CHAN: sampleType {} is not 2 (left), 4 (right) or 6 (stereo)",
                other
            ))),
        }
    }

    /// Parse a `CHAN` chunk body (needs 4 bytes).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::invalid("8SVX CHAN: need 4 bytes"));
        }
        Self::from_raw(u32::from_be_bytes([body[0], body[1], body[2], body[3]]))
    }

    /// The wire value.
    pub fn raw(self) -> u32 {
        match self {
            ChannelAssignment::Left => 2,
            ChannelAssignment::Right => 4,
            ChannelAssignment::Stereo => 6,
        }
    }

    /// Serialise to the 4-byte chunk body.
    pub fn write(self) -> [u8; 4] {
        self.raw().to_be_bytes()
    }

    /// Number of sample channels the BODY carries (1 for the two mono
    /// routings, 2 for stereo).
    pub fn channel_count(self) -> u16 {
        match self {
            ChannelAssignment::Stereo => 2,
            _ => 1,
        }
    }
}

// --- PAN — mono stereo position -------------------------------------------

/// The `PAN` chunk — an optional stereo position for a **mono** sample:
/// a 4-byte big-endian signed `sposition` where `0` = fully right,
/// `0x8000` = center, `0x1_0000` = fully left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pan {
    /// `sposition` — `0` right … `0x8000` center … `0x1_0000` left.
    pub position: i32,
}

impl Pan {
    /// `sposition` value for fully-right placement.
    pub const RIGHT: i32 = 0;
    /// `sposition` value for center placement.
    pub const CENTER: i32 = 0x8000;
    /// `sposition` value for fully-left placement.
    pub const LEFT: i32 = 0x0001_0000;

    /// Parse a `PAN` chunk body (needs 4 bytes).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::invalid("8SVX PAN: need 4 bytes"));
        }
        Ok(Pan {
            position: i32::from_be_bytes([body[0], body[1], body[2], body[3]]),
        })
    }

    /// Serialise to the 4-byte chunk body.
    pub fn write(&self) -> [u8; 4] {
        self.position.to_be_bytes()
    }

    /// Fraction of the signal sent to the **left** speaker, in
    /// `0.0..=1.0` (the wire value clamped into the documented
    /// `0..=0x1_0000` range and scaled).
    pub fn left_weight(&self) -> f32 {
        self.position.clamp(0, Self::LEFT) as f32 / Self::LEFT as f32
    }

    /// Fraction of the signal sent to the **right** speaker
    /// (`1.0 - left_weight()`).
    pub fn right_weight(&self) -> f32 {
        1.0 - self.left_weight()
    }
}

// --- ATAK / RLSE — volume envelopes ---------------------------------------

/// One envelope-generator point: ramp the playback volume toward
/// `dest` over `duration_ms` milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EgPoint {
    /// Segment duration in milliseconds.
    pub duration_ms: u16,
    /// Destination volume, 16.16 fixed point: `0` = off,
    /// [`UNITY_VOLUME`] (`0x1_0000`) = maximum.
    pub dest: i32,
}

/// An `ATAK` (attack) or `RLSE` (release) volume envelope — a list of
/// [`EgPoint`]s that gradually in- or decrease the `VHDR` volume.
/// Multiple `ATAK` and `RLSE` chunks can appear in one FORM to build a
/// full ADSR (attack / decay / sustain / release) envelope; the FORM
/// walker preserves them in document order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Envelope {
    /// The envelope points, in wire order.
    pub points: Vec<EgPoint>,
}

impl Envelope {
    /// Parse an `ATAK` / `RLSE` chunk body: a flat array of 6-byte
    /// `EGPoint`s (`u16` duration in ms + `i32` destination volume,
    /// both big-endian). A body that is not a whole number of points
    /// is rejected.
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() % 6 != 0 {
            return Err(Error::invalid(format!(
                "8SVX envelope: body of {} bytes is not a whole number of 6-byte EGPoints",
                body.len()
            )));
        }
        let points = body
            .chunks_exact(6)
            .map(|c| EgPoint {
                duration_ms: u16::from_be_bytes([c[0], c[1]]),
                dest: i32::from_be_bytes([c[2], c[3], c[4], c[5]]),
            })
            .collect();
        Ok(Envelope { points })
    }

    /// Serialise back to the chunk body (6 bytes per point).
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.points.len() * 6);
        for p in &self.points {
            out.extend_from_slice(&p.duration_ms.to_be_bytes());
            out.extend_from_slice(&p.dest.to_be_bytes());
        }
        out
    }

    /// Sum of all segment durations in milliseconds.
    pub fn total_duration_ms(&self) -> u64 {
        self.points.iter().map(|p| p.duration_ms as u64).sum()
    }

    /// Evaluate the envelope `t_ms` milliseconds in, starting from
    /// volume `start` (16.16 fixed point). Each point ramps linearly
    /// from the running level to its `dest` over its `duration_ms`
    /// (a zero-duration point is an immediate jump); past the final
    /// point the level holds at the last `dest`. An empty envelope
    /// returns `start`.
    pub fn level_at_ms(&self, start: i32, t_ms: u64) -> i32 {
        let mut level = start as i64;
        let mut elapsed: u64 = 0;
        for p in &self.points {
            let d = p.duration_ms as u64;
            if d == 0 || t_ms >= elapsed + d {
                level = p.dest as i64;
                elapsed += d;
                continue;
            }
            // Mid-segment: linear interpolation.
            let into = (t_ms - elapsed) as i64;
            let dest = p.dest as i64;
            level += (dest - level) * into / d as i64;
            return level as i32;
        }
        level as i32
    }
}

// --- SEQN / FADE — waveform sequencing ------------------------------------

/// One `SEQN` segment: a `[start, end)` sample range within the
/// highest-octave waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqnSegment {
    /// Segment start offset in samples, relative to the start of the
    /// highest-octave voice. Must be 32-bit aligned (a multiple of 4).
    pub start: u32,
    /// Segment end offset in samples (same origin and alignment rule).
    pub end: u32,
}

impl SeqnSegment {
    /// Segment length in samples (`0` when `end <= start`).
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// `true` when the segment covers no samples.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// The `SEQN` chunk — a sequence of segments within the voice waveform
/// that should be played in order, letting parts of the waveform play
/// multiple times. When a `SEQN` is used, `VHDR.oneShotHiSamples`
/// should be `0` and `VHDR.repeatHiSamples` should equal the waveform
/// length (halved for a stereo voice, whose VHDR counts one channel).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seqn {
    /// The playback segments, in wire (= playback) order.
    pub segments: Vec<SeqnSegment>,
}

impl Seqn {
    /// Parse a `SEQN` chunk body: a flat array of 8-byte segments
    /// (`u32` start + `u32` end, big-endian). A body that is not a
    /// whole number of segments is rejected.
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() % 8 != 0 {
            return Err(Error::invalid(format!(
                "8SVX SEQN: body of {} bytes is not a whole number of 8-byte segments",
                body.len()
            )));
        }
        let segments = body
            .chunks_exact(8)
            .map(|c| SeqnSegment {
                start: u32::from_be_bytes([c[0], c[1], c[2], c[3]]),
                end: u32::from_be_bytes([c[4], c[5], c[6], c[7]]),
            })
            .collect();
        Ok(Seqn { segments })
    }

    /// Serialise back to the chunk body (8 bytes per segment).
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.segments.len() * 8);
        for s in &self.segments {
            out.extend_from_slice(&s.start.to_be_bytes());
            out.extend_from_slice(&s.end.to_be_bytes());
        }
        out
    }

    /// Validate every segment against the documented constraints:
    /// offsets 32-bit aligned (multiples of 4), `start <= end`, and
    /// `end` within the highest-octave waveform the header describes
    /// (`hi_octave_samples`). Returns the first violation.
    pub fn validate(&self, header: &VoiceHeader) -> Result<()> {
        let limit = header.hi_octave_samples();
        for (i, s) in self.segments.iter().enumerate() {
            if s.start % 4 != 0 || s.end % 4 != 0 {
                return Err(Error::invalid(format!(
                    "8SVX SEQN segment {}: offsets {}..{} are not 32-bit aligned",
                    i, s.start, s.end
                )));
            }
            if s.start > s.end {
                return Err(Error::invalid(format!(
                    "8SVX SEQN segment {}: start {} past end {}",
                    i, s.start, s.end
                )));
            }
            if (s.end as u64) > limit {
                return Err(Error::invalid(format!(
                    "8SVX SEQN segment {}: end {} past the {}-sample waveform",
                    i, s.end, limit
                )));
            }
        }
        Ok(())
    }

    /// `true` when the VHDR follows the documented SEQN convention —
    /// `oneShotHiSamples == 0` (the whole waveform is the loopable
    /// repeat part the segments index into).
    pub fn vhdr_convention_holds(&self, header: &VoiceHeader) -> bool {
        header.one_shot_hi_samples == 0
    }
}

/// The `FADE` chunk — "start fade-out at this segment": a single
/// big-endian `u32` naming the [`Seqn`] segment at which the sound
/// should begin slowly fading to silence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fade {
    /// The wire value — read as an index into the `SEQN` segment list.
    pub segment: u32,
}

impl Fade {
    /// Parse a `FADE` chunk body (needs 4 bytes).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::invalid("8SVX FADE: need 4 bytes"));
        }
        Ok(Fade {
            segment: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
        })
    }

    /// Serialise to the 4-byte chunk body.
    pub fn write(&self) -> [u8; 4] {
        self.segment.to_be_bytes()
    }

    /// `true` when the value indexes an existing segment of `seqn`.
    pub fn is_valid_for(&self, seqn: &Seqn) -> bool {
        (self.segment as usize) < seqn.segments.len()
    }
}

// --- Voice — the structural parse/encode surface --------------------------

/// One channel's decoded waveform data, split per octave (octave 0 is
/// the highest = shortest; each following octave is twice as long).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelSamples {
    /// Per-octave signed 8-bit samples, highest octave first.
    pub octaves: Vec<Vec<i8>>,
}

impl ChannelSamples {
    /// The highest-octave waveform (octave 0).
    pub fn hi(&self) -> &[i8] {
        self.octaves.first().map(|o| o.as_slice()).unwrap_or(&[])
    }

    /// Total decoded samples across all octaves.
    pub fn total_len(&self) -> usize {
        self.octaves.iter().map(|o| o.len()).sum()
    }

    /// All octaves concatenated back into the stored BODY order.
    pub fn flattened(&self) -> Vec<i8> {
        let mut out = Vec::with_capacity(self.total_len());
        for o in &self.octaves {
            out.extend_from_slice(o);
        }
        out
    }

    /// Split octave `k` into its `(one_shot, repeat)` parts per the
    /// header's [`VoiceHeader::one_shot_samples`] /
    /// [`VoiceHeader::repeat_samples`] doubling series — the one-shot
    /// part is played once, the repeat part loops. `None` when octave
    /// `k` is absent or its stored length doesn't match the header's
    /// series (e.g. the single-octave fallback of a zeroed header).
    pub fn loop_split(&self, header: &VoiceHeader, k: u8) -> Option<(&[i8], &[i8])> {
        let octave = self.octaves.get(k as usize)?;
        let one_shot = header.one_shot_samples(k)?;
        let repeat = header.repeat_samples(k)?;
        if one_shot.checked_add(repeat)? != octave.len() as u64 {
            return None;
        }
        Some(octave.split_at(one_shot as usize))
    }
}

/// A fully-parsed `FORM 8SVX` voice: the typed header, every optional
/// voice-structure chunk, the text metadata, and the decoded waveform
/// data per channel and per octave.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Voice {
    /// The mandatory `VHDR` header.
    pub header: VoiceHeader,
    /// The `CHAN` assignment, when present (`None` = mono by default).
    pub channel: Option<ChannelAssignment>,
    /// The `PAN` stereo position, when present.
    pub pan: Option<Pan>,
    /// `ATAK` attack envelopes in document order.
    pub attack: Vec<Envelope>,
    /// `RLSE` release envelopes in document order.
    pub release: Vec<Envelope>,
    /// The `SEQN` segment sequence, when present.
    pub seqn: Option<Seqn>,
    /// The `FADE` fade-out trigger, when present.
    pub fade: Option<Fade>,
    /// Text chunks as `(key, value)` pairs in document order — the
    /// same key mapping the demuxer uses (`NAME` → `title`, `AUTH` →
    /// `artist`, `ANNO` → `comment`, `(c) ` → `copyright`, `CHRS` →
    /// `characters`).
    pub metadata: Vec<(String, String)>,
    /// Decoded waveform data: one entry for mono, two (left, right)
    /// for stereo.
    pub channels: Vec<ChannelSamples>,
}

impl Default for VoiceHeader {
    fn default() -> Self {
        VoiceHeader {
            one_shot_hi_samples: 0,
            repeat_hi_samples: 0,
            samples_per_hi_cycle: 0,
            samples_per_sec: 0,
            ct_octave: 1,
            compression_byte: 0,
            volume: UNITY_VOLUME,
        }
    }
}

impl Voice {
    /// Number of sample channels (1 or 2).
    pub fn channel_count(&self) -> u16 {
        self.channels.len() as u16
    }

    /// Number of octave waveforms per channel.
    pub fn octave_count(&self) -> usize {
        self.channels.first().map(|c| c.octaves.len()).unwrap_or(0)
    }
}

/// Map an 8SVX text-chunk FourCC to its metadata key (and back — see
/// `metadata_fourcc`).
fn text_key(id: &[u8; 4]) -> Option<&'static str> {
    match id {
        b"NAME" => Some("title"),
        b"AUTH" => Some("artist"),
        b"ANNO" => Some("comment"),
        b"(c) " => Some("copyright"),
        b"CHRS" => Some("characters"),
        _ => None,
    }
}

/// Parse an in-memory `FORM 8SVX` file into a typed [`Voice`]: the
/// `VHDR` header, every documented voice-structure chunk (`CHAN`,
/// `PAN`, `ATAK`/`RLSE`, `SEQN`, `FADE`, the text chunks), and the
/// BODY decoded (Fibonacci-delta expanded when `sCompression = 1`)
/// then split into per-channel, per-octave waveforms.
///
/// Structural strictness: a missing `VHDR` or `BODY`, a duplicate
/// single-instance chunk (`VHDR` / `CHAN` / `PAN` / `SEQN` / `FADE`),
/// a chunk running past the FORM, an unknown `CHAN` value, or a BODY
/// too short for the header's octave doubling series are each
/// rejected with `Error::invalid`. Unknown chunk IDs are skipped.
/// When the header's waveform lengths are all zero the whole decoded
/// channel is surfaced as a single octave (matching the demuxer's
/// body-derived fallback).
pub fn parse_voice(bytes: &[u8]) -> Result<Voice> {
    if bytes.len() < 12 {
        return Err(Error::invalid("8SVX: file shorter than FORM header"));
    }
    if &bytes[0..4] != b"FORM" {
        return Err(Error::invalid("8SVX: missing FORM signature"));
    }
    if &bytes[8..12] != b"8SVX" {
        return Err(Error::invalid(format!(
            "IFF: not an 8SVX file (form type {:?})",
            std::str::from_utf8(&bytes[8..12]).unwrap_or("????")
        )));
    }
    let total = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body_end = (8usize.saturating_add(total)).min(bytes.len());

    let mut header: Option<VoiceHeader> = None;
    let mut channel: Option<ChannelAssignment> = None;
    let mut pan: Option<Pan> = None;
    let mut attack: Vec<Envelope> = Vec::new();
    let mut release: Vec<Envelope> = Vec::new();
    let mut seqn: Option<Seqn> = None;
    let mut fade: Option<Fade> = None;
    let mut metadata: Vec<(String, String)> = Vec::new();
    let mut body: Option<&[u8]> = None;

    let mut cursor = 12usize;
    while cursor + 8 <= body_end {
        let id = [
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ];
        let size = u32::from_be_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let payload_start = cursor + 8;
        let payload_end = payload_start.saturating_add(size);
        if payload_end > body_end {
            return Err(Error::invalid(format!(
                "8SVX: chunk {:?} extends past FORM ({} > {})",
                std::str::from_utf8(&id).unwrap_or("????"),
                payload_end,
                body_end
            )));
        }
        let payload = &bytes[payload_start..payload_end];
        let dup = |what: &str| Error::invalid(format!("8SVX: duplicate {} chunk", what));
        match &id {
            b"VHDR" => {
                if header.is_some() {
                    return Err(dup("VHDR"));
                }
                header = Some(VoiceHeader::parse(payload)?);
            }
            b"CHAN" => {
                if channel.is_some() {
                    return Err(dup("CHAN"));
                }
                channel = Some(ChannelAssignment::parse(payload)?);
            }
            b"PAN " => {
                if pan.is_some() {
                    return Err(dup("PAN"));
                }
                pan = Some(Pan::parse(payload)?);
            }
            b"ATAK" => attack.push(Envelope::parse(payload)?),
            b"RLSE" => release.push(Envelope::parse(payload)?),
            b"SEQN" => {
                if seqn.is_some() {
                    return Err(dup("SEQN"));
                }
                seqn = Some(Seqn::parse(payload)?);
            }
            b"FADE" => {
                if fade.is_some() {
                    return Err(dup("FADE"));
                }
                fade = Some(Fade::parse(payload)?);
            }
            b"BODY" => {
                if body.is_some() {
                    return Err(dup("BODY"));
                }
                body = Some(payload);
            }
            _ => {
                if let Some(key) = text_key(&id) {
                    let end = payload
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(payload.len());
                    let value = String::from_utf8_lossy(&payload[..end]).trim().to_string();
                    if !value.is_empty() {
                        metadata.push((key.into(), value));
                    }
                }
                // Unknown chunks are skipped.
            }
        }
        let padded = size + (size & 1);
        cursor = payload_start + padded;
    }

    let header = header.ok_or_else(|| Error::invalid("8SVX: missing VHDR chunk"))?;
    let body = body.ok_or_else(|| Error::invalid("8SVX: missing BODY chunk"))?;
    let compression = header.compression()?;
    let ch_count = channel.map_or(1, |c| c.channel_count()) as usize;

    // Split the BODY into channel streams (stereo = LEFT half in full
    // then RIGHT half in full) and decode each.
    let mut streams: Vec<Vec<i8>> = Vec::with_capacity(ch_count);
    if ch_count == 2 {
        if compression == Compression::Fibonacci && body.len() % 2 != 0 {
            return Err(Error::invalid(
                "8SVX Fibonacci stereo BODY: odd length can't split into equal halves",
            ));
        }
        let half = body.len() / 2;
        for raw in [&body[..half], &body[half..half * 2]] {
            streams.push(decode_channel_stream(raw, compression)?);
        }
    } else {
        streams.push(decode_channel_stream(body, compression)?);
    }

    // Split each channel into octave waveforms per the header's
    // doubling series. A zeroed header means "whole stream, single
    // octave"; otherwise the stream must supply the full series.
    let expected = header
        .total_samples_per_channel()
        .ok_or_else(|| Error::invalid("8SVX VHDR: octave doubling series overflows"))?;
    let mut channels_data: Vec<ChannelSamples> = Vec::with_capacity(ch_count);
    for stream in streams {
        if expected == 0 {
            channels_data.push(ChannelSamples {
                octaves: vec![stream],
            });
            continue;
        }
        if (stream.len() as u64) < expected {
            return Err(Error::invalid(format!(
                "8SVX BODY: channel supplies {} samples but VHDR's {} octave(s) need {}",
                stream.len(),
                header.ct_octave.max(1),
                expected
            )));
        }
        let mut octaves = Vec::with_capacity(header.ct_octave.max(1) as usize);
        let mut at = 0usize;
        let mut len = header.hi_octave_samples() as usize;
        for _ in 0..header.ct_octave.max(1) {
            octaves.push(stream[at..at + len].to_vec());
            at += len;
            len *= 2;
        }
        channels_data.push(ChannelSamples { octaves });
    }

    Ok(Voice {
        header,
        channel,
        pan,
        attack,
        release,
        seqn,
        fade,
        metadata,
        channels: channels_data,
    })
}

/// Decode one channel's raw BODY byte stream to signed samples.
fn decode_channel_stream(raw: &[u8], compression: Compression) -> Result<Vec<i8>> {
    match compression {
        Compression::None => Ok(raw.iter().map(|&b| b as i8).collect()),
        Compression::Fibonacci => fibonacci_decode_channel(raw),
    }
}

/// Encode a typed [`Voice`] back into a complete `FORM 8SVX` byte
/// stream — the inverse of [`parse_voice`].
///
/// The header is authoritative for `samplesPerSec`,
/// `samplesPerHiCycle`, `volume`, `sCompression`, and the
/// one-shot/repeat split; the waveform data must be shape-consistent
/// with it: the channel count must match the `CHAN` assignment (none
/// = mono), every channel must carry `ctOctave.max(1)` octaves whose
/// lengths follow the doubling series, and octave 0 must be
/// `oneShotHiSamples + repeatHiSamples` long (unless the header's
/// waveform lengths are all zero, in which case a single octave of
/// any length is accepted — the body-derived convention). Chunks are
/// written in canonical order: `VHDR`, `CHAN`, `PAN`, `ATAK`s,
/// `RLSE`s, `SEQN`, `FADE`, text chunks, `BODY`.
///
/// A raw voice round-trips losslessly through
/// `parse_voice(encode_voice(v))`; a Fibonacci voice round-trips
/// within the codec's ±2 LSB tolerance (and the first sample exactly).
pub fn encode_voice(voice: &Voice) -> Result<Vec<u8>> {
    let compression = voice.header.compression()?;
    let ch_expected = voice.channel.map_or(1, |c| c.channel_count()) as usize;
    if voice.channels.len() != ch_expected {
        return Err(Error::invalid(format!(
            "8SVX encode: {} channel(s) of data but the CHAN assignment implies {}",
            voice.channels.len(),
            ch_expected
        )));
    }

    // Shape-check the octave series against the header.
    let hi = voice.header.hi_octave_samples();
    let ct = voice.header.ct_octave.max(1) as usize;
    for (ci, ch) in voice.channels.iter().enumerate() {
        if hi == 0 {
            if ch.octaves.len() != 1 {
                return Err(Error::invalid(format!(
                    "8SVX encode: channel {} has {} octaves but the zeroed VHDR implies one",
                    ci,
                    ch.octaves.len()
                )));
            }
            continue;
        }
        if ch.octaves.len() != ct {
            return Err(Error::invalid(format!(
                "8SVX encode: channel {} has {} octaves, VHDR.ctOctave says {}",
                ci,
                ch.octaves.len(),
                ct
            )));
        }
        let mut want = hi;
        for (oi, o) in ch.octaves.iter().enumerate() {
            if o.len() as u64 != want {
                return Err(Error::invalid(format!(
                    "8SVX encode: channel {} octave {} has {} samples, expected {}",
                    ci,
                    oi,
                    o.len(),
                    want
                )));
            }
            want *= 2;
        }
    }
    if ch_expected == 2 && voice.channels[0].total_len() != voice.channels[1].total_len() {
        return Err(Error::invalid(
            "8SVX encode: stereo channels differ in length",
        ));
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"8SVX");

    let push_chunk = |out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]| {
        out.extend_from_slice(id);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(0);
        }
    };

    push_chunk(&mut out, b"VHDR", &voice.header.write());
    if let Some(chan) = voice.channel {
        push_chunk(&mut out, b"CHAN", &chan.write());
    }
    if let Some(pan) = voice.pan {
        push_chunk(&mut out, b"PAN ", &pan.write());
    }
    for env in &voice.attack {
        push_chunk(&mut out, b"ATAK", &env.write());
    }
    for env in &voice.release {
        push_chunk(&mut out, b"RLSE", &env.write());
    }
    if let Some(seqn) = &voice.seqn {
        push_chunk(&mut out, b"SEQN", &seqn.write());
    }
    if let Some(fade) = voice.fade {
        push_chunk(&mut out, b"FADE", &fade.write());
    }
    for (k, v) in &voice.metadata {
        let Some(fourcc) = metadata_fourcc(k) else {
            continue;
        };
        let mut payload = Vec::with_capacity(v.len() + 1);
        payload.extend_from_slice(v.as_bytes());
        payload.push(0);
        push_chunk(&mut out, fourcc, &payload);
    }

    // BODY: each channel's octaves concatenated (highest first), the
    // channels back to back (LEFT in full then RIGHT in full), each
    // channel Fibonacci-encoded independently when requested.
    let mut body = Vec::new();
    for ch in &voice.channels {
        let flat = ch.flattened();
        match compression {
            Compression::None => body.extend(flat.iter().map(|&s| s as u8)),
            Compression::Fibonacci => body.extend(fibonacci_encode_channel(&flat)),
        }
    }
    push_chunk(&mut out, b"BODY", &body);

    let total = out.len() as u32;
    out[4..8].copy_from_slice(&(total - 8).to_be_bytes());
    Ok(out)
}

// --- Demuxer --------------------------------------------------------------

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    // Outer FORM.
    let hdr = read_chunk_header(&mut *input)?.ok_or_else(|| Error::invalid("8SVX: empty file"))?;
    if hdr.id != GROUP_FORM {
        return Err(Error::invalid(format!(
            "8SVX: expected FORM chunk, got {}",
            hdr.id_str()
        )));
    }
    let form_type = read_form_type(&mut *input)?;
    if &form_type != b"8SVX" {
        return Err(Error::invalid(format!(
            "IFF: not an 8SVX file (form type {:?})",
            std::str::from_utf8(&form_type).unwrap_or("????")
        )));
    }
    // hdr.size counts FORM-type + children bytes; body length = hdr.size - 4.
    let body_limit = input.stream_position()? + hdr.size as u64 - 4;

    let mut vhdr: Option<VoiceHeader> = None;
    let mut channels: u16 = 1;
    let mut body_offset: u64 = 0;
    let mut body_size: u64 = 0;
    let mut metadata: Vec<(String, String)> = Vec::new();

    while input.stream_position()? < body_limit {
        let c = match read_chunk_header(&mut *input)? {
            Some(c) => c,
            None => break,
        };
        match &c.id {
            b"VHDR" => {
                let body = read_body(&mut *input, &c)?;
                vhdr = Some(VoiceHeader::parse(&body)?);
                pad_after(&mut *input, &c)?;
            }
            b"CHAN" => {
                // CHAN payload: 4 bytes BE. 2 = left, 4 = right, 6 = stereo.
                let body = read_body(&mut *input, &c)?;
                if body.len() >= 4 {
                    let v = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    channels = if v == 6 { 2 } else { 1 };
                    // A mono voice routed to one specific speaker is
                    // surfaced as metadata — the sample data itself is
                    // plain mono either way.
                    match v {
                        2 => metadata.push(("channel_assignment".into(), "left".into())),
                        4 => metadata.push(("channel_assignment".into(), "right".into())),
                        _ => {}
                    }
                }
                pad_after(&mut *input, &c)?;
            }
            b"NAME" | b"AUTH" | b"ANNO" | b"(c) " | b"CHRS" => {
                let body = read_body(&mut *input, &c)?;
                let key = match &c.id {
                    b"NAME" => "title",
                    b"AUTH" => "artist",
                    b"ANNO" => "comment",
                    b"(c) " => "copyright",
                    b"CHRS" => "characters",
                    _ => unreachable!(),
                };
                let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                let value = String::from_utf8_lossy(&body[..end]).trim().to_string();
                if !value.is_empty() {
                    metadata.push((key.into(), value));
                }
                pad_after(&mut *input, &c)?;
            }
            b"BODY" => {
                body_offset = input.stream_position()?;
                body_size = c.size as u64;
                break;
            }
            _ => skip_chunk_body(&mut *input, &c)?,
        }
    }

    let vhdr = vhdr.ok_or_else(|| Error::invalid("8SVX: missing VHDR chunk"))?;
    let compression = vhdr.compression()?;
    if body_size == 0 {
        return Err(Error::invalid("8SVX: missing BODY chunk"));
    }

    let sample_rate = vhdr.samples_per_sec as u32;
    let time_base = TimeBase::new(1, sample_rate as i64);

    // Work out the total frame count per channel. When VHDR is
    // populated this is the sum of the one-shot and repeat parts of
    // the highest octave, doubled per additional octave (BODY stores
    // ctOctave waveforms, highest first, each twice as long as the
    // previous). Whatever the header claims is bounded by what the
    // BODY can actually supply; a zeroed header falls back to the
    // body-derived capacity outright.
    let body_capacity: u64 = match compression {
        Compression::None => body_size / channels as u64,
        Compression::Fibonacci => {
            // (body_size / channels - 2) header bytes per channel,
            // then 2 decoded samples per remaining byte, plus the
            // stored initial sample.
            let per_channel = body_size / channels as u64;
            if per_channel < 2 {
                0
            } else {
                1 + 2 * (per_channel - 2)
            }
        }
    };
    let header_total = vhdr.total_samples_per_channel().unwrap_or(0);
    let frames_per_channel: u64 = if header_total > 0 {
        header_total.min(body_capacity)
    } else {
        body_capacity
    };
    let total_frames = frames_per_channel * channels as u64;

    if vhdr.ct_octave > 1 {
        metadata.push(("octaves".into(), vhdr.ct_octave.to_string()));
    }

    // Read the whole BODY into memory and decode once. 8SVX voices are
    // typically short (seconds, not hours) so this is fine in practice
    // and keeps the streaming path trivial. Grow-on-read (take +
    // read_to_end) instead of pre-allocating the declared size, so a
    // forged 32-bit BODY ckSize over a tiny stream can't demand an
    // attacker-sized buffer; a short read is then rejected.
    input.seek(SeekFrom::Start(body_offset))?;
    let mut raw_body = Vec::new();
    (&mut *input).take(body_size).read_to_end(&mut raw_body)?;
    if (raw_body.len() as u64) < body_size {
        return Err(Error::invalid(format!(
            "8SVX BODY declares {} bytes but the stream ends after {}",
            body_size,
            raw_body.len()
        )));
    }
    let decoded = decode_body(
        &raw_body,
        compression,
        channels,
        frames_per_channel as usize,
    )?;

    let mut params = CodecParameters::audio(CodecId::new("pcm_s8"));
    params.media_type = MediaType::Audio;
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(SampleFormat::S8);
    params.bit_rate = Some(8 * channels as u64 * sample_rate as u64);

    let stream = StreamInfo {
        index: 0,
        time_base,
        duration: Some(frames_per_channel as i64),
        start_time: Some(0),
        params,
    };

    let duration_micros: i64 = if sample_rate > 0 {
        (frames_per_channel as i128 * 1_000_000 / sample_rate as i128) as i64
    } else {
        0
    };

    let _ = total_frames; // kept for debug symmetry; not otherwise used.

    Ok(Box::new(SvxDemuxer {
        streams: vec![stream],
        decoded,
        cursor: 0,
        channels,
        frames_emitted: 0,
        metadata,
        duration_micros,
    }))
}

fn pad_after<R: Seek + ?Sized>(r: &mut R, c: &ChunkHeader) -> Result<()> {
    if c.size & 1 == 1 {
        r.seek(SeekFrom::Current(1))?;
    }
    Ok(())
}

struct SvxDemuxer {
    streams: Vec<StreamInfo>,
    /// Fully-decoded interleaved `pcm_s8` bytes. For mono this is the raw
    /// BODY (after Fibonacci decompression if needed); for stereo this
    /// has been de-concatenated from LEFT-then-RIGHT halves to interleaved
    /// L R L R … frames.
    decoded: Vec<u8>,
    cursor: usize,
    channels: u16,
    frames_emitted: i64,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
}

const CHUNK_FRAMES: usize = 4096;

impl Demuxer for SvxDemuxer {
    fn format_name(&self) -> &str {
        "iff_8svx"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.cursor >= self.decoded.len() {
            return Err(Error::Eof);
        }
        let bytes_per_frame = self.channels as usize;
        let remaining = self.decoded.len() - self.cursor;
        let want_bytes = (CHUNK_FRAMES * bytes_per_frame).min(remaining);
        let want_bytes = (want_bytes / bytes_per_frame) * bytes_per_frame;
        if want_bytes == 0 {
            return Err(Error::Eof);
        }

        let buf = self.decoded[self.cursor..self.cursor + want_bytes].to_vec();
        self.cursor += want_bytes;

        let stream = &self.streams[0];
        let frames = want_bytes / bytes_per_frame;
        let pts = self.frames_emitted;
        self.frames_emitted += frames as i64;

        let mut pkt = Packet::new(0, stream.time_base, buf);
        pkt.pts = Some(pts);
        pkt.dts = Some(pts);
        pkt.duration = Some(frames as i64);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    /// Seek to the per-channel frame at `pts`. 8SVX is keyframe-only
    /// PCM (raw `pcm_s8` after Fibonacci decompression on open), so the
    /// returned pts equals `pts.clamp(0, total_frames)` — no keyframe
    /// quantisation, no decode reset needed. The whole BODY was already
    /// expanded into an interleaved frame buffer at open-time, so the
    /// seek is a pure cursor reset: `cursor = target * channels`. The
    /// next packet's `pts` will equal the returned value.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index != 0 {
            return Err(Error::invalid(format!(
                "8SVX: stream index {stream_index} out of range"
            )));
        }
        let bytes_per_frame = self.channels as usize;
        // `decoded` is interleaved frames of `pcm_s8` (1 byte/sample),
        // so total_frames = decoded.len() / channels.
        let total_frames = (self.decoded.len() / bytes_per_frame) as i64;
        let target = pts.max(0).min(total_frames);
        let new_cursor = (target as usize)
            .checked_mul(bytes_per_frame)
            .ok_or_else(|| Error::invalid("8SVX seek: cursor overflow"))?;
        debug_assert!(new_cursor <= self.decoded.len());
        self.cursor = new_cursor;
        self.frames_emitted = target;
        Ok(target)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

// --- Muxer ---------------------------------------------------------------

/// Open a muxer through the [`ContainerRegistry`] with no container-level
/// metadata. For callers that need to write `NAME` / `AUTH` / `ANNO` /
/// `(c) ` / `CHRS` chunks, construct [`SvxMuxer`] directly via
/// [`SvxMuxer::with_metadata`] — the `Muxer` trait doesn't currently carry
/// metadata through its opening hook.
fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(SvxMuxer::new(output, streams)?))
}

/// 8SVX container muxer. Wraps one stream of 8-bit signed PCM
/// (`pcm_s8` / [`SampleFormat::S8`]) in an IFF FORM/8SVX tree:
/// `VHDR` (20 bytes) + optional string metadata + `BODY` (the raw samples).
///
/// Construct via [`SvxMuxer::new`] for a bare voice, or
/// [`SvxMuxer::with_metadata`] to attach `NAME` / `AUTH` / `ANNO` /
/// `(c) ` / `CHRS` chunks. The demuxer trims values at the first NUL and
/// decodes them UTF-8-lossy, which matches how the muxer writes them
/// (NUL-terminated, even-padded payload).
pub struct SvxMuxer {
    output: Box<dyn WriteSeek>,
    channels: Channels,
    compression: Compression,
    sample_rate: u32,
    /// Ordered (key, value) pairs. Recognised keys: `title` → `NAME`,
    /// `artist` → `AUTH`, `comment` → `ANNO`, `copyright` → `(c) `,
    /// `characters` → `CHRS`.
    metadata: Vec<(String, String)>,
    /// VHDR playback volume (16.16 fixed; default [`UNITY_VOLUME`]).
    volume: u32,
    /// VHDR samplesPerHiCycle (default 0 = unknown).
    samples_per_hi_cycle: u32,
    /// When set, that many trailing frames are recorded as the looped
    /// repeat part (`VHDR.repeatHiSamples`); clamped to the frame count.
    repeat_frames: Option<u32>,
    /// Mono speaker routing (`CHAN` 2/4). `None` = no CHAN for mono.
    mono_routing: Option<ChannelAssignment>,
    /// Optional `PAN ` chunk (mono voices only).
    pan: Option<Pan>,
    /// `ATAK` envelopes, written in order.
    attacks: Vec<Envelope>,
    /// `RLSE` envelopes, written in order.
    releases: Vec<Envelope>,
    /// Optional `SEQN` chunk.
    seqn: Option<Seqn>,
    /// Optional `FADE` chunk.
    fade: Option<Fade>,
    form_size_offset: u64,
    body_size_offset: u64,
    /// Interleaved pcm_s8 bytes buffered from `write_packet`. We emit
    /// the actual BODY at `write_trailer` time, since both stereo
    /// (concat halves) and Fibonacci (needs full per-channel streams)
    /// require seeing all samples before writing.
    pending: Vec<u8>,
    header_written: bool,
    trailer_written: bool,
}

impl SvxMuxer {
    /// Build a muxer that only writes VHDR + BODY (no string chunks).
    /// Defaults to uncompressed, mono is inferred from the stream's
    /// channel count (1 = mono, 2 = stereo). Use [`Self::with_compression`]
    /// after construction to switch on Fibonacci-delta.
    pub fn new(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        Self::with_metadata(output, streams, &[])
    }

    /// Build a muxer with container-level metadata. Only recognised keys
    /// are emitted; unknown keys are silently dropped. Values are written
    /// as NUL-terminated ASCII-ish text (non-ASCII passes through as raw
    /// bytes — the demuxer reads UTF-8 with lossy fallback).
    pub fn with_metadata(
        output: Box<dyn WriteSeek>,
        streams: &[StreamInfo],
        metadata: &[(String, String)],
    ) -> Result<Self> {
        if streams.len() != 1 {
            return Err(Error::unsupported("8SVX supports exactly one audio stream"));
        }
        let s = &streams[0];
        if s.params.media_type != MediaType::Audio {
            return Err(Error::invalid("8SVX stream must be audio"));
        }
        if s.params.codec_id != CodecId::new("pcm_s8") {
            return Err(Error::unsupported(format!(
                "8SVX muxer only accepts pcm_s8 (got {})",
                s.params.codec_id
            )));
        }
        if let Some(fmt) = s.params.sample_format {
            if fmt != SampleFormat::S8 {
                return Err(Error::unsupported(format!(
                    "8SVX muxer requires SampleFormat::S8 (got {:?})",
                    fmt
                )));
            }
        }
        let ch_count = s
            .params
            .channels
            .ok_or_else(|| Error::invalid("8SVX muxer: missing channels"))?;
        let channels = match ch_count {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => {
                return Err(Error::unsupported(format!(
                    "8SVX muxer: only mono or stereo is supported (got {} channels)",
                    n
                )))
            }
        };
        let sample_rate = s
            .params
            .sample_rate
            .ok_or_else(|| Error::invalid("8SVX muxer: missing sample rate"))?;
        if sample_rate > u16::MAX as u32 {
            return Err(Error::unsupported(format!(
                "8SVX VHDR.samplesPerSec is u16; {} Hz exceeds the range",
                sample_rate
            )));
        }
        Ok(Self {
            output,
            channels,
            compression: Compression::None,
            sample_rate,
            metadata: metadata.to_vec(),
            volume: UNITY_VOLUME,
            samples_per_hi_cycle: 0,
            repeat_frames: None,
            mono_routing: None,
            pan: None,
            attacks: Vec::new(),
            releases: Vec::new(),
            seqn: None,
            fade: None,
            form_size_offset: 0,
            body_size_offset: 0,
            pending: Vec::new(),
            header_written: false,
            trailer_written: false,
        })
    }

    /// Select the compression mode for the BODY. Must be called before
    /// `write_header`.
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Access the configured channel layout (derived from the stream).
    pub fn channels(&self) -> Channels {
        self.channels
    }

    /// Access the configured compression mode.
    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// Set the `VHDR` playback volume (16.16 fixed; [`UNITY_VOLUME`]
    /// is the default). Must be called before `write_header`.
    pub fn with_volume(mut self, volume: u32) -> Self {
        self.volume = volume;
        self
    }

    /// Set `VHDR.samplesPerHiCycle` — the waveform period of the
    /// highest octave in samples. Must be called before `write_header`.
    pub fn with_hi_cycle(mut self, samples_per_hi_cycle: u32) -> Self {
        self.samples_per_hi_cycle = samples_per_hi_cycle;
        self
    }

    /// Mark the trailing `frames` per-channel frames as the looped
    /// repeat part: `VHDR.repeatHiSamples = frames` and
    /// `oneShotHiSamples = total - frames` are patched at
    /// `write_trailer` time (clamped to the frames actually written).
    pub fn with_repeat(mut self, frames: u32) -> Self {
        self.repeat_frames = Some(frames);
        self
    }

    /// Route a **mono** voice to one speaker by writing a `CHAN` chunk
    /// (`ChannelAssignment::Left` or `Right`). Rejected at
    /// `write_header` time for a stereo stream or a `Stereo` argument
    /// (stereo streams get their `CHAN 6` automatically).
    pub fn with_mono_routing(mut self, routing: ChannelAssignment) -> Self {
        self.mono_routing = Some(routing);
        self
    }

    /// Attach a `PAN ` chunk giving the mono voice a stereo position.
    /// Rejected at `write_header` time for a stereo stream (the chunk
    /// pans a mono sample).
    pub fn with_pan(mut self, pan: Pan) -> Self {
        self.pan = Some(pan);
        self
    }

    /// Append an `ATAK` (attack) envelope chunk. May be called several
    /// times; chunks are written in call order (the documented way to
    /// build a full ADSR envelope together with `with_release`).
    pub fn with_attack(mut self, envelope: Envelope) -> Self {
        self.attacks.push(envelope);
        self
    }

    /// Append an `RLSE` (release) envelope chunk. May be called
    /// several times; chunks are written in call order.
    pub fn with_release(mut self, envelope: Envelope) -> Self {
        self.releases.push(envelope);
        self
    }

    /// Attach a `SEQN` playback-segment sequence.
    pub fn with_seqn(mut self, seqn: Seqn) -> Self {
        self.seqn = Some(seqn);
        self
    }

    /// Attach a `FADE` fade-out trigger.
    pub fn with_fade(mut self, fade: Fade) -> Self {
        self.fade = Some(fade);
        self
    }
}

/// Map a metadata key to its 8SVX FourCC. Unknown keys return `None`
/// and are dropped by the muxer.
fn metadata_fourcc(key: &str) -> Option<&'static [u8; 4]> {
    match key {
        "title" => Some(b"NAME"),
        "artist" => Some(b"AUTH"),
        "comment" => Some(b"ANNO"),
        "copyright" => Some(b"(c) "),
        "characters" => Some(b"CHRS"),
        _ => None,
    }
}

impl Muxer for SvxMuxer {
    fn format_name(&self) -> &str {
        "iff_8svx"
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("8SVX muxer: write_header called twice"));
        }
        if self.channels == Channels::Stereo {
            if self.pan.is_some() {
                return Err(Error::invalid(
                    "8SVX muxer: PAN gives a stereo position to a MONO sample; \
                     a stereo stream cannot carry one",
                ));
            }
            if self.mono_routing.is_some() {
                return Err(Error::invalid(
                    "8SVX muxer: mono routing (CHAN 2/4) conflicts with a stereo stream",
                ));
            }
        }
        if matches!(self.mono_routing, Some(ChannelAssignment::Stereo)) {
            return Err(Error::invalid(
                "8SVX muxer: with_mono_routing takes Left or Right; \
                 stereo streams write CHAN 6 automatically",
            ));
        }

        // FORM group chunk header. Size is patched in write_trailer once
        // we know how much we wrote.
        self.output.write_all(b"FORM")?;
        self.form_size_offset = self.output.stream_position()?;
        self.output.write_all(&0u32.to_be_bytes())?; // placeholder
        self.output.write_all(b"8SVX")?;

        // VHDR (20 bytes). We synthesise a single-octave voice: the
        // one-shot/repeat frame counts aren't known yet (patched in
        // write_trailer per the optional `with_repeat` split),
        // samplesPerHiCycle and volume come from the builder options.
        self.output.write_all(b"VHDR")?;
        self.output.write_all(&20u32.to_be_bytes())?;
        // Frame counts aren't known yet; patched in write_trailer.
        self.output.write_all(&0u32.to_be_bytes())?; // oneShotHiSamples
        self.output.write_all(&0u32.to_be_bytes())?; // repeatHiSamples
        self.output
            .write_all(&self.samples_per_hi_cycle.to_be_bytes())?;
        self.output
            .write_all(&(self.sample_rate as u16).to_be_bytes())?;
        self.output.write_all(&[1u8])?; // ctOctave
        self.output.write_all(&[self.compression.to_vhdr_byte()])?; // sCompression
        self.output.write_all(&self.volume.to_be_bytes())?;

        // CHAN chunk: stereo always writes LEFT|RIGHT = 6; a mono
        // stream writes CHAN 2/4 only when a routing was requested
        // (mono is the default when CHAN is absent).
        let chan_value = if self.channels == Channels::Stereo {
            Some(self.channels.chan_value())
        } else {
            self.mono_routing.map(|r| r.raw())
        };
        if let Some(v) = chan_value {
            self.output.write_all(b"CHAN")?;
            self.output.write_all(&4u32.to_be_bytes())?;
            self.output.write_all(&v.to_be_bytes())?;
        }

        // Voice-structure chunks, canonical order: PAN, ATAK*, RLSE*,
        // SEQN, FADE (every payload here is even-sized, no pad byte).
        if let Some(pan) = self.pan {
            self.output.write_all(b"PAN ")?;
            self.output.write_all(&4u32.to_be_bytes())?;
            self.output.write_all(&pan.write())?;
        }
        for (id, envs) in [(b"ATAK", &self.attacks), (b"RLSE", &self.releases)] {
            for env in envs {
                let payload = env.write();
                self.output.write_all(id)?;
                self.output
                    .write_all(&(payload.len() as u32).to_be_bytes())?;
                self.output.write_all(&payload)?;
            }
        }
        if let Some(seqn) = &self.seqn {
            let payload = seqn.write();
            self.output.write_all(b"SEQN")?;
            self.output
                .write_all(&(payload.len() as u32).to_be_bytes())?;
            self.output.write_all(&payload)?;
        }
        if let Some(fade) = self.fade {
            self.output.write_all(b"FADE")?;
            self.output.write_all(&4u32.to_be_bytes())?;
            self.output.write_all(&fade.write())?;
        }

        // Optional metadata chunks. Preserve caller-supplied order so
        // round-trips are stable. The demuxer strips trailing NULs, so
        // we always NUL-terminate and pad to even length.
        for (k, v) in &self.metadata {
            let Some(fourcc) = metadata_fourcc(k) else {
                continue;
            };
            let bytes = v.as_bytes();
            // NUL-terminate: the demuxer splits on the first NUL.
            let mut payload = Vec::with_capacity(bytes.len() + 1);
            payload.extend_from_slice(bytes);
            payload.push(0);
            let size = payload.len() as u32;
            self.output.write_all(fourcc)?;
            self.output.write_all(&size.to_be_bytes())?;
            self.output.write_all(&payload)?;
            if size & 1 == 1 {
                self.output.write_all(&[0u8])?; // IFF pad byte
            }
        }

        // BODY chunk header; body size is patched in write_trailer.
        self.output.write_all(b"BODY")?;
        self.body_size_offset = self.output.stream_position()?;
        self.output.write_all(&0u32.to_be_bytes())?; // placeholder

        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("8SVX muxer: write_header not called"));
        }
        if self.trailer_written {
            return Err(Error::other("8SVX muxer: write_packet after trailer"));
        }
        // Incoming payload is interleaved pcm_s8 — `channels` bytes per
        // frame. We buffer and commit to BODY at `write_trailer` time so
        // we can split stereo into concatenated halves and/or apply
        // Fibonacci-delta encoding to each channel independently.
        self.pending.extend_from_slice(&packet.data);
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(Error::other("8SVX muxer: write_header not called"));
        }

        // Build the on-disk BODY bytes from buffered interleaved pcm_s8.
        let ch_count = self.channels.count();
        if self.pending.len() % ch_count as usize != 0 {
            return Err(Error::invalid(
                "8SVX muxer: packet total not a multiple of channel count",
            ));
        }
        let frames_per_channel = self.pending.len() / ch_count as usize;
        let body = match (self.channels, self.compression) {
            (Channels::Mono, Compression::None) => self.pending.clone(),
            (Channels::Mono, Compression::Fibonacci) => {
                let samples: Vec<i8> = self.pending.iter().map(|&b| b as i8).collect();
                fibonacci_encode_channel(&samples)
            }
            (Channels::Stereo, Compression::None) => {
                // De-interleave into concatenated halves (L…L then R…R).
                let mut out = Vec::with_capacity(self.pending.len());
                out.extend(self.pending.iter().step_by(2).copied());
                out.extend(self.pending.iter().skip(1).step_by(2).copied());
                out
            }
            (Channels::Stereo, Compression::Fibonacci) => {
                let mut left: Vec<i8> = Vec::with_capacity(frames_per_channel);
                let mut right: Vec<i8> = Vec::with_capacity(frames_per_channel);
                for frame in self.pending.chunks_exact(2) {
                    left.push(frame[0] as i8);
                    right.push(frame[1] as i8);
                }
                let mut l_enc = fibonacci_encode_channel(&left);
                let r_enc = fibonacci_encode_channel(&right);
                l_enc.extend_from_slice(&r_enc);
                l_enc
            }
        };

        let body_bytes = body.len() as u64;
        self.output.write_all(&body)?;

        // IFF chunks pad to even length; BODY is the last child chunk so
        // its pad byte (if any) also pads the enclosing FORM.
        if body_bytes & 1 == 1 {
            self.output.write_all(&[0u8])?;
        }
        let end = self.output.stream_position()?;

        // Patch BODY chunk size.
        let body_size_u32: u32 = body_bytes
            .try_into()
            .map_err(|_| Error::other("8SVX BODY chunk exceeds 4 GiB"))?;
        self.output.seek(SeekFrom::Start(self.body_size_offset))?;
        self.output.write_all(&body_size_u32.to_be_bytes())?;

        // Patch VHDR.oneShotHiSamples / repeatHiSamples with the
        // per-channel frame count. `form_size_offset` points at the
        // FORM size field (4 bytes), then comes "8SVX" (4), "VHDR" (4),
        // VHDR size (4) — so oneShotHiSamples lives at
        // form_size_offset + 16 and repeatHiSamples right after it.
        // Writing these lets a decoder that inspects VHDR know the full
        // length of the voice even before reaching BODY (and is
        // especially useful for Fibonacci-compressed voices, where the
        // sample count isn't trivially recoverable from BODY size).
        // A `with_repeat` request marks that many trailing frames as
        // the looped repeat part, clamped to the frames actually
        // written; the sum always equals the per-channel frame count.
        let total = u32::try_from(frames_per_channel)
            .map_err(|_| Error::other("8SVX VHDR sample counts exceed u32"))?;
        let repeat = self.repeat_frames.unwrap_or(0).min(total);
        let one_shot = total - repeat;
        self.output
            .seek(SeekFrom::Start(self.form_size_offset + 16))?;
        self.output.write_all(&one_shot.to_be_bytes())?;
        self.output.write_all(&repeat.to_be_bytes())?;

        // Patch FORM size: everything after the 8-byte FORM header.
        let form_size_u32: u32 = (end - (self.form_size_offset + 4))
            .try_into()
            .map_err(|_| Error::other("8SVX FORM size exceeds 4 GiB"))?;
        self.output.seek(SeekFrom::Start(self.form_size_offset))?;
        self.output.write_all(&form_size_u32.to_be_bytes())?;

        self.output.seek(SeekFrom::Start(end))?;
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Hand-craft a tiny 8SVX file: FORM 8SVX { VHDR, BODY = 10 signed bytes }.
    fn make_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        // FORM header: ID + size (filled in below) + form type
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"8SVX");

        // VHDR (20 bytes)
        out.extend_from_slice(b"VHDR");
        out.extend_from_slice(&20u32.to_be_bytes());
        out.extend_from_slice(&10u32.to_be_bytes()); // oneShotHiSamples
        out.extend_from_slice(&0u32.to_be_bytes()); // repeatHiSamples
        out.extend_from_slice(&0u32.to_be_bytes()); // samplesPerHiCycle
        out.extend_from_slice(&8000u16.to_be_bytes()); // samplesPerSec
        out.push(1); // ctOctave
        out.push(0); // sCompression (none)
        out.extend_from_slice(&0x10000u32.to_be_bytes()); // volume = 1.0

        // BODY: 10 signed 8-bit samples (pad to even: 10 is even, no pad)
        out.extend_from_slice(b"BODY");
        out.extend_from_slice(&10u32.to_be_bytes());
        let samples: [i8; 10] = [0, 16, 32, 48, 64, 48, 32, 16, 0, -16];
        for s in &samples {
            out.push(*s as u8);
        }

        // Patch FORM size = total - 8 (ID + size field).
        let total = out.len() as u32;
        out[4..8].copy_from_slice(&(total - 8).to_be_bytes());
        out
    }

    #[test]
    fn demux_minimal_8svx() {
        let bytes = make_fixture();
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let mut dmx = open(rs, &oxideav_core::NullCodecResolver).unwrap();
        assert_eq!(dmx.format_name(), "iff_8svx");
        let s = &dmx.streams()[0];
        assert_eq!(s.params.codec_id.as_str(), "pcm_s8");
        assert_eq!(s.params.channels, Some(1));
        assert_eq!(s.params.sample_rate, Some(8000));

        let pkt = dmx.next_packet().unwrap();
        assert_eq!(pkt.data.len(), 10);
        assert_eq!(pkt.data[0], 0);
        assert_eq!(pkt.data[9], 0xF0); // -16 as u8

        // End of stream.
        let err = dmx.next_packet().unwrap_err();
        assert!(matches!(err, Error::Eof));
    }

    /// Fibonacci round-trip on a smooth signal should reconstruct each
    /// sample within ±2 LSBs — matching the tolerance the Amiga Devices
    /// Manual cites for Fibonacci-delta.
    #[test]
    fn fibonacci_roundtrip_smooth_sine() {
        // Pure sine at ~120 Hz / 8 kHz, amplitude 100. Step ≈ 9.4 per
        // sample at the zero-crossing, which fits the table comfortably.
        let samples: Vec<i8> = (0..512)
            .map(|i| {
                let v = (100.0 * (i as f64 * std::f64::consts::TAU * 120.0 / 8000.0).sin()).round();
                v as i8
            })
            .collect();
        let encoded = fibonacci_encode_channel(&samples);
        let decoded = fibonacci_decode_channel(&encoded).unwrap();
        assert!(decoded.len() >= samples.len());
        for (i, (&orig, &dec)) in samples.iter().zip(decoded.iter()).enumerate() {
            let err = (orig as i32 - dec as i32).abs();
            assert!(err <= 2, "sample {i}: orig={orig} dec={dec} err={err}");
        }
    }

    /// The initial sample is stored verbatim in byte 1 of the
    /// Fibonacci-encoded stream.
    #[test]
    fn fibonacci_preserves_initial_sample() {
        let samples: Vec<i8> = vec![42, 40, 38, 36];
        let encoded = fibonacci_encode_channel(&samples);
        assert_eq!(encoded[0], 0, "pad byte");
        assert_eq!(encoded[1] as i8, 42, "initial sample");
        let decoded = fibonacci_decode_channel(&encoded).unwrap();
        assert_eq!(decoded[0], 42);
    }

    /// A single zero-delta nibble (index 8) must keep the sample flat.
    #[test]
    fn fibonacci_flat_signal() {
        let samples: Vec<i8> = vec![5; 32];
        let encoded = fibonacci_encode_channel(&samples);
        let decoded = fibonacci_decode_channel(&encoded).unwrap();
        for (i, &v) in decoded.iter().take(samples.len()).enumerate() {
            assert_eq!(v, 5, "sample {i}");
        }
    }
}
