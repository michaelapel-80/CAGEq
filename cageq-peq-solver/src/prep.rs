//! CAGEq's actual FR-prep call sequence — `sidecar_dsp.py`'s
//! `fr.interpolate(); fr.center(); fr.compensate(target); fr.smoothen()` — assembled
//! from [`crate::grid`] and [`crate::smoothing`]'s ported primitives. This is the input
//! `equalize()` (`frequency_response.py:542-807`, not yet ported — its slope-limited
//! LTR/RTL traversal is a substantially larger piece, deliberately left for its own
//! pass) will need next.

use crate::grid::{center_diff, interpolate, standard_grid};
use crate::smoothing::smoothen as smoothen_curve;

const DEFAULT_SMOOTHING_WINDOW_SIZE: f64 = 1.0 / 12.0;
const DEFAULT_TREBLE_SMOOTHING_WINDOW_SIZE: f64 = 2.0;
const DEFAULT_TREBLE_SMOOTHING_F_LOWER: f64 = 6000.0;
const DEFAULT_TREBLE_SMOOTHING_F_UPPER: f64 = 8000.0;

/// The result of `interpolate -> center -> compensate -> smoothen`, on AutoEq's
/// standard grid throughout.
pub struct PreppedCurve {
    pub f: Vec<f64>,
    pub raw: Vec<f64>,
    pub target: Vec<f64>,
    pub error: Vec<f64>,
    pub smoothed: Vec<f64>,
    pub error_smoothed: Vec<f64>,
}

/// `FrequencyResponse._smoothen` (`frequency_response.py:489-513`) at CAGEq's fixed
/// defaults (`smoothen()` is always called with no arguments — `sidecar_dsp.py`): two
/// Savitzky-Golay passes, a fine one and a coarse "treble" one, sigmoid-blended across
/// `treble_f_lower..treble_f_upper` so the transition has no seam.
fn smoothen(f: &[f64], data: &[f64]) -> Vec<f64> {
    smoothen_curve(f, data, DEFAULT_SMOOTHING_WINDOW_SIZE, DEFAULT_TREBLE_SMOOTHING_WINDOW_SIZE, DEFAULT_TREBLE_SMOOTHING_F_LOWER, DEFAULT_TREBLE_SMOOTHING_F_UPPER)
}

/// The whole prep chain CAGEq actually runs, given a raw measurement curve
/// (`measurement_f`/`measurement_raw`) and a raw target curve (`target_f`/`target_raw`)
/// — both exactly as fetched, unresampled, `NaN` standing in for AutoEq's `None` gaps.
///
/// Reproduces `sidecar_dsp.py`'s `fr.interpolate(); fr.center(); fr.compensate(target);
/// fr.smoothen()` for CAGEq's *specific* call site: every `compensate()`/`smoothen()`
/// parameter CAGEq never overrides is baked in at AutoEq's default rather than exposed
/// here, the same convention `optimizer.rs`'s module doc documents for the solver side.
/// In particular `compensate()`'s `bass_boost`/`treble_boost`/`tilt` all default to
/// `0.0` (`constants.py`), making `create_target()`'s contribution provably zero (a 0 dB
/// shelf is unity; `log_tilt(f, 0.0)` is zero by construction) — so the target curve
/// here is just the target CSV, interpolated and centred the same way the measurement
/// is, with no bass/treble-boost or tilt shaping added on top. If CAGEq ever starts
/// passing those, or a `sound_signature`, this stops being equivalent to `peq.py`'s.
pub fn prepare(measurement_f: &[f64], measurement_raw: &[f64], target_f: &[f64], target_raw: &[f64]) -> PreppedCurve {
    let f = standard_grid();

    let mut raw = interpolate(measurement_f, measurement_raw, &f);
    let diff = center_diff(&f, &raw);
    raw.iter_mut().for_each(|v| *v -= diff);

    let mut target = interpolate(target_f, target_raw, &f);
    let target_diff = center_diff(&f, &target);
    target.iter_mut().for_each(|v| *v -= target_diff);
    let error: Vec<f64> = raw.iter().zip(&target).map(|(r, t)| r - t).collect();

    let smoothed = smoothen(&f, &raw);
    let error_smoothed = smoothen(&f, &error);

    PreppedCurve { f, raw, target, error, smoothed, error_smoothed }
}
