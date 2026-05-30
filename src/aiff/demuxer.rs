//! `oxideav_core::Demuxer` wiring for AIFF / AIFF-C.
//!
//! `register(ctx)` (called via the [`oxideav_core::register!`] macro
//! in `lib.rs`) installs the demuxer factory under the format name
//! `"aiff"`, a probe function that recognises `FORM/AIFF` and
//! `FORM/AIFC` magic, and the `.aif` / `.aiff` / `.aifc` filename
//! extensions.
//!
//! ## Trait-API adaptation
//!
//! AIFF is a chunk-based container — `FORM` wraps `COMM` (format) +
//! `SSND` (samples) + optional metadata chunks; the FORM-walker reads
//! the whole file into a [`crate::aiff::Form`] tree before any packet is
//! emitted. This round's demuxer therefore loads the entire input on
//! construction (via [`std::io::Read::read_to_end`]) and produces a
//! **single packet** carrying the SSND payload. That packet's
//! `CodecParameters` carry the AIFF-C `compressionType` FourCC as
//! a [`CodecTag`] so a sibling codec crate (`oxideav-adpcm` for
//! `ima4`, `oxideav-g711` for `ulaw` / `alaw`, …) can resolve the
//! codec from the wire tag.
//!
//! Streaming-with-bounded-buffer chunk parsing is a future-round
//! refinement; for the bootstrap round this matches the way
//! `oxideav-shorten` initially handled an entire `.shn` file as one
//! packet.

use std::io::Read;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, ContainerRegistry, Demuxer, Error, Packet,
    ProbeData, ReadSeek, Result, RuntimeContext, SampleFormat, StreamInfo, TimeBase,
};

use crate::aiff::common::{
    COMPRESSION_FL32, COMPRESSION_FL32_UC, COMPRESSION_FL64, COMPRESSION_FL64_UC,
};
use crate::aiff::error::AiffError;

/// Format name the demuxer installs under in `ContainerRegistry`
/// (`"aiff"`). Same value as [`crate::aiff::CODEC_ID_STR`].
pub const FORMAT_NAME: &str = "aiff";

/// Demuxer for a single FORM/AIFF or FORM/AIFC file.
///
/// Reads the entire input on construction and exposes one audio
/// stream. [`next_packet`](Demuxer::next_packet) returns the SSND
/// payload as a single packet on the first call and `Error::Eof`
/// thereafter.
pub struct AiffDemuxer {
    streams: [StreamInfo; 1],
    /// SSND payload bytes, already past the `offset` padding.
    /// `None` after the single packet has been delivered.
    payload: Option<Vec<u8>>,
    /// Round-up sample-frame count from COMM, used as the packet
    /// duration (in the stream's time base = sample rate).
    num_frames: u32,
}

impl AiffDemuxer {
    /// Build a demuxer from already-buffered file bytes. The bytes
    /// are scanned with [`crate::aiff::form::parse`] and the result is
    /// translated into the [`Demuxer`] trait surface.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let form = crate::aiff::form::parse(&bytes).map_err(map_aiff_error)?;
        let mut params = CodecParameters::audio(codec_id_for(form.common.compression_type));
        params.sample_rate = Some(form.common.sample_rate as u32);
        params.channels = Some(form.common.num_channels);
        params.sample_format =
            sample_format_for(form.common.sample_size, form.common.compression_type);
        // Preserve the on-wire compressionType FourCC so a sibling
        // codec crate (oxideav-adpcm / oxideav-g711 / ...) can
        // resolve to the right decoder. AIFF v1.3 (no compressionType
        // field) has no on-wire tag.
        params.tag = form.common.compression_type.map(CodecTag::Fourcc);

        // Sample rate as a positive integer denominator. The
        // form-level parser already rejected NaN / Inf / non-positive
        // rates via `extended::decode_sample_rate`, so `as i64` here
        // produces a positive integer for every input that reaches
        // this point.
        let rate_den = (form.common.sample_rate as i64).max(1);
        let time_base = TimeBase::new(1, rate_den);
        let stream = StreamInfo {
            index: 0,
            time_base,
            duration: Some(form.common.num_sample_frames as i64),
            start_time: Some(0),
            params,
        };

        // Copy the SSND payload out of the borrowed slice so the
        // demuxer owns the data (the buffer goes away once
        // `from_bytes` returns).
        let payload = form.sound.as_ref().map(|s| s.samples.to_vec());

        Ok(Self {
            streams: [stream],
            payload,
            num_frames: form.common.num_sample_frames,
        })
    }
}

impl Demuxer for AiffDemuxer {
    fn format_name(&self) -> &str {
        FORMAT_NAME
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let Some(data) = self.payload.take() else {
            return Err(Error::Eof);
        };
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.duration = Some(self.num_frames as i64);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }
}

/// Register the AIFF demuxer factory in `ctx.containers`.
pub fn register(ctx: &mut RuntimeContext) {
    register_containers(&mut ctx.containers);
}

/// Same as [`register`] but operates directly on a
/// [`ContainerRegistry`]. Used by callers that want only the
/// container half of the crate, not the unified `register` entry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer(FORMAT_NAME, open_demuxer);
    reg.register_extension("aif", FORMAT_NAME);
    reg.register_extension("aiff", FORMAT_NAME);
    reg.register_extension("aifc", FORMAT_NAME);
    reg.register_probe(FORMAT_NAME, probe);
}

/// `ContainerRegistry`-friendly factory: read the whole stream into
/// memory, then hand off to [`AiffDemuxer::from_bytes`].
fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    let dx = AiffDemuxer::from_bytes(bytes)?;
    Ok(Box::new(dx))
}

/// Direct factory for the dual-API convention: a caller that already
/// has the file bytes can skip the `ReadSeek` indirection.
pub fn make_demuxer(bytes: Vec<u8>) -> Result<AiffDemuxer> {
    AiffDemuxer::from_bytes(bytes)
}

/// Probe an input buffer for the AIFF / AIFF-C magic. Recognises
/// `FORM????AIFF` and `FORM????AIFC` at offset 0 (the 4-byte
/// `ckSize` between `FORM` and the form type is unconstrained).
fn probe(p: &ProbeData<'_>) -> u8 {
    if p.buf.len() < 12 {
        return 0;
    }
    if &p.buf[0..4] != b"FORM" {
        return 0;
    }
    match &p.buf[8..12] {
        b"AIFF" | b"AIFC" => 100,
        _ => 0,
    }
}

/// Map the on-wire AIFF-C `compressionType` FourCC to the codec id
/// the framework registry knows for that bitstream. For PCM flavours
/// the id stays a generic `"pcm"` family; for compressed flavours the
/// id matches a sibling codec crate. AIFF v1.3 (no compressionType)
/// is treated as big-endian 16-bit PCM.
fn codec_id_for(ct: Option<[u8; 4]>) -> CodecId {
    let ct = match ct {
        Some(c) => c,
        None => return CodecId::new("pcm_s16be"),
    };
    match &ct {
        b"NONE" | b"twos" => CodecId::new("pcm_s16be"),
        b"sowt" => CodecId::new("pcm_s16le"),
        b"raw " => CodecId::new("pcm_u8"),
        b"fl32" | b"FL32" => CodecId::new("pcm_f32be"),
        b"fl64" | b"FL64" => CodecId::new("pcm_f64be"),
        b"ima4" => CodecId::new("adpcm_ima_qt"),
        b"alaw" | b"ALAW" => CodecId::new("alaw"),
        b"ulaw" | b"ULAW" => CodecId::new("ulaw"),
        b"MAC3" => CodecId::new("mace3"),
        b"MAC6" => CodecId::new("mace6"),
        b"GSM " => CodecId::new("gsm"),
        b"G722" => CodecId::new("g722"),
        b"G726" | b"ADP4" => CodecId::new("g726"),
        b"G728" => CodecId::new("g728"),
        b"QDMC" => CodecId::new("qdmc"),
        b"QDM2" => CodecId::new("qdm2"),
        b"Qclp" => CodecId::new("qclp"),
        _ => CodecId::new("unknown"),
    }
}

/// Map (sampleSize, compressionType) to an `oxideav-core` packed
/// [`SampleFormat`]. Returns `None` for codecs that aren't pure-PCM
/// (the decoded sample format is the codec's responsibility, not the
/// container's).
fn sample_format_for(sample_size: u16, ct: Option<[u8; 4]>) -> Option<SampleFormat> {
    let ct = ct.unwrap_or(*b"NONE");
    match &ct {
        b"NONE" | b"twos" | b"sowt" => match sample_size {
            1..=8 => Some(SampleFormat::S16), // promoted to 16-bit signed at decode
            9..=16 => Some(SampleFormat::S16),
            17..=24 => Some(SampleFormat::S24),
            25..=32 => Some(SampleFormat::S32),
            _ => None,
        },
        b"raw " => Some(SampleFormat::U8),
        c if *c == COMPRESSION_FL32 || *c == COMPRESSION_FL32_UC => Some(SampleFormat::F32),
        c if *c == COMPRESSION_FL64 || *c == COMPRESSION_FL64_UC => Some(SampleFormat::F64),
        _ => None,
    }
}

fn map_aiff_error(e: AiffError) -> Error {
    Error::invalid(format!("aiff: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(rate: f64) -> [u8; 10] {
        let sign = rate.is_sign_negative();
        let mag = rate.abs();
        let bits = mag.to_bits();
        let f64_exp = ((bits >> 52) & 0x7ff) as i32;
        let f64_frac = bits & 0x000f_ffff_ffff_ffff;
        let (mantissa_64, exp_unbiased): (u64, i32) = if f64_exp == 0 {
            let lead = f64_frac.leading_zeros() as i32 - 11;
            let mantissa = f64_frac << (12 + lead);
            let true_exp = -1022 - lead;
            (mantissa, true_exp)
        } else {
            let mantissa = (1_u64 << 63) | (f64_frac << 11);
            let true_exp = f64_exp - 1023;
            (mantissa, true_exp)
        };
        let biased_ext = exp_unbiased + 16_383;
        let exp_field = biased_ext as u16 & 0x7fff;
        let mut o = [0u8; 10];
        o[0] = ((exp_field >> 8) as u8) | if sign { 0x80 } else { 0 };
        o[1] = (exp_field & 0xff) as u8;
        o[2..10].copy_from_slice(&mantissa_64.to_be_bytes());
        o
    }

    fn pack(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + data.len() + 1);
        v.extend_from_slice(id);
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        if data.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn build_aiff(channels: u16, frames: u32, bits: u16, rate: f64, samples: &[u8]) -> Vec<u8> {
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&(channels as i16).to_be_bytes());
        comm_body.extend_from_slice(&frames.to_be_bytes());
        comm_body.extend_from_slice(&(bits as i16).to_be_bytes());
        comm_body.extend_from_slice(&ext(rate));

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(samples);

        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFF");
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));

        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        file.extend_from_slice(&inner);
        file
    }

    #[test]
    fn probe_recognises_aiff() {
        let f = build_aiff(2, 1, 16, 44_100.0, &[0, 1, 2, 3]);
        let pd = ProbeData { buf: &f, ext: None };
        assert_eq!(probe(&pd), 100);
    }

    #[test]
    fn probe_recognises_aifc() {
        let mut buf = b"FORM\x00\x00\x00\x10AIFC".to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        let pd = ProbeData {
            buf: &buf,
            ext: None,
        };
        assert_eq!(probe(&pd), 100);
    }

    #[test]
    fn probe_rejects_riff_wave() {
        let buf = b"RIFF\x00\x00\x00\x10WAVE".to_vec();
        let pd = ProbeData {
            buf: &buf,
            ext: None,
        };
        assert_eq!(probe(&pd), 0);
    }

    #[test]
    fn demuxer_yields_one_packet_then_eof() {
        let pcm: [u8; 8] = [0, 1, 0xff, 0xff, 0x7f, 0xff, 0x80, 0];
        let f = build_aiff(2, 2, 16, 44_100.0, &pcm);
        let mut dx = AiffDemuxer::from_bytes(f).unwrap();
        let pkt = dx.next_packet().unwrap();
        assert_eq!(pkt.data, pcm);
        assert_eq!(pkt.pts, Some(0));
        assert_eq!(pkt.duration, Some(2));
        assert!(pkt.flags.keyframe);
        let err = dx.next_packet().unwrap_err();
        assert!(matches!(err, Error::Eof));
    }

    #[test]
    fn demuxer_exposes_stream_info() {
        let pcm: [u8; 4] = [0, 1, 0xff, 0xff];
        let f = build_aiff(1, 2, 16, 48_000.0, &pcm);
        let dx = AiffDemuxer::from_bytes(f).unwrap();
        let s = &dx.streams()[0];
        assert_eq!(s.params.sample_rate, Some(48_000));
        assert_eq!(s.params.channels, Some(1));
        assert_eq!(s.params.codec_id, CodecId::new("pcm_s16be"));
        assert_eq!(s.params.sample_format, Some(SampleFormat::S16));
        assert_eq!(s.duration, Some(2));
        assert_eq!(s.time_base, TimeBase::new(1, 48_000));
    }

    #[test]
    fn aifc_sowt_sets_codec_id_to_pcm_s16le() {
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&2_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(44_100.0));
        comm_body.extend_from_slice(b"sowt");
        comm_body.push(0);
        comm_body.push(0);

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&[0x01, 0x00, 0xff, 0xff]);

        let fver_body = 0xA280_5140_u32.to_be_bytes();
        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFC");
        inner.extend_from_slice(&pack(b"FVER", &fver_body));
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        file.extend_from_slice(&inner);

        let dx = AiffDemuxer::from_bytes(file).unwrap();
        let s = &dx.streams()[0];
        assert_eq!(s.params.codec_id, CodecId::new("pcm_s16le"));
        assert_eq!(s.params.tag, Some(CodecTag::Fourcc(*b"sowt")));
    }

    #[test]
    fn aifc_ima4_maps_to_adpcm_ima_qt() {
        // We don't actually decode ima4 here; just confirm the codec
        // resolver picks the right sibling-crate id from the FourCC.
        let mut comm_body = Vec::new();
        comm_body.extend_from_slice(&1_i16.to_be_bytes());
        comm_body.extend_from_slice(&64_u32.to_be_bytes());
        comm_body.extend_from_slice(&16_i16.to_be_bytes());
        comm_body.extend_from_slice(&ext(22_050.0));
        comm_body.extend_from_slice(b"ima4");
        comm_body.push(0);
        comm_body.push(0);

        let mut ssnd_body = Vec::new();
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        ssnd_body.extend_from_slice(&0_u32.to_be_bytes());
        // 1 ima4 packet = 34 bytes / channel; we pad with zeros for
        // shape only.
        ssnd_body.extend_from_slice(&[0u8; 34]);

        let fver_body = 0xA280_5140_u32.to_be_bytes();
        let mut inner = Vec::new();
        inner.extend_from_slice(b"AIFC");
        inner.extend_from_slice(&pack(b"FVER", &fver_body));
        inner.extend_from_slice(&pack(b"COMM", &comm_body));
        inner.extend_from_slice(&pack(b"SSND", &ssnd_body));
        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        file.extend_from_slice(&inner);

        let dx = AiffDemuxer::from_bytes(file).unwrap();
        let s = &dx.streams()[0];
        assert_eq!(s.params.codec_id, CodecId::new("adpcm_ima_qt"));
        assert_eq!(s.params.tag, Some(CodecTag::Fourcc(*b"ima4")));
    }

    #[test]
    fn make_demuxer_is_direct_api() {
        let pcm: [u8; 4] = [0, 1, 0xff, 0xff];
        let f = build_aiff(1, 2, 16, 44_100.0, &pcm);
        let dx = make_demuxer(f).unwrap();
        assert_eq!(dx.streams()[0].params.sample_rate, Some(44_100));
    }

    #[test]
    fn register_installs_into_container_registry() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let names: Vec<_> = ctx.containers.demuxer_names().collect();
        assert!(
            names.contains(&FORMAT_NAME),
            "demuxer factory should be installed under {FORMAT_NAME}, got {names:?}",
        );
    }
}
