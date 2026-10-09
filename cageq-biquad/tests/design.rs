//! The contract of [`cageq_biquad::design`] — what every consumer (APO, fit, chart) relies on.

use std::f64::consts::PI;

use cageq_biquad::{analog, design, matched, rbj, Band, Kind, ResponseModel};

/// Every rate CAGEq may be handed by a device.
const RATES: [f64; 6] = [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0];

fn log_space(n: usize, lo: f64, hi: f64) -> Vec<f64> {
    (0..n).map(|i| lo * (hi / lo).powf(i as f64 / (n - 1) as f64)).collect()
}

/// The UI's full parameter range (Fc 20 Hz–20 kHz, Q 0.1–20, gain ±20 dB, including the
/// near-0 dB values the optimizer passes through).
fn ui_bands(kind: Kind) -> impl Iterator<Item = Band> {
    let fcs = log_space(24, 20.0, 20_000.0);
    let qs = log_space(10, 0.1, 20.0);
    let gains = [-20.0, -6.0, -0.5, -1e-4, 1e-4, 0.5, 6.0, 20.0];
    fcs.into_iter().flat_map(move |freq_hz| {
        let gains = gains;
        qs.clone().into_iter().flat_map(move |q| gains.into_iter().map(move |gain_db| Band { kind, freq_hz, gain_db, q }))
    })
}

/// `Rbj` is exactly today's filter — no model switch can change what an existing preset
/// sounds like on the default.
#[test]
fn rbj_model_is_exactly_rbj() {
    for kind in Kind::MATCHED {
        for b in ui_bands(kind) {
            assert_eq!(design(&b, 48_000.0, ResponseModel::Rbj), rbj::coefficients(&b, 48_000.0));
        }
    }
}

/// Inside the UI's range the matched design never needs its RBJ fallback, at any rate — the
/// fallback exists for parameters no UI path produces.
#[test]
fn matched_never_falls_back_inside_the_ui_range() {
    for fs in RATES {
        for kind in Kind::MATCHED {
            for b in ui_bands(kind) {
                if let Err(e) = matched::design(&b, fs) {
                    panic!("{b:?} at {fs} Hz fell back: {e:?}");
                }
            }
        }
    }
}

/// Outside the domain (Fc at/near Nyquist, nonsense input) the result is still a safe filter.
#[test]
fn out_of_domain_falls_back_to_rbj() {
    let b = Band { kind: Kind::Peaking, freq_hz: 23_000.0, gain_db: 6.0, q: 1.0 };
    assert_eq!(design(&b, 48_000.0, ResponseModel::AnalogMatched), rbj::coefficients(&b, 48_000.0));
    let bad = Band { kind: Kind::HighShelf, freq_hz: f64::NAN, gain_db: 6.0, q: 0.7 };
    assert!(matched::design(&bad, 48_000.0).is_err());
}

/// The point of the model, as a band-by-band guarantee: never further from the analog
/// prototype than RBJ (0.01 dB of slack for rounding), at any rate, over 20 Hz–20 kHz.
#[test]
fn matched_is_never_worse_than_rbj() {
    let freqs = log_space(150, 20.0, 20_000.0);
    for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        for kind in Kind::MATCHED {
            for b in ui_bands(kind) {
                let worst = |c: &cageq_biquad::Coeffs| {
                    freqs.iter().map(|&f| { let w = 2.0 * PI * f / fs; (c.db(w) - analog::db(&b, fs, w)).abs() }).fold(0.0, f64::max)
                };
                let (m, r) = (worst(&design(&b, fs, ResponseModel::AnalogMatched)), worst(&rbj::coefficients(&b, fs)));
                assert!(m <= r + 0.01, "{b:?} at {fs}: matched {m:.3} dB vs rbj {r:.3} dB");
            }
        }
    }
}

/// A high-pass section is RBJ in every model — there is no matched design for it (see
/// `design`'s doc) — and it is the analog Butterworth-style prototype: -3 dB at Fc for Q 1/√2,
/// a 12 dB/oct stopband, and a flat passband, at every rate.
#[test]
fn high_pass_is_rbj_in_every_model_and_follows_its_prototype() {
    for fs in RATES {
        for (fc, q) in [(20.0, std::f64::consts::FRAC_1_SQRT_2), (40.0, 0.5412), (80.0, 2.5629)] {
            let b = Band { kind: Kind::HighPass, freq_hz: fc, gain_db: 0.0, q };
            let rbj = rbj::coefficients(&b, fs);
            assert_eq!(design(&b, fs, ResponseModel::AnalogMatched), rbj);
            assert!(rbj.pole_radius() < 1.0, "stable: {b:?} at {fs}");
            for f in [fc / 8.0, fc / 2.0, fc, fc * 2.0, 1000.0, 10_000.0] {
                let w = 2.0 * PI * f / fs;
                assert!((rbj.db(w) - analog::db(&b, fs, w)).abs() < 0.01, "{b:?} at {f} Hz, {fs}");
            }
        }
        let butterworth = Band { kind: Kind::HighPass, freq_hz: 20.0, gain_db: 0.0, q: std::f64::consts::FRAC_1_SQRT_2 };
        let c = rbj::coefficients(&butterworth, fs);
        assert!((c.db(2.0 * PI * 20.0 / fs) + 3.0103).abs() < 1e-3, "-3 dB at Fc");
        assert!(c.db(2.0 * PI * 1000.0 / fs).abs() < 1e-3, "flat passband");
    }
}

/// The slopes the app offers are cascades of sections at Butterworth Qs; at 48 dB/oct a 20 Hz
/// corner takes 16 Hz down by 15.6 dB — the number the feature was designed around.
#[test]
fn a_butterworth_cascade_has_the_textbook_stopband() {
    let order = 8;
    let qs: Vec<f64> = (1..=order / 2).map(|k| 1.0 / (2.0 * ((2 * k - 1) as f64 * PI / (2 * order) as f64).cos())).collect();
    let fs = 48_000.0;
    let db = |f: f64| qs.iter().map(|&q| rbj::coefficients(&Band { kind: Kind::HighPass, freq_hz: 20.0, gain_db: 0.0, q }, fs).db(2.0 * PI * f / fs)).sum::<f64>();
    assert!((db(20.0) + 3.0103).abs() < 0.01, "-3 dB at Fc: {}", db(20.0));
    assert!((db(16.0) + 15.6).abs() < 0.1, "16 Hz: {}", db(16.0));
    assert!(db(100.0).abs() < 0.01, "flat above: {}", db(100.0));
}
