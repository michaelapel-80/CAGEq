//! Scratch probe (Stage 0): generalised Vicanek shelf vs the others, by Q — worst |error| dB
//! over fc 20 Hz–20 kHz and gains ±0.1..±20 ([n] = design failures of 480).
use std::f64::consts::PI;
use cageq_biquad::matched::{self, Con::{Slope as S, Value as V}};
use cageq_biquad::{analog, rbj, Band, Coeffs, Kind};

fn main() {
    for kind in [Kind::HighShelf, Kind::LowShelf] {
        for fs in [44_100.0, 48_000.0, 96_000.0] {
            let freqs: Vec<f64> = (0..400).map(|i| 20.0 * 1000f64.powf(i as f64 / 399.0)).collect();
            let err = |c: &Coeffs, b: &Band| freqs.iter().map(|&f| { let w = 2.0 * PI * f / fs; (c.db(w) - analog::db(b, fs, w)).abs() }).fold(0.0, f64::max);
            println!("{kind:?} fs {fs}:        rbj   constrained   ivantsov2   vicanek-gen   (+ minphase/stable violations)");
            for q in [0.1, 0.3, 0.5, 0.6, 0.7, 0.7071, 0.8, 1.0, 1.41, 2.0, 4.0, 10.0, 20.0] {
                let mut w = [0.0f64; 4];
                let mut f = [0usize; 4];
                let mut bad = 0;
                for i in 0..40 {
                    let fc = 20.0 * 1000f64.powf(i as f64 / 39.0);
                    for g in [-20.0, -12.0, -6.0, -3.0, -1.0, -0.1, 0.1, 1.0, 3.0, 6.0, 12.0, 20.0] {
                        let b = Band { kind, freq_hz: fc, gain_db: g, q };
                        let ds = [Ok(rbj::coefficients(&b, fs)), matched::constrained(&b, fs, [V(1.0), S(1.0), V(0.5)]), matched::ivantsov(&b, fs, 2.0), matched::vicanek_shelf(&b, fs)];
                        for (i, d) in ds.iter().enumerate() {
                            match d {
                                Ok(c) => {
                                    w[i] = w[i].max(err(c, &b));
                                    if i == 3 && (c.pole_radius() >= 1.0 || c.zero_radius() >= 1.0) { bad += 1; }
                                }
                                Err(_) => f[i] += 1,
                            }
                        }
                    }
                }
                print!("  Q{q:<6}");
                for i in 0..4 { print!(" {:>12}", format!("{:.2}[{}]", w[i], f[i])); }
                println!("   {bad}");
            }
        }
    }
}
