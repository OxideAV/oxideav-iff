//! 80-bit IEEE 754 extended-precision floating-point decoder.
//!
//! AIFF's `COMM.sampleRate` is a 10-byte (80-bit) big-endian
//! IEEE 754 extended-precision float — the Motorola 68881 / SANE
//! "extended" type — not a 32-bit integer. The clean-room layout
//! summary in `docs/audio/aiff/aiff-aifc-format.md` §2.1 fixes:
//!
//! ```text
//! byte 0 .. 1 : sign bit + 15-bit biased exponent  (big-endian)
//! byte 2 .. 9 : 64-bit mantissa (big-endian)
//!                bit 63        = explicit integer bit
//!                bits 62 ..  0 = fraction
//! ```
//!
//! Unlike 32-bit `f32` and 64-bit `f64` the integer bit is
//! **explicit**: a normalised number has it set to 1, denormals
//! have it 0. The exponent bias is `16_383`. The value is
//!
//! ```text
//! (-1)^sign * mantissa_f64 * 2^(exponent - 16_383 - 63)
//! ```
//!
//! where `mantissa_f64` is the full 64-bit unsigned mantissa
//! reinterpreted as a real value.
//!
//! Decoding to `f64` is lossy for the very high-magnitude end of
//! the 80-bit range (the extra ~12 bits of mantissa precision
//! don't fit in f64), but sample rates we care about
//! (8 000 .. 192 000 Hz and friends) all round-trip exactly.

use crate::aiff::error::{AiffError, Result};

/// Decode a 10-byte 80-bit IEEE 754 extended-precision float into
/// an [`f64`], the way macOS / AIFF stores `sampleRate`.
///
/// Returns the decoded value. The result is `0.0` for an exact
/// zero encoding (sign bit + all other bits zero), respects sign,
/// and uses denormals when the exponent field is zero. Infinities
/// (exponent all-ones, mantissa zero) decode to [`f64::INFINITY`]
/// (with sign); NaNs (exponent all-ones, mantissa non-zero) decode
/// to [`f64::NAN`].
///
/// # Panics
///
/// Never panics — the input is exactly 10 bytes by type.
pub fn decode_extended(bytes: [u8; 10]) -> f64 {
    let sign = bytes[0] & 0x80 != 0;
    let exponent = ((bytes[0] as u16 & 0x7f) << 8) | bytes[1] as u16;
    let mantissa = u64::from_be_bytes([
        bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9],
    ]);

    // Special cases: exponent all-ones.
    if exponent == 0x7fff {
        if mantissa == 0 || mantissa == 0x8000_0000_0000_0000 {
            // Infinity. The Motorola encoding is inf when the
            // integer bit (bit 63) is set or clear with all
            // fraction bits zero; treat both as +/- inf.
            return if sign {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        return f64::NAN;
    }

    // Zero or denormal: exponent == 0.
    if exponent == 0 && mantissa == 0 {
        return if sign { -0.0 } else { 0.0 };
    }

    // General case: extract the mantissa as a real in [0, 2^64)
    // and apply the binary exponent. The 80-bit format stores the
    // integer bit EXPLICITLY (bit 63 of mantissa), so the value is
    //
    //     m / 2^63  *  2^(exponent - 16_383)      (normalised)
    //   = m        *  2^(exponent - 16_383 - 63)
    //
    // For denormals (exponent == 0, mantissa != 0) the integer bit
    // is zero and the exponent is treated as 1 (so the formula's
    // `exponent - 16_383 - 63` term uses `1 - 16_383 - 63`); the
    // same multiplicative form covers both cases when we use
    // `exponent.max(1)`.
    let eff_exponent = if exponent == 0 {
        1_i32
    } else {
        exponent as i32
    };
    let mantissa_f = mantissa as f64;
    let scale_exp = eff_exponent - 16_383 - 63;
    let mut value = mantissa_f * pow2(scale_exp);
    if sign {
        value = -value;
    }
    value
}

/// Decode a 10-byte 80-bit IEEE 754 extended-precision float and
/// validate it as a positive, finite sample rate.
///
/// Returns the rate (in Hz) on success, or
/// [`AiffError::InvalidSampleRate`] when the encoded value is NaN,
/// infinite, or `<= 0`.
pub fn decode_sample_rate(bytes: [u8; 10]) -> Result<f64> {
    let v = decode_extended(bytes);
    if !v.is_finite() || v <= 0.0 {
        return Err(AiffError::InvalidSampleRate);
    }
    Ok(v)
}

/// `2.0_f64.powi(n)` without going through libm — keeps the
/// crate free of any C-runtime float dependency for small builds.
fn pow2(exp: i32) -> f64 {
    // f64 binary exponent range is roughly -1074..=1023; for the
    // sample-rate use case `exp` lives in a tiny window centred on
    // ~-43 (rate ~= mantissa * 2^-43 when exponent ~= 16_383 + 20).
    // We still implement the general case so that decode_extended
    // is self-contained.
    if exp >= 1024 {
        return f64::INFINITY;
    }
    if exp <= -1075 {
        return 0.0;
    }
    // Build a normal f64 directly from the biased exponent when in
    // range; falls back to repeated halving for the subnormal tail.
    if exp >= -1022 {
        let biased = (exp + 1023) as u64;
        let bits = biased << 52;
        f64::from_bits(bits)
    } else {
        // Subnormal: start from the smallest normal and divide.
        let mut v = f64::from_bits(1_u64 << 52); // = 2^-1022
        let mut e = -1022;
        while e > exp {
            v *= 0.5;
            e -= 1;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode an `f64` *back* into 80-bit extended so the round-trip
    /// tests don't need a pre-computed table. The encoder is a
    /// separate, simpler routine — it's used here only as a test
    /// helper, not exported.
    fn encode_extended(v: f64) -> [u8; 10] {
        if v.is_nan() {
            // Quiet NaN with the integer bit set.
            return [0x7f, 0xff, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        }
        if v == 0.0 {
            let mut o = [0u8; 10];
            if v.is_sign_negative() {
                o[0] = 0x80;
            }
            return o;
        }
        let sign = v.is_sign_negative();
        let mag = v.abs();
        if mag.is_infinite() {
            let mut o = [0u8; 10];
            o[0] = if sign { 0xff } else { 0x7f };
            o[1] = 0xff;
            return o;
        }
        // Pull the f64 fields out and re-bias for the extended format.
        let bits = mag.to_bits();
        let f64_exp = ((bits >> 52) & 0x7ff) as i32;
        let f64_frac = bits & 0x000f_ffff_ffff_ffff;
        let (mantissa_64, exp_unbiased): (u64, i32) = if f64_exp == 0 {
            // f64 denormal -> renormalise into 80-bit normal.
            let lead = f64_frac.leading_zeros() as i32 - 11;
            let mantissa = f64_frac << (12 + lead);
            let true_exp = -1022 - lead;
            (mantissa, true_exp)
        } else {
            // Normal: include the implicit f64 leading 1.
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

    #[test]
    fn decode_zero_is_zero() {
        assert_eq!(decode_extended([0; 10]), 0.0);
    }

    #[test]
    fn decode_neg_zero_is_neg_zero() {
        let mut b = [0u8; 10];
        b[0] = 0x80;
        let v = decode_extended(b);
        assert_eq!(v, 0.0);
        assert!(v.is_sign_negative());
    }

    #[test]
    fn decode_one() {
        // 1.0 in 80-bit extended: sign=0, exponent=16383 (0x3fff),
        // mantissa = 0x8000_0000_0000_0000 (integer bit set, no
        // fraction).
        let bytes = [0x3f, 0xff, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_extended(bytes), 1.0);
    }

    #[test]
    fn decode_two() {
        // 2.0: exponent = 16384 (0x4000), mantissa = 0x8000_..._0000.
        let bytes = [0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_extended(bytes), 2.0);
    }

    #[test]
    fn decode_half() {
        // 0.5: exponent = 16382 (0x3ffe), mantissa = 0x8000_..._0000.
        let bytes = [0x3f, 0xfe, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_extended(bytes), 0.5);
    }

    #[test]
    fn decode_negative_one() {
        let bytes = [0xbf, 0xff, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_extended(bytes), -1.0);
    }

    #[test]
    fn decode_44100_hz() {
        // 44100.0 — the canonical CD sample rate, the value every
        // 16-bit-stereo AIFF in the wild carries in COMM.sampleRate.
        // Built from encode_extended() for documentation purposes;
        // the same bytes appear in audio files we've inspected.
        let bytes = encode_extended(44100.0);
        assert_eq!(decode_extended(bytes), 44100.0);
    }

    #[test]
    fn decode_round_trip_common_rates() {
        for rate in [
            8_000.0, 11_025.0, 16_000.0, 22_050.0, 24_000.0, 32_000.0, 44_100.0, 48_000.0,
            88_200.0, 96_000.0, 176_400.0, 192_000.0,
        ] {
            let enc = encode_extended(rate);
            let dec = decode_extended(enc);
            assert!(
                (dec - rate).abs() < 1e-9,
                "rate {rate} decoded to {dec} via {enc:?}"
            );
        }
    }

    #[test]
    fn decode_infinity() {
        let bytes = [0x7f, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_extended(bytes), f64::INFINITY);
    }

    #[test]
    fn decode_neg_infinity() {
        let bytes = [0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_extended(bytes), f64::NEG_INFINITY);
    }

    #[test]
    fn decode_nan() {
        let bytes = [0x7f, 0xff, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(decode_extended(bytes).is_nan());
    }

    #[test]
    fn sample_rate_rejects_nan() {
        let bytes = [0x7f, 0xff, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            decode_sample_rate(bytes),
            Err(AiffError::InvalidSampleRate)
        ));
    }

    #[test]
    fn sample_rate_rejects_inf() {
        let bytes = [0x7f, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            decode_sample_rate(bytes),
            Err(AiffError::InvalidSampleRate)
        ));
    }

    #[test]
    fn sample_rate_rejects_zero() {
        let bytes = [0u8; 10];
        assert!(matches!(
            decode_sample_rate(bytes),
            Err(AiffError::InvalidSampleRate)
        ));
    }

    #[test]
    fn sample_rate_rejects_negative() {
        let bytes = encode_extended(-48_000.0);
        assert!(matches!(
            decode_sample_rate(bytes),
            Err(AiffError::InvalidSampleRate)
        ));
    }

    #[test]
    fn sample_rate_accepts_44_1k() {
        let bytes = encode_extended(44_100.0);
        assert_eq!(decode_sample_rate(bytes).unwrap(), 44_100.0);
    }
}
