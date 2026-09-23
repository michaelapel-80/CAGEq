//! Scratch probe (Stage 0b): is the Q = 0.5 degeneracy structural? Sweep the two match
//! points (as multiples of fc) at fc 1 kHz, +1 dB, and count feasible designs per Q.
use cageq_biquad::{matched, Band, Kind};
fn main() {
    let fs = 48_000.0;
    let fc = 1000.0 / (fs / 2.0);
    let ks: Vec<f64> = (0..25).map(|i| 0.2 * 30f64.powf(i as f64 / 24.0)).collect(); // 0.2..6 × fc
    for q in [0.45, 0.49, 0.5, 0.51, 0.55] {
        let mut ok = 0;
        let mut n = 0;
        for &k1 in &ks {
            for &k2 in &ks {
                if k1 >= k2 { continue; }
                n += 1;
                let b = Band { kind: Kind::HighShelf, freq_hz: 1000.0, gain_db: 1.0, q };
                if matched::vicanek_shelf_at(&b, fs, k1 * fc, k2 * fc).is_ok() { ok += 1; }
            }
        }
        println!("Q {q}: {ok}/{n} match-point pairs feasible");
    }
}
