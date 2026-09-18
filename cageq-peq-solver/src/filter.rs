//! Port of `peq.py`'s `PEQFilter`/`Peaking`/`LowShelf`/`HighShelf` — the RBJ biquad
//! model and per-band init heuristics the optimizer fits. See `src/lib.rs` for why this
//! is a fourth copy of the biquad math rather than reusing `cageq_core::morph`'s.
//!
//! Every formula below is transcribed from a specific `peq.py` location cited in its
//! doc comment, including one that looks like a bug (`ix10k`) — ported literally rather
//! than "fixed," since the fixture cross-check in `tests/solver_fixtures.rs` is what
//! gets to decide that, not a guess made while porting.

use crate::peaks::find_peaks;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandKind {
    Peaking,
    LowShelf,
    HighShelf,
}

/// AutoEq's `DEFAULT_{PEAKING,SHELF}_FILTER_*` constants (`constants.py`).
impl BandKind {
    pub fn default_bounds(self) -> (f64, f64, f64, f64, f64, f64) {
        // (min_fc, max_fc, min_q, max_q, min_gain, max_gain)
        match self {
            BandKind::Peaking => (20.0, 10_000.0, 0.182_48, 6.0, -20.0, 20.0),
            BandKind::LowShelf | BandKind::HighShelf => (20.0, 10_000.0, 0.4, 0.7, -20.0, 20.0),
        }
    }
}

/// One optimizable band. Mirrors `PEQFilter` and its three subclasses: a single struct
/// with a `kind` tag rather than a trait object, since the three kinds differ only in
/// their coefficient formula and `init()` heuristic, both plain match arms below.
#[derive(Debug, Clone)]
pub struct Band {
    pub kind: BandKind,
    pub fc: f64,
    pub q: f64,
    pub gain: f64,
    pub optimize_fc: bool,
    pub optimize_q: bool,
    pub optimize_gain: bool,
    pub min_fc: f64,
    pub max_fc: f64,
    pub min_q: f64,
    pub max_q: f64,
    pub min_gain: f64,
    pub max_gain: f64,
}

impl Band {
    /// A fully free band of `kind`, AutoEq's default bounds, fc/q/gain all optimized —
    /// the shape CAGEq's own `calculate_filters` config builds every `PEAKING` entry as
    /// (`sidecar_dsp.py`'s `config` dict, peaking entries with no fc/q/gain given).
    pub fn free(kind: BandKind) -> Self {
        let (min_fc, max_fc, min_q, max_q, min_gain, max_gain) = kind.default_bounds();
        Band {
            kind,
            fc: f64::NAN,
            q: f64::NAN,
            gain: f64::NAN,
            optimize_fc: true,
            optimize_q: true,
            optimize_gain: true,
            min_fc,
            max_fc,
            min_q,
            max_q,
            min_gain,
            max_gain,
        }
    }

    /// A band pinned at `fc`/`q`, only `gain` free — the shape of CAGEq's low/high shelf
    /// entries (`fc: 105.0/10000.0, q: 0.7` fixed in `sidecar_dsp.py`'s config).
    pub fn fixed_fc_q(kind: BandKind, fc: f64, q: f64) -> Self {
        let (min_fc, max_fc, min_q, max_q, min_gain, max_gain) = kind.default_bounds();
        Band {
            kind,
            fc,
            q,
            gain: f64::NAN,
            optimize_fc: false,
            optimize_q: false,
            optimize_gain: true,
            min_fc,
            max_fc,
            min_q,
            max_q,
            min_gain,
            max_gain,
        }
    }

    /// Number of free parameters this band contributes to the optimizer's vector —
    /// `_init_optimizer_bounds`/`_init_optimizer_params`'s implicit per-filter stride.
    pub fn n_free(&self) -> usize {
        self.optimize_fc as usize + self.optimize_q as usize + self.optimize_gain as usize
    }

    /// `PEQFilter.biquad_coefficients` (`peq.py:236-250` Peaking, `:351-365` HighShelf,
    /// `:404-418` LowShelf). Returns `[a0, a1, a2, b0, b1, b2]` with `a1`/`a2` already
    /// negated relative to the standard RBJ cookbook form, exactly as `peq.py` leaves
    /// them (its own `fr()` flips them back — see [`Band::fr`]).
    fn biquad_coefficients(&self, fs: f64) -> [f64; 6] {
        let a = 10f64.powf(self.gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * self.fc / fs;
        let alpha = w0.sin() / (2.0 * self.q);
        let cosw = w0.cos();
        let sqrt_a = a.sqrt();

        match self.kind {
            BandKind::Peaking => {
                let a0 = 1.0 + alpha / a;
                [
                    1.0,
                    -(-2.0 * cosw) / a0,
                    -(1.0 - alpha / a) / a0,
                    (1.0 + alpha * a) / a0,
                    (-2.0 * cosw) / a0,
                    (1.0 - alpha * a) / a0,
                ]
            }
            BandKind::HighShelf => {
                let a0 = (a + 1.0) - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha;
                [
                    1.0,
                    -(2.0 * ((a - 1.0) - (a + 1.0) * cosw)) / a0,
                    -((a + 1.0) - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha) / a0,
                    (a * ((a + 1.0) + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha)) / a0,
                    (-2.0 * a * ((a - 1.0) + (a + 1.0) * cosw)) / a0,
                    (a * ((a + 1.0) + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha)) / a0,
                ]
            }
            BandKind::LowShelf => {
                let a0 = (a + 1.0) + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha;
                [
                    1.0,
                    -(-2.0 * ((a - 1.0) + (a + 1.0) * cosw)) / a0,
                    -((a + 1.0) + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha) / a0,
                    (a * ((a + 1.0) - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha)) / a0,
                    (2.0 * a * ((a - 1.0) - (a + 1.0) * cosw)) / a0,
                    (a * ((a + 1.0) - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha)) / a0,
                ]
            }
        }
    }

    /// `PEQFilter.fr` (`peq.py:110-128`): this band's own frequency response in dB on
    /// `f`, via the numerically-stable `phi` form (avoids evaluating `cos`/`sin` at
    /// every bin for the magnitude, unlike the naive `|H(e^jw)|` route).
    pub fn fr(&self, f: &[f64], fs: f64) -> Vec<f64> {
        let [a0, a1, a2, b0, b1, b2] = self.biquad_coefficients(fs);
        let (a1, a2) = (-a1, -a2); // peq.py's fr() flips these back before evaluating
        let b_sum = (b0 + b1 + b2).powi(2);
        let a_sum = (a0 + a1 + a2).powi(2);
        f.iter()
            .map(|&freq| {
                let w = 2.0 * std::f64::consts::PI * freq / fs;
                let phi = 4.0 * (w / 2.0).sin().powi(2);
                let num = b_sum + (b0 * b2 * phi - (b1 * (b0 + b2) + 4.0 * b0 * b2)) * phi;
                let den = a_sum + (a0 * a2 * phi - (a1 * (a0 + a2) + 4.0 * a0 * a2)) * phi;
                10.0 * num.log10() - 10.0 * den.log10()
            })
            .collect()
    }

    /// `PEQFilter.ix10k` (`peq.py:104-107`): despite the name, `argmin(|f - fs|)`, not
    /// an index of 10 kHz. Since `f`'s standard AutoEq grid tops out at 20 kHz and `fs`
    /// is a sample rate (44.1/48/96 kHz — always ≫ 20 kHz), this always resolves to
    /// `f`'s last index; `band_penalty` below relies on exactly that behaviour, so it is
    /// kept as `argmin`, not simplified to "last index", in case a caller ever hands it
    /// a non-standard grid or `fs`.
    fn ix10k(f: &[f64], fs: f64) -> usize {
        argmin_abs(f, fs)
    }

    /// `Peaking.sharpness_penalty` (`peq.py:252-266`); always `0.0` for shelves
    /// (`ShelfFilter.sharpness_penalty`, `peq.py:294-297` — they "overshoot hard" before
    /// nearing the slope this penalises).
    ///
    /// Takes this band's own response (`fr`) as an argument rather than computing it via
    /// `self.fr()` internally: callers on the optimizer's hot path (`Solver::evaluate`)
    /// already have it computed once per band and reuse it here and in
    /// [`Band::band_penalty`] rather than paying for it three times over.
    pub fn sharpness_penalty(&self, fr: &[f64]) -> f64 {
        if self.kind != BandKind::Peaking {
            return 0.0;
        }
        let gain_limit = -0.095_031_892_701_994_64 + 20.575_128_011_847_003 * (1.0 / self.q);
        let x = self.gain / gain_limit - 1.0;
        let coeff = 1.0 / (1.0 + (-x * 100.0).exp());
        mean(&fr.iter().map(|v| (v * coeff).powi(2)).collect::<Vec<_>>())
    }

    /// `PEQFilter.band_penalty`: `Peaking`'s own version at `peq.py:268-283`,
    /// `ShelfFilter`'s (shared by both shelf kinds) at `peq.py:299-313`. Penalises a
    /// transition band wide enough to run into (and get mirror-distorted by) Nyquist.
    /// Takes this band's own response `fr` as an argument — see
    /// [`Band::sharpness_penalty`]'s doc for why.
    pub fn band_penalty(&self, f: &[f64], fs: f64, fr: &[f64]) -> f64 {
        let fc_ix = argmin_abs(f, self.fc);
        let ix10k = Self::ix10k(f, fs);
        let n = fc_ix.min(ix10k.saturating_sub(fc_ix));
        if n == 0 {
            return 0.0;
        }
        let left = &fr[fc_ix - n..fc_ix];
        // Python: `fr[fc_ix + n - 1:fc_ix - 1:-1]` — n elements are indices `fc_ix ..=
        // fc_ix + n - 1`, reversed. `n >= 1` here (the `n == 0` case already returned),
        // and `n <= fc_ix`, so `fc_ix - 1` never underflows.
        let right = reversed_slice(fr, fc_ix + n - 1, fc_ix - 1);
        match self.kind {
            BandKind::Peaking => mean_sq_diff(left, &right),
            BandKind::LowShelf | BandKind::HighShelf => {
                let mirrored: Vec<f64> = right.iter().map(|v| self.gain - v).collect();
                mean_sq_diff(left, &mirrored)
            }
        }
    }

    /// `Peaking.init`/`HighShelf.init`/`LowShelf.init`: sets this band's fc/q/gain from
    /// `target` (mutating `self`) and returns the initial optimizer parameters for its
    /// free variables, in `[fc?, q?, gain?]` order — the same order
    /// `_init_optimizer_bounds` produces, so the two line up positionally.
    pub fn init(&mut self, f: &[f64], target: &[f64], fs: f64) -> Vec<f64> {
        match self.kind {
            BandKind::Peaking => self.init_peaking(f, target),
            BandKind::HighShelf => self.init_shelf(f, target, fs, true),
            BandKind::LowShelf => self.init_shelf(f, target, fs, false),
        }
    }

    /// `Peaking.init` (`peq.py:165-234`): centres on the target's biggest peak or dip
    /// (ranked by width × height), sized/gained to match it; falls back to the
    /// fc-range midpoint, Q=√2, 0 dB gain when no peak falls in range.
    fn init_peaking(&mut self, f: &[f64], target: &[f64]) -> Vec<f64> {
        let clipped_pos: Vec<f64> = target.iter().map(|&v| v.max(0.0)).collect();
        let clipped_neg: Vec<f64> = target.iter().map(|&v| (-v).max(0.0)).collect();
        let positive = find_peaks(&clipped_pos, 0.0);
        let negative = find_peaks(&clipped_neg, 0.0);

        let min_fc_ix = argmin_abs(f, self.min_fc);
        let max_fc_ix = argmin_abs(f, self.max_fc);

        let candidates: Vec<_> =
            positive.iter().chain(negative.iter()).filter(|p| p.index >= min_fc_ix && p.index <= max_fc_ix).collect();

        let mut params = Vec::with_capacity(self.n_free());
        if candidates.is_empty() {
            if self.optimize_fc {
                self.fc = f[(min_fc_ix + max_fc_ix) / 2];
                params.push(self.fc.log10());
            }
            if self.optimize_q {
                self.q = std::f64::consts::SQRT_2;
                params.push(self.q);
            }
            if self.optimize_gain {
                self.gain = 0.0;
                params.push(self.gain);
            }
            return params;
        }

        // `np.argmax` (peq.py:213) keeps the FIRST index on a tie; `Iterator::max_by`
        // keeps the last, so ties are broken via [`argmax_by`] instead.
        let biggest = argmax_by(candidates.iter().copied(), |p| p.width * p.height).unwrap();
        let ix = biggest.index;

        if self.optimize_fc {
            self.fc = f[ix].clamp(self.min_fc, self.max_fc);
            params.push(self.fc.log10());
        }
        if self.optimize_q {
            let f_step = (f[1] / f[0]).log2();
            let bw = f_step * biggest.width; // log2((2^f_step)^width)
            self.q = ((2f64.powf(bw)).sqrt() / (2f64.powf(bw) - 1.0)).clamp(self.min_q, self.max_q);
            params.push(self.q);
        }
        if self.optimize_gain {
            let signed = if target[ix] > 0.0 { biggest.height } else { -biggest.height };
            self.gain = signed.clamp(self.min_gain, self.max_gain);
            params.push(self.gain);
        }
        params
    }

    /// `HighShelf.init` (`peq.py:317-349`, `high == true`) / `LowShelf.init`
    /// (`peq.py:369-402`, `high == false`): finds the transition point maximising the
    /// average level after/before it, Q pinned to 0.7, gain the target's weighted
    /// average using the shelf's own unity-gain response as the weight vector.
    fn init_shelf(&mut self, f: &[f64], target: &[f64], fs: f64, high: bool) -> Vec<f64> {
        let min_ix = f.iter().filter(|&&v| v < self.min_fc.max(40.0)).count();
        let max_ix = f.iter().filter(|&&v| v < self.max_fc.min(10_000.0)).count();

        let mut params = Vec::with_capacity(self.n_free());
        if self.optimize_fc {
            // `HighShelf.init` (peq.py:336) has an off-by-`min_ix` bug here: it selects
            // via `np.argmax` over a 0-based list comprehension but then uses that
            // local index directly into `f`/`target` with no `+= min_ix` correction
            // (unlike `LowShelf.init`, peq.py:388-389, which does add it back) — so real
            // AutoEq picks a HighShelf transition point about one octave lower than
            // this search actually intends. Deliberately NOT reproduced here: CAGEq
            // never optimizes a shelf's fc today (`cageq_default_bands` always pins
            // both shelves via `Band::fixed_fc_q`), so this branch is presently dead
            // code either way, and there is no reason to wire in an undocumented
            // upstream bug for a path nothing exercises or tests against Python. If a
            // free-fc shelf feature is ever added, evaluate the actual effect on the
            // solver first (initial-guess quality feeding a non-convex SLSQP search)
            // before deciding whether AutoEq-parity or a better guess wins.
            let len = max_ix.saturating_sub(min_ix);
            let ix = if high {
                argmax_by(0..len, |&local| mean(&target[min_ix + local..]).abs()).map(|local| local + min_ix).unwrap_or(min_ix)
            } else {
                argmax_by(0..len, |&local| mean(&target[..=min_ix + local]).abs()).map(|local| local + min_ix).unwrap_or(min_ix)
            };
            self.fc = f[ix].clamp(self.min_fc, self.max_fc);
            params.push(self.fc.log10());
        }
        if self.optimize_q {
            self.q = 0.7f64.clamp(self.min_q, self.max_q);
            params.push(self.q);
        }
        if self.optimize_gain {
            self.gain = 1.0; // unity-gain shape used purely as a weight vector below
            let fr = self.fr(f, fs);
            let weighted: f64 = target.iter().zip(&fr).map(|(t, w)| t * w).sum();
            let norm: f64 = fr.iter().sum();
            self.gain = (weighted / norm).clamp(self.min_gain, self.max_gain);
            params.push(self.gain);
        }
        params
    }
}

fn argmin_abs(xs: &[f64], target: f64) -> usize {
    xs.iter().enumerate().min_by(|(_, a), (_, b)| (*a - target).abs().partial_cmp(&(*b - target).abs()).unwrap()).map(|(i, _)| i).unwrap()
}

/// `np.argmax`'s tie-break rule (first index wins) applied to an arbitrary item/key
/// pair: `Iterator::max_by` keeps the LAST maximal element on a tie, `np.argmax` keeps
/// the FIRST.
fn argmax_by<T>(items: impl IntoIterator<Item = T>, key: impl Fn(&T) -> f64) -> Option<T> {
    let mut best: Option<(T, f64)> = None;
    for item in items {
        let k = key(&item);
        let is_better = match &best {
            Some((_, b)) => k > *b,
            None => true,
        };
        if is_better {
            best = Some((item, k));
        }
    }
    best.map(|(item, _)| item)
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn mean_sq_diff(a: &[f64], b: &[f64]) -> f64 {
    mean(&a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).collect::<Vec<_>>())
}

/// `x[from_inclusive..down_to_exclusive:-1]` in Python: `from_inclusive`, descending,
/// stopping strictly before `down_to_exclusive`.
fn reversed_slice(x: &[f64], from_inclusive: usize, down_to_exclusive: usize) -> Vec<f64> {
    ((down_to_exclusive + 1)..=from_inclusive).rev().map(|i| x[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_grid(points: usize, f_min: f64, f_max: f64) -> Vec<f64> {
        let ratio = (f_max / f_min).ln();
        (0..points).map(|i| f_min * (ratio * i as f64 / (points - 1) as f64).exp()).collect()
    }

    /// A peaking band's response at its own centre frequency is its gain, and it decays
    /// to ~0 dB far away — the same sanity check `cageq-core`'s `biquad_crosscheck.rs`
    /// runs against the live `peq.py` for its own (cascade) copy of this formula.
    #[test]
    fn peaking_response_hits_its_gain_at_fc_and_decays_away_from_it() {
        let f = log_grid(200, 20.0, 20_000.0);
        let band = Band { fc: 1000.0, q: 1.0, gain: 6.0, ..Band::fixed_fc_q(BandKind::Peaking, 1000.0, 1.0) };
        let fr = band.fr(&f, 48_000.0);
        let i = f.iter().enumerate().min_by(|(_, a), (_, b)| (**a - 1000.0).abs().partial_cmp(&(**b - 1000.0).abs()).unwrap()).unwrap().0;
        assert!((fr[i] - 6.0).abs() < 0.05, "expected ~6 dB at fc, got {}", fr[i]);
        assert!(fr[0].abs() < 0.05, "expected ~0 dB far below fc, got {}", fr[0]);
    }

    /// A high shelf's response settles at its gain well above `fc`, and near 0 dB well
    /// below it — the two asymptotes a shelf is defined by.
    #[test]
    fn high_shelf_settles_at_its_gain_above_fc() {
        let f = log_grid(200, 20.0, 20_000.0);
        let band = Band::fixed_fc_q(BandKind::HighShelf, 1000.0, 0.7);
        let band = Band { gain: 4.0, ..band };
        let fr = band.fr(&f, 48_000.0);
        assert!(fr[0].abs() < 0.5, "expected ~0 dB well below fc, got {}", fr[0]);
        assert!((fr.last().unwrap() - 4.0).abs() < 0.5, "expected ~4 dB well above fc, got {}", fr.last().unwrap());
    }
}
