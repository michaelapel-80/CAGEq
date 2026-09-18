//! Replaces the sidecar's `fit_export_eq`/`fit_fixed_band_eq` RPCs (filter.md §8,
//! mobile export) — a *second*, independent AutoEq PEQ pass that fits directly to a
//! slot's own composed curve (`filters`, the same `Band[]` `Applied::filters` already
//! carries), not to a headphone measurement the way `fit.rs`'s main path does.
//!
//! Both reuse [`cageq_peq_solver::Solver`] wholesale — the only thing that differs from
//! the main fit is what curve gets fitted (a slot's own cascade via [`filter_curve_db`]
//! rather than a measurement's error curve) and what the band set looks like
//! (free-band-count for export, one of AutoEq's own fixed 10-/31-band graphic-EQ
//! presets for the other). Neither needs `equalize()` at all: the target *is* the
//! curve, with no peak/dip-detection or slope-limiting step in between.
//!
//! `sidecar_dsp.py`'s `_optimize_peq_filters` (which both `optimize_parametric_eq` and
//! `optimize_fixed_band_eq` funnel through) actually re-grids onto
//! `DEFAULT_BIQUAD_OPTIMIZATION_F_STEP` (`1.02`) for the SLSQP pass itself, rather than
//! the standard grid's `1.01` — a difference this port doesn't reproduce, matching the
//! same simplification already made (and empirically validated:
//! `cageq-core/tests/rust_vs_sidecar.rs` shows curve-shape/loudness agreement well
//! within noise) for the main fit in `fit.rs`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use cageq_peq_solver::{grid::linear_interp_log, grid::standard_grid, Band, BandKind, Solver};

use crate::{filter_curve_db, validate_filters, CoreError, Filter};

const FS: f64 = 48_000.0;
/// `_FIXED_BAND_GAIN_RANGE_DB` (`sidecar_dsp.py:629`) — see its own doc for why an
/// unconstrained fixed-band fit measurably overshoots/ripples.
const FIXED_BAND_GAIN_RANGE_DB: f64 = 4.0;
const CACHE_MAX: usize = 32;

fn filters_key(filters: &[Filter]) -> Vec<(u8, i64, i64, i64)> {
    filters
        .iter()
        .map(|f| {
            let kind = match f.kind {
                cageq_backend::FilterType::Peaking => 0,
                cageq_backend::FilterType::LowShelf => 1,
                cageq_backend::FilterType::HighShelf => 2,
                cageq_backend::FilterType::Bandpass => 3,
                cageq_backend::FilterType::Tilt => 4,
            };
            // Rounded like `sidecar_dsp.py`'s own cache-key digests (`round(x, 2)`/
            // `round(x, 4)`) — this is a cache key, not a computation, so quantizing
            // avoids two floating-point-identical-looking requests missing each other
            // over noise several decimal places below anything audible.
            (kind, (f.freq_hz * 100.0).round() as i64, (f.gain_db * 100.0).round() as i64, (f.q * 10_000.0).round() as i64)
        })
        .collect()
}

/// A small bounded FIFO cache, shared shape for both `fit_export_eq`'s and
/// `fit_fixed_band_eq`'s independent caches (`sidecar_dsp.py`'s `_EXPORT_FIT_CACHE`/
/// `_FIXED_BAND_CACHE`) — the export dialog's band-count/preset control re-fits on
/// every change, and a repeat value should be instant, not a second SLSQP pass.
pub(crate) struct BoundedCache<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> BoundedCache<K, V> {
    pub(crate) fn new() -> Self {
        BoundedCache { entries: HashMap::new(), order: VecDeque::new() }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: K, value: V) {
        if !self.entries.contains_key(&key) {
            if self.order.len() >= CACHE_MAX {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, value);
    }
}

type ExportKey = (Vec<(u8, i64, i64, i64)>, u32);
type FixedBandKey = (Vec<(u8, i64, i64, i64)>, &'static str);
type FitResult = (Vec<Filter>, f64);

pub(crate) type ExportCache = BoundedCache<ExportKey, FitResult>;
pub(crate) type FixedBandCache = BoundedCache<FixedBandKey, FitResult>;

fn preamp_db(achieved_curve: &[f64]) -> f64 {
    // `-max(achieved) - 0.2 dB` headroom (`sidecar_dsp.py:605`), mirroring AutoEq's own
    // `write_eqapo_parametric_eq` preamp line — self-contained ("don't clip on the
    // destination device"), deliberately not the desktop's §4.1 loudness-matched value.
    let peak = achieved_curve.iter().copied().fold(f64::MIN, f64::max);
    ((-peak - 0.2) * 100.0).round() / 100.0
}

/// `fit_export_eq` (`sidecar_dsp.py:551-609`): fits `band_count` filters (>= 3; one low
/// shelf + one high shelf + the rest free peaking, same shape as the main fit's own
/// config) directly to `filters`' own composed curve.
pub(crate) fn fit_export_eq(cache: &Mutex<ExportCache>, filters: &[Filter], band_count: u32) -> Result<FitResult, CoreError> {
    validate_filters(filters)?;
    let band_count = band_count.max(3);
    let key = (filters_key(filters), band_count);
    if let Some(cached) = cache.lock().unwrap().get(&key) {
        return Ok(cached);
    }

    let f = standard_grid();
    let target = filter_curve_db(filters, &f);
    let bands = cageq_peq_solver::cageq_default_bands((band_count - 2) as usize);

    let mut solver = Solver::new(f, FS, bands, target);
    solver.optimize()?;
    let out_filters = cageq_peq_solver::bands_to_filters(&solver.bands);
    let preamp = preamp_db(&solver.fr());

    let result = (out_filters, preamp);
    cache.lock().unwrap().insert(key, result.clone());
    Ok(result)
}

/// AutoEq's own two standard graphic-EQ presets (`autoeq/constants.py`'s
/// `PEQ_CONFIGS`) — fixed ISO center frequencies, one shared `q` per preset, only gain
/// ever optimized.
fn preset_bands(preset: &str) -> Vec<Band> {
    match preset {
        "10" => (0..10).map(|i| Band::fixed_fc_q(BandKind::Peaking, 31.25 * 2f64.powi(i), std::f64::consts::SQRT_2)).collect(),
        _ => (0..31).map(|i| Band::fixed_fc_q(BandKind::Peaking, 20.0 * 2f64.powf(i as f64 / 3.0), 4.318473)).collect(),
    }
}

fn preset_key(preset: &str) -> &'static str {
    if preset == "10" {
        "10"
    } else {
        "31"
    }
}

/// `fit_fixed_band_eq` (`sidecar_dsp.py:646-689`): AutoEq's standard 10-/31-band
/// graphic EQ, gain-only fit to `filters`' own composed curve. Each band's gain is
/// bounded to within [`FIXED_BAND_GAIN_RANGE_DB`] of the curve's own value sampled at
/// that band's exact `fc` (`optimize_fixed_band_eq`'s `gain_range`, `frequency_response.
/// py:182-190`) — left unconstrained, a dense preset's neighbouring bands measurably
/// overshoot/ripple against each other (see `sidecar_dsp.py`'s own doc on that constant).
pub(crate) fn fit_fixed_band_eq(cache: &Mutex<FixedBandCache>, filters: &[Filter], preset: &str) -> Result<FitResult, CoreError> {
    validate_filters(filters)?;
    let preset = preset_key(preset);
    let key = (filters_key(filters), preset);
    if let Some(cached) = cache.lock().unwrap().get(&key) {
        return Ok(cached);
    }

    let f = standard_grid();
    let target = filter_curve_db(filters, &f);
    let mut bands = preset_bands(preset);
    for band in &mut bands {
        let target_at_fc = linear_interp_log(&f, &target, &[band.fc])[0];
        band.min_gain = target_at_fc - FIXED_BAND_GAIN_RANGE_DB;
        band.max_gain = target_at_fc + FIXED_BAND_GAIN_RANGE_DB;
    }

    let mut solver = Solver::new(f, FS, bands, target);
    solver.optimize()?;
    let out_filters = cageq_peq_solver::bands_to_filters(&solver.bands);
    let preamp = preamp_db(&solver.fr());

    let result = (out_filters, preamp);
    cache.lock().unwrap().insert(key, result.clone());
    Ok(result)
}
