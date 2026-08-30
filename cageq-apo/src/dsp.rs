//! The filter engine: an RBJ biquad cascade plus preamp, and the coefficient math behind it.
//!
//! ## The contract this has to meet
//! CAGEq's UI *draws* the correction it is about to apply, and the self-test measures what
//! came back. Both lean on `cageq-core`'s `morph.rs` / the frontend's `biquad.ts`, which
//! agree with AutoEq's own `peq.py`. **This engine must produce exactly that curve** — if it
//! doesn't, the chart quietly becomes a lie about what you are hearing, and every downstream
//! measurement is off by an amount nobody can see.
//!
//! So the coefficient formulas below are a deliberate transcription of `morph.rs`'s
//! `coefficients()`, and the tests check the *realised* response of the actual sample loop
//! against the analytic one — see the test module, which is the point of this file as much
//! as the code is.
//!
//! ## Sign convention (easy to get backwards, so stated once)
//! `morph.rs::coefficients` returns `a1`/`a2` **pre-negated**, AutoEq-style; `biquad.ts`'s
//! `biquadCoeffs` un-negates them for the runtime filter. This module stores the
//! **un-negated** (standard RBJ) form throughout and evaluates
//! `y = b0·x + b1·x₁ + b2·x₂ − a1·y₁ − a2·y₂`, matching `stepBiquad`.
//!
//! ## Rate
//! Coefficients depend on the sample rate, and the APO is locked to whatever the endpoint
//! runs at — *not* the fixed 48 kHz `morph.rs` draws its chart against. The rate is
//! therefore an explicit parameter everywhere rather than a constant.

/// Hard cap on bands in the cascade, so the sample loop never allocates. CAGEq's own limit
/// is 20 (matching AQUA's), and this leaves headroom above it; `set_bands` refuses more
/// rather than silently truncating a correction.
pub const MAX_BANDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    Peaking,
    LowShelf,
    HighShelf,
    /// RBJ band-pass at unity peak; `gain_db` is ignored (the §5.2 isolate audition).
    Bandpass,
}

#[derive(Debug, Clone, Copy)]
pub struct Band {
    pub kind: FilterKind,
    pub freq_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

/// One biquad's difference-equation coefficients, `a0` normalised to 1 and `a1`/`a2` in the
/// standard (un-negated) convention — see the module doc.
#[derive(Debug, Clone, Copy, Default)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coeffs {
    /// The identity filter — passes its input through untouched.
    pub const PASSTHROUGH: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// `20·log₁₀|H(e^{jω})|` at `freq_hz`. Analytic; used by the tests as the reference the
    /// sample loop is checked against, and cheap enough to be worth keeping for diagnostics.
    pub fn response_db(&self, freq_hz: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq_hz / sample_rate;
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        // z = e^{-jω}, so the imaginary parts carry the minus sign.
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let power = (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im);
        10.0 * power.log10()
    }
}

/// RBJ cookbook coefficients for one band at `sample_rate`.
///
/// Transcribed from `cageq-core`'s `morph.rs::coefficients` (itself matching AutoEq's
/// `peq.py`), then un-negated into the standard convention — see the module doc. Keep the
/// two in step: the chart and the audio must not drift apart.
pub fn coefficients(band: &Band, sample_rate: f64) -> Coeffs {
    let a = 10.0_f64.powf(band.gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * band.freq_hz / sample_rate;
    let alpha = w0.sin() / (2.0 * band.q);
    let cosw = w0.cos();
    let sqrt_a = a.sqrt();

    // (a0, a1, a2, b0, b1, b2) in the RAW RBJ convention, before normalising by a0.
    let (a0, a1, a2, b0, b1, b2) = match band.kind {
        FilterKind::Peaking => (
            1.0 + alpha / a,
            -2.0 * cosw,
            1.0 - alpha / a,
            1.0 + alpha * a,
            -2.0 * cosw,
            1.0 - alpha * a,
        ),
        FilterKind::LowShelf => (
            a + 1.0 + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha,
            -2.0 * (a - 1.0 + (a + 1.0) * cosw),
            a + 1.0 + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha,
            a * (a + 1.0 - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha),
            2.0 * a * (a - 1.0 - (a + 1.0) * cosw),
            a * (a + 1.0 - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha),
        ),
        FilterKind::HighShelf => (
            a + 1.0 - (a - 1.0) * cosw + 2.0 * sqrt_a * alpha,
            2.0 * (a - 1.0 - (a + 1.0) * cosw),
            a + 1.0 - (a - 1.0) * cosw - 2.0 * sqrt_a * alpha,
            a * (a + 1.0 + (a - 1.0) * cosw + 2.0 * sqrt_a * alpha),
            -2.0 * a * (a - 1.0 + (a + 1.0) * cosw),
            a * (a + 1.0 + (a - 1.0) * cosw - 2.0 * sqrt_a * alpha),
        ),
        FilterKind::Bandpass => {
            (1.0 + alpha, -2.0 * cosw, 1.0 - alpha, alpha, 0.0, -alpha)
        }
    };

    Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// One biquad's Direct-Form-I delay registers, for one channel.
///
/// These two pairs are the entire reason CAGEq is building its own APO (filter.md §5.3c):
/// Equalizer APO zeroes them on every config reload, and that cold start is the audible
/// "bloom" that undermines A/B comparison. Here they simply persist across coefficient
/// changes — state-carry by construction, with nothing to remember to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadState {
    #[inline]
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        *self = BiquadState::default();
    }
}

/// The full per-endpoint engine: a preamp and up to [`MAX_BANDS`] biquads, with independent
/// filter state per channel.
///
/// All storage is allocated up front, in [`Cascade::new`], because everything after that
/// runs on audiodg's real-time thread.
pub struct Cascade {
    channels: usize,
    sample_rate: f64,
    /// Active coefficients, `[0..band_count]`.
    coeffs: [Coeffs; MAX_BANDS],
    band_count: usize,
    /// Delay registers, indexed `[channel * MAX_BANDS + band]` — flat rather than nested so
    /// the sample loop walks contiguous memory.
    state: Vec<BiquadState>,
    /// Linear preamp gain (not dB): applied before the cascade, as EqAPO's `Preamp:` is.
    preamp: f64,
}

impl Cascade {
    pub fn new(channels: usize, sample_rate: f64) -> Cascade {
        Cascade {
            channels,
            sample_rate,
            coeffs: [Coeffs::PASSTHROUGH; MAX_BANDS],
            band_count: 0,
            state: vec![BiquadState::default(); channels * MAX_BANDS],
            preamp: 1.0,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn band_count(&self) -> usize {
        self.band_count
    }

    /// Replace the filter set. Returns `false` (changing nothing) if `bands` exceeds
    /// [`MAX_BANDS`] — refusing beats silently dropping the tail of someone's correction.
    ///
    /// **Filter state is deliberately NOT reset.** That is the whole point: a retuned band
    /// keeps its predecessor's delay registers, so the discontinuity is proportional to the
    /// coefficient change and decays, instead of the cold-start transient a reload causes.
    /// See [`BiquadState`].
    pub fn set_bands(&mut self, bands: &[Band]) -> bool {
        if bands.len() > MAX_BANDS {
            return false;
        }
        for (slot, band) in self.coeffs.iter_mut().zip(bands) {
            *slot = coefficients(band, self.sample_rate);
        }
        // Newly-unused slots become identity, so a shrinking cascade cannot leave a stale
        // filter running purely because band_count moved.
        for slot in self.coeffs[bands.len()..].iter_mut() {
            *slot = Coeffs::PASSTHROUGH;
        }
        self.band_count = bands.len();
        true
    }

    /// Set the preamp in dB (negative attenuates), matching EqAPO's `Preamp:` line.
    pub fn set_preamp_db(&mut self, db: f64) {
        self.preamp = 10.0_f64.powf(db / 20.0);
    }

    /// Clear every delay register — a genuine cold start. Only for a discontinuity where
    /// carrying state would be wrong (a fresh lock, a device change), never for an edit.
    pub fn reset_state(&mut self) {
        for s in self.state.iter_mut() {
            s.reset();
        }
    }

    /// The cascade's total predicted response in dB at `freq_hz` — biquads multiply, so dB
    /// add. Includes the preamp. This is what the UI's curve should equal; exposed so a test
    /// (or a future self-check) can compare the two without re-deriving the math.
    pub fn response_db(&self, freq_hz: f64) -> f64 {
        let filters: f64 = self.coeffs[..self.band_count]
            .iter()
            .map(|c| c.response_db(freq_hz, self.sample_rate))
            .sum();
        filters + 20.0 * self.preamp.log10()
    }

    /// Process `frames` × `channels` interleaved samples, `input` → `output`.
    ///
    /// **Real-time**: no allocation, no locking, no branching on anything but the band
    /// count. `input` and `output` may be the same buffer (the APO declares
    /// `APO_FLAG_INPLACE`), which is safe here because each sample is read before its own
    /// slot is written and nothing looks ahead.
    pub fn process(&mut self, input: &[f32], output: &mut [f32], frames: usize) {
        let channels = self.channels;
        let count = frames * channels;
        debug_assert!(input.len() >= count && output.len() >= count);

        for frame in 0..frames {
            for ch in 0..channels {
                let i = frame * channels + ch;
                let mut x = input[i] as f64 * self.preamp;
                let base = ch * MAX_BANDS;
                for b in 0..self.band_count {
                    x = self.state[base + b].step(&self.coeffs[b], x);
                }
                output[i] = x as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    fn peaking(f: f64, gain: f64, q: f64) -> Band {
        Band { kind: FilterKind::Peaking, freq_hz: f, gain_db: gain, q }
    }

    /// Magnitude in dB at `freq_hz`, measured from the **actual sample loop** by driving it
    /// with a unit impulse and evaluating the DFT of the impulse response at that frequency.
    ///
    /// This is the ground truth the whole module is checked against, and it is deliberately
    /// end-to-end: it exercises the difference equation, the delay registers, the preamp and
    /// the interleaving, not just the coefficient formulas. A transcription slip that left
    /// the analytic response self-consistently wrong would still be caught, because the
    /// analytic side is compared to *this*.
    ///
    /// 1 << 15 samples is far longer than any of these filters' ring-down, so truncation
    /// error sits well below the tolerances asserted below.
    fn measured_db(cascade: &mut Cascade, freq_hz: f64) -> f64 {
        const N: usize = 1 << 15;
        let channels = cascade.channels;
        cascade.reset_state();

        let mut input = vec![0.0f32; channels];
        let mut output = vec![0.0f32; channels];
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = 2.0 * std::f64::consts::PI * freq_hz / cascade.sample_rate();

        for n in 0..N {
            // Unit impulse on channel 0 only; the others stay silent, which incidentally
            // proves the per-channel state does not bleed across channels.
            input[0] = if n == 0 { 1.0 } else { 0.0 };
            cascade.process(&input, &mut output, 1);
            let h = output[0] as f64;
            re += h * (w * n as f64).cos();
            im -= h * (w * n as f64).sin();
        }
        10.0 * (re * re + im * im).log10()
    }

    /// The realised response of the sample loop must match the analytic one, for every filter
    /// type, across the audible band. This is the test that would catch a wrong sign, a
    /// swapped coefficient, or a mis-ordered delay update.
    #[test]
    fn measured_response_matches_the_analytic_one() {
        let cases = [
            ("peaking boost", peaking(1000.0, 6.0, 1.4)),
            ("peaking cut", peaking(3150.0, -8.5, 4.0)),
            ("low shelf", Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 5.0, q: 0.7 }),
            ("high shelf", Band { kind: FilterKind::HighShelf, freq_hz: 8000.0, gain_db: -4.0, q: 0.7 }),
            ("bandpass", Band { kind: FilterKind::Bandpass, freq_hz: 500.0, gain_db: 0.0, q: 2.0 }),
        ];

        for (name, band) in cases {
            let mut c = Cascade::new(2, FS);
            assert!(c.set_bands(&[band]));
            for &f in &[30.0, 100.0, 440.0, 1000.0, 3150.0, 8000.0, 16000.0] {
                let measured = measured_db(&mut c, f);
                let analytic = c.response_db(f);
                assert!(
                    (measured - analytic).abs() < 0.01,
                    "{name} at {f} Hz: measured {measured:.4} dB vs analytic {analytic:.4} dB",
                );
            }
        }
    }

    /// A biquad cascade multiplies, so its dB response is the sum of the bands' — the
    /// property the UI's composed curve relies on, checked against the real sample loop.
    #[test]
    fn a_cascade_adds_in_db() {
        let bands = [peaking(120.0, 4.0, 1.0), peaking(1000.0, -3.0, 2.0), peaking(6000.0, 2.5, 0.9)];

        let mut single = Cascade::new(2, FS);
        let mut expected = [0.0f64; 5];
        let probes = [80.0, 120.0, 1000.0, 6000.0, 12000.0];
        for band in &bands {
            single.set_bands(std::slice::from_ref(band));
            for (i, &f) in probes.iter().enumerate() {
                expected[i] += single.response_db(f);
            }
        }

        let mut all = Cascade::new(2, FS);
        assert!(all.set_bands(&bands));
        for (i, &f) in probes.iter().enumerate() {
            let measured = measured_db(&mut all, f);
            assert!(
                (measured - expected[i]).abs() < 0.01,
                "cascade at {f} Hz: measured {measured:.4} dB vs summed {:.4} dB",
                expected[i],
            );
        }
    }

    /// The preamp is a flat gain: it shifts the whole response and nothing else.
    #[test]
    fn preamp_is_a_flat_offset() {
        let mut c = Cascade::new(2, FS);
        c.set_bands(&[peaking(1000.0, 6.0, 1.4)]);
        let before: Vec<f64> = [100.0, 1000.0, 10000.0].iter().map(|&f| measured_db(&mut c, f)).collect();

        c.set_preamp_db(-6.0);
        for (i, &f) in [100.0, 1000.0, 10000.0].iter().enumerate() {
            let after = measured_db(&mut c, f);
            assert!(
                (after - (before[i] - 6.0)).abs() < 0.01,
                "preamp at {f} Hz: {after:.4} dB, expected {:.4} dB",
                before[i] - 6.0,
            );
        }
    }

    /// Buffer boundaries must not be audible: the delay registers have to carry across
    /// calls, so chunking a stream cannot change the output. (This is the same property
    /// that makes state-carry work, checked at the buffer level.)
    #[test]
    fn chunking_the_stream_changes_nothing() {
        let bands = [peaking(200.0, 6.0, 3.0), peaking(4000.0, -5.0, 1.2)];
        let frames = 1000;
        let signal: Vec<f32> =
            (0..frames * 2).map(|i| ((i as f64 * 0.017).sin() * 0.3) as f32).collect();

        let mut whole = Cascade::new(2, FS);
        whole.set_bands(&bands);
        let mut out_whole = vec![0.0f32; frames * 2];
        whole.process(&signal, &mut out_whole, frames);

        let mut chunked = Cascade::new(2, FS);
        chunked.set_bands(&bands);
        let mut out_chunked = vec![0.0f32; frames * 2];
        for (chunk, start) in [(300usize, 0usize), (1, 300), (699, 301)] {
            let s = start * 2;
            let e = s + chunk * 2;
            let mut tmp = vec![0.0f32; chunk * 2];
            chunked.process(&signal[s..e], &mut tmp, chunk);
            out_chunked[s..e].copy_from_slice(&tmp);
        }

        for i in 0..frames * 2 {
            assert!(
                (out_whole[i] - out_chunked[i]).abs() < 1e-6,
                "sample {i}: whole {} vs chunked {}",
                out_whole[i],
                out_chunked[i],
            );
        }
    }

    /// APO_FLAG_INPLACE: Windows may hand the same buffer as input and output.
    #[test]
    fn processing_in_place_matches_out_of_place() {
        let bands = [peaking(800.0, -4.0, 1.5)];
        let frames = 256;
        let signal: Vec<f32> =
            (0..frames * 2).map(|i| ((i as f64 * 0.03).sin() * 0.4) as f32).collect();

        let mut a = Cascade::new(2, FS);
        a.set_bands(&bands);
        let mut out = vec![0.0f32; frames * 2];
        a.process(&signal, &mut out, frames);

        let mut b = Cascade::new(2, FS);
        b.set_bands(&bands);
        let mut buf = signal.clone();
        let copy = buf.clone();
        b.process(&copy, &mut buf, frames);

        assert_eq!(out, buf);
    }

    /// No bands and no preamp is exact identity — the state the APO holds until CAGEq
    /// pushes a correction, so it must not colour anything.
    #[test]
    fn an_empty_cascade_is_exact_identity() {
        let mut c = Cascade::new(2, FS);
        let signal: Vec<f32> = (0..64).map(|i| (i as f32 - 32.0) / 64.0).collect();
        let mut out = vec![0.0f32; 64];
        c.process(&signal, &mut out, 32);
        assert_eq!(out, signal, "an unconfigured cascade must pass samples through bit-exact");
    }

    /// Too many bands is refused outright, leaving the running cascade untouched — a
    /// correction that silently lost its tail would be worse than one that did not apply.
    #[test]
    fn too_many_bands_is_refused_without_disturbing_the_running_set() {
        let mut c = Cascade::new(2, FS);
        assert!(c.set_bands(&[peaking(1000.0, 3.0, 1.0)]));
        let before = c.response_db(1000.0);

        let too_many: Vec<Band> = (0..MAX_BANDS + 1).map(|i| peaking(100.0 + i as f64, 1.0, 1.0)).collect();
        assert!(!c.set_bands(&too_many));
        assert_eq!(c.band_count(), 1);
        assert_eq!(c.response_db(1000.0), before);
    }

    /// Shrinking the cascade must actually stop the dropped bands, not just move the count.
    ///
    /// Checked against the *remaining* band's own analytic response, not against 0 dB: a
    /// Q=1.0 peak at 1 kHz still contributes ~0.6 dB at 5 kHz through its skirt, so "the
    /// dropped band is gone" and "5 kHz reads flat" are different claims, and only the first
    /// one is true. (An earlier version of this test asserted the second and failed on
    /// correct behaviour.)
    #[test]
    fn shrinking_the_cascade_stops_the_dropped_bands() {
        let kept = peaking(1000.0, 12.0, 1.0);
        let mut c = Cascade::new(2, FS);
        c.set_bands(&[kept, peaking(5000.0, 12.0, 1.0)]);
        c.set_bands(&[kept]);

        let measured = measured_db(&mut c, 5000.0);
        let expected = c.response_db(5000.0);
        assert!(
            (measured - expected).abs() < 0.01,
            "after dropping the 5 kHz band, 5 kHz reads {measured:.4} dB but the remaining \
             band alone predicts {expected:.4} dB",
        );
        // And the dropped band really was doing something there, or this proves nothing.
        assert!(expected < 11.0, "sanity: the dropped +12 dB band should have dominated 5 kHz");
    }

    /// A continuous tone, sample `start..start+n`. Continuing a signal across a retune means
    /// continuing its *phase*; restarting it at sample 0 injects a discontinuity into the
    /// input and measures nothing about the filter (which an earlier version of the
    /// state-carry test below did, and duly "failed" on correct behaviour).
    fn tone(freq_hz: f64, start: usize, n: usize, amp: f64) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (start + i) as f64 / FS;
                ((2.0 * std::f64::consts::PI * freq_hz * t).sin() * amp) as f32
            })
            .collect()
    }

    /// **The reason this APO exists** (filter.md §5.3c). Retuning a band keeps its delay
    /// registers, so the output continues smoothly; Equalizer APO zeroes them on every config
    /// reload, and that cold start is the audible "bloom" that wrecks A/B comparison.
    ///
    /// Measured as a comparison rather than an absolute threshold: drive three identical
    /// cascades warm, then retune one carrying state (what we do), retune another with the
    /// state cleared (what a reload does), and leave the third running as the continuous
    /// reference. Carrying state must track the reference far more closely than restarting
    /// cold does — which is the actual claim, and is what an absolute tolerance on one sample
    /// could never establish.
    #[test]
    fn retuning_carries_state_instead_of_restarting_cold() {
        const WARM: usize = 4000;
        const TAIL: usize = 500;
        let before = peaking(100.0, 6.0, 2.0);
        let after = peaking(100.0, 6.1, 2.0); // a nudge, as a live tone edit would be

        let warm = tone(100.0, 0, WARM, 0.5);
        let cont = tone(100.0, WARM, TAIL, 0.5); // phase-continuous with `warm`

        let mut carried = Cascade::new(1, FS);
        let mut cold = Cascade::new(1, FS);
        let mut reference = Cascade::new(1, FS);
        let mut sink = vec![0.0f32; WARM];
        for c in [&mut carried, &mut cold, &mut reference] {
            c.set_bands(&[before]);
            c.process(&warm, &mut sink, WARM);
        }

        carried.set_bands(&[after]); // state persists — the point
        cold.set_bands(&[after]);
        cold.reset_state(); // what a config-reload engine does to you
        // `reference` keeps running `before`, uninterrupted.

        let (mut a, mut b, mut r) = (vec![0.0f32; TAIL], vec![0.0f32; TAIL], vec![0.0f32; TAIL]);
        carried.process(&cont, &mut a, TAIL);
        cold.process(&cont, &mut b, TAIL);
        reference.process(&cont, &mut r, TAIL);

        let peak_dev = |x: &[f32]| {
            x.iter().zip(&r).map(|(p, q)| (p - q).abs()).fold(0.0f32, f32::max)
        };
        let carried_dev = peak_dev(&a);
        let cold_dev = peak_dev(&b);

        assert!(
            carried_dev * 5.0 < cold_dev,
            "state-carry should track the uninterrupted signal far better than a cold \
             restart: carried deviates {carried_dev:.5}, cold-started {cold_dev:.5}",
        );
        // And the cold restart must be a genuinely audible transient, or the comparison is
        // between two indistinguishable things and proves nothing.
        assert!(cold_dev > 0.01, "cold restart produced no transient to improve on ({cold_dev:.5})");
    }
}
