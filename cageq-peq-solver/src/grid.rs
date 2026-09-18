//! `autoeq.utils.generate_frequencies` and the linear (log-frequency) interpolation
//! `FrequencyResponse.interpolate`/`center` build on — the pieces every other function
//! in `crate::prep`/`crate::smoothing` is defined in terms of.

pub const DEFAULT_F_MIN: f64 = 20.0;
pub const DEFAULT_F_MAX: f64 = 20_000.0;
pub const DEFAULT_STEP: f64 = 1.01;

/// `generate_frequencies` (`autoeq/utils.py:8-14`): `f_min`, repeatedly `* f_step`,
/// while `<= f_max`. AutoEq's "standard grid" is this at the module defaults.
pub fn generate_frequencies(f_min: f64, f_max: f64, f_step: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut f = f_min;
    while f <= f_max {
        out.push(f);
        f *= f_step;
    }
    out
}

/// AutoEq's standard grid (every CAGEq call site uses the module defaults).
pub fn standard_grid() -> Vec<f64> {
    generate_frequencies(DEFAULT_F_MIN, DEFAULT_F_MAX, DEFAULT_STEP)
}

/// `InterpolatedUnivariateSpline(log10(f), y, k=1)` evaluated at `query` — a degree-1
/// spline *is* linear interpolation, and its default `ext=0` extrapolates rather than
/// clamping, so points outside `[f[0], f[-1]]` continue the boundary segment's slope
/// rather than repeating the edge value. `f`/`y` must be sorted ascending by frequency
/// (true of every curve this crate handles) and contain no `NaN` (see [`interpolate`]
/// for the caller that filters those out first).
pub fn linear_interp_log(f: &[f64], y: &[f64], query: &[f64]) -> Vec<f64> {
    assert_eq!(f.len(), y.len());
    let log_f: Vec<f64> = f.iter().map(|v| v.log10()).collect();
    query.iter().map(|&q| eval_linear(&log_f, y, q.log10())).collect()
}

fn eval_linear(xs: &[f64], ys: &[f64], x: f64) -> f64 {
    let n = xs.len();
    if n == 1 {
        return ys[0];
    }
    if x <= xs[0] {
        return ys[0] + (x - xs[0]) * (ys[1] - ys[0]) / (xs[1] - xs[0]);
    }
    if x >= xs[n - 1] {
        return ys[n - 2] + (x - xs[n - 2]) * (ys[n - 1] - ys[n - 2]) / (xs[n - 1] - xs[n - 2]);
    }
    // `partition_point` finds the first index where `xs[i] > x`; the bracketing segment
    // starts one before that. `x < xs[n-1]` here (checked above), so this never reaches
    // n - 1 and `i + 1` stays in bounds.
    let i = xs.partition_point(|&v| v <= x).saturating_sub(1);
    ys[i] + (x - xs[i]) * (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i])
}

/// `FrequencyResponse.interpolate` (`frequency_response.py:344-377`) at the only
/// `pol_order` CAGEq ever uses (`1`, the default — a linear spline). Drops `NaN` entries
/// first (AutoEq's `None`, a gap in a measurement CSV), matching `interpolate()`'s own
/// "Remove None values" pass, then resamples the rest onto `grid`.
pub fn interpolate(f: &[f64], y: &[f64], grid: &[f64]) -> Vec<f64> {
    let (clean_f, clean_y): (Vec<f64>, Vec<f64>) = f.iter().zip(y).filter(|(_, v)| !v.is_nan()).map(|(&a, &b)| (a, b)).unzip();
    linear_interp_log(&clean_f, &clean_y, grid)
}

/// `FrequencyResponse.center` (`frequency_response.py:379-416`) at CAGEq's fixed
/// `frequency=1000` default, returning the bias to subtract (`raw -= diff`).
///
/// AutoEq's version re-derives its own interpolant from a fresh copy of `(frequency,
/// raw)` (re-running `interpolate()` on it) before evaluating at 1000 Hz, rather than
/// building the interpolant from `self` directly. That round trip is a no-op precisely
/// because every CAGEq call site invokes `interpolate()` immediately before `center()`
/// (`sidecar_dsp.py`'s `fr.interpolate(); fr.center()`, and `compensate()`'s internal
/// `target.interpolate(); target.center()`): resampling data that is already on a grid
/// onto that *same* grid via linear interpolation returns the original values exactly,
/// since a linear interpolant evaluated at its own control points reproduces them. So
/// this takes `f`/`y` as already resampled and skips the redundant round trip — it is
/// only equivalent to `peq.py` under that calling convention, not in general.
pub fn center_diff(f: &[f64], y: &[f64]) -> f64 {
    linear_interp_log(f, y, &[1000.0])[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_the_standard_grid_bounds() {
        let g = standard_grid();
        assert_eq!(g[0], 20.0);
        // The multiplicative walk (`f *= 1.01`) need not land exactly at 20 kHz — only
        // within one step's ratio of it, with the *next* step (which the loop never
        // takes) landing past the ceiling.
        let last = *g.last().unwrap();
        assert!((10_000.0..=20_000.0).contains(&last), "last point should be within one 1.01x step of 20 kHz, got {last}");
        assert!(last * DEFAULT_STEP > DEFAULT_F_MAX, "one more step should have exceeded the ceiling, got {}", last * DEFAULT_STEP);
    }

    #[test]
    fn linear_interp_reproduces_control_points_exactly() {
        let f = [100.0, 1000.0, 10_000.0];
        let y = [1.0, -2.0, 3.0];
        let out = linear_interp_log(&f, &y, &f);
        for (a, b) in out.iter().zip(&y) {
            assert!((a - b).abs() < 1e-12, "{a} != {b}");
        }
    }

    #[test]
    fn linear_interp_extrapolates_the_boundary_slope_rather_than_clamping() {
        let f = [100.0, 1000.0];
        let y = [0.0, 1.0]; // 1 dB per decade of log10(f)
        let out = linear_interp_log(&f, &y, &[10.0, 10_000.0]);
        // One decade below 100 Hz: continuing the same slope gives -1.0, not the
        // clamped-at-the-edge 0.0 a naive "hold last value" interpolator would give.
        assert!((out[0] - -1.0).abs() < 1e-9, "expected extrapolated -1.0, got {}", out[0]);
        assert!((out[1] - 2.0).abs() < 1e-9, "expected extrapolated 2.0, got {}", out[1]);
    }

    #[test]
    fn interpolate_drops_nan_entries_before_resampling() {
        let f = [100.0, 500.0, 1000.0];
        let y = [0.0, f64::NAN, 2.0];
        let out = interpolate(&f, &y, &[100.0, 1000.0]);
        assert!((out[0] - 0.0).abs() < 1e-9);
        assert!((out[1] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn center_diff_reads_off_the_interpolated_1khz_value() {
        let f = [500.0, 1000.0, 2000.0];
        let y = [1.0, 5.0, 9.0];
        assert!((center_diff(&f, &y) - 5.0).abs() < 1e-9);
    }
}
