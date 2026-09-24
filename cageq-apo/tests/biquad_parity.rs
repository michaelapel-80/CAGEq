//! RBJ must never change: CAGEq's own APO and Equalizer APO have to keep sounding identical,
//! and every saved preset was tuned against it. The engine now takes its coefficients from
//! `cageq-biquad` (the single source of truth); this test keeps the transcription that used
//! to live in `dsp.rs` — frozen, verbatim, as it stood before the switch — and requires the
//! engine's RBJ path to reproduce it bit for bit.

use cageq_apo::dsp::{self, Band, Coeffs, FilterKind};

/// `dsp::coefficients` as it was before `cageq-biquad` (itself a transcription of
/// `cageq-core`'s `morph.rs`, matching AutoEq's `peq.py`). Do not edit.
fn legacy_rbj(band: &Band, sample_rate: f64) -> Coeffs {
    let a = 10.0_f64.powf(band.gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * band.freq_hz / sample_rate;
    let alpha = w0.sin() / (2.0 * band.q);
    let cosw = w0.cos();
    let sqrt_a = a.sqrt();

    // (a0, a1, a2, b0, b1, b2) in the RAW RBJ convention, before normalising by a0.
    let (a0, a1, a2, b0, b1, b2) = match band.kind {
        FilterKind::Peaking => (
            1.0 + alpha / a,
            -2.0 * cosw,
            1.0 - alpha / a,
            1.0 + alpha * a,
            -2.0 * cosw,
            1.0 - alpha * a,
        ),
        FilterKind::LowShelf => (
            a + 1.0 + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha,
            -2.0 * (a - 1.0 + (a + 1.0) * cosw),
            a + 1.0 + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha,
            a * (a + 1.0 - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha),
            2.0 * a * (a - 1.0 - (a + 1.0) * cosw),
            a * (a + 1.0 - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha),
        ),
        FilterKind::HighShelf => (
            a + 1.0 - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha,
            2.0 * (a - 1.0 - (a + 1.0) * cosw),
            a + 1.0 - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha,
            a * (a + 1.0 + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha),
            -2.0 * a * (a - 1.0 + (a + 1.0) * cosw),
            a * (a + 1.0 + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha),
        ),
        FilterKind::Bandpass => {
            (1.0 + alpha, -2.0 * cosw, 1.0 - alpha, alpha, 0.0, -alpha)
        }
    };

    Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

#[test]
fn rbj_coefficients_are_bit_identical_to_the_frozen_transcription() {
    let kinds = [FilterKind::Peaking, FilterKind::LowShelf, FilterKind::HighShelf, FilterKind::Bandpass];
    let mut n = 0;
    for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        for kind in kinds {
            for i in 0..40 {
                let freq_hz = 20.0 * 1000f64.powf(i as f64 / 39.0);
                for q in [0.1, 0.18, 0.5, 0.7, 1.0, 1.41, 3.0, 6.0, 20.0] {
                    for gain_db in [-20.0, -6.0, -0.01, 0.0, 0.01, 3.3, 20.0] {
                        let band = Band { kind, freq_hz, gain_db, q };
                        let (old, new) = (legacy_rbj(&band, fs), dsp::coefficients(&band, fs));
                        let bits = |c: Coeffs| [c.b0, c.b1, c.b2, c.a1, c.a2].map(f64::to_bits);
                        assert_eq!(bits(old), bits(new), "{kind:?} {freq_hz} Hz {gain_db} dB Q{q} at {fs}");
                        n += 1;
                    }
                }
            }
        }
    }
    assert!(n > 40_000);
}
