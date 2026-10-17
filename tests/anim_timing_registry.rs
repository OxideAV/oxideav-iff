//! ANIM frames keep their per-frame timing on both paths: `decode_all`
//! reports each frame's display duration (the next frame's
//! `ANHD.reltime`, the last frame holding its own), and the `iff_anim`
//! demuxer emits the same durations as packet `pts` / `duration` in
//! the 1/60 s stream time base. The `ilbm` codec declares no `quality`
//! (lossless), and its decoder labels the layout it emitted.

use std::io::Cursor;
use std::time::Duration;

use oxideav_core::{CodecId, ContainerRegistry, Error, Frame as CoreFrame, PixelFormat, ReadSeek};
use oxideav_iff::{
    decode_all, encode_all, make_decoder, EncodeOptions, Frame, IffImage, Palette, CODEC_ID_STR,
};

fn frames() -> Vec<Frame> {
    let pal = Palette::from_rgb_triples(&[[0, 0, 0], [255, 255, 255], [255, 0, 0]]);
    let mk = |bits: Vec<u8>| IffImage::new_indexed(4, 2, bits, pal.clone()).unwrap();
    vec![
        Frame::new(
            mk(vec![0, 1, 0, 1, 1, 0, 1, 0]),
            Some(Duration::from_millis(100)),
            0,
        ),
        Frame::new(
            mk(vec![2, 2, 0, 1, 1, 0, 2, 2]),
            Some(Duration::from_millis(250)),
            1,
        ),
        Frame::new(mk(vec![0; 8]), Some(Duration::from_millis(50)), 2),
    ]
}

#[test]
fn decode_all_and_the_anim_demuxer_agree_on_per_frame_timing() {
    let frames = frames();
    let bytes = encode_all(&frames, &EncodeOptions::default()).unwrap();

    // Standalone: frame i stays up for its delay (6 / 15 jiffies); the
    // last frame holds its own reltime (15 jiffies = 250 ms).
    let all = decode_all(&bytes).unwrap();
    let delays: Vec<Duration> = all.iter().map(|f| f.delay.unwrap()).collect();
    assert_eq!(
        delays,
        [
            Duration::from_millis(100),
            Duration::from_millis(250),
            Duration::from_millis(250)
        ]
    );
    assert!(all
        .iter()
        .zip(&frames)
        .all(|(a, b)| a.image.planes == b.image.planes));

    // Registry: the demuxer's packets carry the same timeline in 1/60 s.
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let mut dmx = reg
        .open_demuxer("iff_anim", rs, &oxideav_core::NullCodecResolver)
        .unwrap();
    let s = &dmx.streams()[0];
    assert_eq!(s.time_base, oxideav_core::TimeBase::new(1, 60));
    assert_eq!(s.duration, Some(6 + 15 + 15));
    let mut timing = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => timing.push((p.pts.unwrap(), p.duration.unwrap())),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(timing, [(0, 6), (6, 15), (21, 15)]);
    for ((_, dur), delay) in timing.iter().zip(&delays) {
        assert_eq!(Duration::from_micros(*dur as u64 * 1_000_000 / 60), *delay);
    }
    assert_eq!(dmx.duration_micros(), Some(36 * 1_000_000 / 60));
}

#[test]
fn the_ilbm_decoder_labels_what_it_emitted_and_declares_no_quality() {
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_iff::register(&mut ctx);
    let id = CodecId::new(CODEC_ID_STR);
    let schema = ctx.codecs.encoder_options_schema(&id).unwrap();
    assert!(schema.iter().all(|f| f.name != "quality"));
    assert!(ctx.codecs.decoder_options_schema(&id).is_none());

    let frames = frames();
    let bytes = encode_all(&frames, &EncodeOptions::default()).unwrap();
    let params = oxideav_core::CodecParameters::video(id);
    let mut dec = make_decoder(&params).unwrap();
    assert_eq!(dec.output_pixel_format(), None);
    // The whole ANIM as one packet decodes to the seed frame.
    let pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), bytes);
    dec.send_packet(&pkt).unwrap();
    assert_eq!(dec.output_pixel_format(), Some(PixelFormat::Pal8));
    let CoreFrame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("video frame expected");
    };
    assert_eq!(v.planes[0].data, frames[0].image.planes[0].data);
    assert_eq!(v.palette_entry_count_for_test(), 3);
}

trait PaletteCount {
    fn palette_entry_count_for_test(&self) -> usize;
}

impl PaletteCount for oxideav_core::VideoFrame {
    fn palette_entry_count_for_test(&self) -> usize {
        self.palette().map_or(0, |p| p.len() / 3)
    }
}
