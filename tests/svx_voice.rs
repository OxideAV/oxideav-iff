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

// ── parse_voice / encode_voice — the structural surface ──────────────

use oxideav_iff::svx::{
    encode_voice, parse_voice, ChannelAssignment, ChannelSamples, EgPoint, Envelope, Fade, Pan,
    Seqn, SeqnSegment, Voice,
};

fn voice_of(header: VoiceHeader, channels: Vec<ChannelSamples>) -> Voice {
    Voice {
        header,
        channels,
        ..Voice::default()
    }
}

fn octave_ramp(len: usize, base: i8) -> Vec<i8> {
    (0..len)
        .map(|i| base.wrapping_add((i % 100) as i8))
        .collect()
}

/// Raw mono multi-octave voice: encode → parse is lossless, octave
/// boundaries land on the doubling series.
#[test]
fn voice_roundtrip_mono_multi_octave_raw() {
    let header = VoiceHeader {
        one_shot_hi_samples: 4,
        repeat_hi_samples: 4,
        samples_per_hi_cycle: 8,
        samples_per_sec: 8000,
        ct_octave: 3,
        compression_byte: 0,
        volume: 0x8000,
    };
    let ch = ChannelSamples {
        octaves: vec![octave_ramp(8, 0), octave_ramp(16, 10), octave_ramp(32, -50)],
    };
    let v = voice_of(header, vec![ch]);
    let bytes = encode_voice(&v).unwrap();
    let back = parse_voice(&bytes).unwrap();
    assert_eq!(back, v);
}

/// Stereo voice with every optional chunk populated round-trips through
/// encode → parse, chunk for chunk.
#[test]
fn voice_roundtrip_stereo_full_chunk_set() {
    let header = VoiceHeader {
        one_shot_hi_samples: 0,
        repeat_hi_samples: 8,
        samples_per_hi_cycle: 0,
        samples_per_sec: 16000,
        ct_octave: 1,
        compression_byte: 0,
        volume: 0x0001_0000,
    };
    let v = Voice {
        header,
        channel: Some(ChannelAssignment::Stereo),
        pan: None, // PAN is for mono voices; exercised separately below
        attack: vec![Envelope {
            points: vec![
                EgPoint {
                    duration_ms: 10,
                    dest: 0x8000,
                },
                EgPoint {
                    duration_ms: 20,
                    dest: 0x0001_0000,
                },
            ],
        }],
        release: vec![Envelope {
            points: vec![EgPoint {
                duration_ms: 50,
                dest: 0,
            }],
        }],
        seqn: Some(Seqn {
            segments: vec![
                SeqnSegment { start: 0, end: 4 },
                SeqnSegment { start: 4, end: 8 },
                SeqnSegment { start: 0, end: 8 },
            ],
        }),
        fade: Some(Fade { segment: 2 }),
        metadata: vec![
            ("title".into(), "chord".into()),
            ("comment".into(), "two segments then all".into()),
        ],
        channels: vec![
            ChannelSamples {
                octaves: vec![octave_ramp(8, 5)],
            },
            ChannelSamples {
                octaves: vec![octave_ramp(8, -5)],
            },
        ],
    };
    let bytes = encode_voice(&v).unwrap();
    let back = parse_voice(&bytes).unwrap();
    assert_eq!(back, v);
    // The SEQN validates against the header and follows the VHDR
    // convention (oneShot == 0); the FADE names a real segment.
    let seqn = back.seqn.as_ref().unwrap();
    seqn.validate(&back.header).unwrap();
    assert!(seqn.vhdr_convention_holds(&back.header));
    assert!(back.fade.unwrap().is_valid_for(seqn));
}

/// Mono voice with PAN + CHAN(Right): typed accessors answer from the
/// documented value space.
#[test]
fn voice_pan_and_right_routing() {
    let header = VoiceHeader {
        one_shot_hi_samples: 4,
        samples_per_sec: 8000,
        ..VoiceHeader::default()
    };
    let v = Voice {
        header,
        channel: Some(ChannelAssignment::Right),
        pan: Some(Pan { position: 0x8000 }),
        channels: vec![ChannelSamples {
            octaves: vec![octave_ramp(4, 1)],
        }],
        ..Voice::default()
    };
    let back = parse_voice(&encode_voice(&v).unwrap()).unwrap();
    assert_eq!(back, v);
    let pan = back.pan.unwrap();
    assert!((pan.left_weight() - 0.5).abs() < 1e-3);
    assert!((pan.right_weight() - 0.5).abs() < 1e-3);
    assert_eq!(
        Pan {
            position: Pan::LEFT
        }
        .left_weight(),
        1.0
    );
    assert_eq!(
        Pan {
            position: Pan::RIGHT
        }
        .left_weight(),
        0.0
    );
    // Out-of-range positions clamp instead of over/underflowing.
    assert_eq!(Pan { position: i32::MAX }.left_weight(), 1.0);
    assert_eq!(Pan { position: -55 }.left_weight(), 0.0);
    assert_eq!(back.channel, Some(ChannelAssignment::Right));
    assert_eq!(back.channel_count(), 1);
}

/// Fibonacci-compressed stereo voice: first sample exact, the rest
/// within the codec's ±2 LSB tolerance, structure preserved exactly.
#[test]
fn voice_roundtrip_stereo_fibonacci() {
    let header = VoiceHeader {
        one_shot_hi_samples: 64,
        samples_per_sec: 8000,
        compression_byte: 1,
        ..VoiceHeader::default()
    };
    let smooth: Vec<i8> = (0..64)
        .map(|i| (60.0 * (i as f64 * 0.2).sin()) as i8)
        .collect();
    let v = Voice {
        header,
        channel: Some(ChannelAssignment::Stereo),
        channels: vec![
            ChannelSamples {
                octaves: vec![smooth.clone()],
            },
            ChannelSamples {
                octaves: vec![smooth.iter().map(|&s| -s).collect()],
            },
        ],
        ..Voice::default()
    };
    let back = parse_voice(&encode_voice(&v).unwrap()).unwrap();
    assert_eq!(back.channel_count(), 2);
    assert_eq!(back.octave_count(), 1);
    for (ch, orig) in back.channels.iter().zip(v.channels.iter()) {
        assert_eq!(ch.octaves[0].len(), 64);
        assert_eq!(ch.octaves[0][0], orig.octaves[0][0]);
        for (a, b) in ch.octaves[0].iter().zip(orig.octaves[0].iter()) {
            assert!((*a as i32 - *b as i32).abs() <= 2);
        }
    }
}

/// Envelope evaluation: linear ramps between points, hold past the end.
#[test]
fn envelope_level_evaluation() {
    let env = Envelope {
        points: vec![
            EgPoint {
                duration_ms: 100,
                dest: 0x0001_0000,
            },
            EgPoint {
                duration_ms: 100,
                dest: 0x8000,
            },
        ],
    };
    assert_eq!(env.total_duration_ms(), 200);
    assert_eq!(env.level_at_ms(0, 0), 0);
    assert_eq!(env.level_at_ms(0, 50), 0x8000); // halfway up the attack
    assert_eq!(env.level_at_ms(0, 100), 0x0001_0000); // peak
    assert_eq!(env.level_at_ms(0, 150), 0xC000); // halfway down
    assert_eq!(env.level_at_ms(0, 200), 0x8000); // settled
    assert_eq!(env.level_at_ms(0, 10_000), 0x8000); // holds
                                                    // Empty envelope: identity.
    assert_eq!(Envelope::default().level_at_ms(1234, 42), 1234);
    // Zero-duration point: immediate jump.
    let jump = Envelope {
        points: vec![EgPoint {
            duration_ms: 0,
            dest: 7,
        }],
    };
    assert_eq!(jump.level_at_ms(0, 0), 7);
}

/// SEQN validation: misaligned offsets, inverted segments, and
/// past-the-waveform ends are each rejected; a compliant list passes.
#[test]
fn seqn_validation_rules() {
    let header = VoiceHeader {
        repeat_hi_samples: 16,
        ..VoiceHeader::default()
    };
    let ok = Seqn {
        segments: vec![SeqnSegment { start: 4, end: 12 }],
    };
    ok.validate(&header).unwrap();
    let misaligned = Seqn {
        segments: vec![SeqnSegment { start: 2, end: 8 }],
    };
    assert!(misaligned.validate(&header).is_err());
    let inverted = Seqn {
        segments: vec![SeqnSegment { start: 12, end: 4 }],
    };
    assert!(inverted.validate(&header).is_err());
    let past_end = Seqn {
        segments: vec![SeqnSegment { start: 0, end: 20 }],
    };
    assert!(past_end.validate(&header).is_err());
    // Segment helpers.
    let s = SeqnSegment { start: 4, end: 12 };
    assert_eq!(s.len(), 8);
    assert!(!s.is_empty());
    assert!(SeqnSegment { start: 4, end: 4 }.is_empty());
}

/// Hostile inputs to the typed chunk parsers: wrong sizes and unknown
/// enum values are rejected, never panicking.
#[test]
fn typed_chunk_hostile_inputs() {
    assert!(Envelope::parse(&[0u8; 7]).is_err()); // not a multiple of 6
    assert!(Seqn::parse(&[0u8; 12]).is_err()); // not a multiple of 8
    assert!(Pan::parse(&[0u8; 3]).is_err());
    assert!(Fade::parse(&[0u8; 3]).is_err());
    assert!(ChannelAssignment::parse(&[0u8; 2]).is_err());
    assert!(ChannelAssignment::from_raw(5).is_err());
    assert!(ChannelAssignment::from_raw(0).is_err());
    // Empty bodies parse to empty lists (0 is a multiple of both).
    assert_eq!(Envelope::parse(&[]).unwrap().points.len(), 0);
    assert_eq!(Seqn::parse(&[]).unwrap().segments.len(), 0);
}

/// Hostile FORMs through parse_voice: duplicates of single-instance
/// chunks, truncated bodies vs the octave series, and chunks running
/// past the FORM are each rejected.
#[test]
fn parse_voice_hostile_forms() {
    let h = vhdr(4, 0, 2); // needs 4 + 8 = 12 samples
                           // BODY too short for the announced octave series.
    let short = build_svx(&h, &[], &[0u8; 5]);
    assert!(parse_voice(&short).is_err());
    // Duplicate VHDR.
    let dup_vhdr = build_svx(&vhdr(4, 0, 1), &[(b"VHDR", h.write().to_vec())], &[0u8; 4]);
    assert!(parse_voice(&dup_vhdr).is_err());
    // Duplicate CHAN.
    let chan = 2u32.to_be_bytes().to_vec();
    let dup_chan = build_svx(
        &vhdr(4, 0, 1),
        &[(b"CHAN", chan.clone()), (b"CHAN", chan)],
        &[0u8; 4],
    );
    assert!(parse_voice(&dup_chan).is_err());
    // Chunk running past the FORM end.
    let mut overrun = build_svx(&vhdr(4, 0, 1), &[], &[0u8; 4]);
    let len = overrun.len();
    overrun[len - 8..len - 4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    assert!(parse_voice(&overrun).is_err());
    // Not a FORM / not 8SVX.
    assert!(parse_voice(b"LIST....8SVX").is_err());
    assert!(parse_voice(b"FORM\x00\x00\x00\x04ILBM").is_err());
    assert!(parse_voice(&[]).is_err());
}

/// Shape mismatches through encode_voice are rejected: wrong octave
/// count, wrong octave length, wrong channel count vs CHAN.
#[test]
fn encode_voice_shape_validation() {
    let header = VoiceHeader {
        one_shot_hi_samples: 4,
        ct_octave: 2,
        samples_per_sec: 8000,
        ..VoiceHeader::default()
    };
    // Only one octave supplied where ctOctave says 2.
    let missing_octave = voice_of(
        header,
        vec![ChannelSamples {
            octaves: vec![octave_ramp(4, 0)],
        }],
    );
    assert!(encode_voice(&missing_octave).is_err());
    // Octave 1 has the wrong length (should be 8).
    let bad_len = voice_of(
        header,
        vec![ChannelSamples {
            octaves: vec![octave_ramp(4, 0), octave_ramp(9, 0)],
        }],
    );
    assert!(encode_voice(&bad_len).is_err());
    // Stereo CHAN but one channel of data.
    let v = Voice {
        header: VoiceHeader {
            one_shot_hi_samples: 4,
            samples_per_sec: 8000,
            ..VoiceHeader::default()
        },
        channel: Some(ChannelAssignment::Stereo),
        channels: vec![ChannelSamples {
            octaves: vec![octave_ramp(4, 0)],
        }],
        ..Voice::default()
    };
    assert!(encode_voice(&v).is_err());
}

/// A voice whose VHDR waveform lengths are zeroed surfaces the whole
/// decoded stream as one octave (the body-derived fallback), and
/// encodes back byte-identically.
#[test]
fn parse_voice_zeroed_header_single_octave() {
    let h = vhdr(0, 0, 1);
    let body: Vec<u8> = (0u8..6).collect();
    let file = build_svx(&h, &[], &body);
    let v = parse_voice(&file).unwrap();
    assert_eq!(v.octave_count(), 1);
    assert_eq!(v.channels[0].octaves[0].len(), 6);
    assert_eq!(encode_voice(&v).unwrap(), file);
}

// ── SvxMuxer voice-structure options ─────────────────────────────────

use oxideav_core::{
    CodecId, CodecParameters, MediaType, Muxer, Packet, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_iff::svx::SvxMuxer;

fn pcm_s8_stream(sr: u32, channels: u16) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s8"));
    params.media_type = MediaType::Audio;
    params.channels = Some(channels);
    params.sample_rate = Some(sr);
    params.sample_format = Some(SampleFormat::S8);
    params.bit_rate = Some(8 * channels as u64 * sr as u64);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sr as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn tmp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-iff-voice-{tag}-{}-{n}.8svx",
        std::process::id()
    ))
}

fn mux_with(
    mux_build: impl FnOnce(SvxMuxer) -> SvxMuxer,
    stream: &StreamInfo,
    payload: &[u8],
    tag: &str,
) -> Vec<u8> {
    let path = tmp_path(tag);
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = mux_build(SvxMuxer::new(ws, std::slice::from_ref(stream)).unwrap());
        mux.write_header().unwrap();
        let pkt = Packet::new(0, stream.time_base, payload.to_vec());
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// The muxer's builder options land as real chunks: the file parses
/// back with the loop split, volume, hi-cycle, PAN, envelopes, SEQN
/// and FADE all present, and the demuxer's duration covers the loop.
#[test]
fn muxer_voice_structure_options_roundtrip() {
    let stream = pcm_s8_stream(8000, 1);
    let payload: Vec<u8> = (0..16u8).collect();
    let atak = Envelope {
        points: vec![EgPoint {
            duration_ms: 25,
            dest: 0x0001_0000,
        }],
    };
    let rlse = Envelope {
        points: vec![EgPoint {
            duration_ms: 40,
            dest: 0,
        }],
    };
    let seqn = Seqn {
        segments: vec![
            SeqnSegment { start: 0, end: 8 },
            SeqnSegment { start: 8, end: 16 },
        ],
    };
    let bytes = mux_with(
        |m| {
            m.with_volume(0x8000)
                .with_hi_cycle(8)
                .with_repeat(6)
                .with_mono_routing(ChannelAssignment::Left)
                .with_pan(Pan {
                    position: Pan::CENTER,
                })
                .with_attack(atak.clone())
                .with_release(rlse.clone())
                .with_seqn(seqn.clone())
                .with_fade(Fade { segment: 1 })
        },
        &stream,
        &payload,
        "full-options",
    );

    let v = parse_voice(&bytes).unwrap();
    assert_eq!(v.header.one_shot_hi_samples, 10);
    assert_eq!(v.header.repeat_hi_samples, 6);
    assert_eq!(v.header.samples_per_hi_cycle, 8);
    assert_eq!(v.header.volume, 0x8000);
    assert_eq!(v.channel, Some(ChannelAssignment::Left));
    assert_eq!(
        v.pan,
        Some(Pan {
            position: Pan::CENTER
        })
    );
    assert_eq!(v.attack, vec![atak]);
    assert_eq!(v.release, vec![rlse]);
    assert_eq!(v.seqn, Some(seqn));
    assert_eq!(v.fade, Some(Fade { segment: 1 }));
    assert_eq!(v.channels[0].octaves[0].len(), 16);

    // Demux still sees the full 16 frames (one-shot + repeat).
    let mut dmx = open_demuxer(bytes);
    assert_eq!(dmx.streams()[0].duration, Some(16));
    assert_eq!(drain(&mut *dmx), payload);
}

/// with_repeat clamps to the frames actually written; the parts always
/// sum to the frame count.
#[test]
fn muxer_repeat_clamps_to_written_frames() {
    let stream = pcm_s8_stream(8000, 1);
    let bytes = mux_with(
        |m| m.with_repeat(1_000_000),
        &stream,
        &[1u8; 12],
        "repeat-clamp",
    );
    let v = parse_voice(&bytes).unwrap();
    assert_eq!(v.header.one_shot_hi_samples, 0);
    assert_eq!(v.header.repeat_hi_samples, 12);
    assert!(v.header.has_loop());
}

/// PAN / mono routing conflict with a stereo stream and are rejected
/// at write_header time.
#[test]
fn muxer_rejects_pan_and_routing_on_stereo() {
    let stream = pcm_s8_stream(8000, 2);
    for build in [
        (|m: SvxMuxer| {
            m.with_pan(Pan {
                position: Pan::LEFT,
            })
        }) as fn(SvxMuxer) -> SvxMuxer,
        |m: SvxMuxer| m.with_mono_routing(ChannelAssignment::Right),
    ] {
        let path = tmp_path("stereo-conflict");
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = build(SvxMuxer::new(ws, std::slice::from_ref(&stream)).unwrap());
        assert!(mux.write_header().is_err());
        let _ = std::fs::remove_file(&path);
    }
    // Passing Stereo as a "mono routing" is likewise refused.
    let mono = pcm_s8_stream(8000, 1);
    let path = tmp_path("routing-stereo-arg");
    let f = std::fs::File::create(&path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = SvxMuxer::new(ws, std::slice::from_ref(&mono))
        .unwrap()
        .with_mono_routing(ChannelAssignment::Stereo);
    assert!(mux.write_header().is_err());
    let _ = std::fs::remove_file(&path);
}
