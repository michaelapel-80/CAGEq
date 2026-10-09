//! Replaces the sidecar's `fit_export_eq` RPC (filter.md §8, mobile export) — a
//! *second*, independent AutoEq PEQ pass that fits directly to a
//! slot's own composed curve (`filters`, the same `Band[]` `Applied::filters` already
//! carries), not to a headphone measurement the way `fit.rs`'s main path does.
//!
//! It reuses [`cageq_peq_solver::Solver`] wholesale — what differs from the main fit is
//! what curve gets fitted (a slot's own cascade via [`filter_curve_db`] rather than a
//! measurement's error curve), the free band count, and that it drops AutoEq's treble rule,
//! which only protects against measurement noise (see [`EXPORT_MAX_FC`]). It doesn't need
//! `equalize()` at all: the target *is* the curve, with no peak/dip-detection or
//! slope-limiting step in between.
//!
//! The graphic-EQ export used to be a second solver pass here too (AutoEq's 10-/31-band
//! presets, gains fitted for an assumed constant-Q peaking filter per slider). It is now
//! plain curve sampling in the frontend (`exportFormats.ts`): real graphic EQs don't share
//! that filter model, so overlap-compensated gains were a guess at best.
//!
//! `sidecar_dsp.py`'s `_optimize_peq_filters` (which `optimize_parametric_eq` funnels
//! through) actually re-grids onto
//! `DEFAULT_BIQUAD_OPTIMIZATION_F_STEP` (`1.02`) for the SLSQP pass itself, rather than
//! the standard grid's `1.01` — a difference this port doesn't reproduce, matching the
//! same simplification already made (and empirically validated:
//! `cageq-core/tests/rust_vs_sidecar.rs` shows curve-shape/loudness agreement well
//! within noise) for the main fit in `fit.rs`.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use cageq_peq_solver::{grid::standard_grid, BandKind, Solver};

use crate::{filter_curve_db_in, validate_filters, CoreError, Filter, FilterType, ResponseModel};

const FS: f64 = 48_000.0;
/// Highest centre frequency an exported peaking band may take — above AutoEq's 10 kHz cap.
///
/// The export fit drops AutoEq's treble rule (match only the mean above 10 kHz, no band above
/// it): that rule exists because a *measurement* is unreliable up there, but the export's target
/// is the slot's own composed filter curve, exact, with nothing to overcompensate. Measured with
/// `examples/export_sweep.rs` (real + synthetic headphone fits, 6-16 bands): the full-range loss
/// alone cuts the error above 10 kHz 3-15x with no cost below it, and the raised ceiling is what
/// a hand-placed treble band needs — a 6 dB notch at 12 or 15 kHz came out ~4.4 dB off at worst,
/// ~0.2-0.6 dB with it. 14 kHz still missed a 15 kHz notch; 18 kHz made the top end worse.
const EXPORT_MAX_FC: f64 = 16_000.0;
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
                cageq_backend::FilterType::HighPass => 5,
            };
            // Rounded like `sidecar_dsp.py`'s own cache-key digests (`round(x, 2)`/
            // `round(x, 4)`) — this is a cache key, not a computation, so quantizing
            // avoids two floating-point-identical-looking requests missing each other
            // over noise several decimal places below anything audible.
            (kind, (f.freq_hz * 100.0).round() as i64, (f.gain_db * 100.0).round() as i64, (f.q * 10_000.0).round() as i64)
        })
        .collect()
}

/// A small bounded FIFO cache for `fit_export_eq` (`sidecar_dsp.py`'s `_EXPORT_FIT_CACHE`)
/// — the export dialog's band-count control re-fits on every change, and a repeat value
/// should be instant, not a second SLSQP pass.
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

// Two models in the key: how the slot's bands are realised (the curve being approximated),
// and how the receiving app realises the *exported* bands (what the fit optimises).
type ExportKey = (Vec<(u8, i64, i64, i64)>, u32, ResponseModel, ResponseModel);
type FitResult = (Vec<Filter>, f64);

pub(crate) type ExportCache = BoundedCache<ExportKey, FitResult>;

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
///
/// `model` is how `filters` are realised on the desktop — the curve being approximated, i.e.
/// what is actually heard. `band_model` is how the *receiving app* will realise the exported
/// bands, and so what the fit optimises: RBJ for nearly every phone/desktop EQ app, the
/// warping-corrected model for one that designs its filters that way. Independent on purpose —
/// a warping-corrected desktop curve exported to an RBJ app is the common case.
///
/// High-pass sections are passed through rather than fitted: no shelf or peaking band can
/// follow a 24-48 dB/oct stopband, and the solver would bend every low band trying. They are
/// left out of the target curve and appended to the result as they are (`HPQ` lines, which
/// EqAPO-syntax importers either honour or skip), on top of `band_count`.
pub(crate) fn fit_export_eq(
    cache: &Mutex<ExportCache>,
    filters: &[Filter],
    band_count: u32,
    model: ResponseModel,
    band_model: ResponseModel,
) -> Result<FitResult, CoreError> {
    validate_filters(filters)?;
    let band_count = band_count.max(3);
    let key = (filters_key(filters), band_count, model, band_model);
    if let Some(cached) = cache.lock().unwrap().get(&key) {
        return Ok(cached);
    }

    let (high_pass, shaping): (Vec<Filter>, Vec<Filter>) = filters.iter().partition(|f| f.kind == FilterType::HighPass);
    let f = standard_grid();
    let target = filter_curve_db_in(&shaping, &f, model);
    let mut bands = cageq_peq_solver::cageq_default_bands_in((band_count - 2) as usize, crate::biquad_model(band_model));
    for band in bands.iter_mut().filter(|b| b.kind == BandKind::Peaking) {
        band.max_fc = EXPORT_MAX_FC;
    }

    let mut solver = Solver::new(f, FS, bands, target);
    solver.tail_mean = false; // see EXPORT_MAX_FC
    solver.optimize()?;
    let mut out_filters = cageq_peq_solver::bands_to_filters(&solver.bands);
    // A high-pass only ever cuts, so the preamp computed without it still holds with it.
    let preamp = preamp_db(&solver.fr());
    out_filters.extend(high_pass);

    let result = (out_filters, preamp);
    cache.lock().unwrap().insert(key, result.clone());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(kind: FilterType, freq_hz: f64, gain_db: f64, q: f64) -> Filter {
        Filter { kind, freq_hz, gain_db, q }
    }

    /// High-pass sections don't enter the fit: the fitted bands are exactly those of the same
    /// cascade without them, and the sections come back unchanged after them.
    #[test]
    fn high_pass_sections_pass_through_the_export_fit() {
        let shaping = vec![
            band(FilterType::LowShelf, 105.0, 6.0, 0.7),
            band(FilterType::Peaking, 3000.0, -4.0, 2.0),
            band(FilterType::Peaking, 8000.0, 3.0, 1.0),
        ];
        let hp = [band(FilterType::HighPass, 20.0, 0.0, 0.5412), band(FilterType::HighPass, 20.0, 0.0, 1.3066)];
        let mut with_hp = shaping.clone();
        with_hp.extend(hp);

        let cache = Mutex::new(ExportCache::new());
        let (plain, plain_preamp) = fit_export_eq(&cache, &shaping, 5, ResponseModel::Rbj, ResponseModel::Rbj).unwrap();
        let (fitted, preamp) = fit_export_eq(&cache, &with_hp, 5, ResponseModel::Rbj, ResponseModel::Rbj).unwrap();

        assert_eq!(fitted.len(), plain.len() + 2);
        for (a, b) in fitted.iter().zip(&plain) {
            assert_eq!((a.kind, a.freq_hz, a.gain_db, a.q), (b.kind, b.freq_hz, b.gain_db, b.q));
        }
        for (a, b) in fitted[plain.len()..].iter().zip(&hp) {
            assert_eq!((a.kind, a.freq_hz, a.q), (b.kind, b.freq_hz, b.q));
        }
        assert_eq!(preamp, plain_preamp);
    }
}
