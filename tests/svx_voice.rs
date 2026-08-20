//! FORM 8SVX voice-structure tests: the VHDR one-shot/repeat loop
//! split, multi-octave BODY layout, and CHAN mono-routing surface.
//!
//! Wire layout per the staged IFF reference set
//! (`docs/image/ilbm/multimediawiki-iff.html`): the sum of
//! `oneShotHiSamples` and `repeatHiSamples` is the full length of the
//! highest-octave waveform; each following octave waveform is twice as
//! long as the previous one; a stereo BODY is the LEFT channel in full
//! then the RIGHT channel in full.

use oxideav_core::{Demuxer, Error, ReadSeek};
use oxideav_iff::svx::VoiceHeader;
use std::io::Cursor;

/// Assemble a `FORM 8SVX` from a VHDR, optional extra chunks, and a BODY.
fn build_svx(vhdr: &VoiceHeader, extra_chunks: &[(&[u8; 4], Vec<u8>)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"FORM");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"8SVX");

    out.extend_from_slice(b"VHDR");
    out.extend_from_slice(&20u32.to_be_bytes());
    out.extend_from_slice(&vhdr.write());

    for (id, payload) in extra_chunks {
        out.extend_from_slice(*id);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(0);
        }
    }

    out.extend_from_slice(b"BODY");
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }

    let total = out.len() as u32;
    out[4..8].copy_from_slice(&(total - 8).to_be_bytes());
    out
}

fn open_demuxer(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    let mut containers = oxideav_core::ContainerRegistry::new();
    oxideav_iff::register_containers(&mut containers);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    containers
        .open_demuxer("iff_8svx", rs, &oxideav_core::NullCodecResolver)
        .unwrap()
}

fn drain(dmx: &mut dyn Demuxer) -> Vec<u8> {
    let mut all = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(pkt) => all.extend_from_slice(&pkt.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
    all
}

fn vhdr(one_shot: u32, repeat: u32, ct_octave: u8) -> VoiceHeader {
    VoiceHeader {
        one_shot_hi_samples: one_shot,
        repeat_hi_samples: repeat,
        samples_per_hi_cycle: 0,
        samples_per_sec: 8000,
        ct_octave,
        compression_byte: 0,
        volume: 0x0001_0000,
    }
}

/// A looping voice (repeat part > 0) must contribute its repeat part to
/// both the emitted samples and the reported duration — the loop tail
/// is part of the highest-octave waveform, not trailing junk.
#[test]
fn demux_includes_repeat_part_in_mono_voice() {
    let h = vhdr(4, 6, 1);
    let body: Vec<u8> = (0u8..10).collect();
    let mut dmx = open_demuxer(build_svx(&h, &[], &body));
    assert_eq!(dmx.streams()[0].duration, Some(10));
    let data = drain(&mut *dmx);
    assert_eq!(data, body);
}

/// Stereo + loop: the BODY is LEFT in full then RIGHT in full, so the
/// channel split point is half the BODY — a demuxer that splits at
/// `oneShotHiSamples` alone would interleave the left channel's repeat
/// part as right-channel data.
#[test]
fn demux_stereo_loop_splits_channels_at_body_midpoint() {
    let h = vhdr(4, 4, 1);
    // LEFT = 10,11,..17  RIGHT = 20,21,..27
    let mut body = Vec::new();
    body.extend(10u8..18);
    body.extend(20u8..28);
    let chan = (b"CHAN", 6u32.to_be_bytes().to_vec());
    let mut dmx = open_demuxer(build_svx(&h, &[chan], &body));
    assert_eq!(dmx.streams()[0].params.channels, Some(2));
    assert_eq!(dmx.streams()[0].duration, Some(8));
    let data = drain(&mut *dmx);
    let expect: Vec<u8> = (0..8).flat_map(|i| [10 + i, 20 + i]).collect();
    assert_eq!(data, expect);
}

/// A multi-octave voice stores ctOctave waveforms back to back, highest
/// first, each twice the previous length. The demuxer emits the full
/// stored sequence and surfaces the octave count as metadata.
#[test]
fn demux_multi_octave_voice_emits_all_octaves() {
    // hi = 4 samples, 3 octaves → 4 + 8 + 16 = 28 samples.
    let h = vhdr(4, 0, 3);
    let body: Vec<u8> = (0u8..28).collect();
    let mut dmx = open_demuxer(build_svx(&h, &[], &body));
    assert_eq!(dmx.streams()[0].duration, Some(28));
    let octaves = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "octaves")
        .map(|(_, v)| v.clone());
    assert_eq!(octaves.as_deref(), Some("3"));
    let data = drain(&mut *dmx);
    assert_eq!(data.len(), 28);
    assert_eq!(data, body);
}

/// A header whose octave series claims more samples than BODY carries is
/// clamped to what the wire can actually supply — no over-read, no
/// attacker-sized allocation.
#[test]
fn demux_clamps_forged_octave_count_to_body() {
    let h = vhdr(4, 0, 200); // absurd doubling series
    let body: Vec<u8> = (0u8..12).collect();
    let mut dmx = open_demuxer(build_svx(&h, &[], &body));
    assert_eq!(dmx.streams()[0].duration, Some(12));
    let data = drain(&mut *dmx);
    assert_eq!(data, body);
}

/// CHAN = 4 routes a mono sample to the RIGHT speaker. The sample data
/// stays mono; the routing is surfaced as metadata.
#[test]
fn demux_chan_right_is_mono_with_metadata() {
    let h = vhdr(4, 0, 1);
    let chan = (b"CHAN", 4u32.to_be_bytes().to_vec());
    let dmx = open_demuxer(build_svx(&h, &[chan], &(0u8..4).collect::<Vec<u8>>()));
    assert_eq!(dmx.streams()[0].params.channels, Some(1));
    let routing = dmx
        .metadata()
        .iter()
        .find(|(k, _)| k == "channel_assignment")
        .map(|(_, v)| v.clone());
    assert_eq!(routing.as_deref(), Some("right"));
}

/// VoiceHeader round-trips through its 20-byte wire form, and the
/// derived accessors answer from the staged field semantics.
#[test]
fn voice_header_roundtrip_and_accessors() {
    let h = VoiceHeader {
        one_shot_hi_samples: 100,
        repeat_hi_samples: 28,
        samples_per_hi_cycle: 32,
        samples_per_sec: 16000,
        ct_octave: 2,
        compression_byte: 1,
        volume: 0x8000,
    };
    assert_eq!(VoiceHeader::parse(&h.write()).unwrap(), h);
    assert_eq!(h.hi_octave_samples(), 128);
    assert_eq!(h.octave_samples(0), Some(128));
    assert_eq!(h.octave_samples(1), Some(256));
    assert_eq!(h.octave_samples(2), None); // only 2 octaves
    assert_eq!(h.total_samples_per_channel(), Some(128 + 256));
    assert!(h.has_loop());
    assert!((h.volume_f32() - 0.5).abs() < 1e-6);
    assert_eq!(h.hi_cycle_frequency_hz(), Some(500.0));
    assert_eq!(
        h.compression().unwrap(),
        oxideav_iff::svx::Compression::Fibonacci
    );
}

/// Hostile headers: a truncated VHDR is rejected; an overflow-forcing
/// octave doubling series reports `None` instead of wrapping.
#[test]
fn voice_header_hostile_inputs() {
    assert!(VoiceHeader::parse(&[0u8; 19]).is_err());
    let h = vhdr(u32::MAX, u32::MAX, 255);
    assert_eq!(h.total_samples_per_channel(), None);
    // ct_octave == 0 is treated as one octave.
    let h0 = vhdr(8, 0, 0);
    assert_eq!(h0.total_samples_per_channel(), Some(8));
    assert_eq!(h0.octave_samples(0), Some(8));
}
