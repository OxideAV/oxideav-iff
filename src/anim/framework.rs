//! `oxideav-core` container adapters for `FORM ANIM`.
//!
//! The `iff_anim` demuxer (one `rawvideo` / `Rgba` keyframe per
//! delta-decoded frame, `pts` from the ANHD `reltime`s), the
//! [`AnimMuxer`] container muxer and [`register`]. The delta decoders
//! and encoders they wrap live in the parent module and are
//! framework-free; this module is compiled only with the default-on
//! `registry` feature.

use std::io::Read;

use oxideav_core::ReadSeek;
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MediaType, Muxer,
    Packet, PixelFormat, Result, StreamInfo, TimeBase, WriteSeek,
};

use super::*;
use crate::chunk::{read_chunk_header, read_form_type, GROUP_FORM};

/// Install the FORM/ANIM demuxer into a container registry. The
/// registered codec id matches the seed-frame's `rawvideo`+RGBA shape;
/// every decoded frame is emitted as a single keyframe packet at
/// `pts = i * rel_time`. Round 2 doesn't ship a muxer here — the
/// `anim::encode_anim_op0` helper is the only writer (used by tests).
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("iff_anim", open);
    reg.register_muxer("iff_anim", open_anim_muxer);
    reg.register_extension("anim", "iff_anim");
    reg.register_probe("iff_anim", probe_data);
}

fn probe_data(p: &oxideav_core::ProbeData) -> u8 {
    probe(p.buf)
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let hdr = read_chunk_header(&mut *input)?.ok_or_else(|| Error::invalid("ANIM: empty file"))?;
    if hdr.id != GROUP_FORM {
        return Err(Error::invalid(format!(
            "ANIM: expected FORM chunk, got {}",
            hdr.id_str()
        )));
    }
    let form_type = read_form_type(&mut *input)?;
    if &form_type != b"ANIM" {
        return Err(Error::invalid(format!(
            "IFF: not an ANIM file (form type {:?})",
            std::str::from_utf8(&form_type).unwrap_or("????")
        )));
    }
    // Grow-on-read (take + read_to_end) instead of pre-allocating the
    // declared FORM size, so a forged 32-bit size over a tiny stream
    // can't demand an attacker-sized buffer; a short read is rejected.
    let body_size = hdr.size as u64 - 4;
    let mut form_body = Vec::new();
    (&mut *input).take(body_size).read_to_end(&mut form_body)?;
    if (form_body.len() as u64) < body_size {
        return Err(Error::invalid(format!(
            "ANIM: FORM declares {} bytes but the stream ends after {}",
            body_size,
            form_body.len()
        )));
    }
    let mut full = Vec::with_capacity(8 + 4 + form_body.len());
    full.extend_from_slice(b"FORM");
    full.extend_from_slice(&hdr.size.to_be_bytes());
    full.extend_from_slice(b"ANIM");
    full.extend_from_slice(&form_body);
    let anim = parse_anim(&full)?;

    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(anim.width);
    params.height = Some(anim.height);
    params.pixel_format = Some(PixelFormat::Rgba);
    // Build the cumulative playback timeline so packet PTS/duration carry
    // the real per-frame jiffy delays from ANHD rather than a flat 1/frame.
    let playback = anim.playback();
    let total = playback.total_jiffies() as i64;
    let timing: Vec<(i64, i64)> = playback
        .frames
        .iter()
        .map(|f| (f.start_jiffies as i64, f.duration_jiffies as i64))
        .collect();
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 60),
        duration: Some(total),
        start_time: Some(0),
        params,
    };
    Ok(Box::new(AnimDemuxer {
        streams: vec![stream],
        frames: anim.frames.into_iter().map(|f| f.rgba).collect(),
        timing,
        next: 0,
    }))
}

struct AnimDemuxer {
    streams: Vec<StreamInfo>,
    frames: Vec<Vec<u8>>,
    /// `(start_jiffies, duration_jiffies)` per frame, parallel to `frames`.
    timing: Vec<(i64, i64)>,
    next: usize,
}

impl Demuxer for AnimDemuxer {
    fn format_name(&self) -> &str {
        "iff_anim"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        if self.next >= self.frames.len() {
            return Err(Error::Eof);
        }
        let i = self.next;
        let data = std::mem::take(&mut self.frames[i]);
        self.next += 1;
        let stream = &self.streams[0];
        let (start, dur) = self.timing.get(i).copied().unwrap_or((i as i64, 1));
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(start);
        pkt.dts = Some(start);
        pkt.duration = Some(dur);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }
    fn metadata(&self) -> &[(String, String)] {
        &[]
    }
    fn duration_micros(&self) -> Option<i64> {
        // Sum of all frame durations in jiffies (1/60 s) → microseconds.
        let total: i64 = self.timing.iter().map(|(_, d)| *d).sum();
        if total == 0 {
            None
        } else {
            Some(total * 1_000_000 / 60)
        }
    }
}

// ───────────────────── container-level ANIM muxer ─────────────────────

/// Which §2.1 delta operation [`AnimMuxer`] writes for frames 1..N.
///
/// Every variant shares the muxer's model: seed `FORM ILBM` + one
/// delta frame per following packet, one greedy-built CMAP for the
/// whole animation, packet durations converted to jiffy `reltime`s.
/// The long-data variants of op-4 and op-7 require `row_bytes` to be
/// a multiple of 4 (the 4-byte item width); op-8 instead word-splits
/// an odd-long plane per its §3.2 trailing-WORD-column rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnimMuxerOp {
    /// Op-0 — full literal BODY per frame (the universal fallback).
    Op0,
    /// Op-1 — XOR ILBM mode (full-frame rectangle, all planes).
    Op1,
    /// Op-2 — Long Delta mode.
    Op2,
    /// Op-3 — Short Delta mode.
    Op3,
    /// Op-4 — Generalized short/long Delta.
    Op4 {
        /// 4-byte data items when set (`ANHD.bits` bit 0).
        long_data: bool,
    },
    /// Op-5 — Byte Vertical Delta (the DPaint III workhorse). Default.
    #[default]
    Op5,
    /// Op-7 — Short/Long Vertical Delta.
    Op7 {
        /// 4-byte data items when set (`ANHD.bits` bit 0).
        long_data: bool,
    },
    /// Op-8 — Anim8 short/long vertical delta.
    Op8 {
        /// 4-byte data items when set (`ANHD.bits` bit 0).
        long_data: bool,
    },
}

/// Container-level `FORM ANIM` muxer (`iff_anim`). Accepts a single
/// `rawvideo` / `Rgba` video stream and one packet per frame, and emits an
/// animation using the [`AnimMuxerOp`] selected by
/// [`AnimMuxer::with_operation`] (default op-5, Byte Vertical Delta — the
/// DeluxePaint workhorse): the seed frame is a full `FORM ILBM`, every
/// later frame an `ANHD` + delta chunk against the previous frame.
///
/// * **Palette**: one shared CMAP is greedy-built from the unique RGB
///   triples of *all* frames (first-seen order, capped at 256 — ANIM
///   shares a single palette across the animation); frames quantise to it
///   by nearest fit, so an animation with ≤ 256 unique colours round-trips
///   pixel-exactly.
/// * **Timing** (§2.1 `reltime`, jiffies of 1/60 s): each packet's
///   `duration` is converted through the stream time base to the jiffy
///   delay written into the *next* frame's `ANHD` — per the playback
///   model, frame `i` stays on screen for frame `i+1`'s `rel_time`. With a
///   `1/60` time base (what the `iff_anim` demuxer advertises), durations
///   pass through as-is, so demux → mux → demux preserves the timeline.
///   The wire format carries no delay *after* the last frame, so the final
///   packet's duration is representable only when it equals the previous
///   one (players fall back to the last delta's `rel_time`). Packets
///   without durations produce the uniform 1-jiffy default.
pub struct AnimMuxer {
    output: Box<dyn WriteSeek>,
    width: u16,
    height: u16,
    input: crate::ilbm::MuxInput,
    time_base: TimeBase,
    op: AnimMuxerOp,
    frames: Vec<Vec<u8>>,
    durations: Vec<Option<i64>>,
    written: bool,
}

impl AnimMuxer {
    pub fn new(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        let (width, height, time_base, input) =
            crate::ilbm::true_color_muxer_stream_shape("ANIM", streams)?;
        Ok(Self {
            output,
            width,
            height,
            input,
            time_base,
            op: AnimMuxerOp::default(),
            frames: Vec::new(),
            durations: Vec::new(),
            written: false,
        })
    }

    /// Select which delta operation the trailer writes (default
    /// [`AnimMuxerOp::Op5`]). Builder-style; call before
    /// `write_trailer`.
    pub fn with_operation(mut self, op: AnimMuxerOp) -> Self {
        self.op = op;
        self
    }

    /// Convert a packet duration (stream time-base ticks) to §2.1 jiffies
    /// (1/60 s), clamped to at least one jiffy so a frame always advances.
    fn jiffies_of(&self, duration: i64) -> u32 {
        let num = self.time_base.num().max(0);
        let den = self.time_base.den().max(1);
        let j = duration.saturating_mul(60).saturating_mul(num) / den;
        u32::try_from(j.max(1)).unwrap_or(u32::MAX)
    }
}

impl Muxer for AnimMuxer {
    fn format_name(&self) -> &str {
        "iff_anim"
    }
    fn write_header(&mut self) -> Result<()> {
        Ok(()) // the FORM is assembled at write_trailer time
    }
    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let rgba = self.input.to_rgba(
            "ANIM",
            &packet.data,
            usize::from(self.width),
            usize::from(self.height),
        )?;
        self.frames.push(rgba);
        self.durations.push(packet.duration);
        Ok(())
    }
    fn write_trailer(&mut self) -> Result<()> {
        if self.written {
            return Ok(());
        }
        if self.frames.is_empty() {
            return Err(Error::invalid("ANIM muxer: no frames written"));
        }

        // One shared CMAP across the whole animation: unique RGB triples in
        // first-seen order over every frame, capped at 256 (8 bitplanes).
        let mut palette: Vec<[u8; 3]> = Vec::new();
        for frame in &self.frames {
            for px in frame.chunks_exact(4) {
                let triple = [px[0], px[1], px[2]];
                if !palette.contains(&triple) {
                    if palette.len() >= 256 {
                        break;
                    }
                    palette.push(triple);
                }
            }
        }
        let n_planes = if palette.len() <= 1 {
            1
        } else {
            let bits = (palette.len() as u32 - 1)
                .next_power_of_two()
                .trailing_zeros();
            bits.max(1) as u8
        };

        let bmhd = Bmhd {
            width: self.width,
            height: self.height,
            x_origin: 0,
            y_origin: 0,
            n_planes,
            masking: Masking::None,
            compression: Compression::ByteRun1,
            pad: 0,
            transparent_color: 0,
            x_aspect: 1,
            y_aspect: 1,
            page_width: self.width.min(i16::MAX as u16) as i16,
            page_height: self.height.min(i16::MAX as u16) as i16,
        };

        let images: Vec<IlbmImage> = self
            .frames
            .iter()
            .map(|rgba| IlbmImage {
                width: u32::from(self.width),
                height: u32::from(self.height),
                bmhd,
                palette: palette.clone(),
                rgba: rgba.clone(),
                ..IlbmImage::default()
            })
            .collect();

        // §2.1 timing: delta frame i's rel_time is the jiffy delay before
        // it replaces frame i-1, i.e. packet i-1's duration. abs_time is
        // the cumulative start ("currently unused" per the 1988 spec but
        // written for players that key off it).
        let mut timing = Vec::with_capacity(images.len());
        let mut cumulative: u32 = 0;
        timing.push(FrameTiming {
            rel_time: 0,
            abs_time: 0,
        });
        for i in 1..images.len() {
            let rel = self.durations[i - 1]
                .map(|d| self.jiffies_of(d))
                .unwrap_or(1);
            cumulative = cumulative.saturating_add(rel);
            timing.push(FrameTiming {
                rel_time: rel,
                abs_time: cumulative,
            });
        }

        let form = match self.op {
            AnimMuxerOp::Op0 => encode_anim_op0_timed(&images, &timing)?,
            AnimMuxerOp::Op1 => encode_anim_op1_timed(&images, &timing)?,
            AnimMuxerOp::Op2 => encode_anim_op2_timed(&images, &timing)?,
            AnimMuxerOp::Op3 => encode_anim_op3_timed(&images, &timing)?,
            AnimMuxerOp::Op4 { long_data } => encode_anim_op4_timed(&images, long_data, &timing)?,
            AnimMuxerOp::Op5 => encode_anim_op5_timed(&images, &timing)?,
            AnimMuxerOp::Op7 { long_data } => encode_anim_op7_timed(&images, long_data, &timing)?,
            AnimMuxerOp::Op8 { long_data } => encode_anim_op8_timed(&images, long_data, &timing)?,
        };
        self.output.write_all(&form)?;
        self.output.flush()?;
        self.written = true;
        Ok(())
    }
}

fn open_anim_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(AnimMuxer::new(output, streams)?))
}
