//! The warping-corrected ("analog-matched") designs CAGEq ships — the winners of the Stage 0/0b
//! spike (`examples/warp_spike.rs`, `examples/shelf_tune.rs`; the rejected candidates live in
//! [`crate::spike`]). Entry point: [`design`].
//!
//! * Peaking and band-pass: [`prescribed`] — Muranov (2025, UCSD MS thesis, ch. 3), building
//!   on Orfanidis (1997, "prescribed Nyquist-frequency gain"): keep the bilinear transform,
//!   but design the *analog* section it is applied to so that, after warping, the digital
//!   response hits the analog prototype's gain at DC, at Nyquist, at the peak, and at one
//!   bandwidth point.
//! * Shelves (which the thesis leaves unsolved): [`shelf`] — a Q-general extension of
//!   Vicanek's matched two-pole shelf with per-Q tuned match points, blended into Ivantsov's
//!   per-quadratic mapping where that is not realisable.

use std::f64::consts::PI;

use crate::analog::Prototype;
use crate::{Band, Coeffs, Kind};

/// Why a design could not produce a filter. The spike counts these; a production design
/// must have none inside CAGEq's parameter ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Failure {
    /// The method does not cover this filter type.
    Unsupported,
    /// Outside [`design`]'s domain (Fc at or too close to Nyquist, or non-finite input).
    OutOfDomain,
    /// A design came out unstable or non-minimum-phase. Never observed in the verified
    /// range; checked anyway, because an unsafe filter must never reach the audio path.
    Unsafe,
    /// The constraint system had no real solution (a negative square, a zero divisor).
    NoSolution,
}

/// A band exactly at 0 dB is the identity for peaking/shelves; every method short-circuits
/// to it, which also sidesteps the 0/0 the prescribed peaking formula has there.
pub(crate) const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

/// Highest Fc, as a fraction of Nyquist, the matched designs are used for. The thesis's own
/// designs become unstable as Fc reaches Nyquist (its §4.2); 0.95 keeps a margin while still
/// covering CAGEq's 20 kHz ceiling at 44.1 kHz (0.907·Nyquist).
const MAX_FC_NYQUIST: f64 = 0.95;

/// The shipped analog-matched design for any kind: [`prescribed`] for peaking and band-pass,
/// [`shelf`] for shelves — checked for stability and minimum phase before it is returned (a
/// band-pass's zero at DC sits on the unit circle by definition, so only its poles are).
pub fn design(band: &Band, fs: f64) -> Result<Coeffs, Failure> {
    if !(band.freq_hz.is_finite() && band.gain_db.is_finite() && band.q.is_finite() && fs.is_finite())
        || band.freq_hz <= 0.0
        || band.q <= 0.0
        || band.freq_hz >= MAX_FC_NYQUIST * fs / 2.0
    {
        return Err(Failure::OutOfDomain);
    }
    let c = match band.kind {
        Kind::Peaking | Kind::Bandpass => prescribed(band, fs)?,
        Kind::LowShelf | Kind::HighShelf => shelf(band, fs)?,
    };
    let min_phase = band.kind == Kind::Bandpass || c.zero_radius() < 1.0;
    if c.is_finite() && c.pole_radius() < 1.0 && min_phase { Ok(c) } else { Err(Failure::Unsafe) }
}

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

/// Vicanek's matched two-pole shelf (vicanek.de/articles/2poleShelvingFits.pdf; Butterworth
/// only there — see [`crate::spike::vicanek_butterworth_shelf`]) generalised to any Q, with
/// its two match points `f1`, `f2` (Nyquist units) given explicitly. Vicanek's scheme in `φ = sin²(ω/2)`, DC normalised to 1:
///
/// ```text
/// |H|² = (1 − φ + β1·φ(1−φ) + β2·φ²) / (1 − φ + α1·φ(1−φ) + α2·φ²)
/// ```
///
/// Near DC `|H|² ≈ 1 + (β1 − α1)·φ`. His "maximal flatness" condition `β1 = α1` is simply the
/// Butterworth prototype's zero DC curvature; for general Q, match the prototype's own:
/// `β1 = α1 + s`, `s = d|H|²/dφ` at DC. With `β2 = hNy·α2` (Nyquist) the two point matches stay
/// linear in `(α1, α2)`; only their right-hand side gains a `−s·φ(1−φ)` term. Match points
/// are the caller's: his fixed ones were tuned for Butterworth only, so [`shelf`] passes the
/// per-Q [`shelf_match_points`] schedule instead. Low shelf as `g·HS(1/g)`, which holds for
/// RBJ shelves at any Q.
pub fn vicanek_shelf_at(band: &Band, fs: f64, f1: f64, f2: f64) -> Result<Coeffs, Failure> {
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
    let (h1, phi1) = point(f1);
    let (h2, phi2) = point(f2);
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

/// Stage 0b match-point schedule for [`vicanek_shelf_at`]: `(p1, r1, p2, r2)` in
/// `f_i = fc/√(p_i + r_i·fc²)` as a function of Q. Anchor sets are per-Q Nelder–Mead optima
/// from `examples/shelf_tune.rs` (worst error over fc 20 Hz–20 kHz × ±20 dB at 44.1/48 kHz),
/// interpolated linearly in log Q and held constant outside the anchors, so the schedule —
/// and with it the design — is continuous in Q. ([`shelf`] only uses it up to Q 1.2; the Q 2
/// and Q 3 optima the tuning also found are recorded in `examples/shelf_tune.rs`.)
pub fn shelf_match_points(q: f64) -> [f64; 4] {
    const ANCHORS: [(f64, [f64; 4]); 3] = [
        (0.65, [0.132, 1.425, 0.919, 7.129]),
        (0.7, [0.085, 1.401, 0.748, 3.052]),
        (1.41, [0.223, 1.125, 0.805, 2.175]),
    ];
    if q <= ANCHORS[0].0 {
        return ANCHORS[0].1;
    }
    for w in ANCHORS.windows(2) {
        let ((q0, p0), (q1, p1)) = (w[0], w[1]);
        if q <= q1 {
            let t = (q / q0).ln() / (q1 / q0).ln();
            return std::array::from_fn(|k| p0[k] + t * (p1[k] - p0[k]));
        }
    }
    ANCHORS[ANCHORS.len() - 1].1
}

/// The shipped shelf: generalised Vicanek with the tuned [`shelf_match_points`] schedule,
/// [`ivantsov`] (σ = 2) across the Q range Vicanek cannot realise, and plain RBJ for
/// resonant shelves — coefficient-[`blend`]ed across smoothstep windows in log Q, so the
/// result is continuous in every parameter.
///
/// * **Q ≤ 0.38 and 0.69 ≤ Q ≤ 0.85: Vicanek.** The dense scan (`shelf_tune --schedule`)
///   found the schedule realisable for every fc 20 Hz–20 kHz and gain ±20 dB for Q ≤ 0.42 and
///   Q ≥ 0.652. Q 0.7 — the fit's fixed shelves and every macro band — sits at full weight.
/// * **0.42 ≤ Q ≤ 0.655: Ivantsov.** Around Q = 0.5 the shelf's |H|² is a perfect square (two
///   identical first-order shelves) and Vicanek's five-condition system loses rank —
///   structurally, for any match points. Ivantsov is stable/minimum-phase by construction.
/// * **Q ≥ 1.1: RBJ.** Above Q ≈ 1 a shelf overshoots (a bump and a dip beside fc), which RBJ
///   places exactly (it pre-warps at fc) and neither matched design tracks: band by band the
///   matched filter was measurably *worse* than RBJ there (0.09 dB at Q 1.41, ~0.5 dB at Q 2,
///   up to 7.6 dB at Q 20), although its worst case over all fc was better. The model's
///   promise is per band — never further from analog than RBJ (`tests/design.rs`) — so
///   resonant shelves fade to RBJ over Q 0.85–1.1 (the last per-band excess, 0.006 dB at
///   192 kHz, sits inside the fade). CAGEq's own shelves are all Q 0.7.
///
/// Returns `Err` rather than silently switching method if Vicanek ever fails where its weight
/// is non-zero — a silent switch would be a discontinuity, and verification should see it.
pub fn shelf(band: &Band, fs: f64) -> Result<Coeffs, Failure> {
    if !matches!(band.kind, Kind::LowShelf | Kind::HighShelf) {
        return Err(Failure::Unsupported);
    }
    let lq = band.q.ln();
    let ramp = |lo: f64, hi: f64| smoothstep(lo.ln(), hi.ln(), lq);
    let to_rbj = ramp(0.85, 1.1);
    if to_rbj >= 1.0 {
        return Ok(crate::rbj::coefficients(band, fs));
    }
    // Vicanek weight: 1 → 0 over [0.38, 0.42], 0 → 1 over [0.655, 0.69].
    let w = (1.0 - ramp(0.38, 0.42)) + ramp(0.655, 0.69);
    let matched = if w <= 0.0 {
        ivantsov(band, fs, 2.0)?
    } else {
        let fc = band.freq_hz / (fs / 2.0);
        let [p1, r1, p2, r2] = shelf_match_points(band.q);
        let vk = vicanek_shelf_at(band, fs, fc / (p1 + r1 * fc * fc).sqrt(), fc / (p2 + r2 * fc * fc).sqrt())?;
        if w >= 1.0 { vk } else { blend(&ivantsov(band, fs, 2.0)?, &vk, w) }
    };
    if to_rbj <= 0.0 {
        return Ok(matched);
    }
    Ok(blend(&matched, &crate::rbj::coefficients(band, fs), to_rbj))
}

/// Linear coefficient blend `(1 − t)·a + t·b`. Safe for any two stable, minimum-phase
/// biquads: with `a0 = 1` stability is the triangle `|a2| < 1, |a1| < 1 + a2`, and minimum
/// phase is the cone `|b2| < b0, |b1| < b0 + b2` — both convex, so every blend is stable and
/// minimum phase too. Continuous in `t`, which is the point.
pub fn blend(a: &Coeffs, b: &Coeffs, t: f64) -> Coeffs {
    let l = |x: f64, y: f64| x + t * (y - x);
    Coeffs { b0: l(a.b0, b.b0), b1: l(a.b1, b.b1), b2: l(a.b2, b.b2), a1: l(a.a1, b.a1), a2: l(a.a2, b.a2) }
}

/// `0` at/below `lo`, `1` at/above `hi`, smoothstep between (C¹, so the optimizer's gradient
/// has no kink at the window edges).
fn smoothstep(lo: f64, hi: f64, x: f64) -> f64 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
