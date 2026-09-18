//! A from-scratch port of the slice of `scipy.signal.find_peaks` that `peq.py`'s
//! `Peaking`/`LowShelf`/`HighShelf::init()` heuristics rely on: local-maxima detection,
//! per-peak *prominence*, and half-prominence *width* (`rel_height=0.5`).
//!
//! No Rust crate implements this exact algorithm (see the CAGEq stack-decision note on
//! the AutoEq-to-Rust port), so this is a direct translation of the three-stage
//! algorithm scipy itself uses (`_local_maxima_1d`, `_peak_prominences`,
//! `_peak_widths`), not a reimplementation from the docs.
//!
//! Two callers, two different `prominence` thresholds: `peq.py`'s `init()` heuristics
//! call `find_peaks(x, width=0, prominence=0, height=0)` — a threshold of `0` filters
//! nothing (a peak's prominence, like its width, is never negative by construction) —
//! while `equalize()` calls `find_peaks(y, prominence=1)`, a real filter. `min_prominence`
//! below is that threshold; `width`/`height` are never filtered by either caller (scipy
//! only computes `width` at all when a `width` argument is given, and neither caller
//! passes one bounded below zero), so those stay unfiltered here too.
//!
//! Cross-checked against `scipy.signal.find_peaks` directly (not through the sidecar,
//! which never calls this function on its own) in `tests/solver_fixtures.rs`.

/// One detected peak: sample index, the signal's value there, its prominence, and its
/// width in (fractional) samples at half its prominence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak {
    pub index: usize,
    pub height: f64,
    pub prominence: f64,
    pub width: f64,
}

/// Local maxima of `x`, flat-top plateaus included (scipy's `_local_maxima_1d`): a
/// plateau of equal values bounded on both sides by strictly smaller neighbours counts
/// as one peak, sitting at the plateau's midpoint index.
fn local_maxima(x: &[f64]) -> Vec<usize> {
    let mut midpoints = Vec::new();
    if x.len() < 3 {
        return midpoints;
    }
    let i_max = x.len() - 1;
    let mut i = 1;
    while i < i_max {
        if x[i - 1] < x[i] {
            let mut i_ahead = i + 1;
            while i_ahead < i_max && x[i_ahead] == x[i] {
                i_ahead += 1;
            }
            if x[i_ahead] < x[i] {
                let left_edge = i;
                let right_edge = i_ahead - 1;
                midpoints.push((left_edge + right_edge) / 2);
                i = i_ahead;
            }
        }
        i += 1;
    }
    midpoints
}

/// Prominence of the peak at `peak`, plus the base indices the search bottomed out at
/// (needed by [`width_at_half_prominence`]). Unrestricted window (scipy's `wlen=None`):
/// scans outward until either array edge, tracking the lowest point seen on each side.
fn prominence(x: &[f64], peak: usize) -> (f64, usize, usize) {
    let mut left_base = peak;
    let mut left_min = x[peak];
    {
        let mut i = peak as isize;
        while i >= 0 && x[i as usize] <= x[peak] {
            if x[i as usize] < left_min {
                left_min = x[i as usize];
                left_base = i as usize;
            }
            i -= 1;
        }
    }

    let mut right_base = peak;
    let mut right_min = x[peak];
    {
        let mut i = peak;
        while i < x.len() && x[i] <= x[peak] {
            if x[i] < right_min {
                right_min = x[i];
                right_base = i;
            }
            i += 1;
        }
    }

    (x[peak] - left_min.max(right_min), left_base, right_base)
}

/// Width of the peak at `peak` (with prominence `prom`, bases `left_base`/`right_base`)
/// at half its prominence — scipy's default `rel_height=0.5`, the only value `peq.py`
/// uses. Sub-sample precision via linear interpolation between the bracketing points,
/// exactly as scipy does.
fn width_at_half_prominence(x: &[f64], peak: usize, prom: f64, left_base: usize, right_base: usize) -> f64 {
    let height = x[peak] - prom * 0.5;

    let mut i = peak;
    while i > left_base && height < x[i] {
        i -= 1;
    }
    let mut left_ip = i as f64;
    if x[i] < height {
        // Only reachable with `i < peak` (at `i == peak`, `x[i] < height` is false since
        // `height <= x[peak]` by construction), so `x[i + 1]` is always in bounds.
        left_ip += (height - x[i]) / (x[i + 1] - x[i]);
    }

    let mut i = peak;
    while i < right_base && height < x[i] {
        i += 1;
    }
    let mut right_ip = i as f64;
    if x[i] < height {
        // Symmetric reasoning: only reachable with `i > peak`, so `x[i - 1]` is in bounds.
        right_ip -= (height - x[i]) / (x[i - 1] - x[i]);
    }

    right_ip - left_ip
}

/// The peaks of `x` with prominence `>= min_prominence`: local maxima with their
/// height, prominence, and half-prominence width. Mirrors `scipy.signal.find_peaks(x,
/// prominence=min_prominence, ...)` — see the module doc for which callers pass a real
/// threshold and which pass `0` (a no-op).
pub fn find_peaks(x: &[f64], min_prominence: f64) -> Vec<Peak> {
    local_maxima(x)
        .into_iter()
        .filter_map(|peak| {
            let (prom, left_base, right_base) = prominence(x, peak);
            if prom < min_prominence {
                return None;
            }
            let width = width_at_half_prominence(x, peak, prom, left_base, right_base);
            Some(Peak { index: peak, height: x[peak], prominence: prom, width })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_triangular_peak() {
        let x = [0.0, 1.0, 2.0, 1.0, 0.0];
        let peaks = find_peaks(&x, 0.0);
        assert_eq!(peaks.len(), 1);
        assert_eq!(peaks[0].index, 2);
        assert_eq!(peaks[0].height, 2.0);
        // Half-prominence height is 1.0, hit exactly at samples 1 and 3: width 2.0.
        assert!((peaks[0].width - 2.0).abs() < 1e-9);
    }

    #[test]
    fn flat_top_plateau_reports_its_midpoint() {
        let x = [0.0, 1.0, 2.0, 2.0, 2.0, 1.0, 0.0];
        let peaks = find_peaks(&x, 0.0);
        assert_eq!(peaks.len(), 1);
        assert_eq!(peaks[0].index, 3); // midpoint of the len-3 plateau at indices 2..=4
    }

    #[test]
    fn monotonic_signal_has_no_interior_peak() {
        let x = [0.0, 1.0, 2.0, 3.0, 4.0];
        assert!(find_peaks(&x, 0.0).is_empty());
    }

    #[test]
    fn two_peaks_separated_by_a_dip_are_found_independently() {
        let x = [0.0, 3.0, 0.0, 1.0, 0.0];
        let peaks = find_peaks(&x, 0.0);
        assert_eq!(peaks.len(), 2);
        assert_eq!(peaks[0].index, 1);
        assert_eq!(peaks[1].index, 3);
        // The smaller peak's prominence is capped by the dip between them, not by 0.
        assert!((peaks[0].height - 3.0).abs() < 1e-9);
        assert!((peaks[1].height - 1.0).abs() < 1e-9);
    }
}
