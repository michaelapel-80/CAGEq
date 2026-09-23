//! `cageq_biquad`'s RBJ response equals the solver's own `Band::fr` (AutoEq's `peq.py` model,
//! evaluated in its φ form) — the precondition for switching the fit over to the shared crate.
//! Compared on the response rather than coefficients: `peq.py`'s coefficient helper is private
//! and pre-negates `a1`/`a2`, while the response is what the fit actually consumes.

use cageq_biquad::{Kind, ResponseModel};
use cageq_peq_solver::{Band, BandKind};

#[test]
fn rbj_response_matches_band_fr() {
    let fs = 48_000.0;
    let f: Vec<f64> = (0..300).map(|i| 20.0 * 1000f64.powf(i as f64 / 299.0)).collect();
    let kinds = [(BandKind::Peaking, Kind::Peaking), (BandKind::LowShelf, Kind::LowShelf), (BandKind::HighShelf, Kind::HighShelf)];
    for (solver_kind, kind) in kinds {
        for i in 0..30 {
            let fc = 20.0 * 1000f64.powf(i as f64 / 29.0);
            for q in [0.18, 0.4, 0.7, 1.41, 6.0] {
                for gain in [-20.0, -3.0, 0.5, 12.0] {
                    let solver = Band { fc, q, gain, ..Band::fixed_fc_q(solver_kind, fc, q) }.fr(&f, fs);
                    let c = cageq_biquad::design(&cageq_biquad::Band { kind, freq_hz: fc, gain_db: gain, q }, fs, ResponseModel::Rbj);
                    for (k, &freq) in f.iter().enumerate() {
                        let shared = c.db(2.0 * std::f64::consts::PI * freq / fs);
                        // 1e-7 dB: the same φ form with different operation grouping differs by ~1e-9 dB at the
                        // deepest points (a −20 dB, 20 Hz bell) — float rounding, not a model difference.
                        assert!((shared - solver[k]).abs() < 1e-7, "{kind:?} {fc} Hz {gain} dB Q{q} at {freq} Hz: {shared} vs {}", solver[k]);
                    }
                }
            }
        }
    }
}
