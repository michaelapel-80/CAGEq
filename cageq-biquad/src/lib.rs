//! Biquad design math for CAGEq: the analog prototypes the filters are *meant* to be, the
//! RBJ-cookbook digital filters CAGEq (and Equalizer APO, and AutoEq) actually run today, and
//! the candidate designs that correct the bilinear transform's frequency warping.
//!
//! ## Why the analog prototype is the reference
//! Every RBJ filter is the bilinear transform of an analog second-order section, pre-warped
//! so its centre/corner frequency lands correctly. Everything *else* is squeezed toward
//! Nyquist ("cramping"): a 12 kHz, Q 1 bell at 48 kHz is ~2.3 dB off its analog shape at
//! 19 kHz. The analog prototype is exact, cheap and parameterised by the very Fc/gain/Q the
//! user edits, so it is the ground truth a corrected design is measured against — no
//! measurement, no fixture, no Python.
//!
//! ## Units
//! Frequencies inside the design code are **normalised digital** frequencies, rad/sample:
//! `w = 2π·f/fs`, Nyquist at `π`. The analog prototype is evaluated on the same axis (its
//! `ω0 = 2π·fc/fs`), which is what "the digital filter should match the analog one" means.
//!
//! ## Q convention
//! Q is the RBJ cookbook's (and AutoEq's, and Equalizer APO's) — for a peaking filter the
//! bandwidth is measured at the *midpoint* gain, for shelves Q is the cookbook's shelf Q.
//! The thesis this spike follows writes the peaking prototype with `Q_t = Q·√g` (`g` the
//! linear amplitude gain) instead; every conversion happens at that boundary, never in the
//! caller, so a preset's Q means the same thing in either model.

pub mod analog;
pub mod matched;
pub mod rbj;

/// The filter shapes CAGEq builds cascades from. Mirrors `cageq_apo::dsp::FilterKind`; Tilt
/// never reaches this level (it is expanded into a shelf pair upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Peaking,
    LowShelf,
    HighShelf,
    /// Constant-0 dB-peak band-pass (the isolate audition); gain is ignored.
    Bandpass,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Peaking, Kind::LowShelf, Kind::HighShelf, Kind::Bandpass];

    pub fn token(self) -> &'static str {
        match self {
            Kind::Peaking => "PK",
            Kind::LowShelf => "LSC",
            Kind::HighShelf => "HSC",
            Kind::Bandpass => "BP",
        }
    }
}

/// One band as the user (or the fit) describes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: Kind,
    pub freq_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

impl Band {
    /// Centre/corner frequency in rad/sample.
    pub fn w0(&self, fs: f64) -> f64 {
        2.0 * std::f64::consts::PI * self.freq_hz / fs
    }
}

/// Difference-equation coefficients, `a0` normalised to 1, standard (un-negated) sign:
/// `y = b0·x + b1·x₁ + b2·x₂ − a1·y₁ − a2·y₂` — the same convention as `cageq_apo::dsp`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coeffs {
    /// Normalise a raw `(b, a)` set by `a0`.
    pub fn from_raw(b: [f64; 3], a: [f64; 3]) -> Coeffs {
        Coeffs { b0: b[0] / a[0], b1: b[1] / a[0], b2: b[2] / a[0], a1: a[1] / a[0], a2: a[2] / a[0] }
    }

    pub fn is_finite(&self) -> bool {
        [self.b0, self.b1, self.b2, self.a1, self.a2].iter().all(|v| v.is_finite())
    }

    /// `|H(e^{jw})|²`, in the `φ = sin²(w/2)` form AutoEq's `peq.py` uses (and `biquad.ts`
    /// mirrors). The obvious `cos w`/`cos 2w` form loses ~9 digits to cancellation for a
    /// narrow low band at a high rate (20 Hz, Q 10 at 96 kHz read 1.6 dB off at its own
    /// peak); the `φ` form keeps the small quantities small instead of differencing ~4s.
    pub fn power(&self, w: f64) -> f64 {
        let phi = (w / 2.0).sin().powi(2);
        let num = phi_power(self.b0, self.b1, self.b2, phi);
        let den = phi_power(1.0, self.a1, self.a2, phi);
        num / den
    }

    /// `20·log10|H(e^{jw})|`, floored so an exact zero (a band-pass at DC) stays finite.
    pub fn db(&self, w: f64) -> f64 {
        10.0 * self.power(w).max(1e-30).log10()
    }

    /// Largest pole radius. `< 1` is stability.
    pub fn pole_radius(&self) -> f64 {
        max_root_radius(1.0, self.a1, self.a2)
    }

    /// Largest zero radius. `< 1` is minimum phase — which CAGEq relies on beyond taste:
    /// the undistort views run the *inverse* cascade, and an inverted zero outside the unit
    /// circle is an unstable pole.
    pub fn zero_radius(&self) -> f64 {
        max_root_radius(self.b0, self.b1, self.b2)
    }
}

/// `|p0 + p1·e^{-jw} + p2·e^{-2jw}|² = (p0+p1+p2)²·(1−φ) + (p0−p1+p2)²·φ − 16·p0·p2·φ(1−φ)`,
/// `φ = sin²(w/2)` — exact for any real quadratic (it is quadratic in `φ` and matches at
/// `φ = 0, ½, 1`).
fn phi_power(p0: f64, p1: f64, p2: f64, phi: f64) -> f64 {
    let s = p0 + p1 + p2;
    let d = p0 - p1 + p2;
    (s * s * (1.0 - phi) + d * d * phi - 16.0 * p0 * p2 * phi * (1.0 - phi)).max(0.0)
}

/// Largest |root| of `p0·z² + p1·z + p2`.
fn max_root_radius(p0: f64, p1: f64, p2: f64) -> f64 {
    if p0 == 0.0 {
        return f64::INFINITY;
    }
    let (p, q) = (p1 / p0, p2 / p0);
    let disc = p * p - 4.0 * q;
    if disc < 0.0 {
        q.abs().sqrt() // complex pair: |z|² = product of roots
    } else {
        let s = disc.sqrt();
        ((-p + s) / 2.0).abs().max(((-p - s) / 2.0).abs())
    }
}
