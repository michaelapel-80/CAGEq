//! The RBJ cookbook filters CAGEq runs today — transcribed from `cageq_apo::dsp::coefficients`
//! (which matches `morph.rs`, `biquad.ts` and AutoEq's `peq.py`). The spike's baseline.

use crate::{Band, Coeffs, Kind};

pub fn coefficients(band: &Band, fs: f64) -> Coeffs {
    let a = 10f64.powf(band.gain_db / 40.0);
    let w0 = band.w0(fs);
    let alpha = w0.sin() / (2.0 * band.q);
    let cosw = w0.cos();
    let sa = a.sqrt();
    let (b, d) = match band.kind {
        Kind::Peaking => ([1.0 + alpha * a, -2.0 * cosw, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * cosw, 1.0 - alpha / a]),
        Kind::LowShelf => (
            [
                a * (a + 1.0 - (a - 1.0) * cosw + 2.0 * sa * alpha),
                2.0 * a * (a - 1.0 - (a + 1.0) * cosw),
                a * (a + 1.0 - (a - 1.0) * cosw - 2.0 * sa * alpha),
            ],
            [a + 1.0 + (a - 1.0) * cosw + 2.0 * sa * alpha, -2.0 * (a - 1.0 + (a + 1.0) * cosw), a + 1.0 + (a - 1.0) * cosw - 2.0 * sa * alpha],
        ),
        Kind::HighShelf => (
            [
                a * (a + 1.0 + (a - 1.0) * cosw + 2.0 * sa * alpha),
                -2.0 * a * (a - 1.0 + (a + 1.0) * cosw),
                a * (a + 1.0 + (a - 1.0) * cosw - 2.0 * sa * alpha),
            ],
            [a + 1.0 - (a - 1.0) * cosw + 2.0 * sa * alpha, 2.0 * (a - 1.0 - (a + 1.0) * cosw), a + 1.0 - (a - 1.0) * cosw - 2.0 * sa * alpha],
        ),
        Kind::Bandpass => ([alpha, 0.0, -alpha], [1.0 + alpha, -2.0 * cosw, 1.0 - alpha]),
    };
    Coeffs::from_raw(b, d)
}
