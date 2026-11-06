//! Stream output parameters under the core 0.1.42 contract
//! (`Decoder::output_params` / `output_params_generation`,
//! `CodecParameters::merge_output_params`, `OutputParamsWatch`).
//!
//! Every `ilbm` packet is a self-contained `FORM`, so the shape is a
//! property of each picture: a `CAT ` / `LIST` (EA IFF 85 §5) may hold
//! children of different sizes or layouts (indexed / EHB `Pal8`, HAM /
//! 24-bit `Rgb24`, mask-plane `Rgba`), and a `FORM DEEP` may change the
//! body size per `DBOD` through `DLOC` (iff-truecolor-chunks.md §1.3).
//! The demuxers state the first picture's shape on the stream; the
//! decoder reports what each returned frame is and bumps the generation
//! exactly at a shape change. Fixtures are written by the crate's own
//! encoder (or hand-assembled chunk by chunk where the encoder has no
//! such mode).

use std::io::Cursor;

use oxideav_core::{
    ChannelLayout, CodecId, CodecParameters, ContainerRegistry, Decoder, Demuxer, Error, Frame,
    NullCodecResolver, OutputParamsWatch, Packet, PixelFormat, Rational, ReadSeek, SampleFormat,
    StreamInfo, TimeBase, WriteSeek,
};
use oxideav_iff::ilbm::Masking;
use oxideav_iff::{
    CODEC_ID_STR, EncodeOptions, IffImage, IffPixelFormat, Palette, Plane, encode, encode_all,
    make_decoder, make_encoder,
};

// ───────────────────────── fixtures ─────────────────────────

/// `id + ckSize + body (+ pad)`.
fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len() + 1);
    v.extend_from_slice(id);
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    if body.len() & 1 == 1 {
        v.push(0);
    }
    v
}

/// A `CAT ` / `LIST` of `contents` type around complete `FORM`s.
fn group(id: &[u8; 4], contents: &[u8; 4], forms: &[Vec<u8>]) -> Vec<u8> {
    let mut body = contents.to_vec();
    for f in forms {
        body.extend_from_slice(f);
        if f.len() & 1 == 1 {
            body.push(0);
        }
    }
    chunk(id, &body)
}

fn palette(n: usize, seed: u8) -> Palette {
    Palette::new(
        (0..n)
            .map(|i| {
                let i = i as u8;
                [
                    i.wrapping_mul(37).wrapping_add(seed),
                    i.wrapping_mul(91),
                    255 - i,
                    255,
                ]
            })
            .collect(),
    )
}

/// Indexed `FORM ILBM`, `w × h`, `n` palette entries.
fn indexed(w: u32, h: u32, n: usize, seed: u8) -> Vec<u8> {
    let idx: Vec<u8> = (0..w * h).map(|i| (i as usize % n) as u8).collect();
    let img = IffImage::new(
        w,
        h,
        IffPixelFormat::Pal8,
        vec![Plane::new(w as usize, idx)],
    )
    .unwrap()
    .with_palette(palette(n, seed));
    encode(&img, &EncodeOptions::default()).unwrap()
}

fn rgb(w: u32, h: u32) -> IffImage {
    let data: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 % 251) as u8).collect();
    IffImage::new(
        w,
        h,
        IffPixelFormat::Rgb24,
        vec![Plane::new(w as usize * 3, data)],
    )
    .unwrap()
}

/// HAM6 `FORM ILBM` (CAMG HAM bit) — decodes as `Rgb24`.
fn ham6(w: u32, h: u32) -> Vec<u8> {
    encode(
        &rgb(w, h),
        &EncodeOptions::default().with_viewmode(Some(0x0800)),
    )
    .unwrap()
}

/// Extra-Half-Brite `FORM ILBM` (CAMG EHB bit) — decodes as `Pal8`.
fn ehb(w: u32, h: u32) -> Vec<u8> {
    encode(
        &rgb(w, h),
        &EncodeOptions::default().with_viewmode(Some(0x0080)),
    )
    .unwrap()
}

/// 24-bit literal-RGB `FORM ILBM` — decodes as `Rgb24`.
fn truecolor(w: u32, h: u32) -> Vec<u8> {
    encode(&rgb(w, h), &EncodeOptions::default()).unwrap()
}

/// Indexed picture with a mask plane — decodes as `Rgba`.
fn masked(w: u32, h: u32) -> Vec<u8> {
    let idx: Vec<u8> = (0..w * h).map(|i| (i % 4) as u8).collect();
    let img = IffImage::new(
        w,
        h,
        IffPixelFormat::Pal8,
        vec![Plane::new(w as usize, idx)],
    )
    .unwrap()
    .with_palette(palette(4, 3));
    encode(
        &img,
        &EncodeOptions::default().with_masking(Some(Masking::HasMask)),
    )
    .unwrap()
}

// ───────────────────────── harness ─────────────────────────

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    oxideav_iff::register_containers(&mut reg);
    reg
}

fn open(bytes: &[u8], expect: &str) -> Box<dyn Demuxer> {
    let reg = registry();
    let mut cur = Cursor::new(bytes.to_vec());
    assert_eq!(reg.probe_input(&mut cur, None).unwrap(), expect);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    reg.open_demuxer(expect, rs, &NullCodecResolver).unwrap()
}

fn packets(dmx: &mut dyn Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux: {e}"),
        }
    }
}

/// `CodecParameters` has no `PartialEq`; its `Debug` rendering covers
/// every field, so equal renderings are equal parameters.
fn assert_same(a: Option<&CodecParameters>, b: Option<&CodecParameters>) {
    assert_eq!(
        format!("{a:?}"),
        format!("{b:?}"),
        "event payload != output_params()"
    );
}

/// What a consumer sees frame by frame.
struct Run {
    /// `(width, height, pixel_format)` of the stream after each frame.
    shapes: Vec<(u32, u32, PixelFormat)>,
    /// `output_params_generation()` after each frame.
    generations: Vec<u64>,
    /// Frames at which `OutputParamsWatch::observe` reported a change.
    changes: Vec<usize>,
    /// Frames preceded by a `Frame::StreamParams` event.
    events: Vec<usize>,
    /// The decoded frames' plane sizes, to check against the shapes.
    plane_lens: Vec<usize>,
}

/// Feed `pkts` one at a time and drain the decoder after each, the way
/// a consumer does: `OutputParamsWatch::observe` after every successful
/// `receive_frame` on `stream`, and — independently — the in-band
/// events folded into a second copy of the stream with
/// `observe_frame`. Checks the core 0.1.43 contract on the way: the
/// event's payload is `output_params()`, the generation has already
/// moved by exactly one, the next frame has the announced shape, both
/// watches see each change once, and the two stream copies agree after
/// every frame.
fn pump(dec: &mut dyn Decoder, stream: &mut CodecParameters, pkts: &[Packet]) -> Run {
    let mut watch = OutputParamsWatch::new();
    let mut event_watch = OutputParamsWatch::new();
    let mut by_event = stream.clone();
    let mut run = Run {
        shapes: Vec::new(),
        generations: Vec::new(),
        changes: Vec::new(),
        events: Vec::new(),
        plane_lens: Vec::new(),
    };
    let mut event_changes = Vec::new();
    let first_gen = dec.output_params_generation();
    let mut last_gen = first_gen;
    let mut announced: Option<CodecParameters> = None;
    for p in pkts {
        dec.send_packet(p).unwrap();
        if run.shapes.is_empty() {
            // The first shape is announced up front, never by an event.
            event_watch.prime(dec, &mut by_event);
        }
        loop {
            let frame = match dec.receive_frame() {
                Ok(f) => f,
                Err(Error::NeedMore) => break,
                Err(e) => panic!("decode: {e}"),
            };
            let index = run.shapes.len();
            if watch.observe(dec, stream) {
                run.changes.push(index);
            }
            if event_watch.observe_frame(&frame, &mut by_event) {
                event_changes.push(index);
            }
            if let Some(params) = frame.stream_params() {
                assert!(index > 0, "no event for the first shape");
                assert!(announced.is_none(), "two events for one change");
                assert_same(dec.output_params().as_ref(), Some(params));
                let generation = dec.output_params_generation();
                assert_eq!(generation, last_gen.wrapping_add(1), "event without a bump");
                last_gen = generation;
                run.events.push(index);
                announced = Some(params.clone());
                continue;
            }
            let Frame::Video(v) = frame else {
                panic!("video frame expected");
            };
            assert_eq!(
                dec.output_params_generation(),
                last_gen,
                "frame {index}: generation moved without an event"
            );
            if let Some(a) = announced.take() {
                assert_eq!(
                    v.planes[0].data.len(),
                    a.width.unwrap() as usize
                        * a.height.unwrap() as usize
                        * bpp(a.pixel_format.unwrap()),
                    "frame {index} does not have the announced shape"
                );
            }
            run.shapes.push((
                stream.width.unwrap(),
                stream.height.unwrap(),
                stream.pixel_format.unwrap(),
            ));
            assert_eq!(
                (by_event.width, by_event.height, by_event.pixel_format),
                (stream.width, stream.height, stream.pixel_format),
                "frame {index}: the event-fed stream disagrees with the watched one"
            );
            run.generations.push(dec.output_params_generation());
            run.plane_lens.push(v.planes[0].data.len());
        }
    }
    // Counter and events agree 1:1: every bump came with one event, and
    // each watch reported each mid-stream change exactly once.
    assert_eq!(run.events.len() as u64, last_gen.wrapping_sub(first_gen));
    assert_eq!(event_changes, run.events);
    let mid_stream: Vec<usize> = run.changes.iter().copied().filter(|&i| i > 0).collect();
    assert_eq!(mid_stream, run.events);
    run
}

fn bpp(pf: PixelFormat) -> usize {
    match pf {
        PixelFormat::Pal8 => 1,
        PixelFormat::Rgb24 => 3,
        PixelFormat::Rgba => 4,
        other => panic!("unexpected {other:?}"),
    }
}

/// Demux `bytes`, decode every packet, and check that the stream
/// parameters truthfully describe every frame.
fn demux_and_decode(bytes: &[u8], expect: &str) -> (CodecParameters, Run) {
    let mut dmx = open(bytes, expect);
    let mut stream = dmx.streams()[0].params.clone();
    assert_eq!(stream.codec_id, CodecId::new(CODEC_ID_STR));
    let pkts = packets(&mut *dmx);
    let mut dec = make_decoder(&stream).unwrap();
    let run = pump(&mut *dec, &mut stream, &pkts);
    for (i, &(w, h, pf)) in run.shapes.iter().enumerate() {
        assert_eq!(
            run.plane_lens[i],
            w as usize * h as usize * bpp(pf),
            "frame {i}: plane does not match the stream's {w}x{h} {pf:?}"
        );
    }
    (dmx.streams()[0].params.clone(), run)
}

// ───────────────────────── decoder contract ─────────────────────────

fn pkt(data: Vec<u8>, pts: i64) -> Packet {
    let mut p = Packet::new(0, oxideav_core::TimeBase::new(1, 1), data);
    p.pts = Some(pts);
    p
}

#[test]
fn output_params_is_none_before_the_first_packet_and_resolves_before_the_first_frame() {
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    assert!(dec.output_params().is_none());
    assert_eq!(dec.output_pixel_format(), None);
    dec.send_packet(&pkt(ham6(8, 3), 0)).unwrap();
    // Known before receive_frame: the first queued picture.
    let p = dec.output_params().unwrap();
    assert_eq!(p.codec_id, CodecId::new(CODEC_ID_STR));
    assert_eq!((p.width, p.height), (Some(8), Some(3)));
    assert_eq!(p.pixel_format, Some(PixelFormat::Rgb24));
    assert_eq!(dec.output_pixel_format(), Some(PixelFormat::Rgb24));
    assert_eq!(p.frame_rate, None);
    assert_eq!(dec.output_params_generation(), 0);
    dec.receive_frame().unwrap();
    // Resolving None -> Some is not a change.
    assert_eq!(dec.output_params_generation(), 0);
}

#[test]
fn constant_stream_never_bumps_even_when_palettes_differ() {
    let files = [
        indexed(6, 4, 4, 0),
        indexed(6, 4, 4, 99),
        indexed(6, 4, 8, 5),
    ];
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    let mut stream = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    let pkts: Vec<Packet> = files
        .iter()
        .enumerate()
        .map(|(i, f)| pkt(f.clone(), i as i64))
        .collect();
    let run = pump(&mut *dec, &mut stream, &pkts);
    assert_eq!(run.generations, vec![0, 0, 0]);
    // The first observe resolves the empty stream; nothing after.
    assert_eq!(run.changes, vec![0]);
    // A constant stream never sees a `StreamParams` event.
    assert!(run.events.is_empty());
    assert_eq!(run.shapes, vec![(6, 4, PixelFormat::Pal8); 3]);
}

#[test]
fn queued_frames_bump_only_when_the_changed_frame_is_returned() {
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    dec.send_packet(&pkt(indexed(4, 2, 4, 0), 0)).unwrap();
    dec.send_packet(&pkt(indexed(4, 2, 4, 1), 1)).unwrap();
    dec.send_packet(&pkt(truecolor(5, 5), 2)).unwrap();
    for (pts, gen_after, shape) in [
        (0, 0, (4, 2, PixelFormat::Pal8)),
        (1, 0, (4, 2, PixelFormat::Pal8)),
        (2, 1, (5, 5, PixelFormat::Rgb24)),
    ] {
        // Before the frame (or its event) is returned the answer still
        // describes the previous one.
        let mut frame = dec.receive_frame().unwrap();
        if gen_after == 1 {
            // The changed picture is announced in-band first, with the
            // counter already moved and the payload = output_params().
            let ev = frame
                .stream_params()
                .expect("StreamParams before the 5x5 frame");
            assert_eq!(dec.output_params_generation(), 1);
            assert_same(dec.output_params().as_ref(), Some(ev));
            assert_eq!((ev.width, ev.height), (Some(5), Some(5)));
            assert_eq!(ev.pixel_format, Some(PixelFormat::Rgb24));
            assert_eq!(frame.pts(), None);
            frame = dec.receive_frame().unwrap();
        }
        let Frame::Video(v) = frame else { panic!() };
        assert_eq!(v.pts, Some(pts));
        assert_eq!(dec.output_params_generation(), gen_after);
        let p = dec.output_params().unwrap();
        assert_eq!(
            (p.width.unwrap(), p.height.unwrap(), p.pixel_format.unwrap()),
            shape
        );
    }
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
}

#[test]
fn flush_drains_old_shape_frames_then_the_event_then_the_new_shape() {
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    dec.send_packet(&pkt(indexed(4, 2, 4, 0), 0)).unwrap();
    dec.send_packet(&pkt(indexed(4, 2, 4, 1), 1)).unwrap();
    dec.send_packet(&pkt(truecolor(5, 5), 2)).unwrap();
    dec.send_packet(&pkt(truecolor(5, 5), 3)).unwrap();
    dec.flush().unwrap();
    let mut seen = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => seen.push(format!("v{}", v.pts.unwrap())),
            Ok(f) if f.is_stream_params() => seen.push("event".to_string()),
            Ok(_) => panic!("unexpected frame kind"),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(seen, ["v0", "v1", "event", "v2", "v3"]);
    assert_eq!(dec.output_params_generation(), 1);
}

#[test]
fn arena_frames_skip_the_event_and_the_counter_still_moves() {
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    dec.send_packet(&pkt(indexed(4, 2, 4, 0), 0)).unwrap();
    dec.send_packet(&pkt(truecolor(5, 5), 1)).unwrap();
    dec.receive_arena_frame().unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    dec.receive_arena_frame().unwrap();
    assert_eq!(dec.output_params_generation(), 1);
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
}

#[test]
fn every_layout_switch_is_one_bump() {
    // Same size throughout: indexed Pal8 -> HAM6 Rgb24 -> mask Rgba ->
    // EHB Pal8 -> indexed Pal8 (no change) -> 24-bit Rgb24.
    let files = [
        indexed(16, 4, 16, 0),
        ham6(16, 4),
        masked(16, 4),
        ehb(16, 4),
        indexed(16, 4, 4, 7),
        truecolor(16, 4),
    ];
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    let mut stream = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    let pkts: Vec<Packet> = files
        .iter()
        .enumerate()
        .map(|(i, f)| pkt(f.clone(), i as i64))
        .collect();
    let run = pump(&mut *dec, &mut stream, &pkts);
    let formats: Vec<PixelFormat> = run.shapes.iter().map(|s| s.2).collect();
    assert_eq!(
        formats,
        vec![
            PixelFormat::Pal8,
            PixelFormat::Rgb24,
            PixelFormat::Rgba,
            PixelFormat::Pal8,
            PixelFormat::Pal8,
            PixelFormat::Rgb24
        ]
    );
    assert_eq!(run.generations, vec![0, 1, 2, 3, 3, 4]);
    assert_eq!(run.changes, vec![0, 1, 2, 3, 5]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![1, 2, 3, 5]);
}

#[test]
fn reset_keeps_the_last_returned_shape_as_the_reference() {
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    dec.send_packet(&pkt(indexed(6, 4, 4, 0), 0)).unwrap();
    dec.receive_frame().unwrap();
    // A queued picture of another shape is dropped by reset: it was
    // never announced, so it is no change.
    dec.send_packet(&pkt(truecolor(9, 9), 1)).unwrap();
    dec.reset().unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    // Same shape after the seek: no bump, no event.
    dec.send_packet(&pkt(indexed(6, 4, 4, 9), 2)).unwrap();
    assert!(matches!(dec.receive_frame(), Ok(Frame::Video(_))));
    assert_eq!(dec.output_params_generation(), 0);
    // Another shape after a second seek: one event, one bump, then the
    // frame.
    dec.reset().unwrap();
    dec.send_packet(&pkt(truecolor(9, 9), 3)).unwrap();
    assert_eq!(dec.output_params_generation(), 0);
    let ev = dec.receive_frame().unwrap();
    assert!(ev.is_stream_params());
    assert_eq!(dec.output_params_generation(), 1);
    assert_same(dec.output_params().as_ref(), ev.stream_params());
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!()
    };
    assert_eq!(v.pts, Some(3));
    assert_eq!(dec.output_params_generation(), 1);
    let p = dec.output_params().unwrap();
    assert_eq!((p.width, p.height), (Some(9), Some(9)));
    assert_eq!(p.pixel_format, Some(PixelFormat::Rgb24));
}

#[test]
fn reset_between_an_event_and_its_frame_announces_the_way_back() {
    // The consumer was told 9x9 Rgb24; the seek drops that picture and
    // the next one is 6x4 Pal8 again — a change relative to what was
    // announced, so it gets its own event and bump.
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    dec.send_packet(&pkt(indexed(6, 4, 4, 0), 0)).unwrap();
    dec.receive_frame().unwrap();
    dec.send_packet(&pkt(truecolor(9, 9), 1)).unwrap();
    assert!(dec.receive_frame().unwrap().is_stream_params());
    assert_eq!(dec.output_params_generation(), 1);
    dec.reset().unwrap();
    dec.send_packet(&pkt(indexed(6, 4, 4, 1), 2)).unwrap();
    let ev = dec.receive_frame().unwrap();
    let p = ev.stream_params().expect("event for the way back");
    assert_eq!(
        (p.width, p.height, p.pixel_format),
        (Some(6), Some(4), Some(PixelFormat::Pal8))
    );
    assert_eq!(dec.output_params_generation(), 2);
    assert!(matches!(dec.receive_frame(), Ok(Frame::Video(_))));
    assert_eq!(dec.output_params_generation(), 2);
}

#[test]
fn flush_then_reset_accepts_packets_again() {
    let mut dec = make_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR))).unwrap();
    dec.send_packet(&pkt(indexed(6, 4, 4, 0), 0)).unwrap();
    dec.flush().unwrap();
    dec.receive_frame().unwrap();
    assert!(matches!(dec.receive_frame(), Err(Error::Eof)));
    dec.reset().unwrap();
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    dec.send_packet(&pkt(indexed(6, 4, 4, 0), 1)).unwrap();
    dec.receive_frame().unwrap();
    assert_eq!(dec.output_params_generation(), 0);
}

// ───────────────────────── CAT / LIST through iff_ilbm ─────────────────────────

#[test]
fn cat_of_different_sizes_demuxes_one_packet_per_child_and_bumps_once() {
    let bytes = group(
        b"CAT ",
        b"ILBM",
        &[
            indexed(6, 4, 4, 0),
            indexed(6, 4, 4, 50),
            indexed(10, 3, 4, 0),
        ],
    );
    let (declared, run) = demux_and_decode(&bytes, "iff_ilbm");
    // The stream states the first child, the palette as extradata.
    assert_eq!((declared.width, declared.height), (Some(6), Some(4)));
    assert_eq!(declared.pixel_format, Some(PixelFormat::Pal8));
    assert_eq!(declared.extradata, palette(4, 0).to_rgb());
    assert_eq!(
        run.shapes,
        vec![
            (6, 4, PixelFormat::Pal8),
            (6, 4, PixelFormat::Pal8),
            (10, 3, PixelFormat::Pal8)
        ]
    );
    assert_eq!(run.generations, vec![0, 0, 1]);
    // The stream already matched the first child: the only change the
    // watch reports is the resize.
    assert_eq!(run.changes, vec![2]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![2]);
}

#[test]
fn list_of_mixed_depths_reports_each_layout() {
    let bytes = group(
        b"LIST",
        b"ILBM",
        &[ham6(8, 8), ham6(8, 8), masked(8, 8), masked(8, 8)],
    );
    let (declared, run) = demux_and_decode(&bytes, "iff_ilbm");
    assert_eq!(declared.pixel_format, Some(PixelFormat::Rgb24));
    assert_eq!(run.generations, vec![0, 0, 1, 1]);
    assert_eq!(run.changes, vec![2]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![2]);
    assert_eq!(run.shapes[3], (8, 8, PixelFormat::Rgba));
}

#[test]
fn constant_cat_is_a_constant_stream() {
    let bytes = group(
        b"CAT ",
        b"ILBM",
        &[ehb(8, 2), indexed(8, 2, 32, 1), indexed(8, 2, 2, 2)],
    );
    let (declared, run) = demux_and_decode(&bytes, "iff_ilbm");
    assert_eq!(declared.pixel_format, Some(PixelFormat::Pal8));
    assert_eq!(run.generations, vec![0, 0, 0]);
    assert!(run.changes.is_empty());
    assert!(run.events.is_empty());
}

#[test]
fn blank_cat_of_mixed_raster_forms_probes_on_its_first_child() {
    // §5: a CAT's contents type may be four spaces. The children here
    // are an ILBM and a 24-bit DEEP picture.
    let deep = encode(
        &rgb(3, 2),
        &EncodeOptions::default().with_form(oxideav_iff::IffForm::Deep),
    )
    .unwrap();
    let bytes = group(b"CAT ", b"    ", &[indexed(4, 4, 4, 0), deep]);
    let (_, run) = demux_and_decode(&bytes, "iff_ilbm");
    assert_eq!(
        run.shapes,
        vec![(4, 4, PixelFormat::Pal8), (3, 2, PixelFormat::Rgb24)]
    );
    assert_eq!(run.generations, vec![0, 1]);
    assert_eq!(run.changes, vec![1]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![1]);
}

#[test]
fn group_packets_are_the_children_with_sequential_pts() {
    let a = indexed(4, 4, 4, 0);
    let b = indexed(2, 2, 2, 0);
    // A non-raster child (8SVX) is not a picture and is skipped.
    let voice = chunk(b"FORM", &[b"8SVX".to_vec(), chunk(b"NAME", b"x")].concat());
    let bytes = group(b"CAT ", b"ILBM", &[a.clone(), voice, b.clone()]);
    let mut dmx = open(&bytes, "iff_ilbm");
    let s = &dmx.streams()[0];
    assert_eq!(s.duration, Some(2));
    let pkts = packets(&mut *dmx);
    assert_eq!(pkts.len(), 2);
    assert_eq!(pkts[0].data, a);
    assert_eq!(pkts[1].data, b);
    assert_eq!(
        pkts.iter().map(|p| p.pts).collect::<Vec<_>>(),
        vec![Some(0), Some(1)]
    );
    assert!(pkts.iter().all(|p| p.flags.keyframe));
}

// ───────────────────────── DEEP with per-DBOD DLOC ─────────────────────────

fn dgbl(w: u16, h: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes()); // NOCOMPRESSION
    v.extend_from_slice(&[1, 1]);
    chunk(b"DGBL", &v)
}

/// RGB 8:8:8 (`alpha` adds an ALPHA 8 element).
fn dpel(alpha: bool) -> Vec<u8> {
    let mut els: Vec<(u16, u16)> = vec![(1, 8), (2, 8), (3, 8)];
    if alpha {
        els.push((4, 8));
    }
    let mut v = (els.len() as u32).to_be_bytes().to_vec();
    for (t, d) in els {
        v.extend_from_slice(&t.to_be_bytes());
        v.extend_from_slice(&d.to_be_bytes());
    }
    chunk(b"DPEL", &v)
}

fn dloc(w: u16, h: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    chunk(b"DLOC", &v)
}

fn dbod(w: u16, h: u16, comps: usize, seed: u8) -> Vec<u8> {
    let body: Vec<u8> = (0..w as usize * h as usize * comps)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect();
    chunk(b"DBOD", &body)
}

fn dchg(ms: i32) -> Vec<u8> {
    chunk(b"DCHG", &ms.to_be_bytes())
}

fn form(ft: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = ft.to_vec();
    for c in chunks {
        body.extend_from_slice(c);
    }
    chunk(b"FORM", &body)
}

#[test]
fn deep_with_dloc_resizes_rides_the_ilbm_codec_and_bumps_per_resize() {
    // DGBL 4x4; bodies 4x4 (DLOC), 2x3 (DLOC), then 4x4 again (no DLOC:
    // the DGBL size, §1.3).
    let bytes = form(
        b"DEEP",
        &[
            dgbl(4, 4),
            dpel(false),
            dchg(40),
            dloc(4, 4),
            dbod(4, 4, 3, 0),
            dloc(2, 3),
            dbod(2, 3, 3, 1),
            dbod(4, 4, 3, 2),
        ],
    );
    let (declared, run) = demux_and_decode(&bytes, "iff_deep");
    assert_eq!((declared.width, declared.height), (Some(4), Some(4)));
    assert_eq!(declared.pixel_format, Some(PixelFormat::Rgb24));
    // DCHG 40 ms per frame: 25 fps.
    assert_eq!(declared.frame_rate, Some(Rational::new(1000, 40)));
    assert_eq!(
        run.shapes,
        vec![
            (4, 4, PixelFormat::Rgb24),
            (2, 3, PixelFormat::Rgb24),
            (4, 4, PixelFormat::Rgb24)
        ]
    );
    assert_eq!(run.generations, vec![0, 1, 2]);
    assert_eq!(run.changes, vec![1, 2]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![1, 2]);

    // Pixels: what decode_all reports for the same FORM, frame by frame.
    let want = oxideav_iff::decode_all(&bytes).unwrap();
    let mut dmx = open(&bytes, "iff_deep");
    let pkts = packets(&mut *dmx);
    assert_eq!(
        pkts.iter().map(|p| (p.pts, p.duration)).collect::<Vec<_>>(),
        vec![
            (Some(0), Some(40)),
            (Some(40), Some(40)),
            (Some(80), Some(40))
        ]
    );
    let mut dec = make_decoder(&dmx.streams()[0].params).unwrap();
    let mut events = 0;
    for (p, w) in pkts.iter().zip(&want) {
        dec.send_packet(p).unwrap();
        let mut frame = dec.receive_frame().unwrap();
        // A consumer that does not care skips the resize announcements.
        while frame.is_stream_params() {
            events += 1;
            frame = dec.receive_frame().unwrap();
        }
        let Frame::Video(v) = frame else { panic!() };
        assert_eq!(v.planes[0].data, w.image.planes[0].data);
    }
    assert_eq!(events, 2);
}

#[test]
fn tvpp_layers_of_different_sizes_ride_the_ilbm_codec() {
    let bytes = form(
        b"TVPP",
        &[
            dgbl(4, 2),
            dpel(true),
            dbod(4, 2, 4, 0),
            dloc(1, 1),
            dbod(1, 1, 4, 5),
        ],
    );
    let (declared, run) = demux_and_decode(&bytes, "iff_tvpp");
    assert_eq!(declared.pixel_format, Some(PixelFormat::Rgba));
    assert_eq!(
        run.shapes,
        vec![(4, 2, PixelFormat::Rgba), (1, 1, PixelFormat::Rgba)]
    );
    assert_eq!(run.generations, vec![0, 1]);
    assert_eq!(run.changes, vec![1]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![1]);
}

#[test]
fn deep_with_uniform_bodies_stays_rawvideo_with_its_frame_rate() {
    let bytes = form(
        b"DEEP",
        &[
            dgbl(3, 2),
            dpel(true),
            dchg(100),
            dbod(3, 2, 4, 0),
            dloc(3, 2),
            dbod(3, 2, 4, 9),
        ],
    );
    let dmx = open(&bytes, "iff_deep");
    let s = &dmx.streams()[0].params;
    assert_eq!(s.codec_id, CodecId::new("rawvideo"));
    assert_eq!((s.width, s.height), (Some(3), Some(2)));
    assert_eq!(s.pixel_format, Some(PixelFormat::Rgba));
    assert_eq!(s.frame_rate, Some(Rational::new(1000, 100)));
}

#[test]
fn still_deep_states_no_frame_rate() {
    let bytes = form(b"DEEP", &[dgbl(3, 2), dpel(false), dbod(3, 2, 3, 0)]);
    let dmx = open(&bytes, "iff_deep");
    assert_eq!(dmx.streams()[0].params.frame_rate, None);
}

// ───────────────────────── ANIM ─────────────────────────

fn anim_frames(delays_ms: &[u64]) -> Vec<oxideav_iff::Frame> {
    delays_ms
        .iter()
        .enumerate()
        .map(|(i, &ms)| {
            let idx: Vec<u8> = (0..16u32).map(|p| ((p as usize + i) % 4) as u8).collect();
            let img = IffImage::new(4, 4, IffPixelFormat::Pal8, vec![Plane::new(4, idx)])
                .unwrap()
                .with_palette(palette(4, 0));
            oxideav_iff::Frame::new(img, Some(std::time::Duration::from_millis(ms)), i as u32)
        })
        .collect()
}

#[test]
fn anim_with_even_timing_states_its_frame_rate() {
    // 100 ms = 6 jiffies per frame -> 10 fps.
    let bytes = encode_all(
        &anim_frames(&[100, 100, 100, 100]),
        &EncodeOptions::default(),
    )
    .unwrap();
    let dmx = open(&bytes, "iff_anim");
    let s = &dmx.streams()[0].params;
    assert_eq!(s.codec_id, CodecId::new("rawvideo"));
    assert_eq!((s.width, s.height), (Some(4), Some(4)));
    assert_eq!(s.pixel_format, Some(PixelFormat::Rgba));
    assert_eq!(s.frame_rate, Some(Rational::new(60, 6)));
}

#[test]
fn anim_with_uneven_timing_states_none() {
    let bytes = encode_all(&anim_frames(&[100, 50, 100]), &EncodeOptions::default()).unwrap();
    let dmx = open(&bytes, "iff_anim");
    assert_eq!(dmx.streams()[0].params.frame_rate, None);
}

// ───────────────────────── audio ─────────────────────────

fn vhdr(rate: u16, samples: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&samples.to_be_bytes()); // oneShotHiSamples
    v.extend_from_slice(&0u32.to_be_bytes()); // repeatHiSamples
    v.extend_from_slice(&0u32.to_be_bytes()); // samplesPerHiCycle
    v.extend_from_slice(&rate.to_be_bytes());
    v.push(1); // ctOctave
    v.push(0); // sCompression
    v.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // volume
    chunk(b"VHDR", &v)
}

#[test]
fn svx_states_its_channel_layout() {
    let mono = form(b"8SVX", &[vhdr(8000, 4), chunk(b"BODY", &[1, 2, 3, 4])]);
    let dmx = open(&mono, "iff_8svx");
    let s = &dmx.streams()[0].params;
    assert_eq!(s.channels, Some(1));
    assert_eq!(s.channel_layout, Some(ChannelLayout::Mono));
    assert_eq!(s.sample_format, Some(SampleFormat::S8));
    assert_eq!(s.sample_rate, Some(8000));

    let stereo = form(
        b"8SVX",
        &[
            vhdr(11025, 2),
            chunk(b"CHAN", &6u32.to_be_bytes()),
            chunk(b"BODY", &[1, 2, 3, 4]),
        ],
    );
    let dmx = open(&stereo, "iff_8svx");
    let s = &dmx.streams()[0].params;
    assert_eq!(s.channels, Some(2));
    assert_eq!(s.channel_layout, Some(ChannelLayout::Stereo));

    // A one-speaker routing names no standard layout: count only.
    let left = form(
        b"8SVX",
        &[
            vhdr(8000, 4),
            chunk(b"CHAN", &2u32.to_be_bytes()),
            chunk(b"BODY", &[1, 2, 3, 4]),
        ],
    );
    let dmx = open(&left, "iff_8svx");
    let s = &dmx.streams()[0].params;
    assert_eq!(s.channels, Some(1));
    assert_eq!(s.channel_layout, None);
}

/// 80-bit extended for an integral rate.
fn ext80(rate: u32) -> [u8; 10] {
    let shift = rate.leading_zeros();
    let mantissa = u64::from(rate) << (32 + shift);
    let exp = 16383 + 31 - shift as u16;
    let mut o = [0u8; 10];
    o[0..2].copy_from_slice(&exp.to_be_bytes());
    o[2..10].copy_from_slice(&mantissa.to_be_bytes());
    o
}

fn aiff(channels: u16, bits: u16, ct: Option<&[u8; 4]>, frames: u32) -> Vec<u8> {
    let mut comm = Vec::new();
    comm.extend_from_slice(&channels.to_be_bytes());
    comm.extend_from_slice(&frames.to_be_bytes());
    comm.extend_from_slice(&bits.to_be_bytes());
    comm.extend_from_slice(&ext80(44_100));
    let ft: &[u8; 4] = if let Some(ct) = ct {
        comm.extend_from_slice(ct);
        comm.extend_from_slice(&[0, 0]); // empty pstring + pad
        b"AIFC"
    } else {
        b"AIFF"
    };
    let bytes_per = usize::from(bits.div_ceil(8));
    let mut ssnd = vec![0u8; 8];
    ssnd.extend(std::iter::repeat_n(
        0u8,
        bytes_per * channels as usize * frames as usize,
    ));
    let mut chunks = vec![chunk(b"COMM", &comm), chunk(b"SSND", &ssnd)];
    if ct.is_some() {
        chunks.insert(0, chunk(b"FVER", &0xA280_5140u32.to_be_bytes()));
    }
    form(ft, &chunks)
}

#[test]
fn aiff_codec_id_follows_the_sample_point_width() {
    for (ch, bits, ct, codec, sf, layout) in [
        (
            1,
            8,
            None,
            "pcm_s8",
            SampleFormat::S8,
            Some(ChannelLayout::Mono),
        ),
        (
            2,
            12,
            None,
            "pcm_s16be",
            SampleFormat::S16,
            Some(ChannelLayout::Stereo),
        ),
        (
            2,
            16,
            Some(b"sowt"),
            "pcm_s16le",
            SampleFormat::S16,
            Some(ChannelLayout::Stereo),
        ),
        (
            3,
            24,
            None,
            "pcm_s24be",
            SampleFormat::S24,
            Some(ChannelLayout::Surround30),
        ),
        (
            1,
            20,
            Some(b"sowt"),
            "pcm_s24le",
            SampleFormat::S24,
            Some(ChannelLayout::Mono),
        ),
        (4, 32, Some(b"twos"), "pcm_s32be", SampleFormat::S32, None),
        (
            2,
            32,
            Some(b"fl32"),
            "pcm_f32be",
            SampleFormat::F32,
            Some(ChannelLayout::Stereo),
        ),
    ] {
        let bytes = aiff(ch, bits, ct, 4);
        let dmx = open(&bytes, "aiff");
        let s = &dmx.streams()[0].params;
        assert_eq!(s.codec_id, CodecId::new(codec), "{bits}-bit {ct:?}");
        assert_eq!(s.sample_format, Some(sf), "{bits}-bit {ct:?}");
        assert_eq!(s.channels, Some(ch));
        assert_eq!(s.channel_layout, layout, "{ch} channels");
        assert_eq!(s.sample_rate, Some(44_100));
        assert_eq!(
            s.bit_rate,
            Some(u64::from(ch) * sf.bytes_per_sample() as u64 * 8 * 44_100)
        );
    }
}

/// The padded wire bytes of the chunks of `form` whose id is in `ids`.
fn pick(form: &[u8], ids: &[&[u8; 4]]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 12;
    while pos + 8 <= form.len() {
        let id: [u8; 4] = form[pos..pos + 4].try_into().unwrap();
        let size = u32::from_be_bytes(form[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let end = (pos + 8 + size + (size & 1)).min(form.len());
        if ids.contains(&&id) {
            out.extend_from_slice(&form[pos..end]);
        }
        pos = end;
    }
    out
}

#[test]
fn list_prop_children_demux_with_the_shared_header() {
    // PROP ILBM { BMHD CMAP } shared by two body-only FORMs; a third
    // FORM overrides BMHD + CMAP with its own (another size).
    let a = indexed(6, 4, 4, 0);
    let other = indexed(3, 2, 4, 9);
    let prop = chunk(
        b"PROP",
        &[b"ILBM".to_vec(), pick(&a, &[b"BMHD", b"CMAP"])].concat(),
    );
    let body_only = form(b"ILBM", &[pick(&a, &[b"BODY"])]);
    let own = form(b"ILBM", &[pick(&other, &[b"BMHD", b"CMAP", b"BODY"])]);
    let bytes = chunk(
        b"LIST",
        &[b"ILBM".to_vec(), prop, body_only.clone(), body_only, own].concat(),
    );
    let (declared, run) = demux_and_decode(&bytes, "iff_ilbm");
    assert_eq!((declared.width, declared.height), (Some(6), Some(4)));
    assert_eq!(
        run.shapes,
        vec![
            (6, 4, PixelFormat::Pal8),
            (6, 4, PixelFormat::Pal8),
            (3, 2, PixelFormat::Pal8)
        ]
    );
    assert_eq!(run.generations, vec![0, 0, 1]);
    assert_eq!(run.changes, vec![2]);
    // One `StreamParams` event per mid-stream change, right before it.
    assert_eq!(run.events, vec![2]);
}

// ───────────────────────── encoder + muxer: a new FORM per shape ─────────────────────────

/// A `Pal8` picture with its palette, as a framework frame.
fn pal8_frame(w: u32, h: u32, n: usize, seed: u8) -> Frame {
    let idx: Vec<u8> = (0..w * h).map(|i| (i as usize % n) as u8).collect();
    let img = IffImage::new(
        w,
        h,
        IffPixelFormat::Pal8,
        vec![Plane::new(w as usize, idx)],
    )
    .unwrap()
    .with_palette(palette(n, seed));
    Frame::Video(img.into())
}

fn shape_event(w: u32, h: u32, pf: PixelFormat) -> Frame {
    let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(pf);
    Frame::StreamParams(Box::new(p))
}

fn video_params(w: u32, h: u32, pf: PixelFormat) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(pf);
    p
}

fn mux_ilbm(stream: &CodecParameters, pkts: &[Vec<u8>], tag: &str) -> Vec<u8> {
    let info = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1),
        duration: None,
        start_time: Some(0),
        params: stream.clone(),
    };
    let path = std::env::temp_dir().join(format!(
        "oxideav-iff-event-{tag}-{}.lbm",
        std::process::id()
    ));
    {
        let ws: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
        let mut mux = registry()
            .open_muxer("iff_ilbm", ws, std::slice::from_ref(&info))
            .unwrap();
        mux.write_header().unwrap();
        for (i, data) in pkts.iter().enumerate() {
            mux.write_packet(&pkt(data.clone(), i as i64)).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let out = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    out
}

#[test]
fn encoder_starts_a_new_form_at_a_stream_params_event_and_the_muxer_gathers_a_cat() {
    let mut enc = make_encoder(&video_params(4, 2, PixelFormat::Pal8)).unwrap();
    let declared = enc.output_params().clone();
    let mut pkts = Vec::new();
    enc.send_frame(&pal8_frame(4, 2, 4, 0)).unwrap();
    pkts.push(enc.receive_packet().unwrap().data);
    // Mid-stream: 5x5 24-bit RGB from here on.
    enc.send_frame(&shape_event(5, 5, PixelFormat::Rgb24))
        .unwrap();
    assert!(
        matches!(enc.receive_packet(), Err(Error::NeedMore)),
        "an event is no picture"
    );
    let out = enc.output_params();
    assert_eq!((out.width, out.height), (Some(5), Some(5)));
    assert_eq!(out.pixel_format, Some(PixelFormat::Rgb24));
    assert!(out.extradata.is_empty(), "no CMAP for the 24-bit output");
    for _ in 0..2 {
        enc.send_frame(&Frame::Video(rgb(5, 5).into())).unwrap();
        pkts.push(enc.receive_packet().unwrap().data);
    }
    // And back to indexed.
    enc.send_frame(&shape_event(4, 2, PixelFormat::Pal8))
        .unwrap();
    assert_eq!(enc.output_params().pixel_format, Some(PixelFormat::Pal8));
    enc.send_frame(&pal8_frame(4, 2, 4, 9)).unwrap();
    pkts.push(enc.receive_packet().unwrap().data);
    // The refined label carries the written CMAP again.
    assert_eq!(enc.output_params().extradata, palette(4, 9).to_rgb());
    for (i, p) in pkts.iter().enumerate() {
        assert_eq!(&p[..4], b"FORM", "packet {i} is a complete FORM");
    }

    // Four FORMs -> one `CAT ILBM` (all children are ILBMs).
    let file = mux_ilbm(&declared, &pkts, "cat");
    assert_eq!(&file[..4], b"CAT ");
    assert_eq!(&file[8..12], b"ILBM");
    let all = oxideav_iff::decode_all(&file).unwrap();
    assert_eq!(all.len(), 4);

    // Back through the framework: the CAT demuxes to the four FORMs and
    // the decoder announces each shape change in-band.
    let (stream, run) = demux_and_decode(&file, "iff_ilbm");
    assert_eq!((stream.width, stream.height), (Some(4), Some(2)));
    assert_eq!(
        run.shapes,
        vec![
            (4, 2, PixelFormat::Pal8),
            (5, 5, PixelFormat::Rgb24),
            (5, 5, PixelFormat::Rgb24),
            (4, 2, PixelFormat::Pal8)
        ]
    );
    assert_eq!(run.generations, vec![0, 1, 1, 2]);
    assert_eq!(run.events, vec![1, 3]);
    assert_eq!(run.changes, vec![1, 3]);
}

#[test]
fn a_single_encoded_form_is_still_muxed_verbatim() {
    let mut enc = make_encoder(&video_params(4, 2, PixelFormat::Pal8)).unwrap();
    enc.send_frame(&pal8_frame(4, 2, 4, 0)).unwrap();
    let form = enc.receive_packet().unwrap().data;
    let file = mux_ilbm(enc.output_params(), std::slice::from_ref(&form), "one");
    assert_eq!(file, form);
}

#[test]
fn a_cat_of_mixed_raster_forms_gets_a_blank_contents_type() {
    let deep = encode(
        &rgb(3, 2),
        &EncodeOptions::default().with_form(oxideav_iff::IffForm::Deep),
    )
    .unwrap();
    let odd = indexed(3, 1, 2, 0);
    let pkts = vec![odd.clone(), deep.clone(), odd];
    let file = mux_ilbm(&video_params(3, 1, PixelFormat::Pal8), &pkts, "mixed");
    // EA IFF 85 §5: four spaces when the CAT holds more than one kind.
    assert_eq!(&file[..4], b"CAT ");
    assert_eq!(&file[8..12], b"    ");
    let size = u32::from_be_bytes(file[4..8].try_into().unwrap()) as usize;
    assert_eq!(size + 8, file.len());
    assert_eq!(size % 2, 0, "children are padded to even length");
    let (_, run) = demux_and_decode(&file, "iff_ilbm");
    assert_eq!(
        run.shapes,
        vec![
            (3, 1, PixelFormat::Pal8),
            (3, 2, PixelFormat::Rgb24),
            (3, 1, PixelFormat::Pal8)
        ]
    );
    assert_eq!(run.events, vec![1, 2]);
}

#[test]
fn encoder_refuses_a_shape_it_cannot_take_and_keeps_its_configuration() {
    let mut enc = make_encoder(&video_params(4, 2, PixelFormat::Pal8)).unwrap();
    let before = format!("{:?}", enc.output_params());
    let err = enc
        .send_frame(&shape_event(4, 2, PixelFormat::Yuv420P))
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    let audio = Frame::StreamParams(Box::new(CodecParameters::audio(CodecId::new("pcm_s8"))));
    assert!(matches!(enc.send_frame(&audio), Err(Error::Unsupported(_))));
    assert_eq!(format!("{:?}", enc.output_params()), before);
    // Still encodes the original shape.
    enc.send_frame(&pal8_frame(4, 2, 4, 0)).unwrap();
    assert!(enc.receive_packet().is_ok());
}
