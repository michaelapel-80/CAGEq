//! Per-band regression scan: where is the matched design *worse* than RBJ (vs the analog
//! prototype), and by how much? Worst excess (matched err − rbj err) per kind × Q, over fc
//! 20 Hz–20 kHz and gains ±0.5..±20. This is how the resonant-shelf regression was found and
//! how the Q 0.85–1.1 RBJ fade was placed; `tests/design.rs` pins the result.
//!
//! `cargo run --release -p cageq-biquad --example per_band_scan`
use std::f64::consts::PI;
use cageq_biquad::{analog, design, rbj, Band, Kind, ResponseModel};
fn main() {
    let freqs: Vec<f64> = (0..150).map(|i| 20.0 * 1000f64.powf(i as f64 / 149.0)).collect();
    for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        for kind in [Kind::HighShelf, Kind::LowShelf] {
            print!("fs {fs:>6} {:<4}", kind.token());
            for q in [0.7, 0.75, 0.8, 0.85, 0.9, 0.95, 1.0, 1.05, 1.1] {
                let mut excess = 0.0f64;
                let mut at = (0.0, 0.0);
                for i in 0..24 {
                    let fc = 20.0 * 1000f64.powf(i as f64 / 23.0);
                    for g in [-20.0, -6.0, -0.5, 0.5, 6.0, 20.0] {
                        let b = Band { kind, freq_hz: fc, gain_db: g, q };
                        let worst = |c: &cageq_biquad::Coeffs| freqs.iter().map(|&f| { let w = 2.0 * PI * f / fs; (c.db(w) - analog::db(&b, fs, w)).abs() }).fold(0.0, f64::max);
                        let e = worst(&design(&b, fs, ResponseModel::AnalogMatched)) - worst(&rbj::coefficients(&b, fs));
                        if e > excess { excess = e; at = (fc, g); }
                    }
                }
                print!(" Q{q}:{excess:.3}{}", if excess > 0.01 { format!("@{:.0}/{:+}", at.0, at.1) } else { String::new() });
            }
            println!();
        }
    }
}
