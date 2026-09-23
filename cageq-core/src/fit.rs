//! Replaces the sidecar's `calculate_filters` RPC — turns a [`CalcRequest`] into fitted
//! filters plus the two curve-derived quantities [`Applied`] needs, by driving
//! [`cageq_peq_solver`] directly instead of asking Python to do the whole thing.
//!
//! Nothing here reaches the sidecar at all any more: fetching the raw measurement/
//! target curves — the one piece that used to need Python (AutoEq's database isn't
//! bundled) — is now [`cageq_catalog::fetch_curve`], fetching a *known* catalogue path
//! directly. `sidecar_dsp.py`'s `fetch_raw_curves` method (added for this, then
//! superseded within the same effort once the fetch itself moved to Rust) and
//! `calculate_filters` (the reference implementation this was validated against, see
//! `cageq-core/tests/rust_vs_sidecar.rs`) both still exist and work, just unused by
//! this crate — browsing the catalogue to find a path in the first place
//! (`list_headphones`/`list_targets`/`measurement_curves`) is a separate piece of work
//! (a GitHub tree-API index build, not just a file fetch+parse) still on the sidecar.
//!
//! ## What counts as "no fit needed"
//! A request with neither `headphone` nor `measurement` (and no explicit `flat: true`)
//! is treated as flat: empty AutoEq bands, a zero curve, no reference curve — the same
//! shape `flat: true` already produces in `sidecar_dsp.py`. This is a deliberate
//! choice, not an oversight: nothing in CAGEq's own UI sends a request naming neither a
//! headphone, a measurement, nor `flat`, so this only ever matters for a request that
//! specifies nothing to fit against, and treating "nothing to fit" as "no correction"
//! is the same answer `flat: true` gives on purpose.
//!
//! ## The fit cache
//! Keyed like `sidecar_dsp.py`'s own `_FIT_CACHE` — measurement/target/max_gain/
//! peaking_filters/fs, deliberately *not* including custom filters — so editing a
//! custom filter (the common interactive case, e.g. dragging a slider) reuses the
//! cached SLSQP result and only recombines curves, rather than re-running the ~1-4 s
//! fit on every edit. Plus the [`ResponseModel`]: the fit optimises the curve the backend
//! will actually run, so an RBJ fit and a warping-corrected fit of the same measurement are
//! different answers, and toggling back and forth reuses both.

use std::collections::{HashMap, VecDeque};

use cageq_peq_solver::Solver;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{filter_curve_db_in, validate_filters, CalcRequest, CoreError, CurvePoint, Filter, ResponseModel};

/// A request's inputs, parsed out of [`CalcRequest::inputs`]'s opaque JSON blob. Custom
/// defaults (not the field types' own zero values) via the struct-level `#[serde(default)]`
/// plus a matching [`Default`] impl, mirroring `sidecar_dsp.py`'s own `params.get(key,
/// default)` calls one for one.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct FitParams {
    headphone: Option<String>,
    measurement: Vec<MeasurementPoint>,
    target: Option<String>,
    flat: bool,
    max_gain: f64,
    peaking_filters: usize,
    fs: f64,
    custom_filters: Vec<Filter>,
}

impl Default for FitParams {
    fn default() -> Self {
        FitParams {
            headphone: None,
            measurement: Vec::new(),
            target: None,
            flat: false,
            max_gain: 6.0,
            peaking_filters: 8,
            fs: 48_000.0,
            custom_filters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct MeasurementPoint {
    frequency: f64,
    raw_db: f64,
}

/// AutoEq's `DEFAULT_MAX_SLOPE` — CAGEq never overrides `equalize()`'s `max_slope`, only
/// `max_gain` (see `cageq-peq-solver::equalize`'s own doc for the rest of the
/// parameters CAGEq's call site lets it skip).
const MAX_SLOPE: f64 = 18.0;

pub(crate) struct FitOutcome {
    pub(crate) device: String,
    pub(crate) filters: Vec<Filter>,
    pub(crate) g_target_db: f64,
    pub(crate) g_max_peak_db: f64,
    pub(crate) reference_curve: Vec<CurvePoint>,
}

/// The AutoEq-only fit cache — see the module doc for why custom filters aren't part
/// of the key. A bounded FIFO (`sidecar_dsp.py`'s own `_FIT_CACHE_MAX = 32`, evicting
/// the oldest entry — a Python dict's insertion order does that for free; `order`
/// tracks it explicitly here since `HashMap` doesn't).
pub(crate) struct FitCache {
    entries: HashMap<FitCacheKey, FitCacheEntry>,
    order: VecDeque<FitCacheKey>,
}

const FIT_CACHE_MAX: usize = 32;

#[derive(Clone, PartialEq, Eq, Hash)]
struct FitCacheKey {
    headphone: Option<String>,
    /// Bit-pattern pairs of `(frequency, raw_db)` when `headphone` is `None` — exact
    /// equality on a raw request's own f64s needs no rounding/hashing scheme the way
    /// `sidecar_dsp.py`'s cross-process sha1 digest does, since this cache never leaves
    /// this process.
    measurement_bits: Vec<(u64, u64)>,
    target: String,
    max_gain_bits: u64,
    peaking_filters: usize,
    fs_bits: u64,
    model: ResponseModel,
}

#[derive(Clone)]
struct FitCacheEntry {
    filters: Vec<Filter>,
    f: Vec<f64>,
    curve: Vec<f64>,
    reference_curve: Vec<CurvePoint>,
}

impl FitCache {
    pub(crate) fn new() -> Self {
        FitCache { entries: HashMap::new(), order: VecDeque::new() }
    }

    fn get(&self, key: &FitCacheKey) -> Option<FitCacheEntry> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: FitCacheKey, entry: FitCacheEntry) {
        if !self.entries.contains_key(&key) {
            if self.order.len() >= FIT_CACHE_MAX {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, entry);
    }
}

fn fit_key(params: &FitParams, model: ResponseModel) -> FitCacheKey {
    FitCacheKey {
        headphone: params.headphone.clone(),
        measurement_bits: if params.headphone.is_some() {
            Vec::new()
        } else {
            params.measurement.iter().map(|p| (p.frequency.to_bits(), p.raw_db.to_bits())).collect()
        },
        target: params.target.clone().unwrap_or_default(),
        max_gain_bits: params.max_gain.to_bits(),
        peaking_filters: params.peaking_filters,
        fs_bits: params.fs.to_bits(),
        model,
    }
}

/// Raw, unresampled measurement/target curves — a named `headphone`/`target` fetched
/// via [`cageq_catalog::fetch_curve`] (network+disk-cache, no Python involved), or the
/// request's own inline `measurement` array / the flat-target default (`_target_raw`'s
/// own fallback, `sidecar_dsp.py`: no target named means a flat `[20, 20000] -> [0, 0]`
/// target). No `None`/gap handling needed here: neither a real catalogue CSV (see
/// `cageq-catalog/tests/full_corpus.rs`, checked exhaustively) nor a directly-supplied
/// `measurement` array (`MeasurementPoint`'s `raw_db` is a plain `f64`, matching
/// `_measurement_fr`'s own array path) can carry a gap — only a hand-edited custom
/// measurement fed through some future UI path might, and this crate doesn't have one
/// yet.
fn fetch_raw_curves(params: &FitParams) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>), CoreError> {
    let (measurement_f, measurement_raw) = if let Some(hp) = &params.headphone {
        cageq_catalog::fetch_curve(hp)?
    } else {
        // `_measurement_fr` (sidecar_dsp.py:271-272) raises on fewer than 2 points
        // rather than silently interpolating a single point into a flat curve.
        if params.measurement.len() < 2 {
            return Err(CoreError::InvalidMeasurement(format!(
                "measurement array must have at least 2 points, got {}",
                params.measurement.len()
            )));
        }
        (params.measurement.iter().map(|p| p.frequency).collect(), params.measurement.iter().map(|p| p.raw_db).collect())
    };

    let (target_f, target_raw) = if let Some(t) = &params.target {
        cageq_catalog::fetch_curve(t)?
    } else {
        (vec![20.0, 20_000.0], vec![0.0, 0.0])
    };

    Ok((measurement_f, measurement_raw, target_f, target_raw))
}

/// `sidecar_dsp.py`'s `_subsample_curve`: a compact, evenly-spaced sampling of a dense
/// curve for the UI chart — the grid is already log-spaced, so evenly-spaced indices
/// keep the sampling log-even.
pub(crate) fn subsample_curve(f: &[f64], db: &[f64], n: usize) -> Vec<CurvePoint> {
    let m = f.len();
    let mut idx: Vec<usize> = if m <= n {
        (0..m).collect()
    } else {
        let step = m as f64 / n as f64;
        let mut idx: Vec<usize> = (0..n).map(|i| (i as f64 * step) as usize).collect();
        if *idx.last().unwrap() != m - 1 {
            idx.push(m - 1);
        }
        idx
    };
    idx.dedup();
    idx.into_iter().map(|i| CurvePoint { f: (f[i] * 100.0).round() / 100.0, db: (db[i] * 1000.0).round() / 1000.0 }).collect()
}

/// Runs (or reuses from [`FitCache`]) the AutoEq-only fit: fetch, FR-prep, `equalize`,
/// SLSQP. Returns `(filters, f, curve, reference_curve)` — the same four values
/// `sidecar_dsp.py`'s `_autoeq_fit` returns, computed the same way.
fn run_autoeq_fit(
    cache: &std::sync::Mutex<FitCache>,
    params: &FitParams,
    model: ResponseModel,
) -> Result<(Vec<Filter>, Vec<f64>, Vec<f64>, Vec<CurvePoint>), CoreError> {
    let key = fit_key(params, model);
    if let Some(entry) = cache.lock().unwrap().get(&key) {
        return Ok((entry.filters, entry.f, entry.curve, entry.reference_curve));
    }

    let (mf, mr, tf, tr) = fetch_raw_curves(params)?;
    let prepped = cageq_peq_solver::prepare(&mf, &mr, &tf, &tr);
    let equalization = cageq_peq_solver::equalize(&prepped.f, &prepped.error_smoothed, MAX_SLOPE, params.max_gain);

    // `FrequencyResponse._optimize_peq_filters` re-interpolates onto its own coarser
    // grid before the SLSQP band search runs — see `grid::DEFAULT_BIQUAD_OPTIMIZATION_
    // F_STEP`'s doc. `reference_curve` below still uses the finer standard-grid
    // `equalization` untouched, matching `_autoeq_fit`'s own `reference_curve` (built
    // from `fr.equalization` before that re-gridding).
    let opt_f = cageq_peq_solver::grid::biquad_optimization_grid();
    let opt_equalization = cageq_peq_solver::grid::linear_interp_log(&prepped.f, &equalization, &opt_f);

    // `sidecar_dsp.py`'s config: a low shelf at 105 Hz and a high shelf at 10 kHz
    // (both `q = 0.7`, gain free), plus `peaking_filters` fully-free peaking bands.
    let mut bands = cageq_peq_solver::cageq_default_bands_in(params.peaking_filters, crate::biquad_model(model));
    let warm = if model == ResponseModel::Rbj {
        false
    } else {
        // Warping-corrected: start from the RBJ fit (from this cache when it was already made)
        // rather than AutoEq's init heuristic. Not a constraint — what keeps either fit sane is
        // the solver's cancellation penalty (`Solver::cancellation_penalty`) — but the two
        // realisations only differ near the top, so the RBJ answer is the natural start, and a
        // model toggle then moves the bands a little instead of reshuffling them.
        // `bands_to_filters` keeps band order, so the two line up one to one.
        let (rbj_filters, ..) = run_autoeq_fit(cache, params, ResponseModel::Rbj)?;
        for (band, f) in bands.iter_mut().zip(&rbj_filters) {
            (band.fc, band.q, band.gain) = (f.freq_hz, f.q, f.gain_db);
        }
        true
    };
    let mut solver = Solver::new(opt_f.clone(), params.fs, bands, opt_equalization);
    if warm {
        solver.optimize_warm()?;
    } else {
        solver.optimize()?;
    }

    let filters = cageq_peq_solver::bands_to_filters(&solver.bands);
    let curve = solver.fr();
    let reference_curve = subsample_curve(&prepped.f, &equalization, 140);

    cache.lock().unwrap().insert(key, FitCacheEntry { filters: filters.clone(), f: opt_f.clone(), curve: curve.clone(), reference_curve: reference_curve.clone() });
    Ok((filters, opt_f, curve, reference_curve))
}

/// The whole replacement for `calculate_filters`: fit (or reuse) the AutoEq portion,
/// combine with the user's custom filters (validated up front, same checks
/// `sidecar_dsp.py`'s `_custom_filters` raises on), and derive the §4.1/§4.2
/// loudness/peak quantities from the combined curve — matching
/// `sidecar_dsp.py::calculate_filters` field for field.
///
/// Everything is realised in `model` — the fitted bands, the custom bands' curve, and so the
/// loudness/peak quantities — so the preamp is composed for the curve that will actually play.
pub(crate) fn compute(cache: &std::sync::Mutex<FitCache>, request: &CalcRequest, model: ResponseModel) -> Result<FitOutcome, CoreError> {
    let params: FitParams = serde_json::from_value(Value::Object(request.inputs.clone()))?;
    validate_filters(&params.custom_filters)?;

    let treat_as_flat = params.flat || (params.headphone.is_none() && params.measurement.is_empty());
    let (autoeq_filters, f, autoeq_curve, reference_curve) = if treat_as_flat {
        let f = cageq_peq_solver::grid::standard_grid();
        let zeros = vec![0.0; f.len()];
        (Vec::new(), f, zeros, Vec::new())
    } else {
        run_autoeq_fit(cache, &params, model)?
    };

    let custom_curve = filter_curve_db_in(&params.custom_filters, &f, model);
    let combined: Vec<f64> = autoeq_curve.iter().zip(&custom_curve).map(|(a, c)| a + c).collect();

    let g_target_db = cageq_peq_solver::loudness_target_db(&f, &combined, params.fs);
    let g_max_peak_db = cageq_peq_solver::curve_peak_db(&combined);

    let mut filters = autoeq_filters;
    filters.extend(params.custom_filters);

    Ok(FitOutcome {
        device: request.device.clone(),
        filters,
        g_target_db: (g_target_db * 100.0).round() / 100.0,
        g_max_peak_db: (g_max_peak_db * 100.0).round() / 100.0,
        reference_curve,
    })
}

/// The §4.1/§4.2 curve quantities of an already-known band set in `model`, on the standard
/// grid and rounded exactly as [`compute`] rounds them. For a slot whose model changed but
/// that cannot be re-fitted because the core never saw its request (a fit restored from the
/// launch cache, §3.5): its bands stay as they were until the next edit re-fits them, but the
/// preamp is at least composed for the curve those bands now actually produce.
pub(crate) fn curve_quantities(filters: &[Filter], model: ResponseModel) -> (f64, f64) {
    let f = cageq_peq_solver::grid::standard_grid();
    let curve = filter_curve_db_in(filters, &f, model);
    let g_target_db = cageq_peq_solver::loudness_target_db(&f, &curve, FitParams::default().fs);
    let g_max_peak_db = cageq_peq_solver::curve_peak_db(&curve);
    ((g_target_db * 100.0).round() / 100.0, (g_max_peak_db * 100.0).round() / 100.0)
}
