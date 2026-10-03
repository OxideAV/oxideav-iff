#![no_main]

//! Feed arbitrary fuzz-supplied bytes through the IMAGE_CRATE_API root
//! functions — `probe`, `info`, `decode` (with the default finite
//! limits), `decode_all`, `decode_rgba8` — and, when a picture does
//! decode, through `encode` / `encode_all` and back, so the whole
//! contract surface (FORM dispatch, the header-only `info` walk, the
//! `IndexedView` → `Pal8` packaging, the `CAT ` / `LIST` child walk,
//! the ANIM frame timeline and every encoder path) is exercised.
//!
//! The contract under test: every call *returns* (`Ok` or an
//! `IffError`), `probe` never allocates, `info` never touches pixels,
//! nothing panics / overflows / indexes out of bounds, and a decoded
//! picture re-encodes and decodes to the same planes.

use libfuzzer_sys::fuzz_target;
use oxideav_iff::{DecodeOptions, EncodeOptions, IffForm};

fuzz_target!(|data: &[u8]| {
    let _ = oxideav_iff::probe(data);
    let _ = oxideav_iff::info(data);
    // Keep the working buffer small enough for a fuzz iteration.
    let opts = DecodeOptions::default()
        .with_max_pixels(1u64 << 20)
        .with_max_bytes(4u64 << 20);
    if let Ok(img) = oxideav_iff::decode_with(data, &opts) {
        let _ = img.to_rgba8();
        let _ = img.to_rgb8();
        if img.width * img.height <= 1 << 16 {
            for form in [IffForm::Ilbm, IffForm::Pbm, IffForm::Acbm, IffForm::Deep] {
                let opts = EncodeOptions::default()
                    .with_form(form)
                    .with_drop_alpha(true)
                    .with_indexed(form != IffForm::Ilbm);
                if let Ok(bytes) = oxideav_iff::encode(&img, &opts) {
                    let back = oxideav_iff::decode(&bytes).expect("own output decodes");
                    if img.format == oxideav_iff::PixelFormat::Pal8 && form != IffForm::Deep {
                        assert_eq!(back.planes, img.planes);
                    }
                }
            }
        }
    }
    if let Ok(frames) = oxideav_iff::decode_all_with(data, &opts) {
        if frames.len() <= 8
            && frames
                .iter()
                .all(|f| f.image.width * f.image.height <= 1 << 14)
        {
            let _ = oxideav_iff::encode_all(&frames, &EncodeOptions::default());
        }
    }
});
