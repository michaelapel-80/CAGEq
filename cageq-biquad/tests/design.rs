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
    for kind in Kind::ALL {
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
        for kind in Kind::ALL {
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
        for kind in Kind::ALL {
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
