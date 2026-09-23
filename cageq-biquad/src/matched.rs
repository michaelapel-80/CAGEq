//! Candidate warping-corrected designs (Stage 0 spike). Two families:
//!
//! * [`prescribed`] — Muranov (2025, UCSD MS thesis, ch. 3), building on Orfanidis (1997,
//!   "prescribed Nyquist-frequency gain"): keep the bilinear transform, but design the
//!   *analog* section it is applied to so that, after warping, the digital response hits
//!   the analog prototype's gain at DC, at Nyquist, at the peak, and at one bandwidth point.
//!   Peaking and band-pass only — the thesis does not solve shelves (its §3.6).
//! * [`mz`] — "matched" design: poles mapped by impulse invariance (`z = e^{s}`, exact pole
//!   positions, no warping), numerator chosen so `|H|²` equals the analog prototype's at
//!   three frequencies (DC, Nyquist, one mid point). Applies to any second-order section,
//!   shelves included, which is why it is in the spike at all.

use std::f64::consts::PI;

use crate::analog::Prototype;
use crate::{Band, Coeffs, Kind};

/// Why a design could not produce a filter. The spike counts these; a production design
/// must have none inside CAGEq's parameter ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Failure {
    /// The method does not cover this filter type.
    Unsupported,
    /// The constraint system had no real solution (a negative square, a zero divisor).
    NoSolution,
}

/// A band exactly at 0 dB is the identity for peaking/shelves; every method short-circuits
/// to it, which also sidesteps the 0/0 the prescribed peaking formula has there.
const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

// ---------------------------------------------------------------------------------------
// Prescribed-gain BLT (thesis / Orfanidis)
// ---------------------------------------------------------------------------------------

/// Thesis §3.1 (peaking) and §3.2 (band-pass).
///
/// The general analog section, in `s` with the BLT constant `c = ω0/tan(ω0/2)` (pre-warp at
/// `ω0`, so `ω0` is a fixed point of the mapping):
///
/// ```text
/// Hd(s) = (G1·s² + B·(ωd/Qd)·s + G0·ωd²) / (s² + (ωd/Qd)·s + ωd²)
/// ```
///
/// `G0`/`G1` are the prototype's gains at DC/Nyquist (Nyquist is `s → ∞` under the BLT, so
/// `G1` pins the digital gain *at* Nyquist to the analog one). `ωd` places the extremum at
/// `ω0` (Orfanidis' closed form). With `ωd` fixed, the two gain constraints — `|Hd|² = g²`
/// at `ω0` and `|Hd|² = GB²` at the pre-warped bandwidth point — are linear in
/// `X = B²/Qd²` and `P = 1/Qd²`, so they are solved as a 2×2 system rather than via the
/// thesis's expanded eq. 3.15/3.23 (fewer places for a transcription error to hide).
pub fn prescribed(band: &Band, fs: f64) -> Result<Coeffs, Failure> {
    let w0 = band.w0(fs);
    let proto = Prototype::of(band.kind, band.gain_db, band.q);
    let (g2, g0, gb2, xb) = match band.kind {
        Kind::Peaking => {
            if band.gain_db == 0.0 {
                return Ok(IDENTITY);
            }
            let g = 10f64.powf(band.gain_db / 20.0);
            // RBJ's Q is defined at the midpoint gain √g, whose lower frequency is
            // independent of the gain: |1 − x²| = x/Q.
            let q = band.q;
            (g * g, 1.0, g, (-1.0 / q + (1.0 / (q * q) + 4.0).sqrt()) / 2.0)
        }
        Kind::Bandpass => {
            // Thesis: GB = 1/2 "through trial and error". Lower point of |H|² = GB²:
            // (1 − x²)² = x²·k²/Q², k² = 1/GB² − 1.
            let gb2: f64 = 0.25;
            let k = (1.0 / gb2 - 1.0).sqrt();
            let q = band.q;
            (1.0, 0.0, gb2, (-k / q + (k * k / (q * q) + 4.0).sqrt()) / 2.0)
        }
        Kind::LowShelf | Kind::HighShelf => return Err(Failure::Unsupported),
    };
    let g1sq = proto.power_x(PI / w0);
    let g1 = g1sq.sqrt();

    let ratio = (g2 - g1sq) / (g2 - g0 * g0);
    if !(ratio > 0.0) {
        return Err(Failure::NoSolution);
    }
    let wd2 = w0 * w0 * ratio.sqrt();

    let c = w0 / (w0 / 2.0).tan();
    let wb = xb * w0; // digital-domain bandwidth point...
    let wbp = c * (wb / 2.0).tan(); // ...pre-warped into the analog design domain

    // |Hd(Ω)|²·den = (G1Ω² − G0ωd²)² + X·Ω²ωd², den = (Ω² − ωd²)² + P·Ω²ωd².
    // Target T at Ω: X − T·P = (T·(Ω² − ωd²)² − (G1Ω² − G0ωd²)²) / (Ω²ωd²).
    let rhs = |om2: f64, t: f64| (t * (om2 - wd2).powi(2) - (g1 * om2 - g0 * wd2).powi(2)) / (om2 * wd2);
    let r1 = rhs(w0 * w0, g2);
    let r2 = rhs(wbp * wbp, gb2);
    if gb2 == g2 {
        return Err(Failure::NoSolution);
    }
    let p = (r1 - r2) / (gb2 - g2);
    let x = r1 + g2 * p;
    if !(p > 0.0) || !(x >= 0.0) {
        return Err(Failure::NoSolution);
    }
    let (inv_qd, b_over_qd) = (p.sqrt(), x.sqrt());

    let n = c / wd2.sqrt();
    let n2 = n * n;
    let b = [n2 * g1 + n * b_over_qd + g0, 2.0 * (g0 - n2 * g1), n2 * g1 - n * b_over_qd + g0];
    let a = [n2 + n * inv_qd + 1.0, 2.0 * (1.0 - n2), n2 - n * inv_qd + 1.0];
    let coeffs = Coeffs::from_raw(b, a);
    if coeffs.is_finite() { Ok(coeffs) } else { Err(Failure::NoSolution) }
}

/// One constraint on the general section's `|Hd|²`, at digital frequency `x·ω0` (placed
/// below Nyquist by [`place`]): its value, or its slope, equal to the prototype's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Con {
    Value(f64),
    Slope(f64),
}

/// The general section of [`prescribed`] under any three [`Con`]straints, with `G0`/`G1`
/// pinned to the prototype's DC/Nyquist gains (the prescribed-Nyquist idea). This is how
/// the spike extends the thesis to shelves, which it leaves open (§3.6: "more research is
/// needed to determine a system of constraints").
///
/// **Linear, not a root-find.** In `U = Ω²/ω0²` (Ω the pre-warped design-domain frequency,
/// so `U = 1` exactly at `ω0`) the section's squared magnitude is
///
/// ```text
/// |Hd|² = N/D = (G1²·U² + n1·U + G0²·d0) / (U² + d1·U + d0)
/// ```
///
/// A value constraint `N = T·D` and a slope constraint `N' = T·D' + T'·D` (`'` = d/dU) are
/// both *linear* in `(n1, d1, d0)` — so any three of them are one 3×3 solve with one answer.
/// (An earlier version searched `ωd` for a root; same answers, slower, and fragile.)
/// Realisable iff `d0 > 0`, damping `P = d1/√d0 + 2 > 0`, numerator term
/// `X = n1/√d0 + 2·G0·G1 ≥ 0`; then `ωd = ω0·d0^¼`, `1/Qd = √P`, `B/Qd = √X`, and the BLT
/// table of [`prescribed`] gives the coefficients.
///
/// With `[Value(1), Slope(1), Value(x_B)]` on a peaking band this *is* the thesis's §3.1
/// design (value g² and zero slope at ω0, midpoint gain at the lower bandwidth point) —
/// see the `constrained_reproduces_the_thesis_peaking` test.
pub fn constrained(band: &Band, fs: f64, cons: [Con; 3]) -> Result<Coeffs, Failure> {
    constrained_toward_rbj(band, fs, cons, 0.0, 0.0)
}

/// [`constrained`], with a guaranteed, continuous way out where the constraints cannot be met.
///
/// Near Nyquist a second-order section cannot follow the prototype at all: every digital
/// response has zero slope *at* Nyquist, while a high shelf at 16 kHz is still rising there,
/// so "the prototype's gain at Nyquist" plus "the prototype's slope at ω0" asks for more than
/// two poles and two zeros can give (the solve comes back with `X < 0` — a numerator whose
/// magnitude would have to go negative). The spike measured this for Q ≥ 0.5 shelves above
/// ~0.63·Nyquist, for every gain.
///
/// The way out: blend every target — each value and slope, and the Nyquist gain —
/// geometrically from the prototype's (`λ = 0`) toward the RBJ filter's own (`λ = 1`).
/// RBJ is itself a member of this family (the pre-warped BLT of the prototype), and the
/// solve is unique, so at `λ = 1` the answer *is* RBJ: a feasible endpoint always exists.
/// Bisect for the smallest feasible `λ`. Continuous in every band parameter (the optimizer
/// and a live drag both need that), full correction wherever it is achievable, and a gradual
/// hand-back to RBJ only where it is not.
///
/// "Feasible" includes a margin: both the section's pole and zero damping must stay at least
/// `margin` × the prototype's own. Right at the feasibility edge the numerator damping goes to
/// zero — a notch on the unit circle — so the edge itself is not an acceptable answer.
pub fn relaxed(band: &Band, fs: f64, cons: [Con; 3], margin: f64) -> Result<Coeffs, Failure> {
    if let Ok(c) = constrained_toward_rbj(band, fs, cons, 0.0, margin) {
        return Ok(c);
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if constrained_toward_rbj(band, fs, cons, mid, margin).is_ok() { hi = mid } else { lo = mid }
    }
    // `hi` is feasible by construction (or is 1.0, RBJ, which is always a valid filter).
    constrained_toward_rbj(band, fs, cons, hi, margin).or_else(|_| Ok(crate::rbj::coefficients(band, fs)))
}

fn constrained_toward_rbj(band: &Band, fs: f64, cons: [Con; 3], lambda: f64, margin: f64) -> Result<Coeffs, Failure> {
    if band.gain_db == 0.0 && band.kind != Kind::Bandpass {
        return Ok(IDENTITY);
    }
    // The slope row below is linearised using `N = T·D` at the same point, so a slope
    // constraint is only meaningful alongside a value constraint at that frequency.
    for con in cons {
        if let Con::Slope(x) = con {
            if !cons.contains(&Con::Value(x)) {
                return Err(Failure::Unsupported);
            }
        }
    }
    let w0 = band.w0(fs);
    let proto = Prototype::of(band.kind, band.gain_db, band.q);
    let g0 = proto.power_x(0.0).sqrt();
    // Nyquist target: the prototype's gain *at* Nyquist (λ = 0), or at s → ∞, which is where
    // the BLT puts Nyquist and so what RBJ has there (λ = 1).
    let g1_analog = proto.power_x(PI / w0).sqrt();
    let g1_rbj = (proto.n[0] / proto.d[0]).abs();
    let g1 = g1_analog.powf(1.0 - lambda) * g1_rbj.powf(lambda);
    let c = w0 / (w0 / 2.0).tan();

    let mut m = [[0.0; 3]; 3];
    let mut rhs = [0.0; 3];
    for (i, con) in cons.iter().enumerate() {
        let x = match *con {
            Con::Value(x) | Con::Slope(x) => x,
        };
        let w = place(x, w0);
        let tn = (w / 2.0).tan();
        let u = (c * tn / w0).powi(2);
        // Target and its U-slope: the prototype at the *true* frequency (what we want), and
        // at the *warped* one, x = √U (what RBJ delivers), blended in log.
        let ta = proto.power_x(w / w0);
        let tr = proto.power_x(u.sqrt());
        let t = ta.powf(1.0 - lambda) * tr.powf(lambda);
        match con {
            // N − T·D = 0:  n1·U − T·U·d1 + (G0² − T)·d0 = (T − G1²)·U²
            Con::Value(_) => {
                m[i] = [u, -t * u, g0 * g0 - t];
                rhs[i] = (t - g1 * g1) * u * u;
            }
            // N' − T·D' − T'·D = 0 with N' = 2G1²U + n1, D' = 2U + d1:
            //   n1 − (T + T'U)·d1 − T'·d0 = 2TU − 2G1²U + T'U²
            Con::Slope(_) => {
                // Analog: dT/dU = (dT/dw)/(dU/dw), dT/dw = power_x_slope/ω0,
                // dU/dw = (c/ω0)²·tan·sec². RBJ: d/dU of power_x(√U) = power_x_slope/(2√U).
                let ta_p = (proto.power_x_slope(w / w0) / w0) / ((c / w0).powi(2) * tn * (1.0 + tn * tn));
                let tr_p = proto.power_x_slope(u.sqrt()) / (2.0 * u.sqrt());
                let tp = t * ((1.0 - lambda) * ta_p / ta + lambda * tr_p / tr);
                m[i] = [1.0, -(t + tp * u), -tp];
                rhs[i] = 2.0 * t * u - 2.0 * g1 * g1 * u + tp * u * u;
            }
        }
    }
    let [n1, d1, d0] = solve3(m, rhs).ok_or(Failure::NoSolution)?;
    if !(d0 > 0.0) {
        return Err(Failure::NoSolution);
    }
    let wd2n = d0.sqrt(); // (ωd/ω0)²
    let p = d1 / wd2n + 2.0;
    let x = n1 / wd2n + 2.0 * g0 * g1;
    if !(p > 0.0) || !(x >= -1e-12) {
        return Err(Failure::NoSolution);
    }
    if margin > 0.0 {
        let (_, zeta_proto) = proto.pole_shape();
        let zeta_den = p.sqrt() / 2.0;
        let zeta_num = x.max(0.0).sqrt() / (2.0 * (g0 * g1).sqrt());
        if zeta_den < margin * zeta_proto || zeta_num < margin * zeta_proto {
            return Err(Failure::NoSolution);
        }
    }
    let (inv_qd, b_over_qd) = (p.sqrt(), x.max(0.0).sqrt());

    let n = c / (w0 * wd2n.sqrt());
    let n2 = n * n;
    let b = [n2 * g1 + n * b_over_qd + g0, 2.0 * (g0 - n2 * g1), n2 * g1 - n * b_over_qd + g0];
    let a = [n2 + n * inv_qd + 1.0, 2.0 * (1.0 - n2), n2 - n * inv_qd + 1.0];
    let coeffs = Coeffs::from_raw(b, a);
    if coeffs.is_finite() { Ok(coeffs) } else { Err(Failure::NoSolution) }
}

/// RBJ's peaking bandwidth point: the lower frequency (in units of ω0) of the midpoint gain,
/// `|1 − x²| = x/Q` — independent of the gain, which is what RBJ's Q means.
pub fn rbj_peak_bandwidth_point(q: f64) -> f64 {
    (-1.0 / q + (1.0 / (q * q) + 4.0).sqrt()) / 2.0
}

/// Gaussian elimination with partial pivoting; `None` when (numerically) singular.
fn solve3(mut m: [[f64; 3]; 3], mut v: [f64; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let piv = (col..3).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[piv][col].abs() < 1e-300 {
            return None;
        }
        m.swap(col, piv);
        v.swap(col, piv);
        for r in col + 1..3 {
            let f = m[r][col] / m[col][col];
            for k in col..3 {
                m[r][k] -= f * m[col][k];
            }
            v[r] -= f * v[col];
        }
    }
    let mut out = [0.0; 3];
    for r in (0..3).rev() {
        let s: f64 = (r + 1..3).map(|k| m[r][k] * out[k]).sum();
        out[r] = (v[r] - s) / m[r][r];
    }
    out.iter().all(|x| x.is_finite()).then_some(out)
}

/// Digital frequency for match point `x·ω0`. Points above `ω0` are mapped onto the gap
/// `(ω0, 0.95π)` by `ω0 + gap·(1 − e^{−(x−1)·ω0/gap})`: that is `≈ x·ω0` while the gap is
/// wide (low `ω0`), and saturates below Nyquist as it closes, so a shelf near Nyquist still
/// gets distinct points on the correct side of its corner. One smooth formula rather than a
/// threshold, because the optimizer differentiates through this.
fn place(x: f64, w0: f64) -> f64 {
    if x <= 1.0 {
        return x * w0;
    }
    let gap = 0.95 * PI - w0;
    w0 + gap * (1.0 - (-(x - 1.0) * w0 / gap).exp())
}

// ---------------------------------------------------------------------------------------
// Ivantsov: per-quadratic closed-form mapping
// ---------------------------------------------------------------------------------------

/// Y. Ivantsov, "On the ideal bilinear and biquadratic digital filter" (2025, rev. 2026,
/// ivantsovy.com/research/paper1.pdf), §3.2. Each s-plane quadratic `a·s² + b·s + 1` — the
/// numerator and the denominator *separately* — is mapped to `1 + c1·z⁻¹ + c2·z⁻²` by closed
/// algebraic functions `φ1, φ2` of `x = ω·√a` (`ω = fs/fc`) and `y = b/(2√a)` (a damping
/// ratio), with one free parameter `σ` (the paper recommends `σ ∈ [2, π·√(2/3)]`, `σ = 2`).
/// Mapping each factor on its own keeps a left-half-plane root inside the unit circle, so
/// stability *and* minimum phase hold by construction — no solve that can come back
/// infeasible.
///
/// His table, rewritten with RBJ's `ζ = 1/(2Q)` and amplitude gain `g`, is exactly the RBJ
/// prototypes (checked against [`Prototype`]): high shelf numerator `(ω·g^¼, ζ)` over
/// denominator `(ω·g^-¼, ζ)`, low shelf the reverse, peaking ("band-shelf") `(ω, ζ·√g)` over
/// `(ω, ζ/√g)`. So CAGEq's Q keeps its meaning.
pub fn ivantsov(band: &Band, fs: f64, sigma: f64) -> Result<Coeffs, Failure> {
    if band.gain_db == 0.0 && band.kind != Kind::Bandpass {
        return Ok(IDENTITY);
    }
    let om = fs / band.freq_hz;
    let zeta = 1.0 / (2.0 * band.q);
    let g = 10f64.powf(band.gain_db / 20.0);
    let s2 = sigma * sigma;
    let pi2 = PI * PI;
    let phi = |x: f64, y: f64| -> (f64, f64) {
        let x2 = x * x;
        let nu = (x2 * x2 + 2.0 * x2 * s2 * (2.0 * y * y - 1.0) + s2 * s2).sqrt();
        let kappa = x2 * (2.0 * y * y - 1.0) + s2;
        let r = PI * std::f64::consts::SQRT_2 * (nu + kappa).sqrt();
        let d = pi2 + r + nu;
        ((2.0 * pi2 - 2.0 * nu) / d, (pi2 - r + nu) / d)
    };
    let ((a1, a2), (b1, b2), dc) = match band.kind {
        Kind::HighShelf => (phi(om * g.powf(0.25), zeta), phi(om * g.powf(-0.25), zeta), 1.0),
        Kind::LowShelf => (phi(om * g.powf(-0.25), zeta), phi(om * g.powf(0.25), zeta), g),
        Kind::Peaking => (phi(om, zeta * g.sqrt()), phi(om, zeta / g.sqrt()), 1.0),
        Kind::Bandpass => {
            // Zeros at s = 0 and s = ∞ map through the first-order φ to constants:
            // φ(0) = (π − σ)/(π + σ), φ(∞) = −1. Gain set below for a unity peak (CAGEq's
            // band-pass convention) rather than from the paper's G.
            let p0 = (PI - sigma) / (PI + sigma);
            let (b1, b2) = phi(om, zeta);
            let num = [1.0, p0 - 1.0, -p0];
            let raw = Coeffs { b0: num[0], b1: num[1], b2: num[2], a1: b1, a2: b2 };
            let k = 1.0 / raw.power(band.w0(fs)).sqrt();
            let c = Coeffs { b0: num[0] * k, b1: num[1] * k, b2: num[2] * k, a1: b1, a2: b2 };
            return if c.is_finite() { Ok(c) } else { Err(Failure::NoSolution) };
        }
    };
    // H = G·(1+β1+β2)·(1 + α1 z⁻¹ + α2 z⁻²)/(1 + β1 z⁻¹ + β2 z⁻²), G = dc/(1+α1+α2): unity
    // (or g, for the low shelf) at DC.
    let k = dc * (1.0 + b1 + b2) / (1.0 + a1 + a2);
    let c = Coeffs { b0: k, b1: k * a1, b2: k * a2, a1: b1, a2: b2 };
    if c.is_finite() { Ok(c) } else { Err(Failure::NoSolution) }
}

/// M. Vicanek, "Matched Two-Pole Digital Shelving Filters" (2024, rev. 2025,
/// vicanek.de/articles/2poleShelvingFits.pdf), transcribed from its appendix pseudocode.
/// **Butterworth only** (`Q = 1/√2`); `band.q` is ignored — the spike only compares it on
/// Q = 1/√2 bands. Matches DC, maximal flatness at DC, Nyquist, and two points
/// `f1, f2` (numerically tuned by the author to keep every root real).
pub fn vicanek_butterworth_shelf(band: &Band, fs: f64) -> Result<Coeffs, Failure> {
    let high = match band.kind {
        Kind::HighShelf => true,
        Kind::LowShelf => false,
        _ => return Err(Failure::Unsupported),
    };
    let gain = 10f64.powf(band.gain_db / 20.0);
    let g = if (1.0 - gain).abs() < 1e-6 { 1.00001 } else if high { gain } else { 1.0 / gain };
    let fc = band.freq_hz / (fs / 2.0);
    let invg = 1.0 / g;
    let fc4 = fc.powi(4);
    let hny = (fc4 + g) / (fc4 + invg);
    let point = |f: f64| {
        let f4 = f.powi(4);
        ((fc4 + f4 * g) / (fc4 + f4 * invg), (PI / 2.0 * f).sin().powi(2))
    };
    let (h1, phi1) = point(fc / (0.160 + 1.543 * fc * fc).sqrt());
    let (h2, phi2) = point(fc / (0.947 + 3.806 * fc * fc).sqrt());
    let d1 = (h1 - 1.0) * (1.0 - phi1);
    let c11 = -phi1 * d1;
    let c12 = phi1 * phi1 * (hny - h1);
    let d2 = (h2 - 1.0) * (1.0 - phi2);
    let c21 = -phi2 * d2;
    let c22 = phi2 * phi2 * (hny - h2);
    let alfa1 = (c22 * d1 - c12 * d2) / (c11 * c22 - c12 * c21);
    let aa1 = (d1 - c11 * alfa1) / c12;
    let bb1 = hny * aa1;
    let aa2 = 0.25 * (alfa1 - aa1);
    let bb2 = 0.25 * (alfa1 - bb1);
    let v = 0.5 * (1.0 + aa1.sqrt());
    let w = 0.5 * (1.0 + bb1.sqrt());
    let a0 = 0.5 * (v + (v * v + aa2).sqrt());
    let inva0 = 1.0 / a0;
    let a1 = (1.0 - v) * inva0;
    let a2 = -0.25 * aa2 * inva0 * inva0;
    let c = if high {
        let b0 = (0.5 * (w + (w * w + bb2).sqrt())) * inva0;
        // As the paper writes it: b2 from the *scaled* b0, then scaled by inva0² (equivalent
        // to −¼·B2/b0_raw·inva0).
        Coeffs { b0, b1: (1.0 - w) * inva0, b2: (-0.25 * bb2 / b0) * inva0 * inva0, a1, a2 }
    } else {
        let ginva0 = invg * inva0;
        let b0raw = 0.5 * (w + (w * w + bb2).sqrt());
        Coeffs { b0: b0raw * ginva0, b1: (1.0 - w) * ginva0, b2: (-0.25 * bb2 / b0raw) * ginva0, a1, a2 }
    };
    if c.is_finite() { Ok(c) } else { Err(Failure::NoSolution) }
}

/// [`vicanek_butterworth_shelf`] generalised to any Q (this spike's extension, not in the
/// paper). Vicanek's scheme in `φ = sin²(ω/2)`, DC normalised to 1:
///
/// ```text
/// |H|² = (1 − φ + β1·φ(1−φ) + β2·φ²) / (1 − φ + α1·φ(1−φ) + α2·φ²)
/// ```
///
/// Near DC `|H|² ≈ 1 + (β1 − α1)·φ`. His "maximal flatness" condition `β1 = α1` is simply the
/// Butterworth prototype's zero DC curvature; for general Q, match the prototype's own:
/// `β1 = α1 + s`, `s = d|H|²/dφ` at DC. With `β2 = hNy·α2` (Nyquist) the two point matches stay
/// linear in `(α1, α2)`; only their right-hand side gains a `−s·φ(1−φ)` term. Match points
/// are his (tuned for Butterworth), so how well they carry over to other Q is exactly what
/// the spike measures. Low shelf as `g·HS(1/g)`, which holds for RBJ shelves at any Q.
pub fn vicanek_shelf(band: &Band, fs: f64) -> Result<Coeffs, Failure> {
    let high = match band.kind {
        Kind::HighShelf => true,
        Kind::LowShelf => false,
        _ => return Err(Failure::Unsupported),
    };
    if band.gain_db == 0.0 {
        return Ok(IDENTITY);
    }
    // Design the high shelf of gain g (low shelf: of 1/g, rescaled at the end).
    let gain_db = if high { band.gain_db } else { -band.gain_db };
    let proto = Prototype::of(Kind::HighShelf, gain_db, band.q);
    let fc = band.freq_hz / (fs / 2.0); // Nyquist units
    let w0 = PI * fc; // rad/sample
    let h_at = |f: f64| proto.power_x(PI * f / w0);
    let hny = h_at(1.0);
    // DC curvature: |H|² = 1 + k·x² + …, x = ω/ω0; φ ≈ (ω/2)² ⇒ d|H|²/dφ = 4k/ω0².
    let k = {
        let [n2, n1, n0] = proto.n;
        let [d2, d1, d0] = proto.d;
        ((n1 * n1 - 2.0 * n0 * n2) * d0 * d0 - n0 * n0 * (d1 * d1 - 2.0 * d0 * d2)) / (d0 * d0 * d0 * d0)
    };
    let s = 4.0 * k / (w0 * w0);
    let point = |f: f64| (h_at(f), (PI / 2.0 * f).sin().powi(2));
    let (h1, phi1) = point(fc / (0.160 + 1.543 * fc * fc).sqrt());
    let (h2, phi2) = point(fc / (0.947 + 3.806 * fc * fc).sqrt());
    let row = |h: f64, phi: f64| (phi * (1.0 - phi) * (1.0 - h), phi * phi * (hny - h), (h - 1.0) * (1.0 - phi) - s * phi * (1.0 - phi));
    let (c11, c12, d1) = row(h1, phi1);
    let (c21, c22, d2) = row(h2, phi2);
    let det = c11 * c22 - c12 * c21;
    let alfa1 = (c22 * d1 - c12 * d2) / det;
    let alfa2 = (c11 * d2 - c21 * d1) / det;
    let beta1 = alfa1 + s;
    let beta2 = hny * alfa2;
    // A0 = B0 = 1, A1 = α2, A2 = (α1 − α2)/4, likewise B; then the factorisation of eq. (5).
    let (aa1, aa2, bb1, bb2) = (alfa2, 0.25 * (alfa1 - alfa2), beta2, 0.25 * (beta1 - beta2));
    if !(aa1 >= 0.0 && bb1 >= 0.0) {
        return Err(Failure::NoSolution);
    }
    let v = 0.5 * (1.0 + aa1.sqrt());
    let w = 0.5 * (1.0 + bb1.sqrt());
    let (dv, dw) = (v * v + aa2, w * w + bb2);
    if !(dv >= 0.0 && dw >= 0.0) {
        return Err(Failure::NoSolution);
    }
    let a0 = 0.5 * (v + dv.sqrt());
    let b0 = 0.5 * (w + dw.sqrt());
    let raw_b = [b0, 1.0 - w, -0.25 * bb2 / b0];
    let raw_a = [a0, 1.0 - v, -0.25 * aa2 / a0];
    let mut c = Coeffs::from_raw(raw_b, raw_a);
    if !high {
        let g = 10f64.powf(band.gain_db / 20.0);
        c.b0 *= g;
        c.b1 *= g;
        c.b2 *= g;
    }
    if c.is_finite() { Ok(c) } else { Err(Failure::NoSolution) }
}

// ---------------------------------------------------------------------------------------
// Matched poles + magnitude-matched numerator
// ---------------------------------------------------------------------------------------

/// Where [`mz`] places its third magnitude-match point (DC and Nyquist are always two).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MidPoint {
    /// At the band's own `ω0` — the peak, or the shelf's midpoint.
    W0,
    /// A fixed multiple of `ω0` (still clamped below Nyquist).
    Scaled(f64),
}

/// Matched-pole design. Poles: the prototype's poles `p = ω0·S_p` mapped exactly by
/// `z = e^{p}` (impulse invariance), so the resonance sits where the analog one does with
/// its analog damping. Numerator: write `φ = sin²(w/2)`; any real biquad numerator obeys
///
/// ```text
/// |B(e^{jw})|² = B0·(1 − φ) + B1·φ + B2·4φ(1 − φ),
/// B0 = (b0+b1+b2)², B1 = (b0−b1+b2)², B2 = −4·b0·b2
/// ```
///
/// so three target values of `|H_analog|²·|A(e^{jw})|²` fix `B0,B1,B2` linearly, and the
/// numerator is recovered from them choosing the minimum-phase root (`|b2| ≤ b0`).
pub fn mz(band: &Band, fs: f64, mid: MidPoint) -> Result<Coeffs, Failure> {
    if band.gain_db == 0.0 && band.kind != Kind::Bandpass {
        return Ok(IDENTITY);
    }
    let w0 = band.w0(fs);
    let proto = Prototype::of(band.kind, band.gain_db, band.q);

    // --- poles
    let (wn, zeta) = proto.pole_shape();
    let wp = wn * w0;
    let (a1, a2) = if zeta < 1.0 {
        let r = (-zeta * wp).exp();
        (-2.0 * r * (wp * (1.0 - zeta * zeta).sqrt()).cos(), r * r)
    } else {
        let s = (zeta * zeta - 1.0).sqrt();
        let (p1, p2) = (wp * (-zeta + s), wp * (-zeta - s));
        (-(p1.exp() + p2.exp()), (p1 + p2).exp())
    };
    let den_power = |phi: f64| (1.0 + a1 + a2).powi(2) * (1.0 - phi) + (1.0 - a1 + a2).powi(2) * phi - 4.0 * a2 * 4.0 * phi * (1.0 - phi);

    // --- numerator
    let target = |w: f64| proto.power_x(w / w0) * den_power((w / 2.0).sin().powi(2));
    let big_b0 = proto.power_x(0.0) * (1.0 + a1 + a2).powi(2);
    let big_b1 = target(PI);
    let wm = match mid {
        MidPoint::W0 => w0,
        MidPoint::Scaled(k) => k * w0,
    }
    .min(0.95 * PI);
    let phim = (wm / 2.0).sin().powi(2);
    let big_b2 = (target(wm) - big_b0 * (1.0 - phim) - big_b1 * phim) / (4.0 * phim * (1.0 - phim));

    let (r0, r1) = (big_b0.max(0.0).sqrt(), big_b1.max(0.0).sqrt());
    let s = (r0 + r1) / 2.0; // b0 + b2
    let b1 = (r0 - r1) / 2.0;
    let disc = s * s + big_b2; // (b0 − b2)²
    if disc < 0.0 {
        return Err(Failure::NoSolution);
    }
    let d = disc.sqrt();
    let (b0, b2) = ((s + d) / 2.0, (s - d) / 2.0);
    let coeffs = Coeffs { b0, b1, b2, a1, a2 };
    if coeffs.is_finite() { Ok(coeffs) } else { Err(Failure::NoSolution) }
}
