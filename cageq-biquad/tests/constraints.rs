//! Each design is checked against the constraints it *claims* to satisfy, on the realised
//! digital response — not against its own algebra. A sign or pre-warp slip in the
//! derivation shows up here as a violated constraint rather than as a plausible-looking
//! curve in the spike report.

use std::f64::consts::PI;

use cageq_biquad::matched::{self, MidPoint};
use cageq_biquad::{analog, rbj, Band, Kind};

const FS: f64 = 48_000.0;

fn band(kind: Kind, freq_hz: f64, gain_db: f64, q: f64) -> Band {
    Band { kind, freq_hz, gain_db, q }
}

fn close(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a - b).abs() <= tol, "{what}: {a} vs {b} (tol {tol})");
}

/// The analog prototypes reproduce their defining gains: peak/shelf plateaus, and RBJ's
/// Q-at-midpoint convention for the peaking bandwidth.
#[test]
fn analog_prototypes_have_their_defining_gains() {
    let pk = band(Kind::Peaking, 1000.0, 6.0, 2.0);
    let w0 = pk.w0(FS);
    close(analog::db(&pk, FS, w0), 6.0, 1e-12, "peak gain");
    close(analog::db(&pk, FS, 1e-6), 0.0, 1e-9, "peaking DC");
    let x = (-1.0 / 2.0 + (1.0 / 4.0 + 4.0f64).sqrt()) / 2.0;
    close(analog::db(&pk, FS, x * w0), 3.0, 1e-9, "midpoint gain at the RBJ bandwidth edge");

    let hs = band(Kind::HighShelf, 1000.0, 6.0, 0.7);
    close(analog::db(&hs, FS, hs.w0(FS)), 3.0, 1e-9, "shelf midpoint");
    close(analog::db(&hs, FS, 1e-6), 0.0, 1e-6, "high shelf DC");
    let ls = band(Kind::LowShelf, 1000.0, -6.0, 0.7);
    close(analog::db(&ls, FS, 1e-6), -6.0, 1e-6, "low shelf DC");
    close(analog::db(&ls, FS, ls.w0(FS)), -3.0, 1e-9, "low shelf midpoint");
}

/// At a low corner frequency warping is negligible, so the RBJ filter must already sit on the
/// analog prototype — this is what makes the prototype a fair reference for RBJ's own Q.
#[test]
fn rbj_matches_the_prototype_far_below_nyquist() {
    for kind in Kind::ALL {
        let b = band(kind, 200.0, 6.0, 1.0);
        let c = rbj::coefficients(&b, FS);
        // The band-pass is the one exception worth naming: its BLT zero at Nyquist (the
        // analog one's is at infinity) already costs ~0.012 dB of skirt five octaves down.
        let tol = if kind == Kind::Bandpass { 0.02 } else { 0.01 };
        for f in [20.0, 100.0, 200.0, 400.0, 1000.0] {
            let w = 2.0 * PI * f / FS;
            close(c.db(w), analog::db(&b, FS, w), tol, &format!("{kind:?} at {f} Hz"));
        }
    }
}

/// The prescribed design meets all of its constraints: gain `g` at `ω0`, an extremum there,
/// the prototype's Nyquist gain, and unity at DC.
#[test]
fn prescribed_peaking_meets_its_constraints() {
    for (fc, gain, q) in [(15_000.0, 10.0, 0.5623), (10_000.0, -6.0, 1.0), (18_000.0, -10.0, 0.4), (3_000.0, 4.0, 3.0)] {
        let b = band(Kind::Peaking, fc, gain, q);
        let c = matched::prescribed(&b, FS).unwrap();
        let w0 = b.w0(FS);
        close(c.db(w0), gain, 1e-9, "gain at w0");
        let h = 1e-5;
        let slope = (c.db(w0 + h) - c.db(w0 - h)) / (2.0 * h);
        assert!(slope.abs() < 1e-3, "extremum at w0: slope {slope} for {b:?}");
        close(c.db(PI), analog::db(&b, FS, PI), 1e-9, "Nyquist gain");
        close(c.db(0.0), 0.0, 1e-9, "DC gain");
        assert!(c.pole_radius() < 1.0 && c.zero_radius() < 1.0, "stable + minimum phase: {c:?}");
    }
}

#[test]
fn prescribed_bandpass_meets_its_constraints() {
    for (fc, q) in [(15_000.0, 1.0), (500.0, 8.0), (12_000.0, 0.7)] {
        let b = band(Kind::Bandpass, fc, 0.0, q);
        let c = matched::prescribed(&b, FS).unwrap();
        let w0 = b.w0(FS);
        close(c.db(w0), 0.0, 1e-6, "unity peak");
        close(c.db(PI), analog::db(&b, FS, PI), 1e-9, "Nyquist gain");
        assert!(c.pole_radius() < 1.0, "stable: {c:?}");
    }
}

/// The matched design hits the prototype exactly at its three match points, for every kind.
#[test]
fn mz_matches_the_prototype_at_its_three_points() {
    for kind in Kind::ALL {
        for (fc, gain, q) in [(10_000.0, 6.0, 0.7), (12_000.0, -6.0, 1.0), (200.0, 4.0, 2.0)] {
            let b = band(kind, fc, gain, q);
            let c = matched::mz(&b, FS, MidPoint::W0).unwrap();
            let w0 = b.w0(FS);
            // DC is a match point too, except for the band-pass whose DC gain is exactly
            // zero on both sides (-inf dB, so not a comparable number).
            let dc: &[f64] = if kind == Kind::Bandpass { &[] } else { &[0.0] };
            for &w in dc.iter().chain(&[w0, PI]) {
                close(c.db(w), analog::db(&b, FS, w), 1e-6, &format!("{kind:?} {fc} at w={w}"));
            }
            assert!(c.pole_radius() < 1.0, "stable: {c:?}");
        }
    }
}

/// The general value/slope solver, given the thesis's own constraint set, reproduces the
/// thesis's peaking design — the u-domain linearisation is the same construction, derived
/// independently, so agreement checks both.
#[test]
fn constrained_reproduces_the_thesis_peaking() {
    use cageq_biquad::matched::{rbj_peak_bandwidth_point, Con};
    for (fc, gain, q) in [(15_000.0, 10.0, 0.5623), (10_000.0, -6.0, 1.0), (18_000.0, -10.0, 0.4), (3_000.0, 4.0, 3.0), (200.0, 12.0, 0.7)] {
        let b = band(Kind::Peaking, fc, gain, q);
        let thesis = matched::prescribed(&b, FS).unwrap();
        let general = matched::constrained(&b, FS, [Con::Value(1.0), Con::Slope(1.0), Con::Value(rbj_peak_bandwidth_point(q))]).unwrap();
        for f in [20.0, 200.0, 1000.0, 5000.0, 10_000.0, 15_000.0, 20_000.0, 23_900.0] {
            let w = 2.0 * PI * f / FS;
            close(general.db(w), thesis.db(w), 1e-7, &format!("{b:?} at {f} Hz"));
        }
    }
}

/// Ivantsov maps numerator and denominator quadratics separately, so every design is stable
/// and minimum phase by construction — pinned here over the whole UI range, all kinds, since
/// that guarantee is the reason to keep it as the always-available fallback.
#[test]
fn ivantsov_is_stable_and_minimum_phase_everywhere() {
    for fs in [44_100.0, 48_000.0, 96_000.0] {
        for kind in [Kind::Peaking, Kind::LowShelf, Kind::HighShelf] {
            for i in 0..30 {
                let fc = 20.0 * 1000f64.powf(i as f64 / 29.0);
                for q in [0.1, 0.5, 0.7, 2.0, 20.0] {
                    for gain in [-20.0, -0.01, 0.01, 20.0] {
                        let b = band(kind, fc, gain, q);
                        let c = matched::ivantsov(&b, fs, 2.0).unwrap();
                        assert!(c.pole_radius() < 1.0 && c.zero_radius() < 1.0, "{b:?} at {fs}: {c:?}");
                    }
                }
            }
        }
    }
}
