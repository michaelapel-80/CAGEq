//! Port of `PEQ` (`peq.py:432-753`) — fits a set of [`Band`]s to a target curve via
//! SLSQP, the algorithm `peq.py:717`'s comment says beat every other SciPy `minimize`
//! method tried.
//!
//! ## Where this deliberately diverges from `peq.py`, and why
//!
//! `fmin_slsqp`'s `callback` (`peq.py:657-704`) implements a bespoke early-stop rule —
//! moving-average loss change-rate and rolling standard deviation across the last 8
//! iterations — and on early stop, restores the best-seen point from its history
//! (`peq.py:723-725`). The `nlopt` crate's safe API doesn't hand back a live handle
//! `force_stop`-able from inside the objective closure itself (see the crate-level doc
//! comment on the toolchain/API status), so replicating that rule verbatim isn't
//! straightforward. This port instead:
//!   - leans on NLopt's own convergence criteria (`ftol_rel`/`xtol_rel`/`maxeval`) to
//!     decide *when* to stop, and
//!   - **always** tracks and restores the best-loss point seen across every objective
//!     evaluation, not only on an early-stop path — a strict superset of what
//!     `peq.py`'s restore does, since a converged SLSQP run's final point is (barring a
//!     late excursion past a bound) already its own best point.
//!
//! This is a real behavioural difference, not a transcription detail, and it is exactly
//! what `tests/solver_fixtures.rs` exists to catch: it compares final loss and fitted
//! curve against the live `peq.py`, not just the formulas in isolation. If the fixture
//! comparison shows this converges to a worse optimum than SciPy on some target shape,
//! the tolerances here (`XTOL_REL`/`FTOL_REL`/`MAXEVAL`) are the knobs to revisit first.
//!
//! **Build status**: the `nlopt` crate's exact method signatures used below
//! (`set_lower_bounds`, `ObjFn`'s closure shape, `recover_user_data`) are transcribed
//! from its published API surface, not confirmed by a local `cargo build` — this
//! machine has MSVC but no CMake yet, which `nlopt-sys` needs to vendor NLopt's C/C++
//! core. Expect small signature fixups once that gap is closed.

use nlopt::{Algorithm, Nlopt, Target};

use crate::filter::{Band, BandKind};

const XTOL_REL: f64 = 1e-10;
const FTOL_REL: f64 = 1e-12;
const MAXEVAL: u32 = 10_000;
/// Central-difference step fraction — SLSQP needs a gradient and `peq.py` supplies none
/// (SciPy falls back to its own forward-difference approximation); central costs one
/// extra evaluation per dimension in exchange for a visibly less noisy gradient.
const GRAD_EPS: f64 = 1e-6;

#[derive(Debug, Clone)]
pub struct OptimizeReport {
    pub loss: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum SolverError {
    #[error("nlopt setup failed: {0:?}")]
    Setup(nlopt::FailState),
    #[error("nlopt optimization failed: {0:?} (loss {1})")]
    Optimize(nlopt::FailState, f64),
}

/// Port of `PEQ`: the frequency grid, sample rate, target curve and the bands being
/// fit to it.
pub struct Solver {
    pub f: Vec<f64>,
    pub fs: f64,
    pub bands: Vec<Band>,
    pub target: Vec<f64>,
    min_f_ix: usize,
    max_f_ix: usize,
    /// `PEQ.__init__`'s `self._10k_ix` (peq.py:449) — a literal 10 kHz index, distinct
    /// from `Band::ix10k`'s same-named-but-different quantity (see that method's doc).
    ix_10k: usize,
}

impl Solver {
    /// `min_f`/`max_f` fixed at AutoEq's defaults (20 Hz / 20 kHz,
    /// `DEFAULT_PEQ_OPTIMIZER_MIN_F`/`MAX_F`) — CAGEq never overrides them.
    pub fn new(f: Vec<f64>, fs: f64, bands: Vec<Band>, target: Vec<f64>) -> Self {
        let min_f_ix = argmin_abs(&f, 20.0);
        let max_f_ix = argmin_abs(&f, 20_000.0);
        let ix_10k = argmin_abs(&f, 10_000.0);
        Solver { f, fs, bands, target, min_f_ix, max_f_ix, ix_10k }
    }

    /// `PEQ.fr` (peq.py:537-540): the cascade response, bands summed in dB.
    pub fn fr(&self) -> Vec<f64> {
        let mut total = vec![0.0; self.f.len()];
        for band in &self.bands {
            for (t, v) in total.iter_mut().zip(band.fr(&self.f, self.fs)) {
                *t += v;
            }
        }
        total
    }

    /// `_init_optimizer_bounds` (peq.py:642-655). `fc` bounds are in log10, matching
    /// `_parse_optimizer_params`'s `10 ** params[i]` on the way back out.
    fn init_bounds(&self) -> Vec<(f64, f64)> {
        let mut bounds = Vec::new();
        for b in &self.bands {
            if b.optimize_fc {
                bounds.push((b.min_fc.log10(), b.max_fc.log10()));
            }
            if b.optimize_q {
                bounds.push((b.min_q, b.max_q));
            }
            if b.optimize_gain {
                bounds.push((b.min_gain, b.max_gain));
            }
        }
        bounds
    }

    /// `_init_optimizer_params` (peq.py:602-640): initializes bands in a fixed priority
    /// order — by filter class and which of fc/q are free, tie-broken by fc-range
    /// narrowness, most-constrained first — each against the residual left after
    /// previously-initialized bands' own response is subtracted from the target. The
    /// *optimizer's* parameter vector still follows plain per-band order, so the
    /// results are re-flattened back into that order before returning.
    fn init_params(&mut self) -> Vec<f64> {
        fn kind_ix(kind: BandKind) -> usize {
            match kind {
                BandKind::Peaking => 0,
                BandKind::LowShelf => 1,
                BandKind::HighShelf => 2,
            }
        }
        // AutoEq's `order` table (peq.py:607-619), flattened to a rank lookup.
        const ORDER: [(usize, bool, bool); 12] = [
            (0, true, true),
            (1, true, true),
            (2, true, true),
            (0, true, false),
            (1, true, false),
            (2, true, false),
            (0, false, true),
            (1, false, true),
            (2, false, true),
            (0, false, false),
            (1, false, false),
            (2, false, false),
        ];
        let rank = |kind: BandKind, opt_fc: bool, opt_q: bool| -> usize {
            ORDER.iter().position(|&(k, fc, q)| k == kind_ix(kind) && fc == opt_fc && q == opt_q).expect("every (kind, optimize_fc, optimize_q) combination is covered")
        };
        let init_order_value = |b: &Band| -> f64 {
            let mut v = rank(b.kind, b.optimize_fc, b.optimize_q) as f64 * 100.0;
            if b.optimize_fc {
                v += 1.0 / (b.max_fc / b.min_fc).log2();
            }
            v
        };

        let mut order: Vec<usize> = (0..self.bands.len()).collect();
        // `reverse=True` in Python: highest init-order value first.
        order.sort_by(|&a, &b| init_order_value(&self.bands[b]).partial_cmp(&init_order_value(&self.bands[a])).unwrap());

        let mut per_band_params: Vec<Vec<f64>> = vec![Vec::new(); self.bands.len()];
        let mut remaining = self.target.clone();
        let (f, fs) = (self.f.clone(), self.fs);
        for ix in order {
            let params = self.bands[ix].init(&f, &remaining, fs);
            let band_fr = self.bands[ix].fr(&f, fs);
            for (r, v) in remaining.iter_mut().zip(&band_fr) {
                *r -= v;
            }
            per_band_params[ix] = params;
        }
        per_band_params.into_iter().flatten().collect()
    }

    /// `_parse_optimizer_params` (peq.py:567-583): writes the optimizer's flat parameter
    /// vector back onto each band's fc/q/gain, in the same per-band order
    /// `init_bounds`/`init_params` produced it in.
    fn apply_params(&mut self, params: &[f64]) {
        let mut i = 0;
        for band in &mut self.bands {
            if band.optimize_fc {
                band.fc = 10f64.powf(params[i]);
                i += 1;
            }
            if band.optimize_q {
                band.q = params[i];
                i += 1;
            }
            if band.optimize_gain {
                band.gain = params[i];
                i += 1;
            }
        }
    }

    /// Which band and which of its fields each entry of the optimizer's flat parameter
    /// vector controls — built once per [`Solver::optimize`] call and reused by
    /// [`Solver::evaluate`]'s gradient sweep, which perturbs one entry at a time and
    /// needs to know which single band to patch.
    fn param_map(&self) -> Vec<ParamSlot> {
        let mut map = Vec::new();
        for (band_ix, band) in self.bands.iter().enumerate() {
            if band.optimize_fc {
                map.push(ParamSlot { band_ix, field: Field::Fc });
            }
            if band.optimize_q {
                map.push(ParamSlot { band_ix, field: Field::Q });
            }
            if band.optimize_gain {
                map.push(ParamSlot { band_ix, field: Field::Gain });
            }
        }
        map
    }

    /// The MSE component of `_optimizer_loss` (peq.py:590-596) for an already-computed
    /// cascade response — split out from [`Solver::loss`] so the gradient sweep in
    /// [`Solver::evaluate`] can feed it an incrementally-patched cascade instead of a
    /// freshly summed one.
    fn mse_component(&self, cascade: &[f64]) -> f64 {
        let mut fr = cascade.to_vec();
        let mut target = self.target.clone();
        let tail_t = mean(&target[self.ix_10k..]);
        let tail_f = mean(&fr[self.ix_10k..]);
        target[self.ix_10k..].iter_mut().for_each(|v| *v = tail_t);
        fr[self.ix_10k..].iter_mut().for_each(|v| *v = tail_f);
        mean_sq_diff(&target[self.min_f_ix..self.max_f_ix], &fr[self.min_f_ix..self.max_f_ix])
    }

    /// `_optimizer_loss` (peq.py:585-600) computed the straightforward way: every
    /// band's own response and penalties recomputed from scratch. Used where it's only
    /// called once (outside the optimizer's hot loop) — see [`Solver::evaluate`] for the
    /// version the gradient sweep actually uses.
    fn loss(&self) -> f64 {
        let band_frs: Vec<Vec<f64>> = self.bands.iter().map(|b| b.fr(&self.f, self.fs)).collect();
        let cascade = sum_vectors(&band_frs, self.f.len());
        let penalty_sum: f64 = self.bands.iter().zip(&band_frs).map(|(b, fr)| self.band_penalty_sum(b, fr)).sum();
        self.mse_component(&cascade) + penalty_sum
    }

    fn band_penalty_sum(&self, band: &Band, fr: &[f64]) -> f64 {
        band.sharpness_penalty(fr) + band.band_penalty(&self.f, self.fs, fr)
    }

    /// Loss at `params` (`map` describing which entry controls which band/field), and —
    /// when `grad` is `Some` — a central-difference gradient, all on SLSQP's hot path
    /// (every objective call it makes evaluates this).
    ///
    /// The gradient sweep is `2 * params.len()` extra evaluations (central difference:
    /// `+h`/`-h` per free parameter), and each one changes exactly one band. Recomputing
    /// every band's response for each of those — what a direct transcription of
    /// `_optimizer_loss` would do — is `O(bands)` wasted work per perturbation for
    /// `bands - 1` unperturbed bands, on top of `Band::sharpness_penalty`/`band_penalty`
    /// each separately recomputing that band's own response internally (fixed
    /// separately — see their doc comments). Instead: compute every band's response and
    /// penalty once at the nominal point, then for each perturbation, recompute only the
    /// one band that changed and patch it into a cloned copy of the cached cascade.
    fn evaluate(&mut self, params: &[f64], map: &[ParamSlot], grad: Option<&mut [f64]>) -> f64 {
        self.apply_params(params);
        let band_frs: Vec<Vec<f64>> = self.bands.iter().map(|b| b.fr(&self.f, self.fs)).collect();
        let band_pens: Vec<f64> = self.bands.iter().zip(&band_frs).map(|(b, fr)| self.band_penalty_sum(b, fr)).collect();
        let cascade = sum_vectors(&band_frs, self.f.len());
        let penalty_sum: f64 = band_pens.iter().sum();
        let loss = self.mse_component(&cascade) + penalty_sum;

        if let Some(g) = grad {
            for (i, slot) in map.iter().enumerate() {
                let h = GRAD_EPS * params[i].abs().max(1.0);

                let hi = self.perturbed_loss(slot, params[i] + h, &band_frs, &band_pens, &cascade, penalty_sum);
                let lo = self.perturbed_loss(slot, params[i] - h, &band_frs, &band_pens, &cascade, penalty_sum);
                g[i] = (hi - lo) / (2.0 * h);

                // A band with more than one free parameter (every peaking band optimizes
                // fc, q, *and* gain) gets one `ParamSlot` per field, all sharing the same
                // `band_ix`. Restore this dimension's field to its nominal value right
                // away — not just once at the end — so the *next* dimension's probe
                // starts from a band whose other fields are all nominal, rather than
                // whichever of `params[i] ± h` this one happened to leave behind.
                slot.field.set(&mut self.bands[slot.band_ix], params[i]);
            }
        }

        loss
    }

    /// Loss with a single parameter (`slot`) set to `new_value`, reusing the nominal
    /// point's cached per-band responses/penalties for every *other* band instead of
    /// recomputing them. Leaves `self.bands[slot.band_ix]` mutated to the probed value
    /// (the caller, [`Solver::evaluate`], restores every band to nominal once the whole
    /// gradient sweep is done rather than after each individual probe).
    fn perturbed_loss(&mut self, slot: &ParamSlot, new_value: f64, band_frs: &[Vec<f64>], band_pens: &[f64], base_cascade: &[f64], base_penalty_sum: f64) -> f64 {
        slot.field.set(&mut self.bands[slot.band_ix], new_value);
        let new_fr = self.bands[slot.band_ix].fr(&self.f, self.fs);
        let new_pen = self.band_penalty_sum(&self.bands[slot.band_ix], &new_fr);

        let mut cascade = base_cascade.to_vec();
        let old_fr = &band_frs[slot.band_ix];
        for i in 0..cascade.len() {
            cascade[i] += new_fr[i] - old_fr[i];
        }
        let penalty_sum = base_penalty_sum - band_pens[slot.band_ix] + new_pen;
        self.mse_component(&cascade) + penalty_sum
    }

    /// `PEQ.optimize` (peq.py:706-725). No-op if every band is fully pinned (no free fc/q/gain).
    pub fn optimize(&mut self) -> Result<OptimizeReport, SolverError> {
        let has_free = self.bands.iter().any(|b| b.optimize_fc || b.optimize_q || b.optimize_gain);
        if !has_free {
            return Ok(OptimizeReport { loss: self.loss() });
        }

        let mut params = self.init_params();
        let bounds = self.init_bounds();
        let lower: Vec<f64> = bounds.iter().map(|b| b.0).collect();
        let upper: Vec<f64> = bounds.iter().map(|b| b.1).collect();
        let n = params.len();

        let map = self.param_map();
        let mut opt = Nlopt::new(
            Algorithm::Slsqp,
            n,
            objective,
            Target::Minimize,
            ObjState { solver: self, map, best_loss: f64::INFINITY, best_params: params.clone() },
        );
        opt.set_lower_bounds(&lower).map_err(SolverError::Setup)?;
        opt.set_upper_bounds(&upper).map_err(SolverError::Setup)?;
        opt.set_xtol_rel(XTOL_REL).map_err(SolverError::Setup)?;
        opt.set_ftol_rel(FTOL_REL).map_err(SolverError::Setup)?;
        opt.set_maxeval(MAXEVAL).map_err(SolverError::Setup)?;

        let outcome = opt.optimize(&mut params);
        let state = opt.recover_user_data();
        let (best_params, best_loss) = (state.best_params, state.best_loss);

        match outcome {
            Ok(_) | Err(_) => {
                // Whether NLopt reports success or a soft failure (e.g. `ROUNDOFF_LIMITED`
                // once it's near the optimum), the best point observed during the whole
                // run is what we keep — see the module doc's divergence note.
                self.apply_params(&best_params);
                Ok(OptimizeReport { loss: best_loss })
            }
        }
    }
}

/// Which field of a band one entry of the optimizer's flat parameter vector controls.
/// `Fc` is stored log10-scaled in that vector (matching `init_bounds`/`apply_params`),
/// hence `set` exponentiating it back before writing `band.fc`.
#[derive(Debug, Clone, Copy)]
enum Field {
    Fc,
    Q,
    Gain,
}

impl Field {
    fn set(self, band: &mut Band, param_value: f64) {
        match self {
            Field::Fc => band.fc = 10f64.powf(param_value),
            Field::Q => band.q = param_value,
            Field::Gain => band.gain = param_value,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ParamSlot {
    band_ix: usize,
    field: Field,
}

struct ObjState<'a> {
    solver: &'a mut Solver,
    map: Vec<ParamSlot>,
    best_loss: f64,
    best_params: Vec<f64>,
}

fn objective(params: &[f64], grad: Option<&mut [f64]>, state: &mut ObjState) -> f64 {
    let loss = state.solver.evaluate(params, &state.map, grad);
    if loss < state.best_loss {
        state.best_loss = loss;
        state.best_params = params.to_vec();
    }
    loss
}

fn argmin_abs(xs: &[f64], target: f64) -> usize {
    xs.iter().enumerate().min_by(|(_, a), (_, b)| (*a - target).abs().partial_cmp(&(*b - target).abs()).unwrap()).map(|(i, _)| i).unwrap()
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn mean_sq_diff(a: &[f64], b: &[f64]) -> f64 {
    mean(&a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).collect::<Vec<_>>())
}

fn sum_vectors(vs: &[Vec<f64>], len: usize) -> Vec<f64> {
    let mut total = vec![0.0; len];
    for v in vs {
        for (t, x) in total.iter_mut().zip(v) {
            *t += x;
        }
    }
    total
}
