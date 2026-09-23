//! `cageq_biquad`'s RBJ path is the APO's `dsp::coefficients`, bit for bit — the precondition
//! for switching the APO over to the shared crate without changing what anyone hears.

use cageq_apo::dsp::{self, FilterKind};
use cageq_biquad::{Kind, ResponseModel};

#[test]
fn rbj_coefficients_are_bit_identical() {
    let kinds = [
        (FilterKind::Peaking, Kind::Peaking),
        (FilterKind::LowShelf, Kind::LowShelf),
        (FilterKind::HighShelf, Kind::HighShelf),
        (FilterKind::Bandpass, Kind::Bandpass),
    ];
    let mut n = 0;
    for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        for (apo_kind, kind) in kinds {
            for i in 0..40 {
                let freq_hz = 20.0 * 1000f64.powf(i as f64 / 39.0);
                for q in [0.1, 0.18, 0.5, 0.7, 1.0, 1.41, 3.0, 6.0, 20.0] {
                    for gain_db in [-20.0, -6.0, -0.01, 0.0, 0.01, 3.3, 20.0] {
                        let apo = dsp::coefficients(&dsp::Band { kind: apo_kind, freq_hz, gain_db, q }, fs);
                        let shared = cageq_biquad::design(&cageq_biquad::Band { kind, freq_hz, gain_db, q }, fs, ResponseModel::Rbj);
                        let bits = |c: [f64; 5]| c.map(f64::to_bits);
                        assert_eq!(
                            bits([apo.b0, apo.b1, apo.b2, apo.a1, apo.a2]),
                            bits([shared.b0, shared.b1, shared.b2, shared.a1, shared.a2]),
                            "{kind:?} {freq_hz} Hz {gain_db} dB Q{q} at {fs}"
                        );
                        n += 1;
                    }
                }
            }
        }
    }
    assert!(n > 40_000);
}
