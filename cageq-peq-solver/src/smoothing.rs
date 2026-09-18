//! Port of `autoeq.utils.{smoothing_window_size, log_f_sigmoid}` and
//! `scipy.signal.savgol_filter` (as `FrequencyResponse._smoothen`, `peq.py`... no —
//! `frequency_response.py:489-541`, calls it: always `polyorder=2`, `mode` left at
//! scipy's default `'interp'`) — together, `FrequencyResponse.smoothen`'s two-window
//! (normal/treble) sigmoid-blended smoothing.

/// `smoothing_window_size` (`autoeq/utils.py:34-50`): converts a window width in
/// octaves to a window width in samples, using the grid's average per-sample frequency
/// ratio, rounded to the nearest odd integer (`savgol_filter` requires an odd window).
pub fn smoothing_window_size(f: &[f64], octaves: f64) -> usize {
    let k = 2f64.powf(octaves);
    let step_size: f64 = (1..f.len()).map(|i| f[i] / f[i - 1]).sum::<f64>() / (f.len() - 1) as f64;
    let n = round_half_even(k.ln() / step_size.ln());
    let n = if (n as i64) % 2 == 0 { n + 1.0 } else { n };
    n.max(1.0) as usize
}

/// Python's `round()` on a float is round-half-to-even, not Rust's `f64::round`
/// (round-half-away-from-zero) — matters only exactly on a `.5` tie, which a
/// continuously-varying `step_size` will essentially never land on, but ported
/// faithfully rather than assumed harmless.
fn round_half_even(x: f64) -> f64 {
    let floor = x.floor();
    let diff = x - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else if (floor as i64) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// `log_f_sigmoid` (`autoeq/utils.py:53-59`): a logistic sigmoid in log10-frequency,
/// centred at the geometric mean of `f_lower`/`f_upper`, reaching its steep part across
/// that band. `a_normal`/`a_treble` are the asymptotes far below/above the band.
pub fn log_f_sigmoid(f: &[f64], f_lower: f64, f_upper: f64, a_normal: f64, a_treble: f64) -> Vec<f64> {
    let f_center = (f_upper / f_lower).sqrt() * f_lower;
    let half_range = f_upper.log10() - f_center.log10();
    let log_center = f_center.log10();
    f.iter()
        .map(|&freq| {
            let a = expit((freq.log10() - log_center) / (half_range / 4.0));
            a * -(a_normal - a_treble) + a_normal
        })
        .collect()
}

fn expit(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// `FrequencyResponse._smoothen` (`frequency_response.py:489-513`): two Savitzky-Golay
/// passes at different octave widths ("normal" and "treble"), sigmoid-blended across
/// `treble_f_lower..treble_f_upper` so the transition between them has no seam. Shared
/// by `crate::prep` (CAGEq's own `smoothen()` call: 1/12 and 2 octaves) and
/// `crate::equalize` (two different call sites, one of which passes the *same* width
/// for both windows — see that module's doc for why that one degenerates to a single
/// plain pass rather than needing special-casing here).
pub fn smoothen(f: &[f64], data: &[f64], window_octaves: f64, treble_window_octaves: f64, treble_f_lower: f64, treble_f_upper: f64) -> Vec<f64> {
    let normal = savgol_filter(data, smoothing_window_size(f, window_octaves));
    let treble = savgol_filter(data, smoothing_window_size(f, treble_window_octaves));
    let k_treble = log_f_sigmoid(f, treble_f_lower, treble_f_upper, 0.0, 1.0);
    normal.iter().zip(&treble).zip(&k_treble).map(|((&n, &t), &k)| n * (1.0 - k) + t * k).collect()
}

/// `scipy.signal.savgol_filter(data, window, 2)`, `mode='interp'` (scipy's default,
/// unspecified at every AutoEq call site): interior points use the closed-form
/// quadratic/cubic Savitzky-Golay convolution coefficients (the two polynomial orders
/// share one formula); the `window / 2` points at each edge — where the symmetric
/// convolution would need data past the boundary — instead evaluate a single quadratic
/// least-squares fit to that edge's own `window`-sized block, at each edge position.
/// `window` must be odd (guaranteed by [`smoothing_window_size`]).
pub fn savgol_filter(data: &[f64], window: usize) -> Vec<f64> {
    let n = data.len();
    if window >= n {
        // scipy itself would refuse a window this large; every real curve here has
        // hundreds of grid points and window sizes of a handful, so this never fires
        // in practice — a plain copy is a safe, honest fallback rather than a panic.
        return data.to_vec();
    }
    let half = window / 2;
    let mut out = vec![0.0; n];

    let m = half as f64;
    let denom = (2.0 * m - 1.0) * (2.0 * m + 1.0) * (2.0 * m + 3.0);
    let coeff = |i: i64| -> f64 {
        let i = i as f64;
        3.0 * (3.0 * m * m + 3.0 * m - 1.0 - 5.0 * i * i) / denom
    };
    for center in half..(n - half) {
        let mut acc = 0.0;
        for k in 0..window {
            let i = k as i64 - half as i64;
            acc += coeff(i) * data[center - half + k];
        }
        out[center] = acc;
    }

    let local_x: Vec<f64> = (0..window).map(|i| i as f64).collect();

    let (a, b, c) = polyfit_quadratic(&local_x, &data[0..window]);
    for i in 0..half {
        let x = i as f64;
        out[i] = a + b * x + c * x * x;
    }

    let (a, b, c) = polyfit_quadratic(&local_x, &data[n - window..n]);
    for i in 0..half {
        // scipy fits the trailing window in its own 0..window local coordinates, then
        // evaluates at local positions `window - half ..= window - 1` (the block's last
        // `half` samples) — not `0..half`, which would just repeat the leading edge.
        let x = (window - half + i) as f64;
        out[n - half + i] = a + b * x + c * x * x;
    }

    out
}

/// Ordinary least-squares quadratic fit `y = a + b*x + c*x^2` (`np.polyfit(x, y, 2)`,
/// coefficients returned in the opposite order since nothing here needs numpy's
/// highest-degree-first convention — only the fitted curve's *values* have to match).
/// Solved via the normal equations (a 3x3 system, cheap enough to invert directly by
/// Cramer's rule for every call rather than reaching for a linear-algebra crate).
fn polyfit_quadratic(xs: &[f64], ys: &[f64]) -> (f64, f64, f64) {
    let (mut s1, mut s2, mut s3, mut s4) = (0.0, 0.0, 0.0, 0.0);
    let (mut t0, mut t1, mut t2) = (0.0, 0.0, 0.0);
    let n = xs.len() as f64;
    for (&x, &y) in xs.iter().zip(ys) {
        let x2 = x * x;
        s1 += x;
        s2 += x2;
        s3 += x2 * x;
        s4 += x2 * x2;
        t0 += y;
        t1 += x * y;
        t2 += x2 * y;
    }
    // | n  s1 s2 | |a|   |t0|
    // | s1 s2 s3 | |b| = |t1|
    // | s2 s3 s4 | |c|   |t2|
    solve_3x3([[n, s1, s2], [s1, s2, s3], [s2, s3, s4]], [t0, t1, t2])
}

fn solve_3x3(m: [[f64; 3]; 3], b: [f64; 3]) -> (f64, f64, f64) {
    let det3 = |r: [[f64; 3]; 3]| -> f64 {
        r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1]) - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0]) + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0])
    };
    let d = det3(m);
    let col_replaced = |col: usize| -> [[f64; 3]; 3] {
        let mut r = m;
        for i in 0..3 {
            r[i][col] = b[i];
        }
        r
    };
    (det3(col_replaced(0)) / d, det3(col_replaced(1)) / d, det3(col_replaced(2)) / d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savgol_reproduces_an_exact_quadratic_everywhere_including_edges() {
        // A Savitzky-Golay filter of polyorder 2 is exact (zero residual, interior and
        // edges alike) on data that is itself a quadratic — the textbook sanity check.
        let data: Vec<f64> = (0..41).map(|i| { let x = i as f64; 2.0 + 0.5 * x - 0.03 * x * x }).collect();
        let out = savgol_filter(&data, 9);
        for (i, (&a, &b)) in out.iter().zip(&data).enumerate() {
            assert!((a - b).abs() < 1e-9, "index {i}: {a} != {b}");
        }
    }

    #[test]
    fn savgol_smooths_a_single_spike_without_shifting_the_baseline() {
        let mut data = vec![0.0; 41];
        data[20] = 10.0;
        let out = savgol_filter(&data, 9);
        assert!(out[20] < 10.0, "the spike should be attenuated, got {}", out[20]);
        assert!(out[0].abs() < 1e-9, "far from the spike should stay at baseline, got {}", out[0]);
    }

    #[test]
    fn log_f_sigmoid_sits_at_its_asymptotes_far_from_the_transition_band() {
        let f = [10.0, 100_000.0];
        let out = log_f_sigmoid(&f, 6000.0, 8000.0, 0.0, 1.0);
        assert!(out[0] < 0.01, "well below the band should read ~0.0, got {}", out[0]);
        assert!(out[1] > 0.99, "well above the band should read ~1.0, got {}", out[1]);
    }

    #[test]
    fn window_size_is_always_odd() {
        let f: Vec<f64> = (0..500).map(|i| 20.0 * 1.01f64.powi(i)).collect();
        for octaves in [1.0 / 12.0, 1.0 / 5.0, 2.0] {
            assert_eq!(smoothing_window_size(&f, octaves) % 2, 1);
        }
    }
}
