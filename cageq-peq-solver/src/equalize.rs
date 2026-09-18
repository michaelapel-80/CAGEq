//! Port of `FrequencyResponse.equalize` and its four helpers
//! (`frequency_response.py:542-807`) — turns the smoothed error curve into the actual
//! correction target the optimizer fits to (`optimize_parametric_eq`'s `target` is
//! `self.equalization`, this function's only output CAGEq's pipeline reads).
//!
//! Ported at CAGEq's one fixed call site (`sidecar_dsp.py`: `fr.equalize(max_gain=...)`,
//! every other parameter left at AutoEq's default), which lets several branches of the
//! general algorithm be simplified away or omitted rather than implemented as dead code:
//!
//!   - `concha_interference` defaults to `false` and CAGEq never overrides it, so the
//!     "reduce the local slope limit to 25% near 9 kHz" branch and the protection-mask
//!     carve-out near it never fire — omitted rather than threaded through as a
//!     parameter.
//!   - `max_slope_decay` defaults to `0.0`, making the "previous sample clipped, shrink
//!     the limit further" branch (`local_limit *= (1 - decay)^octaves`) multiply by `1`
//!     unconditionally — omitted the same way.
//!   - `treble_gain_k` defaults to `DEFAULT_TREBLE_GAIN_K = 1.0`, which is also the
//!     `a_normal` value at that call site (`log_f_sigmoid(f, ..., a_normal=1.0,
//!     a_treble=treble_gain_k)`); `log_f_sigmoid`'s own formula collapses to the
//!     constant `1.0` whenever its two asymptotes are equal, so the "limit treble gain"
//!     step (`combined *= gain_k`) is a multiply by `1.0` everywhere — omitted.
//!   - `equalize()`'s own internal `fr.smoothen(window_size=1/12, treble_window_size=2,
//!     treble_f_lower=6000, treble_f_upper=8000)` call (on `self.error`) uses parameters
//!     numerically identical to CAGEq's own outer `fr.smoothen()` call, which already
//!     smoothed the very same `self.error` into `self.error_smoothed`
//!     ([`crate::prep::PreppedCurve::error_smoothed`]) — a pure function of its inputs,
//!     so re-running it here would reproduce that value bit for bit. This port takes
//!     `error_smoothed` as an argument instead of recomputing it.
//!   - The Python function's return value (clipped-region masks/indices, for plotting)
//!     is discarded at CAGEq's only call site — only the mutation of `self.equalization`
//!     matters — so [`equalize`] returns just that curve. `equalized_raw`/
//!     `equalized_smoothed` are set on the Python object too but never read by CAGEq's
//!     pipeline (`optimize_parametric_eq` fits to `self.equalization` directly), so they
//!     aren't computed here at all.

use crate::peaks::find_peaks;
use crate::smoothing::smoothen;

const TREBLE_F_LOWER: f64 = 6000.0;
const TREBLE_F_UPPER: f64 = 8000.0;

/// `log_log_gradient` (`autoeq/utils.py:62-66`): slope in dB per octave between two
/// (frequency, gain) points.
fn log_log_gradient(f0: f64, f1: f64, g0: f64, g1: f64) -> f64 {
    let octaves = (f1 / f0).ln() / std::f64::consts::LN_2;
    (g1 - g0) / octaves
}

fn argmin(xs: &[f64]) -> usize {
    xs.iter().enumerate().min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap()).map(|(i, _)| i).unwrap()
}

/// `protection_mask` (`frequency_response.py:636-669`): zones around a dip that sit
/// below both neighbouring dips' levels are marked limitation-free, so the slope
/// limiter (below) doesn't flatten a notch that's already narrower/deeper than its
/// surroundings warrant limiting.
fn protection_mask(y: &[f64], peak_inds: &[usize], dip_inds: &[usize]) -> Vec<bool> {
    let last_peak_after_last_dip = !peak_inds.is_empty() && (dip_inds.is_empty() || *peak_inds.last().unwrap() > *dip_inds.last().unwrap());

    // `dip_positions[i]` is the real index of that dip, except the last entry when
    // there was no trailing peak-after-dip to append: Python represents that case as
    // index `-1`, whose *level* (immediately overwritten to `min(y)`) is all that's
    // ever used — never dereferenced as a real position — so it's `None` here instead
    // of a sentinel index.
    let (dip_positions, dip_levels): (Vec<Option<usize>>, Vec<f64>) = if last_peak_after_last_dip {
        let last_peak = *peak_inds.last().unwrap();
        let last_dip_ind = last_peak + argmin(&y[last_peak..]);
        let positions: Vec<Option<usize>> = dip_inds.iter().map(|&i| Some(i)).chain([Some(last_dip_ind)]).collect();
        let levels: Vec<f64> = positions.iter().map(|p| y[p.unwrap()]).collect();
        (positions, levels)
    } else {
        let positions: Vec<Option<usize>> = dip_inds.iter().map(|&i| Some(i)).chain([None]).collect();
        let mut levels: Vec<f64> = dip_inds.iter().map(|&i| y[i]).collect();
        levels.push(y.iter().cloned().fold(f64::INFINITY, f64::min));
        (positions, levels)
    };

    let mut mask = vec![false; y.len()];
    if dip_positions.len() < 3 {
        return mask;
    }

    for i in 1..dip_positions.len() - 1 {
        // `i` never reaches `dip_positions.len() - 1` (this loop's own exclusive
        // bound) — the only slot that can be the sentinel — so every `dip_ind` used
        // below as an array index is real.
        let dip_ind = dip_positions[i].expect("only the last slot can be the sentinel");
        let target_left = dip_levels[i - 1];
        let target_right = dip_levels[i + 1];

        let left_ind = (0..dip_ind).rev().find(|&j| y[j] >= target_left).expect("a prior real dip's own level always satisfies this search") + 1;
        let right_ind = (dip_ind..y.len()).find(|&j| y[j] >= target_right).expect("a later dip's level, or the global minimum, always satisfies this search") - 1;
        for slot in mask.iter_mut().take(right_ind + 1).skip(left_ind) {
            *slot = true;
        }
    }
    mask
}

/// `find_rtl_start` (`frequency_response.py:778-806`): where the right-to-left slope
/// pass should begin — from the last dip if the curve ends on a dip, otherwise from
/// wherever the curve first drops back to the last dip's level (or the lower of the
/// two endpoints, if there is no dip at all) after the last peak.
fn find_rtl_start(y: &[f64], peak_inds: &[usize], dip_inds: &[usize]) -> usize {
    let last_peak_after_last_dip = !peak_inds.is_empty() && (dip_inds.is_empty() || *peak_inds.last().unwrap() > *dip_inds.last().unwrap());
    if last_peak_after_last_dip {
        let last_peak = *peak_inds.last().unwrap();
        let threshold = match dip_inds.last() {
            Some(&last_dip) => y[last_dip],
            None => y[0].max(y[y.len() - 1]),
        };
        (last_peak..y.len()).find(|&j| y[j] <= threshold).unwrap_or(y.len() - 1)
    } else {
        // `last_peak_after_last_dip` false with `peak_inds` empty needs `dip_inds`
        // non-empty — `equalize`'s flat-line branch already returned before this can be
        // called with both empty.
        *dip_inds.last().expect("at least one of peak_inds/dip_inds is non-empty here")
    }
}

/// `limited_ltr_slope` (`frequency_response.py:704-776`) — see the module doc for why
/// `concha_interference`/`max_slope_decay` aren't parameters here. Returns only the
/// limited curve; `equalize`'s only caller discards the `clipped`/`regions`
/// diagnostics, though the algorithm still needs that bookkeeping internally to decide
/// when a clipped region touches no peak and should be discarded.
fn limited_ltr_slope(x: &[f64], y: &[f64], max_slope: f64, start_index: usize, peak_inds: &[usize], limit_free_mask: &[bool]) -> Vec<f64> {
    let n = x.len();
    let mut limited: Vec<f64> = Vec::with_capacity(n);
    let mut clipped: Vec<bool> = Vec::with_capacity(n);
    let mut region_start: Option<usize> = None;

    for i in 0..n {
        if i <= start_index {
            limited.push(y[i]);
            clipped.push(false);
            continue;
        }

        let slope = log_log_gradient(x[i], x[i - 1], y[i], limited[i - 1]);

        if slope > max_slope && !limit_free_mask[i] {
            if !clipped[i - 1] {
                region_start = Some(i);
            }
            clipped.push(true);
            let octaves = (x[i] / x[i - 1]).ln() / std::f64::consts::LN_2;
            limited.push(limited[i - 1] + max_slope * octaves);
        } else {
            limited.push(y[i]);
            if clipped[i - 1] {
                let start = region_start.take().expect("clipped[i - 1] true implies a region was opened at some earlier index");
                let has_peak_in_region = peak_inds.iter().any(|&p| p >= start && p < i);
                if !has_peak_in_region {
                    // No peak touches this clipped region — it's an artefact of the
                    // slope limit rather than a real feature worth preserving.
                    // Discard the clipping and restore the original curve there.
                    for j in start..i {
                        limited[j] = y[j];
                        clipped[j] = false;
                    }
                }
            }
            clipped.push(false);
        }
    }
    limited
}

/// `limited_rtl_slope` (`frequency_response.py:671-702`): the same left-to-right
/// algorithm run on the curve reversed, then flipped back. Notably `x` itself is
/// *not* reversed in AutoEq's own implementation, only `y`/`peak_inds`/
/// `limit_free_mask` — ported literally rather than "fixed", since it is only correct
/// because AutoEq's standard grid has a uniform ratio between consecutive frequencies
/// (`x[i] / x[i-1]` is the same value everywhere), which makes the mismatched pairing
/// numerically harmless. A caller passing a non-uniform grid would silently get a
/// different (wrong) answer from this — as it would from `peq.py` itself.
fn limited_rtl_slope(x: &[f64], y: &[f64], max_slope: f64, start_index: usize, peak_inds: &[usize], limit_free_mask: &[bool]) -> Vec<f64> {
    let n = x.len();
    let flipped_start = n - start_index - 1;
    let flipped_peaks: Vec<usize> = peak_inds.iter().map(|&p| n - p - 1).collect();
    let flipped_mask: Vec<bool> = limit_free_mask.iter().rev().copied().collect();
    let flipped_y: Vec<f64> = y.iter().rev().copied().collect();

    let mut limited_rtl = limited_ltr_slope(x, &flipped_y, max_slope, flipped_start, &flipped_peaks, &flipped_mask);
    limited_rtl.reverse();
    limited_rtl
}

/// `FrequencyResponse.equalize` (`frequency_response.py:542-634`) — see the module doc
/// for the parameters/branches CAGEq's fixed call site lets this skip. `max_slope`
/// is AutoEq's `DEFAULT_MAX_SLOPE` (`18.0`) at CAGEq's call site; `max_gain` is the one
/// parameter CAGEq's own config actually surfaces (`sidecar_dsp.py`'s
/// `params.get("max_gain", 6.0)`).
pub fn equalize(f: &[f64], error_smoothed: &[f64], max_slope: f64, max_gain: f64) -> Vec<f64> {
    let y: Vec<f64> = error_smoothed.iter().map(|v| -v).collect();

    let peak_inds: Vec<usize> = find_peaks(&y, 1.0).into_iter().map(|p| p.index).collect();
    let dip_inds: Vec<usize> = find_peaks(error_smoothed, 1.0).into_iter().map(|p| p.index).collect();

    if peak_inds.is_empty() && dip_inds.is_empty() {
        // Flat line: the inverse of the smoothed error *is* the equalization target,
        // with no slope limiting needed (`frequency_response.py:580-589`).
        return y;
    }

    let limit_free_mask = protection_mask(&y, &peak_inds, &dip_inds);
    let rtl_start = find_rtl_start(&y, &peak_inds, &dip_inds);

    let limited_ltr = limited_ltr_slope(f, &y, max_slope, 0, &peak_inds, &limit_free_mask);
    let limited_rtl = limited_rtl_slope(f, &y, max_slope, rtl_start, &peak_inds, &limit_free_mask);

    let mut combined: Vec<f64> = limited_ltr.iter().zip(&limited_rtl).map(|(a, b)| a.min(*b)).collect();
    for v in &mut combined {
        *v = v.min(max_gain);
    }

    // `combined.smoothen(window_size=1/5, treble_window_size=1/5)`: both windows are
    // the same width in octaves, so the two Savitzky-Golay passes `smoothen` blends
    // are identical and the sigmoid blend between them is a weighted average of a
    // value with itself — mathematically a plain single pass, computed here via the
    // shared two-pass helper rather than special-cased, since the two are equal by
    // construction whenever the caller passes equal window widths.
    smoothen(f, &combined, 1.0 / 5.0, 1.0 / 5.0, TREBLE_F_LOWER, TREBLE_F_UPPER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::standard_grid;

    #[test]
    fn a_flat_error_curve_takes_the_no_peaks_shortcut() {
        let f = standard_grid();
        let error_smoothed = vec![0.0; f.len()];
        let out = equalize(&f, &error_smoothed, 18.0, 6.0);
        assert!(out.iter().all(|&v| v == 0.0), "a flat curve should equalize to exactly flat");
    }

    #[test]
    fn a_narrow_steep_dip_gets_slope_limited_rather_than_matched_exactly() {
        let f = standard_grid();
        // A 20 dB dip over ~0.1 octave is far steeper than the 18 dB/octave default
        // limit — the equalization curve boosting it back out must not fully match
        // that slope.
        let error_smoothed: Vec<f64> = f.iter().map(|&freq| -20.0 * (-((freq / 1000.0).log2() / 0.05).powi(2)).exp()).collect();
        let out = equalize(&f, &error_smoothed, 18.0, 6.0);

        let peak_slope = out
            .windows(2)
            .zip(f.windows(2))
            .map(|(w, fw)| (w[1] - w[0]).abs() / ((fw[1] / fw[0]).log2()))
            .fold(0.0, f64::max);
        // The final 1/5-octave smoothing pass can slightly overshoot the raw 18 dB/oct
        // limit right at a clipped kink (ordinary Savitzky-Golay behaviour near a sharp
        // corner) — the bound here is "close to the limit", not "never exceeds it".
        assert!(peak_slope < 20.0, "steepest slope in the equalization curve should stay close to the 18 dB/octave limit, got {peak_slope}");
    }
}
