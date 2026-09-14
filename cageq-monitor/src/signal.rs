//! Signal synthesis shared between the `testtone` dev example
//! (`cageq-monitor/examples/testtone.rs`) and the in-app test-tone generator window
//! (`cageq-app`'s `start_test_generator` command, driving `TestSignal::start_generator`) — one
//! source of truth for the actual DSP math, so the two "drivers" (a CLI loop, a Tauri-controlled
//! background thread) can't independently drift the way `testtone.rs`'s own `--isp` construction
//! briefly did earlier this session (a wrong, then-unshared, copy of this exact math).
//!
//! Platform-agnostic on purpose — none of this touches WASAPI — even though both of its actual
//! callers are Windows-only; keeps it buildable and testable on any target.

use serde::{Deserialize, Serialize};

/// A periodic waveform shape. Ported verbatim from `testtone.rs` — nothing about the synthesis
/// itself changed by this extraction, only its location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Waveform {
    Sine,
    Square,
    Triangle,
    Sawtooth,
    Pulse,
}

// Duty cycle for `Waveform::Pulse` — the fraction of each period the pulse is "high". Fixed rather
// than a parameter (unlike the other shapes, which need only a frequency): narrow enough to give a
// genuinely rich, near-flat harmonic spectrum (this is the whole point of a pulse train over a
// square wave — see `Waveform::sample`'s own doc), not so narrow the fundamental's own amplitude
// gets awkwardly small relative to the noise floor at a sane playback level.
pub const PULSE_DUTY: f64 = 0.1;

impl Waveform {
    pub fn name(self) -> &'static str {
        match self {
            Waveform::Sine => "sine",
            Waveform::Square => "square",
            Waveform::Triangle => "triangle",
            Waveform::Sawtooth => "sawtooth",
            Waveform::Pulse => "pulse",
        }
    }

    /// One sample of this shape at phase `theta` (radians, any real value — `sin` wraps it),
    /// summing harmonics 1..=`k_max` at each shape's textbook Fourier amplitude. Peak amplitude is
    /// ~1 (plus a few percent of Gibbs overshoot right at an edge for square/sawtooth, same as any
    /// finite-harmonic approximation of a discontinuous waveform — left to the caller's own
    /// gain/sample-ceiling handling, exactly like a sine's own ~1 peak already is).
    ///
    /// `theta`/the internal accumulation are `f64`, not `f32`, though the return value is `f32`
    /// throughout — the caller's own per-sample phase wrapping already keeps `theta` itself
    /// bounded and precise, but each harmonic here evaluates `sin(k * theta)`/`cos(k * theta)`,
    /// which *multiplies* whatever rounding error `theta` carries by `k` before the trig call.
    /// That error is utterly invisible in the fundamental's own shape but, for a rich signal with
    /// a large `k_max` (a narrow-duty pulse train can run into the hundreds), it's amplified
    /// enough by the top harmonics to visibly drift the Gibbs ringing's fine structure
    /// cycle-to-cycle even though the edge itself sits rock-stable. `f32`'s ~7 decimal digits of
    /// precision aren't enough headroom once multiplied by a few hundred; `f64`'s ~15-16 are, for
    /// any `k_max` this ever produces.
    pub fn sample(self, theta: f64, k_max: u32) -> f32 {
        const FRAC_4_PI: f64 = 4.0 / std::f64::consts::PI;
        const FRAC_2_PI: f64 = 2.0 / std::f64::consts::PI;
        const FRAC_8_PI2: f64 = 8.0 / (std::f64::consts::PI * std::f64::consts::PI);
        (match self {
            Waveform::Sine => theta.sin(),
            // Odd harmonics only, amplitude 1/k — the textbook square-wave series.
            Waveform::Square => {
                let mut acc = 0.0f64;
                let mut k = 1u32;
                while k <= k_max {
                    acc += (k as f64 * theta).sin() / k as f64;
                    k += 2;
                }
                acc * FRAC_4_PI
            }
            // All harmonics, amplitude 1/k, alternating sign — the textbook (rising) sawtooth series.
            Waveform::Sawtooth => {
                let mut acc = 0.0f64;
                let mut sign = 1.0f64;
                for k in 1..=k_max {
                    acc += sign * (k as f64 * theta).sin() / k as f64;
                    sign = -sign;
                }
                acc * FRAC_2_PI
            }
            // Odd harmonics only, amplitude 1/k² (converges much faster than square/sawtooth — no
            // audible discontinuity in the waveform itself, just a slope change, so far less Gibbs
            // ringing and a visibly steeper roll-off on screen: -12 dB/octave vs -6).
            Waveform::Triangle => {
                let mut acc = 0.0f64;
                let mut k = 1u32;
                let mut sign = 1.0f64;
                while k <= k_max {
                    acc += sign * (k as f64 * theta).sin() / (k as f64 * k as f64);
                    sign = -sign;
                    k += 2;
                }
                acc * FRAC_8_PI2
            }
            // All harmonics, amplitude 2*d*sinc(k*d) (`d` = PULSE_DUTY) — the textbook Fourier
            // series of a DC-free bipolar rectangular pulse train (high +1 for a `d` fraction of
            // the period, low -d/(1-d) for the rest, so it stays zero-mean without a separate DC
            // term to drop). Unlike square/triangle/sawtooth, nothing here cancels every other
            // harmonic or rolls off with k — a narrow duty cycle gives a genuinely rich, close-to-
            // flat harmonic amplitude envelope out to the cutoff (the sinc envelope's first null
            // sits at k ≈ 1/d, well past k_max for a narrow-enough duty cycle), the reason a pulse
            // train earns its own signal here rather than just being a narrower square wave.
            Waveform::Pulse => {
                let mut acc = 0.0f64;
                for k in 1..=k_max {
                    let x = k as f64 * PULSE_DUTY;
                    let sinc = if x < 1e-6 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                    acc += (k as f64 * theta).cos() * sinc;
                }
                acc * (2.0 * PULSE_DUTY)
            }
        }) as f32
    }
}

/// The one signal actually being played — a tuned waveform (needs a frequency), untuned noise
/// (doesn't), or the fixed `Isp` true-peak-over construction (needs a dB target instead of a
/// frequency — its frequency is always Fs/4, not a free parameter). Struct-style variants (rather
/// than `testtone.rs`'s original tuple ones) so this serializes to clean, self-describing JSON
/// across the Tauri IPC boundary — e.g. `{"kind":"Tone","waveform":"Sine","hz":1000.0}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Signal {
    Tone { waveform: Waveform, hz: f64 },
    Pink,
    White,
    /// True-peak-over torture test (see `testtone.rs`'s file header doc for the full derivation):
    /// a fixed Fs/4, 45°-phase sine, parameterised by how many dB above 0 dBFS the *reconstructed*
    /// peak should reach (clamped to `0.0..=ISP_MAX_OVER_DB` by the caller).
    Isp { db_over: f64 },
    /// A swept sine from `f0` to `f1` Hz over `duration_secs`, then repeating — see
    /// [`chirp_phase`] for the construction. `log` selects a logarithmic (equal time per octave,
    /// matching the spectrum view's own log-Hz axis) vs. linear (equal Hz/sec) sweep. Unlike
    /// `Tone`/`Isp`, this doesn't use the wavetable machinery at all (see `build_wavetable`'s own
    /// doc) — its instantaneous frequency changes continuously, so it's synthesized directly by
    /// the render loops via `chirp_phase` instead.
    Chirp { f0: f64, f1: f64, duration_secs: f64, log: bool },
}

/// Instantaneous phase (radians, unwrapped — the caller applies `.sin()`) of a `Signal::Chirp`
/// sweep from `f0` to `f1` Hz over `duration_secs`, evaluated at `t` seconds (`0.0..=duration_secs`
/// — the caller wraps `t` for a repeating sweep, not this function). This is the closed-form
/// integral of the instantaneous frequency over time, not a per-sample accumulated step like
/// `wavetable_step` — exact and driftless at any `t`, which matters here because the render loops
/// re-evaluate it independently every cycle rather than carrying accumulated phase across the
/// repeat boundary (see `render_generator`'s own doc on the per-cycle fade that hides the seam).
///
/// Linear (`log: false`): instantaneous frequency `f(t) = f0 + (f1-f0)·t/T`, so
/// `phase(t) = 2π·(f0·t + (f1-f0)·t²/(2T))` (the integral of `2π·f(t)`).
///
/// Logarithmic (`log: true`): instantaneous frequency `f(t) = f0·(f1/f0)^(t/T)` — equal time per
/// octave — so `phase(t) = 2π·f0·T/ln(f1/f0)·((f1/f0)^(t/T) - 1)`, the standard exponential
/// ("log") sine sweep construction. Falls back to the linear formula when `f0` and `f1` are
/// (nearly) equal, since `ln(f1/f0)` would otherwise divide by ~0 — degenerate case, but a
/// legitimate one (a "sweep" with no actual sweep is just a constant tone, which the linear
/// formula already produces correctly when `f0 == f1`).
pub fn chirp_phase(f0: f64, f1: f64, duration_secs: f64, log: bool, t: f64) -> f64 {
    use std::f64::consts::TAU;
    let linear = || TAU * (f0 * t + (f1 - f0) * t * t / (2.0 * duration_secs));
    if log && f0 > 0.0 && f1 > 0.0 && (f1 / f0 - 1.0).abs() > 1e-9 {
        let k = (f1 / f0).ln();
        TAU * f0 * duration_secs / k * ((k * t / duration_secs).exp() - 1.0)
    } else {
        linear()
    }
}

/// The threshold this project treats as "safe for a real user's device" for a signal aimed at an
/// arbitrary/unknown playback chain — audible but conservative. Extra headroom over the -3 dBFS
/// the dev `testtone` CLI allows on its own non-`--unsafe` ceiling: that's fine for a tool you
/// have to deliberately open a terminal to run, but not for a GUI default anyone can reach by
/// clicking around. The app's Self-test player uses this as its fixed level; the in-app test-tone
/// generator window's own non-`unsafe_mode` ceiling reuses the same number for the same reason —
/// one shared, deliberately-chosen threshold rather than two independently-picked ones drifting
/// apart over time.
pub const SAFE_PLAYBACK_CEILING_DBFS: f32 = -18.0;

/// The exact dB the true (reconstructed) peak sits above the sample peak for `Signal::Isp`'s Fs/4,
/// 45°-phase-offset construction (20·log10(√2) = 10·log10(2)) — see `testtone.rs`'s file header
/// doc for the derivation. Also the largest true-peak overshoot this construction can produce,
/// since it's the deterministic maximum a single pure tone gives you.
pub const ISP_MAX_OVER_DB: f64 = 3.0103;

/// Fixed table resolution for a `Signal::Tone`'s wavetable, independent of the requested
/// frequency — see [`build_wavetable`]'s own doc for why: tying the table length to an exact
/// integer cycle count (as an earlier version of this did, `round(rate/hz)`) means the *actual*
/// played frequency is `rate/table_len`, not the requested one, and that rounding error is small
/// at low frequencies (`table_len` in the hundreds/thousands) but large at high ones (`table_len`
/// in the single digits — several percent off, degenerating completely near Nyquist/2, e.g. a
/// 2-sample "sine" table is `[sin(0), sin(π)] = [0, 0]`, silent rather than merely detuned). A
/// fixed, large table plus fractional-phase linear interpolation ([`wavetable_step`]/
/// [`wavetable_sample`]) reproduces the exact requested frequency at *any* table size, so this
/// number is a pure quality knob (interpolation distortion — negligible at this size for any
/// audible content) rather than something coupled to pitch accuracy at all.
pub const TONE_TABLE_LEN: usize = 4096;

/// Build one full period's wavetable for `signal` at `rate` Hz — unit-ish amplitude (a `Tone`
/// peaks at ~1; `Isp`'s samples peak at ±1/√2, the gain applied on top is what pushes the *true*
/// peak to the requested dBTP — see `testtone.rs`'s own extensive doc on that derivation). The
/// caller applies its own per-sample gain; this only builds the shape — playback advances through
/// it via [`wavetable_step`]/[`wavetable_sample`], not a plain integer index (see their own docs
/// for why: a plain wraparound index is exactly what used to make `Tone` play the wrong
/// frequency at the top of its range).
///
/// Only meaningful for `Signal::Tone`/`Signal::Isp` — `Pink`/`White` aren't periodic, so callers
/// handle those separately via [`PinkNoise`], which generates per-sample from an RNG+filter
/// instead of a lookup table.
pub fn build_wavetable(signal: Signal, rate: u32) -> Vec<f32> {
    let tone_hz = if let Signal::Tone { hz, .. } = signal { hz } else { 0.0 };
    // Highest harmonic to sum, kept a few percent below true Nyquist rather than right up against
    // it — a naive square/triangle/sawtooth has harmonics to infinity, which would alias back down
    // and contaminate the very spectrum this signal exists to let you check against a known-correct
    // shape. Depends only on rate/hz, not on the table's own resolution below.
    let k_max: u32 = if tone_hz > 0.0 { (((rate as f64 * 0.48) / tone_hz).floor().max(1.0)) as u32 } else { 1 };
    // `Tone`'s table is always `TONE_TABLE_LEN` samples spanning one full 2π cycle, regardless of
    // `hz` — see that constant's own doc. `Signal::Isp` is fixed at exactly 4 samples/cycle (Fs/4,
    // by construction, not rounding) with a 45° phase offset baked in, reusing `Waveform::Sine`'s
    // own `sin()` rather than needing a shape of its own.
    let table_len = match signal {
        Signal::Tone { .. } if tone_hz > 0.0 => TONE_TABLE_LEN,
        Signal::Isp { .. } => 4,
        _ => 1,
    };
    (0..table_len)
        .map(|i| match signal {
            Signal::Tone { waveform, .. } => {
                waveform.sample(std::f64::consts::TAU * (i as f64) / (table_len as f64), k_max)
            }
            Signal::Isp { .. } => Waveform::Sine.sample(
                std::f64::consts::TAU * (i as f64) / (table_len as f64) + std::f64::consts::FRAC_PI_4,
                1,
            ),
            _ => 0.0,
        })
        .collect()
}

/// Per-sample phase-accumulator advance (in table-index units) for `signal`'s wavetable, paired
/// with [`wavetable_sample`]. `Tone`'s step is a real, generally non-integer number — that's the
/// whole fix: the exact requested `hz` comes from advancing through a *fixed-size* table at
/// whatever fractional rate reproduces it, rather than by choosing the table's size to make the
/// step exactly 1. `Isp`'s step is always exactly `1.0` (its 4-sample table has no fractional
/// positions to interpolate between — see [`wavetable_sample`]'s own doc for why that's safe).
/// `Pink`/`White` don't use a wavetable at all; `0.0` is unused dead weight for them.
pub fn wavetable_step(signal: Signal, rate: u32) -> f64 {
    match signal {
        Signal::Tone { hz, .. } => TONE_TABLE_LEN as f64 * hz / rate as f64,
        Signal::Isp { .. } => 1.0,
        _ => 0.0,
    }
}

/// Read `table` at a fractional `phase` (table-index units, any real value — wraps automatically),
/// linearly interpolating between the two entries straddling it. Serves both `Tone` and `Isp`
/// wavetables with the one function: `Isp`'s step ([`wavetable_step`]) is always exactly `1.0` and
/// phase starts at `0.0`, so phase never lands anywhere but an exact integer for it — the
/// interpolation's fractional part is always `0.0`, which degenerates to plain indexing
/// automatically and preserves its exact ±1/√2 construction bit-for-bit. No branching needed.
pub fn wavetable_sample(table: &[f32], phase: f64) -> f32 {
    let len = table.len();
    let p = phase.rem_euclid(len as f64);
    let i0 = p as usize;
    let i1 = (i0 + 1) % len;
    let frac = (p - i0 as f64) as f32;
    table[i0] * (1.0 - frac) + table[i1] * frac
}

/// Stateful white/pink noise generator — xorshift RNG + Paul Kellet's economy pink filter.
/// Extracted from what were, before this, two independent copies of the same math (`testtone.rs`,
/// and `cageq-monitor`'s own `render_pink`). `render_pink` (the app's Self-test signal) is
/// deliberately left on its own inline copy rather than switched to this — see its doc for why:
/// zero behavior risk to an existing, relied-on feature outweighs deduplicating an already-correct
/// dozen lines. Only the new generator path (`render_generator`) uses this.
pub struct PinkNoise {
    rng: u32,
    b0: f32,
    b1: f32,
    b2: f32,
}

impl Default for PinkNoise {
    fn default() -> Self {
        PinkNoise { rng: 0x2545_f491, b0: 0.0, b1: 0.0, b2: 0.0 } // xorshift seed, filter state
    }
}

impl PinkNoise {
    pub fn new() -> Self {
        Self::default()
    }

    /// Next white-noise sample in `[-1, 1]`.
    pub fn next_white(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Next pink-noise sample — Paul Kellet's economy filter over [`next_white`](Self::next_white).
    pub fn next_pink(&mut self) -> f32 {
        let white = self.next_white();
        self.b0 = 0.99765 * self.b0 + white * 0.0990460;
        self.b1 = 0.96300 * self.b1 + white * 0.2965164;
        self.b2 = 0.57000 * self.b2 + white * 1.0526913;
        (self.b0 + self.b1 + self.b2 + white * 0.1848) * 0.11
    }
}

/// Parameters for the Tauri-driven test-tone generator (`TestSignal::start_generator`). Already
/// resolved/clamped by the caller (mirrors `testtone.rs`'s own arg validation: `Isp` only valid
/// under `unsafe_mode`, `level_dbfs` ceiling -3 dBFS unless `unsafe_mode`, etc.) — this struct
/// carries the *outcome* of that policy, not the policy itself, so it stays reusable regardless of
/// which UI (or CLI) is deciding what's safe.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GeneratorParams {
    pub signal: Signal,
    /// Gain, in dB. For `Signal::Isp` this IS the requested true-peak target directly, not an
    /// offset from it — see `testtone.rs`'s own extensive doc on why (and the sign-error bug that
    /// briefly hid behind getting this backwards).
    pub level_dbfs: f32,
    /// Forces a *source* rate different from the device's — Windows' shared-mode resampler then
    /// converts it, so resampling artifacts show up wherever this signal is captured.
    pub rate_override: Option<u32>,
    /// Auto-stop after this many seconds (with a fade-out); `None` plays until [`stop`](super::TestSignal::stop).
    pub seconds: Option<f32>,
    /// Final per-sample hard limiter: ~0.891 (-1 dBFS) normally, 1.0 under `unsafe_mode` so a
    /// 0 dBFS signal passes through untouched.
    pub sample_ceil: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the Isp construction (see this module's own doc): samples land at
    /// exactly ±1/√2, independent of rate. Pins that after the tuple→struct-variant extraction.
    #[test]
    fn isp_wavetable_is_four_samples_at_plus_minus_one_over_root_two() {
        let table = build_wavetable(Signal::Isp { db_over: 3.0103 }, 48_000);
        assert_eq!(table.len(), 4);
        let expected = (1.0 / 2f64.sqrt()) as f32;
        for &s in &table {
            assert!((s.abs() - expected).abs() < 1e-6, "{s} not within 1e-6 of ±{expected}");
        }
        // Signs alternate in pairs: +, +, -, - (see this module's `build_wavetable` doc).
        assert!(table[0] > 0.0 && table[1] > 0.0 && table[2] < 0.0 && table[3] < 0.0);
    }

    /// A tone's wavetable is always `TONE_TABLE_LEN` samples, regardless of `hz` — the old
    /// contract (`round(rate/hz)`) is gone on purpose, see that constant's own doc for why tying
    /// table length to frequency was the actual bug.
    #[test]
    fn tone_wavetable_length_is_fixed_regardless_of_frequency() {
        let low = build_wavetable(Signal::Tone { waveform: Waveform::Sine, hz: 100.0 }, 48_000);
        let high = build_wavetable(Signal::Tone { waveform: Waveform::Sine, hz: 15_437.0 }, 48_000);
        assert_eq!(low.len(), TONE_TABLE_LEN);
        assert_eq!(high.len(), TONE_TABLE_LEN);
    }

    /// The actual regression test for the bug this session found live: at `round(rate/hz)` table
    /// sizing, a 15,437 Hz tone at 48 kHz played at 16,000 Hz instead (`table_len` rounds to 3).
    /// Runs the real per-sample playback loop (`wavetable_step`/`wavetable_sample`, exactly what
    /// `render_generator`/`testtone.rs` do) and estimates the resulting frequency from rising
    /// zero-crossings — simple and sufficient for a sanity bound, no need for the FFT machinery
    /// `cageq-monitor`'s own `find_peaks` regression test already covers that with.
    #[test]
    fn an_awkward_high_frequency_tone_plays_at_the_requested_frequency() {
        let rate = 48_000u32;
        let hz = 15_437.0f64;
        let signal = Signal::Tone { waveform: Waveform::Sine, hz };
        let table = build_wavetable(signal, rate);
        let step = wavetable_step(signal, rate);

        let n = rate as usize; // 1 second — many cycles at this frequency
        let mut phase = 0.0f64;
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            samples.push(wavetable_sample(&table, phase));
            phase += step;
        }

        // Rising zero-crossings, linearly interpolated to a fractional sample index.
        let mut crossings = Vec::new();
        for i in 1..samples.len() {
            let (prev, cur) = (samples[i - 1], samples[i]);
            if prev <= 0.0 && cur > 0.0 {
                let frac = -prev / (cur - prev);
                crossings.push((i - 1) as f64 + frac as f64);
            }
        }
        assert!(crossings.len() >= 2, "need at least one full cycle to estimate frequency");
        let cycles = (crossings.len() - 1) as f64;
        let estimated_hz = cycles * rate as f64 / (crossings.last().unwrap() - crossings[0]);

        assert!(
            (estimated_hz - hz).abs() < 1.0,
            "played at {estimated_hz:.2} Hz, requested {hz} Hz — the old round(rate/hz) table \
             sizing would have played this at 16000 Hz (table_len rounds to 3)"
        );
    }

    /// Estimate `chirp_phase`'s instantaneous frequency at `t` by central finite difference —
    /// `f(t) = phase'(t) / 2π`. Independent of the closed-form derivation, so this pins the actual
    /// behavior rather than just re-deriving the same formula.
    fn chirp_instantaneous_hz(f0: f64, f1: f64, duration_secs: f64, log: bool, t: f64) -> f64 {
        let dt = 1e-6;
        let dphase = chirp_phase(f0, f1, duration_secs, log, t + dt) - chirp_phase(f0, f1, duration_secs, log, t - dt);
        dphase / (2.0 * dt) / std::f64::consts::TAU
    }

    #[test]
    fn chirp_phase_starts_at_zero() {
        for log in [false, true] {
            assert_eq!(chirp_phase(20.0, 20_000.0, 8.0, log, 0.0), 0.0);
        }
    }

    /// The instantaneous frequency `chirp_phase`'s derivative actually produces must match each
    /// sweep type's own defining formula at an arbitrary interior point `t` — not just "near f0/f1
    /// at the edges", which (for a fast sweep like 20 Hz→20 kHz over a few seconds) can already be
    /// many Hz away from f0/f1 within a millisecond of t=0/duration, a property of the *ramp rate*
    /// rather than a bug (checked at t=0/duration exactly by `chirp_phase_starts_at_zero` and by
    /// construction — `chirp_phase`'s own doc — at t=duration).
    #[test]
    fn chirp_instantaneous_frequency_matches_the_defining_formula() {
        let (f0, f1, duration) = (20.0, 20_000.0, 8.0);
        for t in [0.5, 2.0, duration / 2.0, 6.0, duration - 0.5] {
            let linear_expected = f0 + (f1 - f0) * t / duration;
            let linear_actual = chirp_instantaneous_hz(f0, f1, duration, false, t);
            assert!(
                (linear_actual - linear_expected).abs() / linear_expected < 1e-4,
                "linear t={t}: {linear_actual} not near expected {linear_expected}"
            );

            let log_expected = f0 * (f1 / f0).powf(t / duration);
            let log_actual = chirp_instantaneous_hz(f0, f1, duration, true, t);
            assert!(
                (log_actual - log_expected).abs() / log_expected < 1e-4,
                "log t={t}: {log_actual} not near expected {log_expected}"
            );
        }
    }

    #[test]
    fn chirp_instantaneous_frequency_is_near_f0_and_f1_right_at_the_endpoints() {
        // Right at the very edge (a small fraction of duration, not a fixed 1ms — see the
        // defining-formula test's own doc for why a fixed offset isn't meaningful across sweep
        // rates), both sweep types should read close to f0/f1.
        let (f0, f1, duration) = (20.0, 20_000.0, 8.0);
        for log in [false, true] {
            let start_hz = chirp_instantaneous_hz(f0, f1, duration, log, duration * 1e-5);
            let end_hz = chirp_instantaneous_hz(f0, f1, duration, log, duration * (1.0 - 1e-5));
            assert!((start_hz - f0).abs() / f0 < 0.01, "log={log}: {start_hz} not near f0={f0}");
            assert!((end_hz - f1).abs() / f1 < 0.01, "log={log}: {end_hz} not near f1={f1}");
        }
    }

    #[test]
    fn chirp_phase_falls_back_to_linear_when_f0_equals_f1() {
        // A degenerate "sweep" with no actual sweep is just a constant tone — log's ln(f1/f0)
        // would divide by ~0, so this must take the linear branch instead (see chirp_phase's doc).
        let hz = chirp_instantaneous_hz(1000.0, 1000.0, 8.0, true, 4.0);
        assert!((hz - 1000.0).abs() < 1.0, "expected ~1000 Hz, got {hz}");
    }

    #[test]
    fn pink_noise_is_deterministic_and_bounded() {
        let mut a = PinkNoise::new();
        let mut b = PinkNoise::new();
        for _ in 0..1000 {
            let (wa, wb) = (a.next_white(), b.next_white());
            assert_eq!(wa, wb, "same seed must reproduce the same sequence");
            assert!((-1.0..=1.0).contains(&wa), "white sample {wa} outside [-1,1]");
        }
        for _ in 0..1000 {
            let p = a.next_pink();
            assert!(p.is_finite(), "pink sample was not finite");
            assert!(p.abs() < 2.0, "pink sample {p} implausibly large for a normalized filter");
        }
    }
}
