//! `AnimMuxer::with_operation` — the container-level muxer can emit
//! any delta operation the crate encodes (op-0/1/2/3/4/5/7/8), not
//! just the op-5 default. Each variant is verified by inspecting the
//! first delta frame's ANHD operation byte and by a pixel + timing
//! round-trip through `parse_anim`.

use oxideav_core::{
    CodecId, CodecParameters, MediaType, Packet, PixelFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_iff::anim::{parse_anim, AnimMuxer, AnimMuxerOp};

fn video_stream(width: u32, height: u32) -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("rawvideo"));
    params.media_type = MediaType::Video;
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(PixelFormat::Rgba);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 60),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// 32-px-wide stripes so `row_bytes == 4` satisfies the op-4/op-7
/// long-data width requirement.
fn stripes(phase: usize) -> Vec<u8> {
    let colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    let mut rgba = Vec::with_capacity(32 * 4 * 4);
    for _y in 0..4 {
        for x in 0..32usize {
            let c = colors[(x + phase) % 4];
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    rgba
}

fn mux_with(op: AnimMuxerOp) -> Vec<u8> {
    let stream = video_stream(32, 4);
    let buf: Vec<u8> = Vec::new();
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(buf));
    // Keep a handle on the cursor: write to a temp file instead for
    // simplicity of retrieval.
    drop(ws);
    let path = std::env::temp_dir().join(format!(
        "oxideav-iff-animmuxops-{}-{op:?}.anim",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = AnimMuxer::new(ws, std::slice::from_ref(&stream))
            .unwrap()
            .with_operation(op);
        use oxideav_core::Muxer;
        mux.write_header().unwrap();
        for i in 0..3i64 {
            let mut pkt = Packet::new(0, stream.time_base, stripes(i as usize));
            pkt.pts = Some(i * 7);
            pkt.dts = Some(i * 7);
            pkt.duration = Some(7);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// The operation byte of the first delta frame's ANHD (the seed FORM
/// ILBM carries no ANHD in the muxer's output).
fn first_anhd_operation(form: &[u8]) -> u8 {
    let at = form
        .windows(4)
        .position(|w| w == b"ANHD")
        .expect("output has a delta ANHD");
    form[at + 8]
}

fn check(op: AnimMuxerOp, expect_op_byte: u8) {
    let bytes = mux_with(op);
    assert_eq!(
        first_anhd_operation(&bytes),
        expect_op_byte,
        "{op:?}: ANHD operation byte"
    );
    let dec = parse_anim(&bytes).unwrap();
    assert_eq!(dec.frames.len(), 3, "{op:?}");
    for (i, frame) in dec.frames.iter().enumerate() {
        assert_eq!(frame.rgba, stripes(i), "{op:?}: frame {i} pixels");
    }
    // Packet durations (7 jiffy ticks at 1/60) become the *next*
    // frame's reltime, exactly as with the op-5 default.
    assert_eq!(dec.frame_timing[1].rel_time, 7, "{op:?}: reltime");
    assert_eq!(dec.frame_timing[2].rel_time, 7, "{op:?}: reltime");
}

#[test]
fn muxer_op0_literal() {
    check(AnimMuxerOp::Op0, 0);
}

#[test]
fn muxer_op1_xor() {
    check(AnimMuxerOp::Op1, 1);
}

#[test]
fn muxer_op2_long_delta() {
    check(AnimMuxerOp::Op2, 2);
}

#[test]
fn muxer_op3_short_delta() {
    check(AnimMuxerOp::Op3, 3);
}

#[test]
fn muxer_op4_both_widths() {
    check(AnimMuxerOp::Op4 { long_data: false }, 4);
    check(AnimMuxerOp::Op4 { long_data: true }, 4);
}

#[test]
fn muxer_op5_is_the_default() {
    assert_eq!(AnimMuxerOp::default(), AnimMuxerOp::Op5);
    check(AnimMuxerOp::Op5, 5);
}

#[test]
fn muxer_op7_both_widths() {
    check(AnimMuxerOp::Op7 { long_data: false }, 7);
    check(AnimMuxerOp::Op7 { long_data: true }, 7);
}

#[test]
fn muxer_op8_both_widths() {
    check(AnimMuxerOp::Op8 { long_data: false }, 8);
    check(AnimMuxerOp::Op8 { long_data: true }, 8);
}
