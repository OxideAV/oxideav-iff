//! FORM 8SVX — Amiga 8-bit sampled voice audio.
//!
//! Layout: `FORM` group chunk → 4-byte `8SVX` form type → children:
//! - `VHDR` (20 bytes): voice header (one-shot/repeat sample counts,
//!   samples per high-cycle, samples per second, octave count, compression
//!   code, 16.16 volume).
//! - optional `NAME`, `ANNO`, `AUTH`, `(c) `, `CHAN`, `ATAK`, `RLSE`.
//! - `BODY`: raw signed 8-bit samples (or Fibonacci-delta compressed).
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

use oxideav_container::{ContainerRegistry, Demuxer, Muxer, ReadSeek, WriteSeek};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, MediaType, Packet, Result, SampleFormat,
    StreamInfo, TimeBase,
};

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
fn probe(p: &oxideav_container::ProbeData) -> u8 {
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
                return Ok(body.to_vec());
            }
            if body.len() < 2 * frames_per_channel {
                return Err(Error::invalid(
                    "8SVX stereo BODY shorter than 2 * frames_per_channel",
                ));
            }
            let (left, rest) = body.split_at(frames_per_channel);
            let right = &rest[..frames_per_channel];
            let mut out = Vec::with_capacity(2 * frames_per_channel);
            for i in 0..frames_per_channel {
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

// --- VHDR parsing ---------------------------------------------------------

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // VHDR holds metadata that's informational for now
struct Vhdr {
    one_shot_hi_samples: u32,
    repeat_hi_samples: u32,
    samples_per_hi_cycle: u32,
    samples_per_sec: u16,
    ct_octave: u8,
    compression: u8,
    volume_fixed: u32,
}

fn parse_vhdr(body: &[u8]) -> Result<Vhdr> {
    if body.len() < 20 {
        return Err(Error::invalid("8SVX VHDR: need 20 bytes"));
    }
    Ok(Vhdr {
        one_shot_hi_samples: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
        repeat_hi_samples: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
        samples_per_hi_cycle: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
        samples_per_sec: u16::from_be_bytes([body[12], body[13]]),
        ct_octave: body[14],
        compression: body[15],
        volume_fixed: u32::from_be_bytes([body[16], body[17], body[18], body[19]]),
    })
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

    let mut vhdr: Option<Vhdr> = None;
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
                vhdr = Some(parse_vhdr(&body)?);
                pad_after(&mut *input, &c)?;
            }
            b"CHAN" => {
                // CHAN payload: 4 bytes BE. 2 = left, 4 = right, 6 = stereo.
                let body = read_body(&mut *input, &c)?;
                if body.len() >= 4 {
                    let v = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    channels = if v == 6 { 2 } else { 1 };
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
    let compression = Compression::from_vhdr_byte(vhdr.compression)?;
    if body_size == 0 {
        return Err(Error::invalid("8SVX: missing BODY chunk"));
    }

    let sample_rate = vhdr.samples_per_sec as u32;
    let time_base = TimeBase::new(1, sample_rate as i64);

    // Work out the total frame count. VHDR.one_shot_hi_samples counts
    // frames per channel when it's populated; fall back to deriving from
    // BODY size (only valid for uncompressed).
    let frames_per_channel: u64 = if vhdr.one_shot_hi_samples > 0 {
        vhdr.one_shot_hi_samples as u64
    } else {
        match compression {
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
        }
    };
    let total_frames = frames_per_channel * channels as u64;

    // Read the whole BODY into memory and decode once. 8SVX voices are
    // typically short (seconds, not hours) so this is fine in practice
    // and keeps the streaming path trivial.
    input.seek(SeekFrom::Start(body_offset))?;
    let mut raw_body = vec![0u8; body_size as usize];
    input.read_exact(&mut raw_body)?;
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
        // FORM group chunk header. Size is patched in write_trailer once
        // we know how much we wrote.
        self.output.write_all(b"FORM")?;
        self.form_size_offset = self.output.stream_position()?;
        self.output.write_all(&0u32.to_be_bytes())?; // placeholder
        self.output.write_all(b"8SVX")?;

        // VHDR (20 bytes). We synthesise a one-shot voice with no
        // sustain/loop and no upper octaves: oneShotHiSamples is the
        // total frame count (or 0 when the stream duration is unknown —
        // FORM sizes are patched at close anyway), repeatHiSamples = 0,
        // samplesPerHiCycle = 0, volume = 1.0 (0x00010000, 16.16 fixed).
        self.output.write_all(b"VHDR")?;
        self.output.write_all(&20u32.to_be_bytes())?;
        // Frame count isn't known yet; patched in write_trailer.
        self.output.write_all(&0u32.to_be_bytes())?; // oneShotHiSamples
        self.output.write_all(&0u32.to_be_bytes())?; // repeatHiSamples
        self.output.write_all(&0u32.to_be_bytes())?; // samplesPerHiCycle
        self.output
            .write_all(&(self.sample_rate as u16).to_be_bytes())?;
        self.output.write_all(&[1u8])?; // ctOctave
        self.output.write_all(&[self.compression.to_vhdr_byte()])?; // sCompression
        self.output.write_all(&0x0001_0000u32.to_be_bytes())?; // volume 1.0

        // CHAN chunk for stereo (LEFT|RIGHT = 6). Mono is the default
        // when CHAN is absent, so we skip it there.
        if self.channels == Channels::Stereo {
            self.output.write_all(b"CHAN")?;
            self.output.write_all(&4u32.to_be_bytes())?;
            self.output
                .write_all(&self.channels.chan_value().to_be_bytes())?;
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

        // Patch VHDR.oneShotHiSamples with the per-channel frame count.
        // `form_size_offset` points at the FORM size field (4 bytes),
        // then comes "8SVX" (4), "VHDR" (4), VHDR size (4) — so
        // oneShotHiSamples lives at form_size_offset + 16. Writing this
        // lets a decoder that inspects VHDR know the full length of the
        // voice even before reaching BODY (and is especially useful for
        // Fibonacci-compressed voices, where the sample count isn't
        // trivially recoverable from BODY size).
        let one_shot = frames_per_channel as u32;
        self.output
            .seek(SeekFrom::Start(self.form_size_offset + 16))?;
        self.output.write_all(&one_shot.to_be_bytes())?;

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
