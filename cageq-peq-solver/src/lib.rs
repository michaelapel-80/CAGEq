//! A Rust port of the slice of [AutoEq](https://github.com/jaakkopasanen/AutoEq)'s
//! `autoeq.peq` that CAGEq actually uses: fitting a parametric EQ (a low shelf, a high
//! shelf, and N peaking bands) to a target frequency-response curve via SLSQP.
//!
//! ## Why this exists
//! CAGEq's only use of Python is a sidecar process whose sole real job is calling into
//! AutoEq for this fit (`cageq-sidecar/python/sidecar_dsp.py`) — paying a Python
//! interpreter's cold-start cost on every app launch for ~850 LOC of `autoeq.peq` +
//! `autoeq.frequency_response` prep, out of AutoEq's ~6,300 LOC total (the rest —
//! `dbtools/`'s scrapers, the webapp, plotting, batch CLI tooling — is dead weight this
//! app never calls). This crate is the fit itself, ported so that call becomes an
//! in-process function and the sidecar can eventually go away entirely. The
//! measurement/target *database* needs no port at all — it's already plain CSV/TSV
//! fetched over HTTP (`sidecar_dsp.py`'s own GitHub-API fetch-and-cache), which the
//! orchestrator can do directly.
//!
//! ## A fourth copy of the biquad math, on purpose
//! CAGEq already carries three copies of AutoEq's biquad response model — `peq.py`
//! itself, `biquad.ts` (the §5.2 chart), and `cageq_core::morph` (the tonal-morph
//! metric, `cageq-core/src/morph.rs`) — each with its own reason to exist rather than
//! share code (see `morph.rs`'s module doc and `cageq-core/tests/biquad_crosscheck.rs`).
//! This crate is a fourth: `cageq_core::morph`'s copy is `pub(crate)` to `cageq-core`
//! and only evaluates the *cascade* curve, while the optimizer needs each band's own
//! individual response (`HighShelf`/`LowShelf::init`'s weighted average,
//! `sharpness_penalty`, `band_penalty`) — and `cageq-core` depends on `cageq-sidecar`,
//! the very thing this crate exists to make optional, so depending on it here would be
//! backwards. [`filter::Band::fr`] is transcribed independently from `peq.py`, not
//! derived from `morph.rs`, and cross-checked against the live `peq.py` in
//! `tests/solver_fixtures.rs` — the same discipline that pins the other three copies
//! together.
//!
//! ## Status
//! All three pieces `sidecar_dsp.py`'s `calculate_filters` chains together are ported
//! and passing their fixture cross-checks against the live `autoeq` package:
//!
//!   1. **FR prep** ([`prep::prepare`], built on [`grid`]/[`smoothing`]) —
//!      `interpolate`/`center`/`compensate`/`smoothen`. Validated to within `1e-6` of
//!      `peq.py` (in practice exact) on every `tests/fixtures_prep/*.json` fixture —
//!      this and (2) below are direct formula/filter math with no non-convex search to
//!      land differently, so parity is checked tightly rather than with a
//!      fit-quality-style tolerance band.
//!   2. **`equalize()`** ([`equalize::equalize`], `frequency_response.py:542-807`) —
//!      the step that turns the smoothed error curve into the actual target the fit
//!      chases: peak/dip detection, a "protection mask" around dips, a left-to-right
//!      *and* right-to-left slope-limited traversal combined with `min`, and a final
//!      re-smoothing. Bit-exact (`0.0` max abs diff) against `peq.py` on every
//!      `tests/fixtures_prep/*.json` fixture, including one built specifically to force
//!      the slope limiter to clip. See its module doc for the several parameters/branches
//!      CAGEq's one fixed call site (`fr.equalize(max_gain=...)`, everything else at
//!      AutoEq's default) lets this skip or simplify away entirely.
//!   3. **The PEQ fit itself** ([`optimizer::Solver`], `filter`, `peaks`) — biquad
//!      coefficients, per-band response, the `find_peaks` port, loss/penalty formulas,
//!      init heuristics, and the SLSQP driver (via the `nlopt` crate — needs CMake on
//!      top of the usual MSVC Rust prerequisites to build `nlopt-sys`). Validated
//!      against `tests/fixtures/*.json` on fit quality (loss, curve RMSE), not
//!      bit-exact parameters — see `optimizer.rs`'s module doc for the one deliberate
//!      behavioural divergence from `peq.py` (early-stopping and "restore the best
//!      point").
//!
//! **Wired into the app**: `cageq-core`'s `fit.rs` drives this crate directly in place
//! of the sidecar's `calculate_filters` (validated end to end against the live sidecar
//! in `cageq-core/tests/rust_vs_sidecar.rs`), and — via `cageq-catalog` — fetches
//! measurement/target CSVs straight from GitHub instead of asking Python to. The
//! sidecar is no longer in the main apply path at all; it's still needed only for
//! catalogue *browsing* (`list_headphones`/`list_targets`/`measurement_curves` — a
//! GitHub tree-API index build this crate doesn't do, separate from fetching a curve
//! once a path is known) and for `fit_export_eq`/`fit_fixed_band_eq`.

pub mod equalize;
pub mod filter;
pub mod grid;
pub mod loudness;
#[cfg(feature = "slsqp")]
pub mod optimizer;
pub mod peaks;
pub mod prep;
pub mod smoothing;

pub use equalize::equalize;
pub use filter::{Band, BandKind};
pub use loudness::{curve_peak_db, loudness_target_db};
#[cfg(feature = "slsqp")]
pub use optimizer::{OptimizeReport, Solver, SolverError};
pub use prep::{prepare, PreppedCurve};

/// A fitted [`Band`] set as the domain type every backend already speaks
/// (`cageq_backend::Filter`) — the boundary this crate hands its result across.
pub fn bands_to_filters(bands: &[Band]) -> Vec<cageq_backend::Filter> {
    bands
        .iter()
        .map(|b| cageq_backend::Filter {
            kind: match b.kind {
                BandKind::Peaking => cageq_backend::FilterType::Peaking,
                BandKind::LowShelf => cageq_backend::FilterType::LowShelf,
                BandKind::HighShelf => cageq_backend::FilterType::HighShelf,
            },
            freq_hz: b.fc,
            gain_db: b.gain,
            q: b.q,
        })
        .collect()
}

/// The band set CAGEq's `calculate_filters` config builds (`sidecar_dsp.py:406-412`):
/// a low shelf at 105 Hz and a high shelf at 10 kHz (both `q = 0.7`, gain free), plus
/// `peaking` fully-free peaking bands.
pub fn cageq_default_bands(peaking: usize) -> Vec<Band> {
    cageq_default_bands_in(peaking, cageq_biquad::ResponseModel::Rbj)
}

/// [`cageq_default_bands`], every band realised in `model` — so the fit optimises the curve
/// the backend will actually apply.
pub fn cageq_default_bands_in(peaking: usize, model: cageq_biquad::ResponseModel) -> Vec<Band> {
    let mut bands = vec![
        Band::fixed_fc_q(BandKind::LowShelf, 105.0, 0.7),
        Band::fixed_fc_q(BandKind::HighShelf, 10_000.0, 0.7),
    ];
    bands.extend((0..peaking).map(|_| Band::free(BandKind::Peaking)));
    for b in &mut bands {
        b.model = model;
    }
    bands
}
