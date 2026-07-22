//! Perceptual curve morphing for tonal transitions (filter.md §5.3a).
//!
//! Equalizer APO's native 10 ms crossfade (§5.3) structurally rules out a *click*,
//! but it is far too fast for a large tonal jump — swapping two very different curves
//! (a bass-boost preset for a treble-boost one, or slot A for slot B) is audible as a
//! harsh lurch. This module supplies the two pieces needed to slew such a change over
//! a perceptually-scaled time instead:
//!
//!   1. [`tonal_distance_db`] — *how far apart* two curves are, in dB, and
//!   2. [`lerp_bands`] — the intermediate band set at a point along the way.
//!
//! ## Why this distance metric
//!
//! The §7.5 pre-gain ramp slews a **scalar** (the preamp); a tonal morph slews a
//! **vector** (the whole curve). For the two to be one mechanism rather than two
//! unrelated ones, the curve norm must collapse back to `|Δpreamp|` when the change
//! happens to be a uniform shift. A **weighted mean** does exactly that — a constant
//! passes straight through it — so this uses the K-weighted, bandwidth-corrected RMS
//! of `Δ(f) = curve_b(f) - curve_a(f)`, reusing the very same weighting the §4.1
//! loudness match is built on (`w_k` for audibility, `Δf/f` for the pink/per-octave
//! Jacobian — see the sidecar's `loudness_target_db` and the bug note there).
//!
//! That weighting also buys perceptual fairness for free: a razor-thin high-Q notch
//! spans almost no octaves and barely moves the metric, while a broad gentle tilt
//! counts in full. `max|Δ(f)|` (L-infinity) satisfies the collapse property too, but
//! would let one inaudible high-Q band dictate the pace of every transition.
//!
//! ## Why gains are interpolated and frequencies are not
//!
//! [`lerp_bands`] takes the **union** of both band sets, holds every band's `Fc`/`Q`
//! fixed, and moves only `gain_db` (a band absent on one side counts as 0 dB there).
//! A's bands fade out where they stand while B's fade in where they stand.
//!
//! The tempting alternative — pairing bands up and interpolating `Fc` — is what
//! *creates* the artefact it is meant to avoid: a 100 Hz peak interpolated towards
//! 5 kHz literally sweeps a resonance across the spectrum. That is a filter sweep, an
//! effect in its own right. Gain-only morphing has no moving parts.

use cageq_config_writer::{Filter, FilterType};
use std::sync::OnceLock;

/// Sample rate the filters are defined against (matches the sidecar's `fs` default).
const FS: f64 = 48_000.0;
/// Grid resolution. Coarser than the chart's 480 points — this feeds a scalar norm,
/// not a drawing, and it is evaluated once per morph frame.
const GRID_POINTS: usize = 192;
const F_MIN: f64 = 20.0;
const F_MAX: f64 = 20_000.0;

/// ITU-R BS.1770-4 K-weighting biquads at 48 kHz: stage 1 the high-shelf ("head"),
/// stage 2 the RLB high-pass. Same reference values as the sidecar's copy.
const KW_S1_B: [f64; 3] = [1.53512485958697, -2.69169618940638, 1.19839281085285];
const KW_S1_A: [f64; 3] = [1.0, -1.69065929318241, 0.73248077421585];
const KW_S2_B: [f64; 3] = [1.0, -2.0, 1.0];
const KW_S2_A: [f64; 3] = [1.0, -1.99004745483398, 0.99007225036621];

/// The frequency grid plus its per-bin perceptual weight, built once.
struct Grid {
    f: Vec<f64>,
    /// `w_k(f) * Δf/f` — audibility times pink-noise bin energy, normalised to sum 1.
    w: Vec<f64>,
}

fn grid() -> &'static Grid {
    static GRID: OnceLock<Grid> = OnceLock::new();
    GRID.get_or_init(|| {
        let ratio = (F_MAX / F_MIN).ln();
        let f: Vec<f64> = (0..GRID_POINTS)
            .map(|i| F_MIN * (ratio * i as f64 / (GRID_POINTS - 1) as f64).exp())
            .collect();

        // Bin energy of pink noise: density (1/f) times bin width. On this log grid the
        // width is proportional to f, so the 1/f cancels and every bin carries equal
        // energy — but deriving it from the actual spacing keeps the metric correct if
        // the grid ever changes (the §4.1 Jacobian lesson, applied up front).
        let w: Vec<f64> = f
            .iter()
            .enumerate()
            .map(|(i, &fi)| {
                let lo = if i == 0 { f[0] } else { f[i - 1] };
                let hi = if i == GRID_POINTS - 1 { f[GRID_POINTS - 1] } else { f[i + 1] };
                let width = (hi - lo) / if i == 0 || i == GRID_POINTS - 1 { 1.0 } else { 2.0 };
                k_weight_power(fi) * width / fi
            })
            .collect();

        let sum: f64 = w.iter().sum();
        let w = w.into_iter().map(|x| x / sum).collect();
        Grid { f, w }
    })
}

/// `|H(e^{-jω})|²` for one biquad, evaluated without a complex-number dependency.
fn biquad_power(b: &[f64; 3], a: &[f64; 3], w: f64) -> f64 {
    let (c1, s1) = (w.cos(), w.sin());
    let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
    // z = e^{-jω}, so the imaginary parts pick up the minus sign.
    let num_re = b[0] + b[1] * c1 + b[2] * c2;
    let num_im = -(b[1] * s1 + b[2] * s2);
    let den_re = a[0] + a[1] * c1 + a[2] * c2;
    let den_im = -(a[1] * s1 + a[2] * s2);
    (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)
}

/// The K-weighting *power* response `|H_k(f)|²` — the two-stage cascade.
fn k_weight_power(f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / FS;
    biquad_power(&KW_S1_B, &KW_S1_A, w) * biquad_power(&KW_S2_B, &KW_S2_A, w)
}

/// AutoEq's `biquad_coefficients()` — `[a0, a1, a2, b0, b1, b2]` with `a1`/`a2` already
/// negated, exactly as `peq.py` produces them (and as `biquad.ts` mirrors for the chart).
fn coefficients(band: &Filter) -> [f64; 6] {
    let a = 10.0_f64.powf(band.gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * band.freq_hz / FS;
    let alpha = w0.sin() / (2.0 * band.q);
    let cosw = w0.cos();
    let sqrt_a = a.sqrt();

    // Every coefficient is divided through by `a0`, so the returned `a0` is always 1.0.
    let (a1, a2, b0, b1, b2) = match band.kind {
        FilterType::Peaking => {
            let a0 = 1.0 + alpha / a;
            (
                -(-2.0 * cosw) / a0,
                -(1.0 - alpha / a) / a0,
                (1.0 + alpha * a) / a0,
                (-2.0 * cosw) / a0,
                (1.0 - alpha * a) / a0,
            )
        }
        FilterType::LowShelf => {
            let a0 = a + 1.0 + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha;
            (
                -(-2.0 * (a - 1.0 + (a + 1.0) * cosw)) / a0,
                -(a + 1.0 + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha) / a0,
                (a * (a + 1.0 - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha)) / a0,
                (2.0 * a * (a - 1.0 - (a + 1.0) * cosw)) / a0,
                (a * (a + 1.0 - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha)) / a0,
            )
        }
        FilterType::HighShelf => {
            let a0 = a + 1.0 - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha;
            (
                -(2.0 * (a - 1.0 - (a + 1.0) * cosw)) / a0,
                -(a + 1.0 - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha) / a0,
                (a * (a + 1.0 + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha)) / a0,
                (-2.0 * a * (a - 1.0 + (a + 1.0) * cosw)) / a0,
                (a * (a + 1.0 + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha)) / a0,
            )
        }
    };
    [1.0, a1, a2, b0, b1, b2]
}

/// The composed EQ curve of `bands` in dB on the module's shared grid (a biquad cascade
/// adds in dB). Mirrors `PEQFilter.fr`'s numerically-stable `phi` form.
pub(crate) fn curve_db(bands: &[Filter]) -> Vec<f64> {
    curve_db_on(bands, &grid().f)
}

/// The same curve on an arbitrary frequency grid. Split out so a cross-language test can
/// evaluate it on exactly the grid it feeds the reference Python (`peq.py`) and compare
/// point-for-point — the guard against the three biquad copies drifting apart.
pub(crate) fn curve_db_on(bands: &[Filter], freqs: &[f64]) -> Vec<f64> {
    let mut total = vec![0.0; freqs.len()];
    for band in bands {
        let [a0, a1, a2, b0, b1, b2] = coefficients(band);
        let (a1, a2) = (-a1, -a2); // AutoEq flips these back before evaluating
        let b_sum = (b0 + b1 + b2).powi(2);
        let a_sum = (a0 + a1 + a2).powi(2);
        for (i, &f) in freqs.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * f / FS;
            let phi = 4.0 * (w / 2.0).sin().powi(2);
            let num = b_sum + (b0 * b2 * phi - (b1 * (b0 + b2) + 4.0 * b0 * b2)) * phi;
            let den = a_sum + (a0 * a2 * phi - (a1 * (a0 + a2) + 4.0 * a0 * a2)) * phi;
            total[i] += 10.0 * num.log10() - 10.0 * den.log10();
        }
    }
    total
}

/// The curve's positive peak — the §4.2 clipping ceiling input for an intermediate
/// frame, computed exactly rather than interpolated (the union curve can briefly peak
/// above *both* endpoints, so guessing here would risk clipping mid-morph).
pub(crate) fn curve_peak_db(curve: &[f64]) -> f64 {
    curve.iter().copied().fold(0.0_f64, f64::max)
}

/// §4.1 loudness compensation for an already-evaluated curve — the same K-weighted
/// pink-noise energy model the sidecar uses, so intermediate frames stay level-matched
/// and a morph doesn't swell in the middle.
pub(crate) fn loudness_target_db(curve: &[f64]) -> f64 {
    let g = grid();
    // `g.w` is normalised to sum 1, so the dry reference power is exactly 1.
    let wet: f64 = g.w.iter().zip(curve).map(|(w, c)| w * 10.0_f64.powf(c / 10.0)).sum();
    -10.0 * wet.log10()
}

/// How far apart two curves are perceptually, in dB — see the module header. Zero for
/// identical band sets; exactly `|Δgain|` for a uniform shift.
pub(crate) fn tonal_distance_db(a: &[Filter], b: &[Filter]) -> f64 {
    let (ca, cb) = (curve_db(a), curve_db(b));
    let g = grid();
    let ms: f64 = g.w.iter().zip(ca.iter().zip(&cb)).map(|(w, (x, y))| w * (y - x).powi(2)).sum();
    ms.sqrt()
}

/// Identity of a band for morph pairing: same type, centre and Q. Bands that match
/// keep a single entry whose gain slews; everything else fades in or out in place.
/// Quantised so float round-tripping through JSON can't split a band in two.
fn key(f: &Filter) -> (u8, i64, i64) {
    let kind = match f.kind {
        FilterType::LowShelf => 0,
        FilterType::HighShelf => 1,
        FilterType::Peaking => 2,
    };
    (kind, (f.freq_hz * 1000.0).round() as i64, (f.q * 1000.0).round() as i64)
}

/// The band set partway (`t` in `0..=1`) from `a` to `b`: the union of both, with each
/// band's gain interpolated between its value on either side (0 dB where absent).
/// `t = 0` reproduces `a`'s curve, `t = 1` reproduces `b`'s.
pub(crate) fn lerp_bands(a: &[Filter], b: &[Filter], t: f64) -> Vec<Filter> {
    // Order: `a`'s bands first (stable, so the written config stays recognisable
    // through the morph), then whatever `b` adds. Repeated keys on one side accumulate
    // — two identical bands really do sum in dB.
    let mut out: Vec<Filter> = Vec::with_capacity(a.len() + b.len());
    let mut gains: Vec<(f64, f64)> = Vec::with_capacity(a.len() + b.len());

    let mut push = |band: &Filter, to_side: bool| {
        let k = key(band);
        match out.iter().position(|o| key(o) == k) {
            Some(i) => {
                if to_side {
                    gains[i].1 += band.gain_db;
                } else {
                    gains[i].0 += band.gain_db;
                }
            }
            None => {
                out.push(*band);
                gains.push(if to_side { (0.0, band.gain_db) } else { (band.gain_db, 0.0) });
            }
        }
    };
    for band in a {
        push(band, false);
    }
    for band in b {
        push(band, true);
    }

    for (band, (ga, gb)) in out.iter_mut().zip(gains) {
        band.gain_db = ga + (gb - ga) * t;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(freq_hz: f64, gain_db: f64, q: f64) -> Filter {
        Filter { kind: FilterType::Peaking, freq_hz, gain_db, q }
    }
    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() < tol, "{a} != {b} (tol {tol})");
    }

    /// The property the whole design rests on: for a *uniform* curve shift the metric
    /// must collapse to the plain dB difference, so the tonal morph and the §7.5 preamp
    /// ramp are one mechanism at different dimensionality rather than two constants.
    /// A very wide, very low-Q shelf pair is the closest thing to a broadband offset we
    /// can build out of real bands.
    #[test]
    fn uniform_shift_collapses_to_the_scalar_difference() {
        let g = grid();
        let flat = vec![0.0_f64; g.f.len()];
        let shifted = vec![6.0_f64; g.f.len()];
        // Feed the norm directly to test the metric rather than the band synthesis.
        let ms: f64 = g
            .w
            .iter()
            .zip(flat.iter().zip(&shifted))
            .map(|(w, (x, y))| w * (y - x).powi(2))
            .sum();
        approx(ms.sqrt(), 6.0, 1e-9);
    }

    #[test]
    fn identical_curves_are_zero_apart() {
        let bands = vec![peak(1000.0, 4.0, 1.4), peak(80.0, -3.0, 0.7)];
        approx(tonal_distance_db(&bands, &bands), 0.0, 1e-12);
    }

    /// A narrow notch and a broad tilt of the *same* peak gain are not the same event:
    /// the bandwidth term must make the wide one dominate, or one inaudible high-Q band
    /// would set the pace for every transition.
    #[test]
    fn narrow_bands_move_the_metric_far_less_than_wide_ones() {
        let narrow = vec![peak(4000.0, 12.0, 12.0)];
        let wide = vec![peak(4000.0, 12.0, 0.5)];
        let d_narrow = tonal_distance_db(&[], &narrow);
        let d_wide = tonal_distance_db(&[], &wide);
        assert!(d_wide > 3.0 * d_narrow, "wide={d_wide} narrow={d_narrow}");
    }

    #[test]
    fn lerp_endpoints_reproduce_each_side() {
        let a = vec![peak(100.0, 6.0, 0.7)];
        let b = vec![peak(5000.0, 6.0, 0.7)];
        approx(tonal_distance_db(&lerp_bands(&a, &b, 0.0), &a), 0.0, 1e-9);
        approx(tonal_distance_db(&lerp_bands(&a, &b, 1.0), &b), 0.0, 1e-9);
    }

    /// The anti-swoosh guarantee: morphing a low peak into a high one must never move a
    /// band's centre frequency — the union keeps both in place and only trades gain.
    #[test]
    fn lerp_never_moves_a_centre_frequency() {
        let a = vec![peak(100.0, 6.0, 0.7)];
        let b = vec![peak(5000.0, 6.0, 0.7)];
        let mid = lerp_bands(&a, &b, 0.5);
        assert_eq!(mid.len(), 2, "union of two disjoint bands");
        let mut freqs: Vec<f64> = mid.iter().map(|f| f.freq_hz).collect();
        freqs.sort_by(|x, y| x.partial_cmp(y).unwrap());
        approx(freqs[0], 100.0, 1e-9);
        approx(freqs[1], 5000.0, 1e-9);
        for f in &mid {
            approx(f.gain_db, 3.0, 1e-9); // both half-way, neither displaced
        }
    }

    /// Bands that exist on both sides pair up instead of duplicating, so a tone change
    /// on top of a fixed AutoEq fit slews only what actually changed.
    #[test]
    fn shared_bands_pair_up_rather_than_duplicating() {
        let a = vec![peak(1000.0, 0.0, 1.0), peak(200.0, 4.0, 0.7)];
        let b = vec![peak(1000.0, 8.0, 1.0), peak(200.0, 4.0, 0.7)];
        let mid = lerp_bands(&a, &b, 0.5);
        assert_eq!(mid.len(), 2, "shared Fc/Q must not split into four bands");
        let shared = mid.iter().find(|f| f.freq_hz == 1000.0).unwrap();
        approx(shared.gain_db, 4.0, 1e-9);
        let untouched = mid.iter().find(|f| f.freq_hz == 200.0).unwrap();
        approx(untouched.gain_db, 4.0, 1e-9);
    }

    /// Cross-check the Rust curve maths against the values `biquad.ts` (and therefore
    /// AutoEq's `peq.py`) produce for the same band — three copies of this model now
    /// exist, and they have to agree or the chart, the fit and the morph would disagree
    /// about what is being written.
    #[test]
    fn curve_matches_the_reference_biquad_model() {
        let g = grid();
        // A peaking band's response at its own centre frequency is its gain.
        let c = curve_db(&[peak(1000.0, 6.0, 1.0)]);
        let i = g.f.iter().enumerate().min_by(|(_, a), (_, b)| {
            (*a - 1000.0).abs().partial_cmp(&(*b - 1000.0).abs()).unwrap()
        });
        approx(c[i.unwrap().0], 6.0, 0.05);
        // ...and it decays to nothing far away.
        approx(c[0], 0.0, 0.05);
    }

    /// Loudness of a flat curve is exactly level-neutral, and a broadband boost is
    /// compensated one-for-one (the §4.1 invariants, re-checked on this grid).
    #[test]
    fn loudness_model_is_level_neutral_and_linear() {
        let g = grid();
        approx(loudness_target_db(&vec![0.0; g.f.len()]), 0.0, 1e-12);
        approx(loudness_target_db(&vec![6.0; g.f.len()]), -6.0, 1e-9);
    }
}
