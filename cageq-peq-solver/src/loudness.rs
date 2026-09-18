//! Port of `sidecar_dsp.py`'s `loudness_target_db`/`_k_weight_power` (filter.md §4.1) —
//! the K-weighted pink-noise loudness-compensation formula, plus the plain positive
//! peak (§4.2's clipping ceiling input) — the two curve-derived quantities
//! `calculate_filters`'s reply carries alongside the fitted filters
//! (`cageq-core/src/lib.rs`'s `CalcResult`).
//!
//! A from-scratch port rather than a reuse of `cageq_core::morph`'s own K-weighting
//! copy (`morph.rs`'s `k_weight_power`/`Grid`): that module is `pub(crate)` to
//! `cageq-core`, which depends on `cageq-sidecar` — the very crate this dependency
//! direction exists to make optional — so depending on it here would be backwards (see
//! `src/lib.rs`'s "fourth copy of the biquad math" note for the identical reasoning
//! applied to a different formula). This makes the K-weighting biquad coefficients a
//! *fifth* copy across the codebase (`sidecar_dsp.py`, `morph.rs`, and now this);
//! cross-checked against the live sidecar's `loudness_target_db` in
//! `tests/loudness_fixtures.rs`, the same discipline every other copy is held to.

const KW_S1_B: [f64; 3] = [1.53512485958697, -2.69169618940638, 1.19839281085285];
const KW_S1_A: [f64; 3] = [1.0, -1.69065929318241, 0.73248077421585];
const KW_S2_B: [f64; 3] = [1.0, -2.0, 1.0];
const KW_S2_A: [f64; 3] = [1.0, -1.99004745483398, 0.99007225036621];

/// `|H(e^{-jw})|^2` for one biquad — same closed form `cageq_core::morph`'s
/// `biquad_power` uses, avoiding a complex-number dependency.
fn biquad_power(b: &[f64; 3], a: &[f64; 3], w: f64) -> f64 {
    let (c1, s1) = (w.cos(), w.sin());
    let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
    let num_re = b[0] + b[1] * c1 + b[2] * c2;
    let num_im = -(b[1] * s1 + b[2] * s2);
    let den_re = a[0] + a[1] * c1 + a[2] * c2;
    let den_im = -(a[1] * s1 + a[2] * s2);
    (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)
}

/// `_k_weight_power` (`sidecar_dsp.py:221-231`): the two-stage K-weighting cascade's
/// power response at `f`, evaluated against sample rate `fs` (CAGEq always calls this
/// with `48_000.0`, matching the Python default and the coefficients' own reference
/// rate).
fn k_weight_power(f: f64, fs: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / fs;
    biquad_power(&KW_S1_B, &KW_S1_A, w) * biquad_power(&KW_S2_B, &KW_S2_A, w)
}

/// `np.gradient(f)`: central difference at interior points, one-sided at each edge —
/// the same edge convention `cageq_core::morph`'s `Grid` builds its own per-bin
/// weights with.
fn gradient(f: &[f64]) -> Vec<f64> {
    let n = f.len();
    (0..n)
        .map(|i| {
            if i == 0 {
                f[1] - f[0]
            } else if i == n - 1 {
                f[n - 1] - f[n - 2]
            } else {
                (f[i + 1] - f[i - 1]) / 2.0
            }
        })
        .collect()
}

/// `loudness_target_db` (`sidecar_dsp.py:234-253`): the §4.1 relative loudness
/// compensation for an EQ curve `g_eq_db` (dB) on grid `f` — a K-weighted pink-noise
/// energy model of how much the curve raises perceived loudness, negated so applying
/// it is level-neutral versus dry. Not an absolute LUFS measurement; a per-curve
/// broadband offset so A/B/Dry comparisons judge timbre, not level.
pub fn loudness_target_db(f: &[f64], g_eq_db: &[f64], fs: f64) -> f64 {
    let grad = gradient(f);
    // Pink noise's power *density* is 1/f, but a bin's energy is density × bin width;
    // `grad[i] / f[i]` is that width's Jacobian, making this correct on any grid — see
    // the Python docstring's note on the bug this replaced (using `1/f` directly
    // double-counts the pink slope and over-weights the bottom octaves).
    let bin_energy: Vec<f64> = grad.iter().zip(f).map(|(g, &freq)| g / freq).collect();
    let w_k: Vec<f64> = f.iter().map(|&freq| k_weight_power(freq, fs)).collect();
    let p_dry: f64 = bin_energy.iter().zip(&w_k).map(|(b, w)| b * w).sum();
    let p_wet: f64 = bin_energy.iter().zip(&w_k).zip(g_eq_db).map(|((b, w), g)| b * w * 10f64.powf(g / 10.0)).sum();
    let delta_l = 10.0 * (p_wet / p_dry).log10();
    -delta_l
}

/// `float(np.max(combined))` (`sidecar_dsp.py:479`) — the composed curve's positive
/// peak, the §4.2 clipping-ceiling input.
pub fn curve_peak_db(curve: &[f64]) -> f64 {
    curve.iter().copied().fold(f64::MIN, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_curve_is_level_neutral() {
        let f: Vec<f64> = (0..200).map(|i| 20.0 * 1.05f64.powi(i)).collect();
        let flat = vec![0.0; f.len()];
        assert!(loudness_target_db(&f, &flat, 48_000.0).abs() < 1e-9);
    }

    #[test]
    fn a_uniform_boost_is_compensated_one_for_one() {
        let f: Vec<f64> = (0..200).map(|i| 20.0 * 1.05f64.powi(i)).collect();
        let boosted = vec![6.0; f.len()];
        assert!((loudness_target_db(&f, &boosted, 48_000.0) - -6.0).abs() < 1e-9);
    }

    #[test]
    fn curve_peak_reads_off_the_maximum() {
        assert_eq!(curve_peak_db(&[-3.0, 5.0, 1.0, -8.0]), 5.0);
    }
}
