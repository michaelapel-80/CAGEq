//! The analog prototypes — the ground truth. Each is a second-order section in the
//! normalised Laplace variable `S = s/ω0`, `H(S) = (n2·S² + n1·S + n0) / (d2·S² + d1·S + d0)`,
//! exactly the RBJ cookbook's (so the Q convention is the cookbook's, see the crate doc).

use crate::{Band, Kind};

/// Numerator and denominator coefficients `[x2, x1, x0]` in `S = s/ω0`.
#[derive(Debug, Clone, Copy)]
pub struct Prototype {
    pub n: [f64; 3],
    pub d: [f64; 3],
}

impl Prototype {
    pub fn of(kind: Kind, gain_db: f64, q: f64) -> Prototype {
        let a = 10f64.powf(gain_db / 40.0); // RBJ's A: amplitude = A², so A is "half the gain"
        let sa = a.sqrt();
        match kind {
            Kind::Peaking => Prototype { n: [1.0, a / q, 1.0], d: [1.0, 1.0 / (a * q), 1.0] },
            Kind::Bandpass => Prototype { n: [0.0, 1.0 / q, 0.0], d: [1.0, 1.0 / q, 1.0] },
            Kind::LowShelf => Prototype { n: [a, a * sa / q, a * a], d: [a, sa / q, 1.0] },
            Kind::HighShelf => Prototype { n: [a * a, a * sa / q, a], d: [1.0, sa / q, a] },
        }
    }

    /// `|H(jx)|²` at normalised frequency `x = ω/ω0`.
    pub fn power_x(&self, x: f64) -> f64 {
        quad_power(self.n, x) / quad_power(self.d, x)
    }

    /// `d|H(jx)|²/dx`, analytically (quotient rule on the two `quad_power`s).
    pub fn power_x_slope(&self, x: f64) -> f64 {
        let (n, d) = (quad_power(self.n, x), quad_power(self.d, x));
        (quad_power_slope(self.n, x) * d - n * quad_power_slope(self.d, x)) / (d * d)
    }

    /// Denominator's natural frequency (in units of ω0) and damping ratio ζ.
    pub fn pole_shape(&self) -> (f64, f64) {
        let [d2, d1, d0] = self.d;
        let wn = (d0 / d2).sqrt();
        (wn, d1 / (2.0 * (d0 * d2).sqrt()))
    }
}

/// `|p2·(jx)² + p1·(jx) + p0|²`.
fn quad_power([p2, p1, p0]: [f64; 3], x: f64) -> f64 {
    let re = p0 - p2 * x * x;
    let im = p1 * x;
    re * re + im * im
}

fn quad_power_slope([p2, p1, p0]: [f64; 3], x: f64) -> f64 {
    2.0 * (p0 - p2 * x * x) * (-2.0 * p2 * x) + 2.0 * p1 * p1 * x
}

/// The analog band's `|H|²` at digital frequency `w` (rad/sample) for sample rate `fs`.
pub fn power(band: &Band, fs: f64, w: f64) -> f64 {
    Prototype::of(band.kind, band.gain_db, band.q).power_x(w / band.w0(fs))
}

pub fn db(band: &Band, fs: f64, w: f64) -> f64 {
    10.0 * power(band, fs, w).max(1e-30).log10()
}
