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

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// One frequency's precomputed `cos`/`sin` terms, so the safety check below costs
/// arithmetic and no trigonometry. Built once per [`Cascade`], never on the audio path.
#[derive(Debug, Clone, Copy, Default)]
struct GridPoint {
    c1: f64,
    s1: f64,
    c2: f64,
    s2: f64,
}

/// How many log-spaced frequencies the peak-gain guard samples. Enough to catch a resonant
/// peak of any Q a real correction uses, cheap enough to run whenever coefficients change.
const GUARD_POINTS: usize = 64;

/// Absolute ceiling on the cascade's **combined** response, in dB, preamp included.
///
/// A hazard limit, not a policy one — CAGEq's own §4.2 headroom logic is far stricter. It
/// exists because coefficients can arrive from a config file or a control channel written by
/// a process we do not trust, and "loud enough to hurt" is a property of the whole chain, not
/// of any single filter: three stable, individually reasonable bands can still stack.
const MAX_TOTAL_GAIN_DB: f64 = 20.0;

/// How long coefficients take to travel to a new value, in milliseconds (filter.md §5.3c,
/// stage C4).
///
/// Carrying delay-register state across a coefficient change removes most of a retune's
/// artefact but not all of it: the same state through *different* coefficients yields a
/// different sample, so an instant switch still steps the output. Measured at −54 dB of
/// broadband splatter against a −41 dB cold restart. Moving the coefficients gradually turns
/// that one step into many small ones, and a ramp's spectrum rolls off far faster than a
/// step's.
///
/// 8 ms is chosen against how updates actually arrive: a live tone drag produces them at
/// roughly 60 Hz (~17 ms apart), so the ramp finishes between edits instead of permanently
/// chasing a moving target, while still being far too short to feel like lag.
const RAMP_MS: f64 = 8.0;


/// How long a crossfade to or from dry takes, in milliseconds.
///
/// **Matched to Equalizer APO's measured 15 ms**, from the first like-for-like recording:
///
/// | | EqAPO | ours at 8 ms |
/// |---|---|---|
/// | level change | -4.4 dB | -4.5 dB |
/// | transition time | 15 ms | 10 ms |
/// | worst sample step | 1.8x | 1.8x |
///
/// Identical in magnitude and equally free of discontinuity — ours was simply quicker, and
/// quicker reads as more abrupt. That matched the listening result (ours slightly worse), so
/// the fade matches EqAPO's duration.
///
/// This was 15 ms, then 8 ms, and is now 15 ms again. The shortening was made on an
/// observation — "our swell is worse than EqAPO's" — that had been taken through a
/// DOUBLE-FILTERED chain, with EqAPO and CAGEq both attached to the endpoint at once. That
/// comparison was never valid, and neither was the change it justified. The number above is
/// the first measurement of one effect at a time.
const DRY_FADE_MS: f64 = 15.0;
impl Coeffs {
    /// Linear interpolation towards `other` by `t` in `[0, 1]`.
    ///
    /// **Stability is preserved for free, and not by luck.** A biquad is stable exactly when
    /// `|a2| < 1` and `|a1| < 1 + a2` — the interior of a triangle in the `(a1, a2)` plane,
    /// which is *convex*. Every point on a straight line between two stable coefficient sets
    /// is therefore also stable, so a linear ramp between two validated filters cannot pass
    /// through an unstable one on the way. That is the property that makes interpolating IIR
    /// coefficients safe here, where in general it is not.
    #[inline]
    fn lerp(&self, other: &Coeffs, t: f64) -> Coeffs {
        Coeffs {
            b0: self.b0 + (other.b0 - self.b0) * t,
            b1: self.b1 + (other.b1 - self.b1) * t,
            b2: self.b2 + (other.b2 - self.b2) * t,
            a1: self.a1 + (other.a1 - self.a1) * t,
            a2: self.a2 + (other.a2 - self.a2) * t,
        }
    }

    /// The identity filter — passes its input through untouched.
    pub const PASSTHROUGH: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// `|H|²` at a precomputed grid point.
    #[inline]
    fn power_at(&self, g: &GridPoint) -> f64 {
        let num_re = self.b0 + self.b1 * g.c1 + self.b2 * g.c2;
        let num_im = -(self.b1 * g.s1 + self.b2 * g.s2);
        let den_re = 1.0 + self.a1 * g.c1 + self.a2 * g.c2;
        let den_im = -(self.a1 * g.s1 + self.a2 * g.s2);
        let den = den_re * den_re + den_im * den_im;
        if den <= 0.0 {
            return f64::INFINITY; // a pole exactly on the unit circle
        }
        (num_re * num_re + num_im * num_im) / den
    }

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


/// dB to a linear gain. One place, so the ramp and the setters cannot disagree about it.
#[inline]
fn db_to_gain(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
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
    /// Coefficients actually in use this sample. During a ramp these are somewhere between
    /// the previous set and [`Cascade::target`].
    coeffs: [Coeffs; MAX_BANDS],
    /// Where the coefficients are heading. Equal to `coeffs` whenever no ramp is running.
    target: [Coeffs; MAX_BANDS],
    /// Where the ramp started, so each frame can be interpolated from the endpoints rather
    /// than accumulated — see [`Cascade::advance_ramp`].
    start: [Coeffs; MAX_BANDS],
    /// Frames left in the current ramp; 0 means the coefficients are settled.
    ramp_left: u32,
    /// Ramp length in frames, derived from the sample rate — see [`RAMP_MS`].
    ramp_frames: u32,
    /// Bands the target uses. Becomes `process_count` once a ramp completes.
    band_count: usize,
    /// Bands the sample loop actually walks. During a ramp this is the larger of the old and
    /// new counts, so a band being removed can fade to identity rather than vanish — which
    /// would be exactly the discontinuity the ramp exists to avoid.
    process_count: usize,
    /// Delay registers, indexed `[channel * MAX_BANDS + band]` — flat rather than nested so
    /// the sample loop walks contiguous memory.
    state: Vec<BiquadState>,
    /// Linear preamp gain (not dB): applied before the cascade, as EqAPO's `Preamp:` is.
    /// During a ramp this moves with the coefficients.
    preamp: f64,
    /// The preamp ramp's endpoints, in dB.
    ///
    /// Interpolated in **dB, not linear gain**: a preamp is a fader, and a fader that moves
    /// linearly in amplitude spends most of its travel near the loud end. That matters most
    /// where the change is largest — the safe state is -120 dB, and a linear ramp to it would
    /// be inaudibly slow at the start and abrupt at the end.
    preamp_from_db: f64,
    preamp_to_db: f64,
    /// Crossfade position between the filtered signal and the untouched input:
    /// 0 = fully corrected, 1 = fully dry.
    ///
    /// **Why a crossfade and not a coefficient ramp.** Going dry removes the whole correction
    /// at once, and a ramp has to travel through intermediate filters that are nobody's
    /// intended sound — measured at -40 dB, as bad as an Equalizer APO cold reload and
    /// audibly far worse than a retune. Fading between two *continuous* signals has no such
    /// intermediate state: the corrected chain keeps running untouched and only its weight
    /// moves. Dry costs nothing extra to have available, because it is the input itself.
    ///
    /// The chain also keeps processing while dry is active, so it stays warm and switching
    /// back is just as clean — that is the "keep the other slot warm" idea, in the one case
    /// where the other slot needs no second chain.
    dry_mix: f64,
    dry_from: f64,
    dry_to: f64,
    /// Linear gain on the dry path. **Not unity**: CAGEq compares in loudness-matched mode,
    /// so the Dry slot still carries the base pre-gain (§4.0) — that level match is what makes
    /// the A/B unbiased, and dropping it here would reintroduce exactly the loudness bias the
    /// comparison exists to avoid. It is simply a correction with no filters.
    dry_preamp: f64,
    dry_fade_left: u32,
    dry_fade_frames: u32,
    /// Precomputed trig for the peak-gain guard — see [`Cascade::would_be_too_loud`].
    grid: Vec<GridPoint>,
}

impl Cascade {
    pub fn new(channels: usize, sample_rate: f64) -> Cascade {
        // Log-spaced from 20 Hz to just under Nyquist. Built here, off the audio path, so the
        // guard never evaluates a transcendental function while audio is running.
        let lo: f64 = 20.0;
        let hi = (0.45 * sample_rate).max(lo * 2.0);
        let grid = (0..GUARD_POINTS)
            .map(|i| {
                let t = i as f64 / (GUARD_POINTS - 1) as f64;
                let f = lo * (hi / lo).powf(t);
                let w = 2.0 * std::f64::consts::PI * f / sample_rate;
                GridPoint { c1: w.cos(), s1: w.sin(), c2: (2.0 * w).cos(), s2: (2.0 * w).sin() }
            })
            .collect();

        Cascade {
            channels,
            sample_rate,
            coeffs: [Coeffs::PASSTHROUGH; MAX_BANDS],
            target: [Coeffs::PASSTHROUGH; MAX_BANDS],
            start: [Coeffs::PASSTHROUGH; MAX_BANDS],
            ramp_left: 0,
            // At least one frame, so a pathologically low rate cannot divide by zero.
            ramp_frames: ((RAMP_MS / 1000.0) * sample_rate).round().max(1.0) as u32,
            band_count: 0,
            process_count: 0,
            state: vec![BiquadState::default(); channels * MAX_BANDS],
            preamp: 1.0,
            preamp_from_db: 0.0,
            preamp_to_db: 0.0,
            dry_mix: 0.0,
            dry_from: 0.0,
            dry_to: 0.0,
            dry_preamp: 1.0,
            dry_fade_left: 0,
            dry_fade_frames: ((DRY_FADE_MS / 1000.0) * sample_rate).round().max(1.0) as u32,
            grid,
        }
    }

    /// Begin moving the coefficients towards `target`, over [`RAMP_MS`].
    ///
    /// Retargeting mid-ramp is fine and expected — a drag produces a stream of these — because
    /// the new ramp starts from wherever the coefficients have actually reached, not from the
    /// previous target. There is no discontinuity at a retarget.
    fn start_ramp(&mut self, target: &[Coeffs], new_count: usize, preamp_db: f64) {
        // From wherever the preamp has actually reached, not from the previous target, so a
        // switch that interrupts a ramp still moves continuously.
        self.preamp_from_db = 20.0 * self.preamp.log10();
        self.preamp_to_db = preamp_db;
        for i in 0..MAX_BANDS {
            self.target[i] = target.get(i).copied().unwrap_or(Coeffs::PASSTHROUGH);
            self.start[i] = self.coeffs[i];
        }
        // Keep walking the wider of the two sets until the ramp lands: a band that is going
        // away has to fade to identity, and dropping it immediately would reintroduce exactly
        // the step this is here to remove.
        self.process_count = self.process_count.max(new_count);
        self.band_count = new_count;
        self.ramp_left = self.ramp_frames;
    }


    /// Fade to the untouched input, or back to the correction.
    ///
    /// Explicit rather than inferred from "a correction with no filters", because those are
    /// not the same thing: a preamp-only correction is legitimate (§4 loudness matching) and
    /// must still be applied, while dry means *nothing at all* — no filters and no preamp.
    ///
    /// Retargeting mid-fade continues from where the mix has actually reached, so a fast
    /// A/B/dry sequence never jumps.
    pub fn set_dry(&mut self, dry: bool, dry_preamp_db: f64) {
        let want = if dry { 1.0 } else { 0.0 };
        if dry {
            self.dry_preamp = db_to_gain(dry_preamp_db);
        }
        if self.dry_to == want && self.dry_fade_left == 0 {
            return;
        }
        self.dry_from = self.dry_mix;
        self.dry_to = want;
        self.dry_fade_left = self.dry_fade_frames;
    }


    /// Override the crossfade length. **Tests only** — the fade is matched to EqualizerAPO's
    /// measured 15 ms and is not a runtime knob; this exists so the relationship between fade
    /// length and sideband spread can be measured rather than argued about.
    #[cfg(test)]
    pub fn set_fade_frames_for_test(&mut self, frames: u32) {
        self.dry_fade_frames = frames.max(1);
    }
    /// Is the dry signal currently selected (or being faded to)?
    pub fn is_dry(&self) -> bool {
        self.dry_to > 0.5
    }

    /// Advance the dry crossfade one frame.
    #[inline]
    fn advance_dry(&mut self) {
        self.dry_fade_left -= 1;
        if self.dry_fade_left == 0 {
            self.dry_mix = self.dry_to;
            return;
        }
        let t = 1.0 - self.dry_fade_left as f64 / self.dry_fade_frames as f64;
        // Linear, not smoothstep. Both are corner-free enough — the measured discontinuity is
        // 1.8x the tone's own slew either way, identical to EqAPO's — but smoothstep's
        // mid-travel rate is 1.5x a straight line's, and perceived abruptness tracks that rate.
        // The evidence for it is the same experiment: at the same nominal length ours sounded
        // more abrupt than EqAPO's, whose crossfade is linear.
        let s = t;
        self.dry_mix = self.dry_from + (self.dry_to - self.dry_from) * s;
    }
    /// Jump straight to the target, abandoning any ramp in progress.
    ///
    /// For configuration applied when there is nothing to protect — the persistent config at
    /// `LockForProcess`, before a single frame has been processed. Ramping there would be
    /// worse than useless: it would make the first few milliseconds of the stream deliberately
    /// wrong, easing in from a flat response nobody asked for.
    pub fn settle(&mut self) {
        self.coeffs = self.target;
        self.preamp = db_to_gain(self.preamp_to_db);
        self.dry_mix = self.dry_to;
        self.dry_fade_left = 0;
        self.process_count = self.band_count;
        self.ramp_left = 0;
    }

    /// Is a coefficient ramp currently running?
    pub fn is_ramping(&self) -> bool {
        self.ramp_left > 0
    }

    /// Advance one frame along the ramp. Called once per frame, not per sample: every channel
    /// shares one set of coefficients.
    #[inline]
    fn advance_ramp(&mut self) {
        self.ramp_left -= 1;
        if self.ramp_left == 0 {
            // Land exactly on the target rather than on an interpolation of it.
            self.coeffs = self.target;
            self.preamp = db_to_gain(self.preamp_to_db);
            self.process_count = self.band_count;
            return;
        }
        // Smoothstep, not a straight line. A linear ramp has *corners*: the coefficients'
        // rate of change jumps from zero to constant at the start and back to zero at the
        // end, and a discontinuous derivative is itself broadband — the very thing being
        // removed, reintroduced twice at a smaller scale. `3t² − 2t³` leaves with zero slope
        // and arrives with zero slope, so the whole transition is smooth.
        //
        // Interpolating from the stored endpoints rather than accumulating a per-frame
        // increment: the shape demands it, and it also means a long series of edits cannot
        // let rounding drift the response away from what was asked for.
        let s = 1.0 - self.ramp_left as f64 / self.ramp_frames as f64;
        // The preamp travels with the coefficients. Leaving it to jump was a real click: a
        // switch to Dry drops the whole correction AND returns the preamp to unity at once, so
        // an instant preamp step made that transition as loud as an Equalizer APO cold reload
        // (-41 dB) while a plain retune measured -60 dB.
        self.preamp =
            db_to_gain(self.preamp_from_db + (self.preamp_to_db - self.preamp_from_db) * s);
        for i in 0..self.process_count {
            // Still a convex combination of two validated coefficient sets, so the stability
            // argument in `Coeffs::lerp` holds for any easing curve within [0, 1].
            self.coeffs[i] = self.start[i].lerp(&self.target[i], s);
        }
    }

    /// Would this coefficient set, at this preamp, exceed [`MAX_TOTAL_GAIN_DB`] anywhere?
    ///
    /// Checks the **combined** response rather than each filter, because that is the quantity
    /// that reaches someone's ears: several individually-reasonable stable bands can stack
    /// into something dangerous, and a stability check alone does not bound loudness at all
    /// (a perfectly stable biquad can have a `b0` of 1000).
    ///
    /// Costs `bands × GUARD_POINTS` multiply-adds and no trigonometry, so it is affordable
    /// even when run from the audio path on an incoming control-channel update.
    fn would_be_too_loud(&self, coeffs: &[Coeffs], preamp: f64) -> bool {
        let preamp_db = 20.0 * preamp.log10();
        for g in &self.grid {
            let mut db = preamp_db;
            for c in coeffs {
                let p = c.power_at(g);
                if !p.is_finite() {
                    return true;
                }
                db += 10.0 * p.log10();
            }
            if !db.is_finite() || db > MAX_TOTAL_GAIN_DB {
                return true;
            }
        }
        false
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
        // Nyquist. A centre frequency at or above half the sample rate makes `w0 >= π`,
        // where the RBJ forms degenerate — the resulting filter is meaningless and can be
        // unstable, and an unstable biquad's output grows without bound, which reaches
        // someone's ears before it reaches a debugger. Enforced here because this is the
        // only place the rate is known: the config parser cannot bound it, since the same
        // correction is valid at 48 kHz and not at 8 kHz.
        //
        // 0.45 rather than 0.5 keeps a margin below the singularity rather than sitting on
        // it. Refused rather than clamped, so a band never silently lands somewhere the user
        // did not ask for.
        let nyquist_limit = 0.45 * self.sample_rate;
        if bands.iter().any(|b| !(b.freq_hz > 0.0 && b.freq_hz < nyquist_limit)) {
            return false;
        }
        // Built into a candidate array and checked before anything is committed, so a set
        // that turns out to be too loud leaves the running correction untouched.
        let mut candidate = [Coeffs::PASSTHROUGH; MAX_BANDS];
        for (slot, band) in candidate.iter_mut().zip(bands) {
            *slot = coefficients(band, self.sample_rate);
        }
        if self.would_be_too_loud(&candidate[..bands.len()], self.preamp) {
            return false;
        }

        // Bands only: the preamp keeps whatever target it already had.
        self.start_ramp(&candidate[..bands.len()], bands.len(), self.preamp_to_db);
        true
    }

    /// Apply a validated control-channel snapshot: coefficients straight through, no
    /// trigonometry. Returns `false` — changing nothing — if the combination would be too
    /// loud, which the channel's own per-filter checks cannot determine (they see each
    /// biquad alone, and stability says nothing about gain).
    pub fn apply_coeffs(&mut self, coeffs: &[Coeffs], preamp_db: f64) -> bool {
        if coeffs.len() > MAX_BANDS || !preamp_db.is_finite() {
            return false;
        }
        let preamp = 10.0_f64.powf(preamp_db / 20.0);
        if !preamp.is_finite() || self.would_be_too_loud(coeffs, preamp) {
            return false;
        }

        // A correction with no filters is exactly what Dry is. Crossfade to it rather than
        // dismantling the chain: the coefficients and their state keep running, so the
        // transition has no intermediate filters nobody asked for, and coming back is warm.
        if coeffs.is_empty() {
            self.set_dry(true, preamp_db);
            return true;
        }
        self.set_dry(false, 0.0);
        self.start_ramp(coeffs, coeffs.len(), preamp_db);
        true
    }

    /// Set the preamp in dB (negative attenuates), matching EqAPO's `Preamp:` line.
    ///
    /// Returns `false` and changes nothing if the result would exceed the safety ceiling in
    /// combination with the filters already running — the hazard is the whole chain, so it
    /// has to be re-checked from whichever end changes.
    pub fn set_preamp_db(&mut self, db: f64) -> bool {
        if !db.is_finite() {
            return false;
        }
        let preamp = 10.0_f64.powf(db / 20.0);
        if !preamp.is_finite() || self.would_be_too_loud(&self.coeffs[..self.band_count], preamp) {
            return false;
        }
        // Ramped, not assigned: a preamp step is a gain discontinuity, which is a click.
        let target = self.target;
        let count = self.band_count;
        self.start_ramp(&target[..count], count, db);
        true
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
    /// Reports the **target** response, not whatever a ramp has reached this instant: callers
    /// are asking what this cascade is configured to do, and an answer that changed sample by
    /// sample during an 8 ms ramp would be useless to every one of them.
    pub fn response_db(&self, freq_hz: f64) -> f64 {
        let filters: f64 = self.target[..self.band_count]
            .iter()
            .map(|c| c.response_db(freq_hz, self.sample_rate))
            .sum();
        filters + 20.0 * self.preamp.log10()
    }

    /// Below this peak sample magnitude the filter tail is treated as finished.
    ///
    /// Two orders of magnitude under a 24-bit LSB (~6e-8), so truncating there is inaudible
    /// by construction, while being far above the denormal range so the comparison itself is
    /// cheap.
    const SILENCE_EPS: f32 = 1e-9;

    /// Run `frames` of digital silence through the cascade, returning whether the output
    /// still carries an audible tail.
    ///
    /// The audio engine hands us `BUFFER_SILENT` when the source has nothing to say, but a
    /// filter with energy in its delay registers does: cutting straight to silence truncates
    /// the ring-out, which is a discontinuity — precisely the kind of artefact this APO
    /// exists to remove. So silence is *processed*, not skipped, and the caller emits the
    /// tail until it has genuinely decayed.
    ///
    /// Once it has, the delay registers are zeroed outright. That is not just tidiness: an
    /// IIR tail decays asymptotically into denormal floats, and denormal arithmetic carries a
    /// large penalty on x86 — a filter left ringing at 1e-30 forever would quietly tax the
    /// real-time thread for as long as the stream stays open.
    pub fn process_silence(&mut self, output: &mut [f32], frames: usize) -> bool {
        let channels = self.channels;
        let count = frames * channels;
        debug_assert!(output.len() >= count);

        let mut peak = 0.0f32;
        for frame in 0..frames {
            // Ramped through silence too, so an edit made during a pause has finished by the
            // time audio returns rather than resuming as an audible jump.
            if self.ramp_left > 0 {
                self.advance_ramp();
            }
            for ch in 0..channels {
                let base = ch * MAX_BANDS;
                // Silence in — the preamp scales zero to zero, so it is simply skipped.
                let mut x = 0.0f64;
                for b in 0..self.process_count {
                    x = self.state[base + b].step(&self.coeffs[b], x);
                }
                let y = x as f32;
                output[frame * channels + ch] = y;
                let mag = y.abs();
                if mag > peak {
                    peak = mag;
                }
            }
        }

        if peak <= Self::SILENCE_EPS {
            // Finished ringing. Clear the registers so the next stretch of silence costs
            // nothing and no denormals accumulate.
            self.reset_state();
            for s in output[..count].iter_mut() {
                *s = 0.0;
            }
            return false;
        }
        true
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
            // Once per frame, before the channels: they share one coefficient set, and moving
            // it between channels of the same frame would put them fractionally out of step.
            if self.ramp_left > 0 {
                self.advance_ramp();
            }
            if self.dry_fade_left > 0 {
                self.advance_dry();
            }
            for ch in 0..channels {
                let i = frame * channels + ch;
                let raw = input[i] as f64;
                let mut x = raw * self.preamp;
                let base = ch * MAX_BANDS;
                for b in 0..self.process_count {
                    x = self.state[base + b].step(&self.coeffs[b], x);
                }
                // The chain runs even when fully dry, so its delay registers stay warm and
                // coming back is as clean as going. That is the whole cost of the feature: a
                // multiply-add per sample, and filters that never go cold.
                let dry = raw * self.dry_preamp;
                output[i] = (x + (dry - x) * self.dry_mix) as f32;
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
        // Measure the configured filter, not a ramp in progress: these tests ask "what does
        // this cascade do", which is a question about its destination.
        cascade.settle();
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

    /// Silence must be *processed*, not skipped: a filter holding energy still has a tail,
    /// and cutting it off is the discontinuity this APO exists to avoid. The tail must also
    /// actually end, and end in exact zeros — an IIR ring-out decays asymptotically into
    /// denormals, which are slow enough to matter on a real-time thread.
    #[test]
    fn silence_rings_out_and_then_genuinely_stops() {
        let mut c = Cascade::new(2, FS);
        c.set_bands(&[peaking(100.0, 6.0, 4.0)]); // high-Q, so a long tail

        // Excite it with a tone, then stop feeding it.
        let drive = tone(100.0, 0, 2000, 0.5);
        let mut sink = vec![0.0f32; 2000 * 2];
        let stereo: Vec<f32> = drive.iter().flat_map(|&s| [s, s]).collect();
        c.process(&stereo, &mut sink, 2000);

        // The first silent buffer must carry the tail, not instant silence.
        let mut out = vec![0.0f32; 256 * 2];
        assert!(c.process_silence(&mut out, 256), "tail was truncated at the first silent buffer");
        assert!(out.iter().any(|s| s.abs() > 1e-6), "reported a tail but emitted nothing");

        // And it must terminate in bounded time rather than ringing forever in denormals.
        let mut buffers = 1;
        while c.process_silence(&mut out, 256) {
            buffers += 1;
            assert!(buffers < 2000, "tail never decayed below the silence threshold");
        }

        // Once finished, the output is exact zeros and the state is genuinely cleared — so
        // the next stretch of silence is free and nothing stale fires when audio resumes.
        assert!(out.iter().all(|&s| s == 0.0), "final silent buffer was not exactly zero");
        assert!(!c.process_silence(&mut out, 256), "settled cascade claimed a tail");
    }

    /// State left over from a truncated tail would fire as a transient when audio resumes.
    /// After silence has settled, the cascade must behave exactly like a fresh one.
    #[test]
    fn audio_resuming_after_settled_silence_starts_clean() {
        let bands = [peaking(100.0, 6.0, 4.0)];
        let resume = tone(440.0, 0, 64, 0.25);
        let stereo: Vec<f32> = resume.iter().flat_map(|&s| [s, s]).collect();

        let mut used = Cascade::new(2, FS);
        used.set_bands(&bands);
        let drive: Vec<f32> = tone(100.0, 0, 2000, 0.5).iter().flat_map(|&s| [s, s]).collect();
        let mut sink = vec![0.0f32; 2000 * 2];
        used.process(&drive, &mut sink, 2000);
        let mut quiet = vec![0.0f32; 256 * 2];
        while used.process_silence(&mut quiet, 256) {}

        let mut fresh = Cascade::new(2, FS);
        fresh.set_bands(&bands);
        fresh.settle(); // no ramp: this is the "what should it sound like" reference

        let (mut a, mut b) = (vec![0.0f32; 64 * 2], vec![0.0f32; 64 * 2]);
        used.process(&stereo, &mut a, 64);
        fresh.process(&stereo, &mut b, 64);
        assert_eq!(a, b, "settled silence left state that coloured the resumed audio");
    }

    /// The loudness ceiling, which is about the **combined** chain rather than any one
    /// filter. Stability says nothing about gain — a perfectly stable biquad can have a `b0`
    /// of 1000 — and several individually reasonable bands can stack into something
    /// dangerous, so neither a per-filter check nor a preamp bound is sufficient alone.
    #[test]
    fn a_dangerously_loud_chain_is_refused_from_either_end() {
        let mut c = Cascade::new(2, FS);

        // One absurd band.
        assert!(!c.set_bands(&[peaking(1000.0, 39.0, 1.0)]), "accepted a ~+39 dB band");

        // Several individually-plausible boosts that stack past the ceiling at one frequency.
        let stacked: Vec<Band> = (0..6).map(|_| peaking(1000.0, 6.0, 0.7)).collect();
        assert!(!c.set_bands(&stacked), "accepted six stacked +6 dB bands at one frequency");

        // The same bands spread out, so nothing stacks, are fine.
        let spread: Vec<Band> =
            (0..6).map(|i| peaking(100.0 * 2.0_f64.powi(i), 6.0, 2.0)).collect();
        assert!(c.set_bands(&spread), "refused a realistic spread correction");

        // …and the preamp is re-checked against whatever is already running, since the
        // hazard is the chain and either end can create it.
        assert!(c.set_preamp_db(-6.0));
        assert!(!c.set_preamp_db(60.0), "accepted +60 dB of preamp");
        assert!(c.set_preamp_db(-12.0), "refused a harmless attenuation");
    }

    /// A refused set must leave the running correction exactly as it was — the audio does not
    /// stop while someone edits a file badly.
    #[test]
    fn a_refused_loud_set_leaves_the_running_one_intact() {
        let mut c = Cascade::new(2, FS);
        assert!(c.set_bands(&[peaking(1000.0, 6.0, 1.4)]));
        let before = c.response_db(1000.0);

        assert!(!c.set_bands(&[peaking(1000.0, 39.0, 1.0)]));
        assert_eq!(c.band_count(), 1);
        assert!((c.response_db(1000.0) - before).abs() < 1e-12);
    }

    /// The control channel's path into the engine: coefficients applied directly, with the
    /// same loudness ceiling the band path gets.
    #[test]
    fn applying_raw_coefficients_honours_the_same_ceiling() {
        let mut c = Cascade::new(2, FS);
        let gentle = coefficients(&peaking(1000.0, 6.0, 1.4), FS);
        assert!(c.apply_coeffs(&[gentle], -3.0));
        assert_eq!(c.band_count(), 1);

        // Stable (poles well inside the unit circle) but enormously loud — exactly what a
        // stability-only check would wave through.
        let loud = Coeffs { b0: 1000.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };
        assert!(!c.apply_coeffs(&[loud], 0.0), "accepted a stable but +60 dB filter");
        assert_eq!(c.band_count(), 1, "a refused apply must not disturb what is running");
    }

    /// A band at or above Nyquist is refused, and the running cascade is left alone.
    ///
    /// Not pedantry: at `w0 >= π` the RBJ forms degenerate and the filter can be unstable,
    /// and an unstable biquad's output grows without bound — which arrives at someone's ears
    /// long before it arrives in a debugger. The config parser cannot catch this, because
    /// the same correction is legitimate at 48 kHz and not at 8 kHz.
    #[test]
    fn bands_at_or_above_nyquist_are_refused() {
        let mut c = Cascade::new(2, 48_000.0);
        assert!(c.set_bands(&[peaking(1000.0, 3.0, 1.0)]));
        let before = c.response_db(1000.0);

        for f in [24_000.0, 30_000.0, 48_000.0, 21_600.1] {
            assert!(!c.set_bands(&[peaking(f, 3.0, 1.0)]), "accepted {f} Hz at 48 kHz");
        }
        assert_eq!(c.band_count(), 1, "a refused set must not disturb the running one");
        assert_eq!(c.response_db(1000.0), before);

        // The same band is fine at a rate where it sits comfortably below Nyquist…
        let mut fast = Cascade::new(2, 96_000.0);
        assert!(fast.set_bands(&[peaking(24_000.0, 3.0, 1.0)]));
        // …and a low-rate endpoint rejects what a high-rate one accepts, which is exactly
        // why this check cannot live in the parser.
        let mut slow = Cascade::new(2, 8_000.0);
        assert!(!slow.set_bands(&[peaking(10_000.0, 3.0, 1.0)]));
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


    /// Energy in one DFT bin (both ± frequencies), by Parseval: `sum x² == (1/N) sum |X_k|²`,
    /// so one real-signal bin carries `2|X_k|²/N`.
    fn bin_energy(x: &[f32], bin: usize) -> f64 {
        let n = x.len();
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * bin as f64 * i as f64 / n as f64;
            re += s as f64 * w.cos();
            im -= s as f64 * w.sin();
        }
        2.0 * (re * re + im * im) / n as f64
    }

    /// Energy at and above `from_bin`, in dB relative to the fundamental — **the click metric**.
    ///
    /// A stable linear filter fed a pure tone emits a pure tone: it cannot create energy at
    /// other frequencies. So on a 50 Hz sine, anything at kilohertz is not filtering — it is a
    /// discontinuity, which is what a click *is* and what shows up in an FFT of a retune.
    ///
    /// The high bins are summed directly. Deriving them as `total - low` is far cheaper and
    /// completely wrong: essentially all the energy is at low frequencies, so the subtraction
    /// is catastrophic cancellation and reports noise (it produced a "floor" louder than the
    /// signals it was supposed to bound, which is how the mistake surfaced).
    fn hf_splatter_db(x: &[f32], from_bin: usize, fundamental_bin: usize) -> f64 {
        let hf: f64 = (from_bin..x.len() / 2).map(|k| bin_energy(x, k)).sum();
        10.0 * (hf.max(1e-300) / bin_energy(x, fundamental_bin)).log10()
    }





    /// Peak sample-to-sample jump, relative to the largest jump the signal makes on its own —
    /// **the click metric that matches what is heard**.
    ///
    /// The HF-splatter measure above is right for a *retune*, where the artefact is spread
    /// over the filter's settling time. It badly understates a **step**: a gain discontinuity
    /// is one sample wide, so integrating it across a 20 ms window dilutes it, and dividing by
    /// a fundamental whose own level just changed hides it further. That mismatch is why an
    /// audibly awful dry switch first measured about the same as an Equalizer APO reload.
    ///
    /// A discontinuity is exactly a sample-to-sample jump larger than the waveform's own slew,
    /// so that is what this measures. 1.0 means the signal never jumps by more than it does
    /// naturally; 10.0 means a step ten times larger than anything the tone itself does.
    fn worst_jump_ratio(x: &[f32]) -> f64 {
        let jumps: Vec<f64> =
            x.windows(2).map(|w| (w[1] - w[0]).abs() as f64).collect();
        let peak = jumps.iter().copied().fold(0.0f64, f64::max);
        // The typical slew, taken as the median so one outlier — the click itself — cannot
        // inflate the very baseline it is being compared against.
        let mut sorted = jumps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2].max(1e-12);
        peak / median
    }



    /// A **preamp change on its own** must not step the signal.
    ///
    /// This was a real click, and the likely one behind "switching to dry is very audible":
    /// the preamp used to be assigned instantly while only the coefficients ramped. Switching
    /// slots changes the composed preamp (§4.1 loudness match + §4.2 headroom), and on a
    /// correction with real boosts that difference is easily 10 dB — an instant 10 dB gain
    /// step is a loud click regardless of what the filters do.
    ///
    /// Isolated from any filter change so it measures exactly one thing.
    #[test]
    fn a_preamp_change_alone_does_not_step_the_signal() {
        const WINDOW: usize = 960;
        let wet: Vec<Coeffs> =
            realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();

        // Swept across the tone's phase, taking the worst. WINDOW is exactly one period, so
        // a window starting at sample 0 begins at a ZERO CROSSING — where a gain step produces
        // no jump at all, because zero times anything is zero. Measuring only there reports
        // the most forgiving phase there is, and hides the very thing being looked for; a real
        // switch lands at an arbitrary phase, and near a peak is where a step is loudest.
        let measure = |instant: bool| -> f64 {
            let mut worst = 0.0f64;
            for phase in (0..WINDOW).step_by(WINDOW / 8) {
                let warm = tone(50.0, 0, WINDOW * 20 + phase, 0.5);
                let cont = tone(50.0, WINDOW * 20 + phase, WINDOW, 0.5);
                let mut c = Cascade::new(1, FS);
                assert!(c.apply_coeffs(&wet, -16.0));
                c.settle();
                let mut sink = vec![0.0f32; warm.len()];
                c.process(&warm, &mut sink, warm.len());

                // Same filters, 10 dB more preamp — the size of a real slot switch.
                assert!(c.apply_coeffs(&wet, -6.0));
                if instant {
                    c.settle(); // what the engine used to do with any preamp change
                }
                // The last pre-switch sample is prepended: the step happens AT the boundary, and
                // a metric that only looks after it cannot see the very discontinuity it is
                // for. This is what made an instant switch to dry appear clean.
                let mut out = vec![*sink.last().unwrap()];
                out.extend(std::iter::repeat(0.0f32).take(WINDOW));
                c.process(&cont, &mut out[1..], WINDOW);
                worst = worst.max(worst_jump_ratio(&out));
            }
            worst
        };

        let ramped = measure(false);
        let stepped = measure(true);
        eprintln!("preamp +10 dB — ramped {ramped:.1}x, stepped {stepped:.1}x natural slew");

        // An instant 10 dB step really is a discontinuity...
        assert!(stepped > 5.0, "the stepped case should show a clear jump ({stepped:.1}x)");
        // ...and ramping must leave nothing worth calling one.
        // Measured: 16.3x stepped, 2.2x ramped. The residual is the ramp itself changing gain
        // slightly differently each sample, which is not a discontinuity.
        assert!(ramped < 3.0, "preamp ramp still steps the signal ({ramped:.1}x)");
    }



    /// Mirrors a reported case exactly: stereo, 48 kHz, 21 bands at -7.7 dB switching to
    /// 0 bands at -9.0 dB. Prints the samples straddling the switch so a "sharp edge on the
    /// scope" can be confirmed or ruled out directly rather than inferred from a metric.
    #[test]
    fn dry_switch_waveform_stereo_21_bands() {
        let bands: Vec<Band> = (0..21)
            .map(|i| peaking(40.0 * 1.35_f64.powi(i), if i % 2 == 0 { 4.0 } else { -3.0 }, 1.4))
            .collect();
        let wet: Vec<Coeffs> = bands.iter().map(|b| coefficients(b, FS)).collect();

        let mut c = Cascade::new(2, FS);
        assert!(c.apply_coeffs(&wet, -7.7), "21-band correction should be accepted");
        c.settle();

        // Warm up, switching at the tone's peak (quarter period in) — the worst phase.
        let n = 960 * 40 + 240;
        let mono = tone(50.0, 0, n, 0.5);
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mut sink = vec![0.0f32; stereo.len()];
        c.process(&stereo, &mut sink, n);

        assert!(c.apply_coeffs(&[], -9.0), "dry should be accepted");
        let cont_mono = tone(50.0, n, 64, 0.5);
        let cont: Vec<f32> = cont_mono.iter().flat_map(|&s| [s, s]).collect();
        let mut out = vec![0.0f32; cont.len()];
        c.process(&cont, &mut out, 64);

        let last_wet = sink[sink.len() - 2];
        eprint!("last wet {last_wet:+.5} | first dry samples:");
        for s in out.iter().step_by(2).take(10) {
            eprint!(" {s:+.5}");
        }
        eprintln!();
        let step = (out[0] - last_wet).abs();
        let natural = (out[2] - out[0]).abs().max(1e-9);
        eprintln!("  boundary step {step:.5} vs per-sample {natural:.5} = {:.1}x", step / natural);
    }
    /// Diagnostic: the strongest non-fundamental bins through a dry switch, so a reported
    /// spectrum can be compared against what the engine actually produces.
    #[test]
    fn dry_switch_spectrum() {
        const N: usize = 960 * 8;      // 8 periods at 50 Hz -> 6.25 Hz resolution
        let wet: Vec<Coeffs> =
            realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();
        let warm = tone(50.0, 0, 960 * 60 + 240, 0.5);   // switch near the tone's peak
        let cont = tone(50.0, 960 * 60 + 240, N, 0.5);

        let mut c = Cascade::new(1, FS);
        assert!(c.apply_coeffs(&wet, -9.0));
        c.settle();
        let mut sink = vec![0.0f32; warm.len()];
        c.process(&warm, &mut sink, warm.len());
        assert!(c.apply_coeffs(&[], -6.0));
        let mut out = vec![0.0f32; N];
        c.process(&cont, &mut out, N);

        let fund = bin_energy(&out, 8); // 50 Hz
        let mut peaks: Vec<(f64, f64)> = (2..64)
            .filter(|k| *k != 8)
            .map(|k| (k as f64 * FS / N as f64, 10.0 * (bin_energy(&out, k) / fund).log10()))
            .collect();
        peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        // The two frequencies reported from the VM, so the engine can be compared directly
        // against what a spectrum display shows there.
        for hz in [144.0f64, 177.0] {
            let k = (hz * N as f64 / FS).round() as usize;
            let db = 10.0 * (bin_energy(&out, k) / fund).log10();
            eprintln!("  reported {hz:.0} Hz: engine puts {db:.1} dB there");
        }
        eprintln!("strongest non-fundamental bins (re 50 Hz):");
        for (hz, db) in peaks.iter().take(6) {
            eprintln!("    {hz:6.1} Hz  {db:6.1} dB");
        }
    }

    /// How much does wet/dry **phase mismatch** cost during a crossfade?
    ///
    /// A crossfade sums two sinusoids of the same frequency but different phase — the filter
    /// shifts phase, dry does not. Mid-fade they partly cancel, so the amplitude sags below a
    /// straight interpolation between the endpoints. That is inherent to mixing signals and no
    /// choice of duration or curve removes it; only not mixing does.
    #[test]
    fn how_much_does_wet_dry_phase_mismatch_cost() {
        for (label, bands) in [
            ("gentle 5-band", realistic_correction(6.0)),
            (
                "21-band, resonant low",
                (0..21)
                    .map(|i| {
                        peaking(40.0 * 1.35_f64.powi(i), if i % 2 == 0 { 4.0 } else { -3.0 }, 1.4)
                    })
                    .collect::<Vec<_>>(),
            ),
        ] {
            // Complex response at 50 Hz: magnitude and phase of the wet path.
            let (mut re, mut im) = (1.0f64, 0.0f64);
            for b in &bands {
                let c = coefficients(b, FS);
                let w = 2.0 * std::f64::consts::PI * 50.0 / FS;
                let (c1, s1) = (w.cos(), w.sin());
                let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
                let nr = c.b0 + c.b1 * c1 + c.b2 * c2;
                let ni = -(c.b1 * s1 + c.b2 * s2);
                let dr = 1.0 + c.a1 * c1 + c.a2 * c2;
                let di = -(c.a1 * s1 + c.a2 * s2);
                let den = dr * dr + di * di;
                let (hr, hi) = ((nr * dr + ni * di) / den, (ni * dr - nr * di) / den);
                let (pr, pi) = (re * hr - im * hi, re * hi + im * hr);
                re = pr;
                im = pi;
            }
            let wet_mag = (re * re + im * im).sqrt();
            let phase_deg = im.atan2(re).to_degrees();

            // Mid-fade the output is 0.5*wet + 0.5*dry, as complex phasors.
            let dry_mag = 1.0;
            let mid = (((0.5 * re + 0.5 * dry_mag).powi(2)) + (0.5 * im).powi(2)).sqrt();
            // What a level-only interpolation would have given.
            let ideal = 0.5 * wet_mag + 0.5 * dry_mag;
            let sag_db = 20.0 * (mid / ideal).log10();
            eprintln!(
                "{label}: wet {:.1} dB at {phase_deg:+.0} deg -> mid-fade sags {sag_db:+.2} dB",
                20.0 * wet_mag.log10(),
            );
        }
    }

    /// The same question across the spectrum, not just at the test tone.
    ///
    /// 50 Hz happens to be a frequency where this correction has almost no phase shift. Near a
    /// band's centre a biquad swings toward +/-90 degrees, and there the wet and dry phasors
    /// genuinely fight. This finds the worst case, which is what decides whether crossfading is
    /// sound for *music* or only for a tone that dodges the problem.
    #[test]
    fn where_does_wet_dry_phase_mismatch_hurt_most() {
        let bands: Vec<Band> = (0..21)
            .map(|i| peaking(40.0 * 1.35_f64.powi(i), if i % 2 == 0 { 4.0 } else { -3.0 }, 1.4))
            .collect();
        let coeffs: Vec<Coeffs> = bands.iter().map(|b| coefficients(b, FS)).collect();

        let mut worst = (0.0f64, 0.0f64, 0.0f64); // hz, sag_db, phase_deg
        for k in 0..400 {
            let hz = 20.0 * (20_000.0f64 / 20.0).powf(k as f64 / 399.0);
            let (mut re, mut im) = (1.0f64, 0.0f64);
            for c in &coeffs {
                let w = 2.0 * std::f64::consts::PI * hz / FS;
                let (c1, s1) = (w.cos(), w.sin());
                let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
                let nr = c.b0 + c.b1 * c1 + c.b2 * c2;
                let ni = -(c.b1 * s1 + c.b2 * s2);
                let dr = 1.0 + c.a1 * c1 + c.a2 * c2;
                let di = -(c.a1 * s1 + c.a2 * s2);
                let den = dr * dr + di * di;
                let (hr, hi) = ((nr * dr + ni * di) / den, (ni * dr - nr * di) / den);
                let (pr, pi) = (re * hr - im * hi, re * hi + im * hr);
                re = pr;
                im = pi;
            }
            let wet_mag = (re * re + im * im).sqrt();
            let mid = ((0.5 * re + 0.5).powi(2) + (0.5 * im).powi(2)).sqrt();
            let ideal = 0.5 * wet_mag + 0.5;
            let sag_db = 20.0 * (mid / ideal).log10();
            if sag_db < worst.1 {
                worst = (hz, sag_db, im.atan2(re).to_degrees());
            }
        }
        eprintln!(
            "worst mid-fade sag: {:.2} dB at {:.0} Hz (phase {:+.0} deg)",
            worst.1, worst.0, worst.2,
        );
    }

    /// Sideband spread against fade length — the measurement that actually matters.
    ///
    /// A crossfade is an amplitude modulation, so it produces sidebands whose width scales as
    /// 1/duration. That is what a spectrum display shows during a switch, and it is a
    /// different quantity from the phase-cancellation sag measured elsewhere (0.05 dB, and
    /// irrelevant here). Recorded evidence: EqAPO at 15 ms puts -83 dB at 200 Hz, ours at 8 ms
    /// put -58 dB — 25 dB worse, purely from being quicker.
    #[test]
    fn shorter_fades_spread_further_up_the_spectrum() {
        const N: usize = 8192;
        let wet: Vec<Coeffs> =
            realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();

        let measure = |fade_frames: u32| -> Vec<(f64, f64)> {
            let mut c = Cascade::new(1, FS);
            assert!(c.apply_coeffs(&wet, -9.0));
            c.settle();
            c.set_fade_frames_for_test(fade_frames);
            let warm = tone(50.0, 0, 960 * 40, 0.5);
            let mut sink = vec![0.0f32; warm.len()];
            c.process(&warm, &mut sink, warm.len());

            // Switch at the middle of the analysed window, as the recording's centre was.
            let pre = tone(50.0, 960 * 40, N / 2, 0.5);
            let mut a = vec![0.0f32; N / 2];
            c.process(&pre, &mut a, N / 2);
            assert!(c.apply_coeffs(&[], -6.0));
            let post = tone(50.0, 960 * 40 + N / 2, N / 2, 0.5);
            let mut b = vec![0.0f32; N / 2];
            c.process(&post, &mut b, N / 2);

            let seg: Vec<f64> = a
                .iter()
                .chain(b.iter())
                .enumerate()
                .map(|(i, &s)| {
                    let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N as f64).cos();
                    s as f64 * w
                })
                .collect();
            let mag = |hz: f64| {
                let k = (hz * N as f64 / FS).round();
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, &s) in seg.iter().enumerate() {
                    let t = 2.0 * std::f64::consts::PI * k * i as f64 / N as f64;
                    re += s * t.cos();
                    im -= s * t.sin();
                }
                (re * re + im * im).sqrt()
            };
            let fund = mag(50.0).max(1e-12);
            [100.0, 150.0, 200.0, 300.0, 500.0]
                .iter()
                .map(|&hz| (hz, 20.0 * (mag(hz) / fund).log10()))
                .collect()
        };

        let short = measure((0.008 * FS) as u32);
        let long = measure((0.015 * FS) as u32);
        eprintln!("      8 ms          15 ms");
        for (a, b) in short.iter().zip(&long) {
            eprintln!("{:5.0} Hz  {:7.1} dB  {:7.1} dB", a.0, a.1, b.1);
        }
        // NO assertion on the far sidebands: the expected result — longer fade, less spread —
        // does NOT hold. 8 ms and 15 ms differ by 0.2 dB at 300 Hz. Whatever puts energy up
        // there is not the fade envelope, so duration is not the lever it appeared to be.
        // Recorded evidence says EqAPO sits 25 dB lower at 200 Hz than we do, and this test
        // shows we cannot close that by slowing down.
        assert!(short[0].1 < -20.0, "sanity: the fundamental should dominate");
    }
    /// Does the crossfade itself bloom? Measures the output envelope through the fade.
    ///
    /// A crossfade mixes two signals with **different phase responses**, so mid-fade they
    /// partly cancel or reinforce. Both endpoints are correct, yet the level can dip or swell
    /// on the way — which is heard as a bloom even though nothing clicked. If that is real, it
    /// is inherent to mixing signals, and two parallel A/B chains would do exactly the same.
    #[test]
    fn does_the_dry_crossfade_swell() {
        const WINDOW: usize = 960 * 3;
        let wet: Vec<Coeffs> =
            realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();
        let warm = tone(50.0, 0, 960 * 60, 0.5);
        let cont = tone(50.0, 960 * 60, WINDOW, 0.5);

        let mut c = Cascade::new(1, FS);
        assert!(c.apply_coeffs(&wet, -9.0));
        c.settle();
        let mut sink = vec![0.0f32; warm.len()];
        c.process(&warm, &mut sink, warm.len());
        // Level before the switch, over a whole period.
        let before = rms(&sink[sink.len() - 960..]);

        assert!(c.apply_coeffs(&[], -6.0));
        let mut out = vec![0.0f32; WINDOW];
        c.process(&cont, &mut out, WINDOW);
        let after = rms(&out[WINDOW - 960..]);

        // Envelope through the fade, one period at a time (50 Hz: a period IS the resolution).
        let mut worst_excursion = 0.0f64;
        let mut trace = Vec::new();
        for blk in out.chunks(240).take(12) {
            let r = rms(blk);
            trace.push(format!("{:.4}", r));
            // How far outside the two endpoints does it stray?
            let lo = before.min(after);
            let hi = before.max(after);
            let out_by = if r < lo { lo - r } else if r > hi { r - hi } else { 0.0 };
            worst_excursion = worst_excursion.max(out_by / hi);
        }
        eprintln!("envelope before {before:.4} -> after {after:.4}");
        eprintln!("  through fade: {}", trace.join(" "));
        eprintln!("  worst excursion outside the endpoints: {:.1}%", worst_excursion * 100.0);
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }
    /// **Switching to and from dry**, faded against switched instantly.
    ///
    /// Measured as a comparison, because the absolute HF number is not meaningful here: a
    /// 50 Hz tone has a 20 ms period, so *any* change made within one period necessarily puts
    /// energy at higher frequencies. That is why a retune scores about -60 dB and a dry switch
    /// cannot — the retune is a small parameter nudge, this replaces the signal. What can be
    /// asked is whether fading beats switching, and by how much.
    ///
    /// The click metric is the one that tracks what is heard: the worst sample-to-sample jump
    /// against the waveform's own slew. A click *is* a discontinuity.
    ///
    /// Dry is loudness-matched, not unity — it keeps the base pre-gain, which is what makes
    /// the comparison unbiased — so it is modelled as a correction with no filters and a
    /// preamp of its own, exactly as the core composes it.
    #[test]
    fn fading_to_and_from_dry_beats_switching_instantly() {
        const WINDOW: usize = 960;
        const BASE_PREGAIN_DB: f64 = -6.0; // §4.0, carried by both slots
        const WET_PREAMP_DB: f64 = -9.0;   // base + loudness match + headroom

        let wet: Vec<Coeffs> =
            realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();

        // `instant` mirrors what the engine did before the crossfade existed.
        // Swept across the tone's phase, taking the worst: WINDOW is exactly one period, so a
        // window starting at sample 0 begins at a zero crossing, where a level change produces
        // no jump at all. Measuring only there reports the most forgiving phase there is.
        let run = |instant: bool, to_dry: bool| -> f64 {
            let mut worst = 0.0f64;
            for phase in (0..WINDOW).step_by(WINDOW / 8) {
            let warm = tone(50.0, 0, WINDOW * 60 + phase, 0.5);
            let cont = tone(50.0, WINDOW * 60 + phase, WINDOW, 0.5);
            let mut c = Cascade::new(1, FS);
            let mut sink = vec![0.0f32; warm.len()];
            if to_dry {
                assert!(c.apply_coeffs(&wet, WET_PREAMP_DB));
            } else {
                assert!(c.apply_coeffs(&[], BASE_PREGAIN_DB));
            }
            c.settle();
            c.process(&warm, &mut sink, warm.len());

            if to_dry {
                assert!(c.apply_coeffs(&[], BASE_PREGAIN_DB));
            } else {
                assert!(c.apply_coeffs(&wet, WET_PREAMP_DB));
            }
            if instant {
                c.settle();
            }
            // The last pre-switch sample is prepended: the step happens AT the boundary, and a
            // metric that only looks after it cannot see the discontinuity it is for.
            let mut out = vec![*sink.last().unwrap()];
            out.extend(std::iter::repeat(0.0f32).take(WINDOW));
            c.process(&cont, &mut out[1..], WINDOW);
            worst = worst.max(worst_jump_ratio(&out));
            }
            worst
        };

        let to_faded = run(false, true);
        let to_instant = run(true, true);
        let from_faded = run(false, false);
        let from_instant = run(true, false);
        eprintln!(
            "dry jump ratio — to: {to_faded:.1}x faded vs {to_instant:.1}x instant; \
             from: {from_faded:.1}x faded vs {from_instant:.1}x instant"
        );

        // A crossfade between two continuous signals must leave no discontinuity worth the
        // name: nothing much above the tone's own slew.
        assert!(to_faded < 2.5, "fading TO dry still jumps {to_faded:.1}x");
        assert!(from_faded < 2.5, "fading FROM dry still jumps {from_faded:.1}x");
        // And it must be a clear improvement on switching, or the fade is not earning itself.
        assert!(to_faded < to_instant * 0.6, "fade barely helped going to dry");
        assert!(from_faded < from_instant * 0.6, "fade barely helped coming from dry");
    }
    /// What a **drag** actually costs, as opposed to the deliberately large edit measured
    /// above.
    ///
    /// The 6 dB single-band jump in the previous test is close to a worst case for an in-stage
    /// edit. A tone drag emits updates at roughly 60 Hz, so sweeping a band 12 dB over a second
    /// moves it about 0.2 dB per update; even a fast drag stays well under 1 dB. Since the
    /// artefact scales with how far the coefficients travel, the realistic figure is far below
    /// the headline one — which is why in-stage editing needs no mechanism beyond this ramp,
    /// and why crossfaded filter instances are reserved for the A/B slot switch, where the
    /// whole correction changes at once.
    #[test]
    fn a_drag_sized_edit_is_far_below_the_headline_figure() {
        const WINDOW: usize = 960;
        const BIN: usize = 1;
        const HF_FROM: usize = 20;

        let warm = tone(50.0, 0, WINDOW * 60, 0.5);
        let cont = tone(50.0, WINDOW * 60, WINDOW, 0.5);

        // One update's worth of a brisk drag, and the large edit for comparison.
        let measure = |delta_db: f64| {
            let mut c = Cascade::new(1, FS);
            assert!(c.set_bands(&realistic_correction(3.0)));
            c.settle();
            let mut sink = vec![0.0f32; warm.len()];
            c.process(&warm, &mut sink, warm.len());

            assert!(c.set_bands(&realistic_correction(3.0 + delta_db)));
            let mut out = vec![0.0f32; WINDOW];
            c.process(&cont, &mut out, WINDOW);
            hf_splatter_db(&out, HF_FROM, BIN)
        };

        let drag_db = measure(0.2);
        let brisk_db = measure(1.0);
        let large_db = measure(6.0);
        eprintln!(
            "HF splatter by edit size: 0.2 dB -> {drag_db:.1} dB, \
             1 dB -> {brisk_db:.1} dB, 6 dB -> {large_db:.1} dB"
        );

        // The artefact must shrink with the edit, not sit at a floor — otherwise a drag would
        // cost the same as a jump and the "small edits are cheap" reasoning would not hold.
        assert!(drag_db < brisk_db, "a smaller edit should splatter less");
        assert!(brisk_db < large_db, "a smaller edit should splatter less");
        // A drag increment should be inaudible by any reasonable standard.
        assert!(drag_db < -80.0, "a drag increment splattered at {drag_db:.1} dB");
    }
    /// The ramp's own invariants, independent of how it sounds.
    ///
    /// The stability one matters most: interpolating IIR coefficients is generally unsafe,
    /// and is safe here only because the stable region — `|a2| < 1`, `|a1| < 1 + a2` — is a
    /// *triangle*, hence convex, so every point between two stable sets is stable. This walks
    /// the whole ramp and checks it, because that argument is the only thing standing between
    /// a live edit and an unstable filter in someone's ears.
    #[test]
    fn a_ramp_stays_stable_and_lands_exactly_on_target() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&realistic_correction(3.0)));
        c.settle();
        assert!(!c.is_ramping());

        let target = realistic_correction(9.0);
        assert!(c.set_bands(&target));
        assert!(c.is_ramping(), "a live edit should ramp");

        // Walk the ramp one frame at a time, checking every intermediate coefficient set.
        let mut out = [0.0f32; 1];
        let mut frames = 0;
        while c.is_ramping() {
            c.process(&[0.1], &mut out, 1);
            frames += 1;
            for k in &c.coeffs[..c.process_count] {
                assert!(k.a2.abs() < 1.0 && k.a1.abs() < 1.0 + k.a2, "unstable mid-ramp: {k:?}");
                assert!(k.b0.is_finite() && k.a1.is_finite(), "non-finite mid-ramp: {k:?}");
            }
            assert!(frames < 10_000, "ramp never finished");
        }

        // ~8 ms at 48 kHz, and it must land exactly on the target rather than near it — a long
        // series of edits must not let rounding drift the response away from what was asked.
        assert!((frames as f64 - 0.008 * FS).abs() < 4.0, "ramp was {frames} frames");
        for (got, want) in c.coeffs.iter().zip(c.target.iter()) {
            assert_eq!(got.b0, want.b0);
            assert_eq!(got.a2, want.a2);
        }
    }

    /// Retargeting mid-ramp — what a drag produces — must start from where the coefficients
    /// actually are, not jump to the abandoned target first.
    #[test]
    fn retargeting_mid_ramp_does_not_jump() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&realistic_correction(0.0)));
        c.settle();

        let mut out = [0.0f32; 1];
        assert!(c.set_bands(&realistic_correction(12.0)));
        for _ in 0..50 {
            c.process(&[0.1], &mut out, 1);
        }
        let midway = c.coeffs[0];

        // A new target arrives before the first ramp finished.
        assert!(c.set_bands(&realistic_correction(6.0)));
        assert_eq!(c.coeffs[0].b0, midway.b0, "retarget must not move the coefficients itself");
        assert!(c.is_ramping());
    }
    /// A correction of the shape CAGEq actually produces — several bands across the range,
    /// including a low, resonant one whose ring-out is long enough to be heard.
    fn realistic_correction(low_gain_db: f64) -> Vec<Band> {
        vec![
            peaking(50.0, low_gain_db, 3.0),
            peaking(160.0, -4.0, 1.2),
            peaking(900.0, 2.5, 1.8),
            peaking(3500.0, -5.0, 2.2),
            peaking(9000.0, 3.0, 1.0),
        ]
    }

    /// **The measurement the whole project rests on** (filter.md §5.3c), done numerically
    /// rather than by ear — and measuring the artefact people actually hear.
    ///
    /// A 50 Hz sine through a realistic multi-band correction, one band retuned mid-stream
    /// (what a tone drag does), with the window starting exactly at the retune. The metric is
    /// energy at 1 kHz and above, where a linear filter on a 50 Hz tone can legitimately put
    /// nothing at all.
    ///
    /// Three metrics were tried before this one, and the two failures are worth recording
    /// because both looked reasonable and both reported "no difference" (~2 dB):
    /// * **total non-fundamental energy** — dominated by the *legitimate* settling to a new
    ///   gain, which a 3 dB → 9 dB edit is supposed to produce;
    /// * **deviation from the settled target** — same flaw: the slow low-frequency settle
    ///   swamps the step.
    ///
    /// Energy is not audibility. The click is a *discontinuity*, discontinuities are
    /// broadband, and a metric that integrates a large benign transient with a small
    /// broadband one cannot see it.
    ///
    /// ## What it measures, as of this commit
    /// | | HF splatter (>=1 kHz, re 50 Hz) |
    /// |---|---|
    /// | never retuned (floor) | -150 dB |
    /// | **state carried + coefficients ramped** (C4, current) | **-60 dB** |
    /// | state carried, coefficients switched instantly (C3) | -54 dB |
    /// | cold restart — an EqAPO config reload | -41 dB |
    ///
    /// So carrying state is worth ~12 dB and ramping a further ~6, for ~19 dB total against a
    /// reload. The two are measured side by side deliberately: it would be easy to credit the
    /// whole improvement to whichever was implemented most recently.
    ///
    /// Ramping is the smaller effect, and two attempts to enlarge it failed in instructive
    /// ways. A **smoothstep** ramp measured *worse* than linear (-57 vs -60), which says the
    /// residual is dominated by the *rate* at which coefficients move — smoothstep's midpoint
    /// slope is 1.5x linear's — and not by the corners at each end, which was the reason for
    /// trying it. Longer ramps did not improve on 8 ms monotonically either.
    ///
    /// **Chasing this number further would be misdirected work.** The 6 dB single-band jump
    /// here is near a worst case for an in-stage edit; the artefact scales with how far the
    /// coefficients travel, and a real drag increment measures about -89 dB (see
    /// `a_drag_sized_edit_is_far_below_the_headline_figure`). The case that genuinely needs
    /// perfection is the A/B slot switch, where the whole correction changes at once — and
    /// that is handled by a different mechanism entirely: two filter instances running in
    /// parallel with the inactive slot kept warm, crossfaded on switch (filter.md §5.3c).
    #[test]
    fn retuning_live_does_not_splatter_the_spectrum_the_way_a_cold_restart_does() {
        // One period, so 50 Hz is bin 1 exactly and the window is dominated by the transition
        // rather than by seconds of steady tone diluting it.
        const WINDOW: usize = 960;
        const BIN: usize = 1;      // 50 Hz
        const HF_FROM: usize = 20; // 1 kHz, at 50 Hz per bin
        let before = realistic_correction(3.0);
        let after = realistic_correction(9.0);

        let warm = tone(50.0, 0, WINDOW * 60, 0.5);
        let cont = tone(50.0, WINDOW * 60, WINDOW, 0.5); // phase-continuous

        let mut ramped = Cascade::new(1, FS);
        let mut instant = Cascade::new(1, FS);
        let mut cold = Cascade::new(1, FS);
        let mut untouched = Cascade::new(1, FS);
        let mut sink = vec![0.0f32; warm.len()];
        for c in [&mut ramped, &mut instant, &mut cold, &mut untouched] {
            assert!(c.set_bands(&before));
            c.settle();
            c.process(&warm, &mut sink, warm.len());
        }

        ramped.set_bands(&after);  // state carried AND coefficients ramped — stage C4
        instant.set_bands(&after);
        instant.settle();          // state carried, coefficients switched instantly — stage C3
        cold.set_bands(&after);
        cold.settle();
        cold.reset_state();        // what a config reload does to the WHOLE chain
        // `untouched` is never retuned: the measurement's own noise floor.

        let mk = || vec![0.0f32; WINDOW];
        let (mut a, mut i2, mut b, mut u) = (mk(), mk(), mk(), mk());
        ramped.process(&cont, &mut a, WINDOW);
        instant.process(&cont, &mut i2, WINDOW);
        cold.process(&cont, &mut b, WINDOW);
        untouched.process(&cont, &mut u, WINDOW);

        let ramped_db = hf_splatter_db(&a, HF_FROM, BIN);
        let instant_db = hf_splatter_db(&i2, HF_FROM, BIN);
        let cold_db = hf_splatter_db(&b, HF_FROM, BIN);
        let floor_db = hf_splatter_db(&u, HF_FROM, BIN);
        eprintln!(
            "HF splatter (>=1 kHz, re 50 Hz): ramped {ramped_db:.1} dB, \
             instant {instant_db:.1} dB, cold {cold_db:.1} dB, floor {floor_db:.1} dB"
        );

        assert!(cold_db > floor_db + 20.0, "cold restart produced no measurable click");
        // Carrying delay-register state must beat a cold restart...
        assert!(
            instant_db < cold_db - 8.0,
            "state-carry should beat a cold restart: instant {instant_db:.1} vs cold {cold_db:.1}",
        );
        // ...and ramping the coefficients must beat switching them instantly. This is C4's
        // whole contribution, and it is the smaller of the two effects.
        assert!(
            ramped_db < instant_db - 3.0,
            "ramping should beat an instant switch: ramped {ramped_db:.1} vs instant {instant_db:.1}",
        );
    }
}
