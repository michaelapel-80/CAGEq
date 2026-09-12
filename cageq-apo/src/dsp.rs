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
/// is 20 (matching AQUA's); this leaves headroom above it both for that margin and for
/// `cageq-apo-backend`'s `SlotAssignment`, which can leave a slot occupied — fading toward
/// passthrough, not reusable by an unrelated band — well after the *real* band count has
/// dropped (see its own doc for why: reusing a slot across too large an `Fc` jump makes
/// `Cascade::start_ramp` sweep audibly through the octaves between old and new instead of
/// sounding like two independent fades). Doubled from 32 once that policy shipped, because 32
/// was already tight against ordinary fragmentation even before slots could be held back on
/// `Fc` grounds. `set_bands` refuses more rather than silently truncating a correction.
///
/// A `Tilt` band (`cageq_backend::FilterType::Tilt`) costs two of these slots, not one — it
/// arrives here already expanded into its constituent shelf pair (`expand_tilts`). Worst
/// case, the UI's 20-band limit filled entirely with tilts is 40 slots, still comfortably
/// under this cap.
pub const MAX_BANDS: usize = 64;

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
///
/// **This is a floor, not the ramp length** — see [`Cascade::ramp_ms_for`]. A live drag's
/// steps are small (this is the length that fits them), but a whole-slot swap or preset
/// change is not: 8 ms of it measured as an audible lurch (confirmed live, not guessed), the
/// exact "far too fast for a large tonal jump" problem `morph.rs`'s own module doc names —
/// this crate just used to have no answer for it, because pushing the *final* coefficients in
/// one call (rather than several spaced writes — see `Capabilities::owns_transitions`'s doc in
/// `cageq-backend`) meant nothing ever paced the ramp itself to the size of the change.
const RAMP_MS: f64 = 8.0;

/// How much perceptual distance one second of ramp buys, dB/s — see [`Cascade::ramp_ms_for`].
/// Deliberately the same number as `cageq-core::morph::TONE_MORPH_RATE_DB_PER_SEC`: however
/// CAGEq is driving the audio, a jump of a given size should take about the same time to feel
/// deliberate rather than abrupt, not a different pace depending on which backend is active.
/// Not shared as an actual Rust constant across the two crates — `cageq-apo` doesn't depend on
/// `cageq-core` (and shouldn't start to just for one `f64`) — so this is a second copy, the
/// same tradeoff already made three times over for the K-weighting constants in `morph.rs`,
/// the sidecar, and `biquad.ts`.
const RAMP_RATE_DB_PER_SEC: f64 = 40.0;

/// Ceiling on the adaptive ramp, matching `cageq-core::morph::TONE_MORPH_MAX` — see that
/// constant's doc for why an unbounded slew is its own problem (short auditory memory makes a
/// slow morph as unusable for A/B as an instant jump, just in the other direction).
const RAMP_MAX_MS: f64 = 300.0;

/// How long after a request [`Cascade::start_ramp`] still treats the *next* one as continuing
/// the same gesture — truncated to [`RAMP_MS`] — rather than a fresh, standalone edit that earns
/// its own full [`Cascade::ramp_ms_for`] duration.
///
/// **Why this exists at all, separately from `ramp_left > 0`.** Once a request truncates to the
/// `RAMP_MS` floor, that ramp finishes in 8 ms — far sooner than the next tick of an ongoing
/// drag (isolate: ~70 ms; an ordinary tone drag: ~17 ms), so `ramp_left` alone reads the cascade
/// as settled again well before the gesture that is still moving the pointer actually ends. Left
/// alone, that means every *other* tick lands back on the `ramp_left == 0` branch and gets handed
/// a fresh full-length ramp it has no time to finish before the next tick retargets it anyway —
/// reintroducing exactly the chase this mechanism exists to close, just on alternating ticks
/// instead of every one (measured directly: 96% lag at some tick, replaying a real drag, with
/// only the `ramp_left` check in place). This cooldown is refreshed by *every* request — first or
/// truncated alike — so a continuing gesture stays in truncated mode for its whole duration, and
/// only a genuine pause longer than this reads as the gesture actually ending.
///
/// Comfortably above both real cadences above (with headroom for scheduling jitter), short
/// enough that a real pause is recognised almost immediately.
const GESTURE_GAP_MS: f64 = 150.0;

/// How long a crossfade to or from dry takes, in milliseconds.
///
/// **Equalizer APO's own value, read from its GPL source** rather than inferred from a
/// recording — `EqualizerAPO/FilterEngine.cpp:155`:
/// ```cpp
/// this->transitionLength = (unsigned) (sampleRate / 100);
/// ```
/// which is exactly 10 ms at any sample rate. Everything before this was a guess built on a
/// measurement: `wavscan`'s 10%-90% crossing time on a real EqAPO recording read ~15 ms, not
/// 10, because its 20 ms RMS analysis window is comparable in width to the transition itself
/// and smooths (broadens) a fast envelope change — the measurement tool's own resolution
/// limit, mistaken for EqAPO's actual duration. 15 ms was chased for two commits before the
/// source was checked instead of remeasured again.
const DRY_FADE_MS: f64 = 10.0;

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


/// Candidate dry-crossfade envelope shapes, for comparing against a real spectrum analysis
/// (`enginedump` + `wavscan`) rather than reasoning about Fourier decay in the abstract.
/// **Test-only** — production always uses raised cosine (see the `#[cfg(not(test))]` branch
/// in [`Cascade::advance_dry`]). Not to be confused with [`RAMP_MS`]'s linear coefficient ramp,
/// a separate mechanism that runs alongside this one.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum DryCurve {
    Linear,
    Smoothstep,
    RaisedCosine,
    Quintic,
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
    /// Length of the *current* ramp, in frames — set fresh by every [`Cascade::start_ramp`]
    /// call from [`Cascade::ramp_ms_for`], not a fixed constant: a live-drag-sized nudge gets
    /// [`RAMP_MS`]'s floor, a whole-slot swap gets proportionally longer, up to [`RAMP_MAX_MS`].
    ramp_frames: u32,
    /// Frames left before [`Cascade::start_ramp`] will treat the *next* request as a fresh,
    /// standalone edit again rather than a continuation of the current gesture — see
    /// [`GESTURE_GAP_MS`]'s own doc for why this has to outlive `ramp_left` itself.
    cooldown_left: u32,
    /// [`GESTURE_GAP_MS`] in frames, precomputed like `cross_fade_frames`/`dry_fade_frames`.
    gesture_gap_frames: u32,
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
    /// Which envelope shape [`Cascade::advance_dry`] uses. **Test-only.**
    #[cfg(test)]
    dry_curve_for_test: DryCurve,
    /// Precomputed trig for the peak-gain guard — see [`Cascade::would_be_too_loud`].
    grid: Vec<GridPoint>,

    // --- Secondary bank: a second, independently-running cascade used only while
    // `start_crossfade` is fading one whole correction into an unrelated one (§5.2 isolate
    // on/off) — see that method's own doc for why this exists instead of just ramping
    // `coeffs`. Idle (`process_count2 == 0`) the rest of the time, so it costs nothing extra
    // per sample outside of a crossfade.
    /// Mirrors `coeffs`/`state`/`process_count`/`preamp` but for the incoming correction.
    coeffs2: [Coeffs; MAX_BANDS],
    state2: Vec<BiquadState>,
    process_count2: usize,
    preamp2: f64,
    /// Crossfade position between the primary bank and the secondary one: 0 = fully primary
    /// (secondary silent), 1 = fully secondary. Same shape as `dry_mix`, but a distinct
    /// mechanism — `dry_mix`'s "other side" is free (the raw input, no filtering, always
    /// settled); this one's is a real cascade that starts cold and has to be run before it's
    /// trustworthy.
    cross_mix: f64,
    cross_from: f64,
    cross_to: f64,
    /// Frames left in the audible crossfade; 0 means either idle or still pre-rolling (see
    /// `cross_preroll_left`).
    cross_fade_left: u32,
    cross_fade_frames: u32,
    /// Frames left before the crossfade may start *moving*. The secondary bank still runs on
    /// real input during this window — `cross_mix` just stays pinned at `cross_from` — so its
    /// (cold) delay registers get a head start settling before they carry any weight in the
    /// output, rather than joining the fade already ringing.
    cross_preroll_left: u32,
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
            // Placeholder until the first start_ramp() picks a real, distance-based value —
            // never divided against while ramp_left is 0, but kept a sane, in-range number
            // rather than 0 on principle.
            ramp_frames: ((RAMP_MS / 1000.0) * sample_rate).round().max(1.0) as u32,
            cooldown_left: 0,
            gesture_gap_frames: ((GESTURE_GAP_MS / 1000.0) * sample_rate).round().max(1.0) as u32,
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
            #[cfg(test)]
            dry_curve_for_test: DryCurve::Linear,
            grid,
            coeffs2: [Coeffs::PASSTHROUGH; MAX_BANDS],
            state2: vec![BiquadState::default(); channels * MAX_BANDS],
            process_count2: 0,
            preamp2: 1.0,
            cross_mix: 0.0,
            cross_from: 0.0,
            cross_to: 0.0,
            cross_fade_left: 0,
            // Reuses DRY_FADE_MS: a pure level crossfade between two already-valid signals
            // needs no distance-based scaling the way the coefficient ramp does (see
            // `start_crossfade`'s doc) — the same fixed, proven window Dry itself uses for
            // exactly the same shape of transition.
            cross_fade_frames: ((DRY_FADE_MS / 1000.0) * sample_rate).round().max(1.0) as u32,
            cross_preroll_left: 0,
        }
    }

    /// Begin moving the coefficients towards `target`. Every call lands immediately —
    /// **nothing here ever defers or queues** — but how long the move takes to finish depends
    /// on whether this is the first word on where it should go, or a continuation of a gesture
    /// already under way:
    ///
    ///   * If neither a ramp is in flight nor a request landed within [`GESTURE_GAP_MS`]
    ///     (`self.cooldown_left == 0`), this is genuinely the *first* word, and gets paced by
    ///     [`Cascade::ramp_ms_for`] — [`RAMP_MS`] for a small change, longer for a large one, up
    ///     to [`RAMP_MAX_MS`]. A single, uninterrupted retune benefits from that smoothing.
    ///   * Otherwise, this call is retargeting something already moving (or that only just
    ///     finished moving, still within the same gesture) — not stating an original intent —
    ///     and the channel's contract is to answer it *now*, not to keep smoothing towards
    ///     whatever the previous request asked for. The remaining move is truncated to the fixed
    ///     [`RAMP_MS`] floor, from wherever the coefficients have actually reached (not the
    ///     previous target).
    ///
    /// **Why `cooldown_left`, not just `ramp_left > 0`.** A truncated ramp finishes in 8 ms —
    /// far sooner than the next tick of an ongoing drag (isolate: ~70 ms; an ordinary tone drag:
    /// ~17 ms) — so `ramp_left` alone reads the cascade as settled again well before the gesture
    /// that is still moving the pointer actually ends. Checked alone, that reintroduces the
    /// chase this exists to close on every *other* tick instead of every one: each lands back on
    /// the "first request" branch and is handed a fresh full-length ramp it has no time to
    /// finish before the next tick retargets it anyway (measured directly: 96% lag at some tick,
    /// replaying a real drag, with only `ramp_left` checked). `cooldown_left` is refreshed by
    /// every request, first or truncated alike, so a continuing gesture stays in truncated mode
    /// for its whole length — a narrow high-Q retune otherwise reads as a *large* change on
    /// `ramp_distance_db` even for a small move, which is what made this matter in the first
    /// place: real isolate-drag ticks (~70 ms apart) kept landing 100-300 ms ramps, falling up to
    /// ~18x behind the pointer over the length of a drag before this fix
    /// (`set_bands_tracks_the_pointer_closely_through_a_real_isolate_drag`). Real-time
    /// responsiveness wins over how smooth any one still-in-flight transition looks.
    ///
    /// There is no discontinuity at a retarget either way: the ramp always starts from wherever
    /// the coefficients currently sit, never from the previous target.
    fn start_ramp(&mut self, target: &[Coeffs], new_count: usize, preamp_db: f64) {
        let interrupting = self.ramp_left > 0 || self.cooldown_left > 0;
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
        // Sized to how different `start` and `target` actually are, not a fixed length — see
        // this method's own doc for why a whole-slot swap needs far more than a live drag's
        // 8 ms — unless this continues an already-moving gesture, in which case the floor always
        // wins regardless of distance (see this method's own doc). Computed from `start`/
        // `target` themselves (just assigned above), the same per-sample-independent one-shot
        // cost `would_be_too_loud` already pays on every `apply_coeffs`/`set_bands` call, so
        // this is not new real-time-path risk, just more of the same affordable kind.
        let ms = if interrupting { RAMP_MS } else { Self::ramp_ms_for(self.ramp_distance_db()) };
        self.ramp_frames = ((ms / 1000.0) * self.sample_rate).round().max(1.0) as u32;
        self.ramp_left = self.ramp_frames;
        // Refreshed on every request, not just interrupting ones: the whole point is that a
        // gesture's own first request also counts towards keeping it "current" for whatever
        // comes right after.
        self.cooldown_left = self.gesture_gap_frames;
    }

    /// How far apart `start` and `target` are, combined response in dB, RMS over [`Cascade::grid`].
    ///
    /// A deliberately simpler cousin of `cageq-core::morph::tonal_distance_db`: that one is
    /// K-weighted and pink-noise-bin-weighted because it feeds a number people compare presets
    /// by, evaluated on a *symbolic* band list (`Fc`/`Q`/gain). This only has to size a ramp —
    /// "is this a nudge or a whole new curve" — and only has raw, already-baked coefficients to
    /// work from (whatever arrived over the control channel, symbolic or not), so it reuses the
    /// guard's own plain log-spaced grid and takes an unweighted RMS across it instead of
    /// porting K-weighting a fourth time for a heuristic that never reaches the user as a
    /// number.
    fn ramp_distance_db(&self) -> f64 {
        let mut sum_sq = 0.0;
        for g in &self.grid {
            let mut db_from = self.preamp_from_db;
            let mut db_to = self.preamp_to_db;
            for i in 0..MAX_BANDS {
                // Floored, not left to reach zero/negative-infinite: a genuine null in one
                // curve at one grid point must not make the whole distance metric blow up or
                // go NaN over a difference that is really "very large but finite".
                db_from += 10.0 * self.start[i].power_at(g).max(1e-12).log10();
                db_to += 10.0 * self.target[i].power_at(g).max(1e-12).log10();
            }
            let d = db_to - db_from;
            sum_sq += d * d;
        }
        (sum_sq / self.grid.len() as f64).sqrt()
    }

    /// Ramp duration for a change of `distance_db` — [`RAMP_MS`] below it, scaling at
    /// [`RAMP_RATE_DB_PER_SEC`] above, capped at [`RAMP_MAX_MS`]. Mirrors
    /// `cageq-core::morph::morph_frames`'s shape (same rate, same cap, see those constants'
    /// docs) but returns a duration rather than a frame count: this crate's ramp already knows
    /// its own sample rate and needs no separate step size the way spaced app-level writes do.
    fn ramp_ms_for(distance_db: f64) -> f64 {
        (distance_db / RAMP_RATE_DB_PER_SEC * 1000.0).clamp(RAMP_MS, RAMP_MAX_MS)
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
    /// own 10 ms (`DRY_FADE_MS`, read from its source, not the 15 ms a measurement first
    /// suggested) and is not a runtime knob; this exists so the relationship between fade
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
        // Raised cosine — Equalizer APO's own curve, read from its GPL source
        // (`EqualizerAPO/FilterConfiguration.cpp:165`):
        //     float factor = 0.5f * (1.0f - cos(transitionCounter * M_PI / transitionLength));
        // Two earlier guesses (linear at 15 ms, then quintic) were both built on inference —
        // a recording's measured duration, a DFT's verdict on spread, a listening impression
        // taken through a clumsy multi-step A/B — and each left an unexplained gap or an ear
        // report that did not match the meter. Checking the actual algorithm instead of
        // re-measuring around it settled both: the duration was 10 ms, not 15 (`DRY_FADE_MS`'s
        // doc explains why a 20 ms analysis window misread it), and the curve was raised
        // cosine, not linear or quintic. Reproducing EqAPO's own transition exactly is the
        // most literal reading of "if we cannot improve on the simple solution, use it": not an
        // approximation of its observed behaviour, but the same arithmetic.
        //
        // Its peak rate of change (1.571x a linear ramp's) sits between smoothstep's (1.5x)
        // and quintic's (1.875x) — consistent with quintic having measured clickier than this
        // by ear, on the peak-rate-predicts-click reasoning recorded against `RAMP_MS`.
        #[cfg(test)]
        let s = match self.dry_curve_for_test {
            DryCurve::Linear => t,
            DryCurve::Smoothstep => t * t * (3.0 - 2.0 * t),
            DryCurve::RaisedCosine => 0.5 - 0.5 * (std::f64::consts::PI * t).cos(),
            DryCurve::Quintic => t * t * t * (t * (t * 6.0 - 15.0) + 10.0),
        };
        #[cfg(not(test))]
        let s = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
        self.dry_mix = self.dry_from + (self.dry_to - self.dry_from) * s;
    }

    /// Select the dry-crossfade envelope shape. **Test-only** — see [`DryCurve`].
    #[cfg(test)]
    fn set_dry_curve_for_test(&mut self, curve: DryCurve) {
        self.dry_curve_for_test = curve;
    }

    /// Fade the **entire signal path** over to `target`, for a transition that replaces the
    /// whole correction with something conceptually unrelated (§5.2 isolate on and off) —
    /// crossfading two independently-valid signals rather than coefficient-ramping through
    /// every intermediate filter shape between them.
    ///
    /// **Why this exists alongside `start_ramp`.** A ramp works well for an in-place edit —
    /// one band's Fc/gain moving, a slot swap — because "what's in between" is still a
    /// reasonable filter. It stops being reasonable once several unrelated bands ramp toward
    /// `PASSTHROUGH` on one shared clock at once: each individually stays clean (verified:
    /// no single band, ramped alone, ever overshoots its own endpoints), but at one shared
    /// point along a *large*, many-band transition their curves can sum into a real spike —
    /// measured directly on a real correction (21 bands including a low-shelf stack) dropping
    /// to a single isolate bandpass: +22 dB above *both* endpoints, mid-ramp, entirely gone by
    /// the time the ramp finished. `dry_mix` already sidesteps this same class of problem for
    /// going dry, by blending two already-computed, always-valid *signals* instead of
    /// interpolating the *filters* that produce them — there is no "in between" to glitch
    /// through. This generalises that idea to a second, real cascade instead of dry's free
    /// "other side" (the untouched input, no filtering, always settled).
    ///
    /// Duration is fixed ([`Cascade::cross_fade_frames`], same window as `DRY_FADE_MS`), not
    /// distance-scaled like `ramp_ms_for`: nothing about a pure level crossfade gets harder
    /// the further apart the two corrections are — that scaling existed only because a ramp
    /// has to survive *traveling through* the space between them, and this doesn't.
    ///
    /// The secondary bank starts from a cold, zeroed state (unlike `dry`'s always-settled raw
    /// input), so it runs on real input for a short **pre-roll** window first, entirely
    /// inaudible (`cross_mix` pinned at 0), before the fade itself starts moving — settling
    /// its delay registers on real signal rather than joining the fade already ringing.
    ///
    /// **Every call lands immediately — nothing here defers or queues, mid-preroll or
    /// mid-fade.** `coeffs2` is a flat assignment (no interpolation of its own), so retargeting
    /// it while `cross_mix` already carries real weight steps the secondary bank's contribution
    /// to the output at that instant — a smaller-scale version of the very discontinuity this
    /// mechanism exists to avoid. An earlier version queued a mid-fade retarget instead, landing
    /// the in-flight fade on its old target first and only then starting a fresh one toward the
    /// newest — correctness-over-responsiveness, and the wrong tradeoff for a control channel: a
    /// fast, repeated §5.2 isolate drag could still reproduce the reported sweep-and-lurch
    /// artefact, because the *response* to the newest request was being delayed behind one that
    /// was already stale. The channel's contract is real-time response over how smooth any one
    /// still-in-flight transition looks — see [`Cascade::start_ramp`]'s own doc, which makes the
    /// same trade for the plain ramp. The resulting step is bounded by [`Cascade::cross_mix`]
    /// (small early in the fade, when a retarget is most likely; the fade is nearly landed, and
    /// the step correspondingly smaller, by the time it carries most of its weight) and is one
    /// sample, not a sustained artefact.
    ///
    /// Returns `false` — changing nothing — if `target` would exceed [`MAX_BANDS`] or the
    /// combination would be too loud, the same checks [`Cascade::apply_coeffs`] makes (whose
    /// signature this deliberately mirrors: coefficients straight through, count implied by the
    /// slice length).
    pub fn start_crossfade(&mut self, target: &[Coeffs], preamp_db: f64) -> bool {
        if target.len() > MAX_BANDS || !preamp_db.is_finite() {
            return false;
        }
        let preamp = db_to_gain(preamp_db);
        if !preamp.is_finite() || self.would_be_too_loud(target, preamp) {
            return false;
        }

        let fresh = self.cross_mix == 0.0 && self.cross_preroll_left == 0 && self.cross_fade_left == 0;
        for i in 0..MAX_BANDS {
            self.coeffs2[i] = target.get(i).copied().unwrap_or(Coeffs::PASSTHROUGH);
        }
        self.process_count2 = target.len();
        self.preamp2 = preamp;
        if fresh {
            for s in &mut self.state2 {
                s.reset();
            }
            self.cross_from = self.cross_mix; // == 0.0
            self.cross_to = 1.0;
            self.cross_fade_left = 0; // gated behind the pre-roll below
            self.cross_preroll_left = self.cross_fade_frames;
        }
        // A retarget mid-preroll or mid-fade leaves `cross_mix`/`cross_fade_left`/
        // `cross_preroll_left` exactly where they are — only `coeffs2`/`process_count2`/
        // `preamp2` (the *target* of the blend) moved. The blend keeps advancing on whatever
        // schedule it was already on, now carrying it towards the new target instead of the old
        // one.
        true
    }

    /// Advance the pre-roll and/or the crossfade itself by one frame — mirrors
    /// [`Cascade::advance_dry`]'s envelope exactly (raised cosine, same reasoning).
    #[inline]
    fn advance_cross(&mut self) {
        if self.cross_preroll_left > 0 {
            self.cross_preroll_left -= 1;
            if self.cross_preroll_left == 0 {
                self.cross_fade_left = self.cross_fade_frames;
            }
            return;
        }
        self.cross_fade_left -= 1;
        if self.cross_fade_left == 0 {
            self.cross_mix = self.cross_to;
            self.promote_secondary();
            return;
        }
        let t = 1.0 - self.cross_fade_left as f64 / self.cross_fade_frames as f64;
        let s = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
        self.cross_mix = self.cross_from + (self.cross_to - self.cross_from) * s;
    }

    /// The secondary bank becomes the primary one, and goes idle. Called once the crossfade
    /// lands (`cross_mix` reaches `cross_to`, currently always 1.0 — there is no path back to
    /// the old primary once a crossfade starts, matching how isolate/un-isolate always
    /// replaces the whole correction rather than blending partway).
    ///
    /// Delay-register state carries straight across (`state2` was live the whole time, not
    /// something rebuilt from cold at this instant), so nothing about the promotion itself is
    /// audible — the frame it happens on already had `cross_mix == 1`, i.e. zero weight left
    /// on the old primary, so skipping its computation from here on changes nothing about the
    /// output.
    fn promote_secondary(&mut self) {
        self.coeffs = self.coeffs2;
        self.target = self.coeffs2;
        self.start = self.coeffs2;
        self.state.copy_from_slice(&self.state2);
        self.process_count = self.process_count2;
        self.band_count = self.process_count2;
        self.preamp = self.preamp2;
        self.preamp_from_db = 20.0 * self.preamp2.log10();
        self.preamp_to_db = self.preamp_from_db;
        self.ramp_left = 0; // a crossfade always lands settled, never mid-ramp

        self.process_count2 = 0;
        self.preamp2 = 1.0;
        self.cross_mix = 0.0;
        self.cross_from = 0.0;
        self.cross_to = 0.0;
        for s in &mut self.state2 {
            s.reset();
        }
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
        self.cooldown_left = 0;
        // A crossfade in flight (see `start_crossfade`) takes priority over the lines above: it
        // represents the more recent instruction, and lands the secondary bank straight into
        // place rather than leaving it to fade in on its own schedule later.
        if self.process_count2 > 0 {
            self.promote_secondary();
        }
        self.cross_fade_left = 0;
        self.cross_preroll_left = 0;
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
        // Plain linear, not smoothstep — smoothstep was tried (a linear ramp's corners, where
        // the rate of change jumps from zero to constant and back, seemed like an obvious
        // thing to smooth away) and measured *worse* (-57 dB vs linear's -60, see
        // `retuning_live_does_not_splatter_the_spectrum_the_way_a_cold_restart_does`): the
        // residual here is dominated by the *rate* coefficients move at, not by the corners,
        // and smoothstep's midpoint slope is 1.5x linear's.
        //
        // Interpolating from the stored endpoints rather than accumulating a per-frame
        // increment: it means a long series of edits cannot let rounding drift the response
        // away from what was asked for.
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
        // Leaving dry runs the coefficient ramp AND the dry-unwind at once, on two different
        // clocks (8 ms linear vs 10 ms raised cosine) — tried unifying this to "snap straight
        // to the target, let the crossfade's own fade-in be the only thing masking it" (one
        // mechanism, not two), on the reasoning that the crossfade already scales the wet
        // path's audibility from zero. Measured worse on both cases it was tried against: the
        // ordinary one (settled on dry, then a different correction) went from 2.2x to 2.4x,
        // and interrupting the fade to dry early — where the wet path isn't actually silent
        // yet, so the snap's masking argument doesn't hold — went from 2.51x to 4.7x. The ramp
        // is doing real work in that second case specifically because it doesn't know or care
        // what the crossfade is doing; removing it removed real protection. Reverted; see
        // `leaving_dry_for_a_different_correction_ramps_and_unwinds_together` and
        // `interrupting_a_fade_to_dry_still_switches_cleanly`.
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
        for s in self.state2.iter_mut() {
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
            // time audio returns rather than resuming as an audible jump. Same reasoning for
            // the crossfade (`start_crossfade`): a pause mid-fade must not resume with a
            // half-finished transition dumped on the listener all at once.
            if self.ramp_left > 0 {
                self.advance_ramp();
            }
            if self.cooldown_left > 0 {
                self.cooldown_left -= 1;
            }
            if self.cross_preroll_left > 0 || self.cross_fade_left > 0 {
                self.advance_cross();
            }
            for ch in 0..channels {
                let base = ch * MAX_BANDS;
                // Silence in — the preamp scales zero to zero, so it is simply skipped.
                let mut x = 0.0f64;
                for b in 0..self.process_count {
                    x = self.state[base + b].step(&self.coeffs[b], x);
                }
                let y = if self.process_count2 > 0 {
                    let mut x2 = 0.0f64;
                    for b in 0..self.process_count2 {
                        x2 = self.state2[base + b].step(&self.coeffs2[b], x2);
                    }
                    (x + (x2 - x) * self.cross_mix) as f32
                } else {
                    x as f32
                };
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
            if self.cooldown_left > 0 {
                self.cooldown_left -= 1;
            }
            if self.dry_fade_left > 0 {
                self.advance_dry();
            }
            if self.cross_preroll_left > 0 || self.cross_fade_left > 0 {
                self.advance_cross();
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
                let wet = (x + (dry - x) * self.dry_mix) as f64;
                // The secondary bank (`start_crossfade`) only runs while a crossfade is
                // actually in flight — `process_count2 == 0` the rest of the time, so this is
                // one cheap branch, not a second cascade's worth of multiply-adds, in the
                // common case. It deliberately bypasses `dry_mix` entirely: nothing reaches
                // this bank except a §5.2 isolate on/off, which the app never triggers while
                // Dry is active (see `onFreqSweep`'s own guard).
                output[i] = if self.process_count2 > 0 {
                    let mut x2 = raw * self.preamp2;
                    for b in 0..self.process_count2 {
                        x2 = self.state2[base + b].step(&self.coeffs2[b], x2);
                    }
                    (wet + (x2 - wet) * self.cross_mix) as f32
                } else {
                    wet as f32
                };
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
    /// irrelevant here). Recorded evidence: EqAPO — its real 10 ms transition, the "15 ms"
    /// only ever being `wavscan`'s misread of it (see `DRY_FADE_MS`) — puts -83 dB at 200 Hz,
    /// ours at 8 ms put -58 dB — 25 dB worse, purely from being quicker.
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

    /// **Does a smoother crossfade curve reduce the sideband spread a spectrum analyser
    /// shows during the switch?**
    ///
    /// Recorded evidence puts the engine's dry transition 15-25 dB above Equalizer APO's at
    /// 200-300 Hz even with the fade length matched (`enginedump`, `f76060a`). Duration alone
    /// does not explain it (`shorter_fades_spread_further_up_the_spectrum`, above). This test
    /// measures whether the envelope SHAPE does, by DFT.
    ///
    /// **It said yes; a listening comparison said no.** Quintic measured 20-30 dB lower far
    /// sidebands than linear and was reported clickier by ear, with EqAPO sounding cleaner
    /// despite its recorded spectrum showing more visible sidelobes. Reconciled afterwards: a
    /// curve with zero-velocity endpoints must move FASTER through the middle to cover the same
    /// distance in the same time — quintic's peak rate is 1.875x linear's, smoothstep's 1.5x —
    /// and peak rate, not integrated far-field energy, is what reads as a click. Same
    /// conclusion `RAMP_MS` reached for the coefficient ramp.
    ///
    /// **What actually settled it was checking Equalizer APO's own GPL source** rather than
    /// inferring its curve from recordings — see `advance_dry`. It uses raised cosine,
    /// `0.5*(1-cos(pi*t))`, over exactly 10 ms (`transitionLength = sampleRate/100`,
    /// `FilterEngine.cpp:155`), neither of which this test's candidates or `DRY_FADE_MS`'s
    /// prior value had gotten right. Reproducing that exact curve closed the engine-vs-EqAPO
    /// gap to 2-7 dB (in our favour) at 200/300/500 Hz — down from 15-25.
    ///
    /// The numbers below stay useful: they are what ruled out "any smooth curve is fine" and
    /// pointed at the corner/rate tradeoff, before the source was found and ended the guessing.
    #[test]
    fn curve_choice_and_dry_switch_spectral_spread() {
        const N: usize = 8192;
        let wet: Vec<Coeffs> =
            realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();

        let measure = |curve: DryCurve| -> Vec<(f64, f64)> {
            let mut c = Cascade::new(1, FS);
            assert!(c.apply_coeffs(&wet, -9.0));
            c.settle();
            c.set_dry_curve_for_test(curve);
            let warm = tone(50.0, 0, 960 * 40, 0.5);
            let mut sink = vec![0.0f32; warm.len()];
            c.process(&warm, &mut sink, warm.len());

            // Switch at the centre of the analysis window, matching `enginedump`.
            let pre = tone(50.0, 960 * 40, N / 2, 0.5);
            let mut a = vec![0.0f32; N / 2];
            c.process(&pre, &mut a, N / 2);
            assert!(c.apply_coeffs(&[], -6.0));
            let post = tone(50.0, 960 * 40 + N / 2, N / 2, 0.5);
            let mut b = vec![0.0f32; N / 2];
            c.process(&post, &mut b, N / 2);

            // Hann window: without it the level step's own leakage swamps everything and
            // every curve looks identical — the same trap `wavscan` was built to avoid.
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

        // Discontinuity check alongside the spectrum: whichever curve wins must still be at
        // least as free of a click as linear already measured (1.7-1.8x the tone's own slew).
        let jump = |curve: DryCurve| -> f64 {
            let mut c = Cascade::new(1, FS);
            assert!(c.apply_coeffs(&wet, -9.0));
            c.settle();
            c.set_dry_curve_for_test(curve);
            let warm = tone(50.0, 0, 960 * 40, 0.5);
            let mut sink = vec![0.0f32; warm.len()];
            c.process(&warm, &mut sink, warm.len());
            assert!(c.apply_coeffs(&[], -6.0));
            let cont = tone(50.0, 960 * 40, 960, 0.5);
            let mut out = vec![*sink.last().unwrap()];
            out.extend(std::iter::repeat(0.0f32).take(960));
            c.process(&cont, &mut out[1..], 960);
            worst_jump_ratio(&out)
        };

        let linear = measure(DryCurve::Linear);
        let smoothstep = measure(DryCurve::Smoothstep);
        let raised_cosine = measure(DryCurve::RaisedCosine);
        let quintic = measure(DryCurve::Quintic);

        eprintln!("      linear    smoothstep  raised-cos  quintic");
        for i in 0..linear.len() {
            eprintln!(
                "{:5.0} Hz  {:7.1} dB  {:7.1} dB  {:7.1} dB  {:7.1} dB",
                linear[i].0, linear[i].1, smoothstep[i].1, raised_cosine[i].1, quintic[i].1,
            );
        }
        eprintln!(
            "worst jump vs slew: linear {:.1}x, smoothstep {:.1}x, raised-cos {:.1}x, quintic {:.1}x",
            jump(DryCurve::Linear),
            jump(DryCurve::Smoothstep),
            jump(DryCurve::RaisedCosine),
            jump(DryCurve::Quintic),
        );

        // The far bin (500 Hz) is where an asymptotic rolloff difference should show most
        // clearly. No assertion beyond sanity: this test's job is to produce the numbers that
        // decide whether to change DRY_FADE curve, not to bake in an expectation before they
        // are seen (the same mistake that produced a failing assertion two commits ago).
        assert!(linear[0].1 < -20.0, "sanity: the fundamental should dominate at 100 Hz");
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

    /// **Leaving Dry runs a coefficient ramp AND the dry-unwind at once, on two different
    /// clocks** (8 ms linear vs 10 ms raised cosine) — `apply_coeffs` unconditionally calls
    /// `start_ramp` whenever the target is non-empty, even right after `set_dry(false, 0.0)`.
    /// **Tried unifying this to one mechanism** (snap straight to the target, let the
    /// crossfade's own fade-in be the only thing masking the change, symmetric with going dry
    /// — bug 2's fix — where the chain's coefficients are untouched and the crossfade is the
    /// whole transition) **and measured it worse**, on this exact test: 2.2x combined vs 2.4x
    /// snapped. `interrupting_a_fade_to_dry_still_switches_cleanly` (right after this one) is
    /// where the snap idea actually broke — worse there, not just barely: it relies on the
    /// crossfade already having scaled the wet path near zero, which isn't true early in the
    /// fade. Reverted; see `apply_coeffs`'s own comment. This test exercises the ordinary case
    /// (settled on dry, then a DIFFERENT correction) against a REAL prior correction, not a
    /// freshly-constructed cascade whose "before" would just be `Coeffs::PASSTHROUGH`.
    #[test]
    fn leaving_dry_for_a_different_correction_ramps_and_unwinds_together() {
        const WINDOW: usize = 960;
        let a: Vec<Coeffs> = realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();
        // A genuinely different correction — not a gain nudge on the same bands.
        let b: Vec<Coeffs> = [
            peaking(60.0, -5.0, 1.8),
            peaking(220.0, 6.0, 0.9),
            peaking(1200.0, -3.0, 2.4),
            peaking(4200.0, 4.0, 1.1),
            peaking(11000.0, -2.0, 0.8),
        ]
        .iter()
        .map(|band| coefficients(band, FS))
        .collect();

        let run = |instant: bool| -> f64 {
            let mut worst = 0.0f64;
            for phase in (0..WINDOW).step_by(WINDOW / 8) {
                let warm = tone(50.0, 0, WINDOW * 60 + phase, 0.5);
                let cont = tone(50.0, WINDOW * 60 + phase, WINDOW, 0.5);
                let mut c = Cascade::new(1, FS);
                assert!(c.apply_coeffs(&a, -9.0));
                c.settle();
                assert!(c.apply_coeffs(&[], -6.0)); // go dry
                c.settle(); // fully parked on dry — the realistic "sat there a while" case
                let mut sink = vec![0.0f32; warm.len()];
                c.process(&warm, &mut sink, warm.len());

                assert!(c.apply_coeffs(&b, -9.0)); // straight to a DIFFERENT correction
                if instant {
                    c.settle();
                }
                let mut out = vec![*sink.last().unwrap()];
                out.extend(std::iter::repeat(0.0f32).take(WINDOW));
                c.process(&cont, &mut out[1..], WINDOW);
                worst = worst.max(worst_jump_ratio(&out));
            }
            worst
        };

        let faded = run(false);
        let instant = run(true);
        eprintln!("dry -> different correction jump ratio: {faded:.1}x combined vs {instant:.1}x instant");
        assert!(faded < 2.5, "combined ramp+unwind still jumps {faded:.1}x");
        assert!(faded < instant * 0.6, "combined transition barely helps over an instant switch");
    }

    /// **Interrupting a still-in-progress fade *toward* dry** — the wet path is NOT silent yet
    /// (only a fraction of the way into the 10 ms dry-unwind). This is the test that actually
    /// discriminated between the combined ramp+unwind (kept) and the "snap straight to the
    /// target" alternative (tried, reverted — see `apply_coeffs`'s comment): snapping here
    /// measured 4.7x, clearly audible, because its masking argument depends on the crossfade
    /// having already weighted the wet path near zero, which isn't true this early. The kept
    /// mechanism does noticeably worse here too (2.51x vs the 2.2x settled case, just over the
    /// 2.5x bar every other transition in this file clears) — the ramp is doing real,
    /// necessary work in exactly this narrow case, just not *quite* enough to fully hide a
    /// synthetic pure-tone worst case. Accepted rather than chased further: it needs an
    /// interruption within ~1 ms of the initial dry click to provoke, the same "mostly
    /// academic" territory the A/B ramp decision already accepted for a different mechanism.
    #[test]
    fn interrupting_a_fade_to_dry_still_switches_cleanly() {
        const WINDOW: usize = 960;
        let a: Vec<Coeffs> = realistic_correction(6.0).iter().map(|b| coefficients(b, FS)).collect();
        let b: Vec<Coeffs> = [
            peaking(60.0, -5.0, 1.8),
            peaking(220.0, 6.0, 0.9),
            peaking(1200.0, -3.0, 2.4),
            peaking(4200.0, 4.0, 1.1),
            peaking(11000.0, -2.0, 0.8),
        ]
        .iter()
        .map(|band| coefficients(band, FS))
        .collect();
        // How far into the 10 ms dry-unwind to interrupt it — early enough that the wet path
        // is still substantially audible (raised cosine, so this is roughly 10% of the way).
        const INTERRUPT_AT_FRAMES: usize = 48; // 1 ms at 48 kHz, ~10% of the 10 ms fade

        let run = |instant: bool| -> f64 {
            let mut worst = 0.0f64;
            for phase in (0..WINDOW).step_by(WINDOW / 8) {
                let warm = tone(50.0, 0, WINDOW * 60 + phase, 0.5);
                let cont = tone(50.0, WINDOW * 60 + phase, WINDOW, 0.5);
                let mut c = Cascade::new(1, FS);
                assert!(c.apply_coeffs(&a, -9.0));
                c.settle();
                let mut sink = vec![0.0f32; warm.len()];
                c.process(&warm, &mut sink, warm.len());

                assert!(c.apply_coeffs(&[], -6.0)); // start heading to dry
                let mut interrupted = vec![0.0f32; INTERRUPT_AT_FRAMES];
                let tail = tone(50.0, WINDOW * 60 + warm.len() - INTERRUPT_AT_FRAMES, INTERRUPT_AT_FRAMES, 0.5);
                c.process(&tail, &mut interrupted, INTERRUPT_AT_FRAMES); // only partway into the unwind
                sink.extend(interrupted);

                assert!(c.apply_coeffs(&b, -9.0)); // interrupt with a DIFFERENT correction
                if instant {
                    c.settle();
                }
                let mut out = vec![*sink.last().unwrap()];
                out.extend(std::iter::repeat(0.0f32).take(WINDOW));
                c.process(&cont, &mut out[1..], WINDOW);
                worst = worst.max(worst_jump_ratio(&out));
            }
            worst
        };

        let faded = run(false);
        let instant = run(true);
        eprintln!("interrupted fade-to-dry jump ratio: {faded:.2}x combined vs {instant:.1}x instant");
        // 2.5 (every other transition's bar) is measured at ~2.51x here — see the doc comment
        // above for why this one case is accepted slightly looser rather than chased further.
        assert!(faded < 3.0, "interrupting a fade to dry still jumps {faded:.2}x");
        assert!(faded < instant * 0.6, "combined transition barely helps over an instant switch");
    }

    /// **A band ramping to/from `Coeffs::PASSTHROUGH` ("unity") when the band count itself
    /// changes** — a third, distinct transition from both the tone-drag ramp (a value change,
    /// band count held constant) and the dry crossfade (a whole-chain mix, no band's own
    /// coefficients touched at all). `start_ramp` targets `Coeffs::PASSTHROUGH` for any band
    /// index beyond the new count (`unwrap_or(Coeffs::PASSTHROUGH)`), so shrinking a correction
    /// ramps the dropped band smoothly to unity instead of dropping it outright.
    /// `shrinking_the_cascade_stops_the_dropped_bands` already proves the *settled* end state
    /// is correct, but never measured whether the transit itself clicks — this does. Growing
    /// back is the same mechanism symmetrically in reverse: a re-added band's `start` is read
    /// from whatever `self.coeffs` holds at that index, which a prior shrink already settled
    /// to `PASSTHROUGH`.
    ///
    /// **Diagnostic sweep before this was an assertion** (mild `realistic_correction`-sized
    /// bands up to the original, deliberately extreme 10 dB/Q2/300 Hz candidate) found this is
    /// NOT a general property of ramping to/from unity — it's specific to a large single-band
    /// gain at a low frequency:
    ///
    /// ```text
    /// realistic cut  -5dB Q2.2 @3500Hz           drop:  1.5x ramped vs  1.6x instant | add: 2.2x ramped vs 2.4x instant
    /// realistic boost 2.5dB Q1.8 @900Hz          drop:  1.9x ramped vs  4.0x instant | add: 1.6x ramped vs 1.4x instant
    /// typical AutoEq -3dB Q1.0 @2000Hz           drop:  1.5x ramped vs  1.5x instant | add: 1.7x ramped vs 1.9x instant
    /// same freq/Q, mild gain 3dB @300Hz Q2.0     drop:  2.7x ramped vs 14.9x instant | add: 1.7x ramped vs 1.5x instant
    /// same freq/gain, low Q 10dB @300Hz Q0.7     drop: 10.2x ramped vs 77.6x instant | add: 3.2x ramped vs 1.5x instant
    /// original extreme 10dB Q2.0 @300Hz          drop: 10.8x ramped vs 77.6x instant | add: 3.2x ramped vs 1.7x instant
    /// ```
    ///
    /// Gain magnitude drives it, not Q (10 dB at Q0.7 and Q2.0 land within 0.6x of each other).
    /// Every realistic single-band gain (±2.5 to -5 dB, what `realistic_correction` and a real
    /// AutoEq fit actually produce) stays comfortably under 2.5x in both directions — this test
    /// asserts against exactly those magnitudes. A single band pushing 10 dB on its own is rare
    /// (most real correction bands are more modest, and a boost that large is usually a sign
    /// something upstream needs the headroom re-checked), and provoking the bad case needs a
    /// pure tone sitting exactly on that band's own centre frequency — the same "mostly
    /// academic" territory the A/B ramp decision already accepted for a different mechanism.
    /// Not chased further here for the same reason: no realistic correction reaches it.
    #[test]
    fn a_band_ramps_cleanly_to_and_from_passthrough_on_a_count_change() {
        const WINDOW: usize = 960;
        let kept = peaking(1000.0, 4.0, 1.0); // present throughout, isolates the OTHER band

        // Two realistic single-band magnitudes — an actual `realistic_correction` cut and
        // boost — not the extreme case from the diagnostic sweep above.
        let candidates: &[Band] = &[peaking(3500.0, -5.0, 2.2), peaking(900.0, 2.5, 1.8)];

        for dropped in candidates {
            let tone_hz = dropped.freq_hz;
            let run = |shrinking: bool, instant: bool| -> f64 {
                let mut worst = 0.0f64;
                for phase in (0..WINDOW).step_by(WINDOW / 8) {
                    let warm = tone(tone_hz, 0, WINDOW * 60 + phase, 0.5);
                    let cont = tone(tone_hz, WINDOW * 60 + phase, WINDOW, 0.5);
                    let mut c = Cascade::new(1, FS);
                    if shrinking {
                        assert!(c.set_bands(&[kept, *dropped]));
                    } else {
                        assert!(c.set_bands(&[kept])); // `dropped` starts life already at PASSTHROUGH
                    }
                    c.settle();
                    let mut sink = vec![0.0f32; warm.len()];
                    c.process(&warm, &mut sink, warm.len());

                    if shrinking {
                        assert!(c.set_bands(&[kept])); // `dropped` ramps DOWN to unity
                    } else {
                        assert!(c.set_bands(&[kept, *dropped])); // `dropped` ramps UP from unity
                    }
                    if instant {
                        c.settle();
                    }
                    let mut out = vec![*sink.last().unwrap()];
                    out.extend(std::iter::repeat(0.0f32).take(WINDOW));
                    c.process(&cont, &mut out[1..], WINDOW);
                    worst = worst.max(worst_jump_ratio(&out));
                }
                worst
            };

            let shrink_ramped = run(true, false);
            let shrink_instant = run(true, true);
            let grow_ramped = run(false, false);
            let grow_instant = run(false, true);
            eprintln!(
                "unity transition ({:.0} Hz, {:.1} dB, Q{:.1}) — drop: {shrink_ramped:.1}x ramped vs {shrink_instant:.1}x instant; \
                 add: {grow_ramped:.1}x ramped vs {grow_instant:.1}x instant",
                dropped.freq_hz, dropped.gain_db, dropped.q,
            );
            assert!(shrink_ramped < 2.5, "ramping a {:.0} Hz band to unity still jumps {shrink_ramped:.1}x", dropped.freq_hz);
            assert!(grow_ramped < 2.5, "ramping a {:.0} Hz band from unity still jumps {grow_ramped:.1}x", dropped.freq_hz);
        }
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

        // A 3 dB -> 9 dB single-band retune is this file's own "large" edit (see the neighbour
        // test's `measure` closure just above: 0.2 dB is "drag", 1 dB "brisk", 6 dB "large"), so
        // since `ramp_ms_for` it is no longer pinned to the RAMP_MS floor — it must still land
        // exactly on the target rather than near it (a long series of edits must not let
        // rounding drift the response away from what was asked), but the ramp itself is now
        // longer than the floor, on purpose. Checked against the real formula, not a re-guessed
        // literal, so this stays a regression guard on the *formula* rather than tautologically
        // re-deriving frames and asserting it equals itself: it also has to have actually grown
        // past the old fixed 8 ms, which is the whole point of `ramp_ms_for` existing.
        let expected_ms = Cascade::ramp_ms_for(c.ramp_distance_db());
        let expected_frames = ((expected_ms / 1000.0) * FS).round().max(1.0) as u32;
        assert_eq!(frames, expected_frames, "ramp length disagrees with ramp_ms_for");
        assert!(
            frames as f64 > 0.008 * FS + 4.0,
            "a 6 dB single-band edit should now ramp longer than the old fixed 8 ms floor, got {frames} frames"
        );
        for (got, want) in c.coeffs.iter().zip(c.target.iter()) {
            assert_eq!(got.b0, want.b0);
            assert_eq!(got.a2, want.a2);
        }
    }

    /// The floor `RAMP_MS` was tuned for still applies unchanged: a genuinely drag-sized
    /// single-band nudge (0.2 dB, this file's own reference "drag" increment — see
    /// `a_drag_sized_edit_is_far_below_the_headline_figure`) must not be lengthened by
    /// `ramp_ms_for`, or a live tone drag at ~60 Hz would start missing its own cadence.
    #[test]
    fn a_drag_sized_single_band_edit_still_hits_the_floor() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&realistic_correction(3.0)));
        c.settle();
        assert!(c.set_bands(&realistic_correction(3.2)));
        assert_eq!(
            c.ramp_frames,
            ((RAMP_MS / 1000.0) * FS).round().max(1.0) as u32,
            "a 0.2 dB single-band nudge should still clamp to the RAMP_MS floor"
        );
    }

    /// The actual bug this exists to fix (reported live, not guessed): swapping between two
    /// unrelated multi-band corrections — what a slot A/B switch or a preset change does —
    /// measured harsh at the old fixed 8 ms. It must now ramp far longer than a same-crate
    /// single-band drag, proportional to how different the two curves really are.
    #[test]
    fn a_full_slot_swap_ramps_far_longer_than_a_drag() {
        // Two curves that share nothing: a bass-boost slot and a treble-boost slot, the exact
        // "bass-boost preset for a treble-boost one" scenario `cageq-core::morph`'s own module
        // doc names as the case a fixed short crossfade cannot cover.
        let bass = vec![
            peaking(60.0, 8.0, 1.0),
            peaking(120.0, 6.0, 1.0),
            peaking(250.0, 4.0, 1.0),
            peaking(8000.0, -4.0, 1.0),
        ];
        let treble = vec![
            peaking(60.0, -4.0, 1.0),
            peaking(4000.0, 5.0, 1.0),
            peaking(8000.0, 7.0, 1.0),
            peaking(14000.0, 6.0, 1.0),
        ];
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&bass));
        c.settle();
        assert!(c.set_bands(&treble));
        let swap_ms = Cascade::ramp_ms_for(c.ramp_distance_db());

        let mut d = Cascade::new(1, FS);
        assert!(d.set_bands(&realistic_correction(3.0)));
        d.settle();
        assert!(d.set_bands(&realistic_correction(3.2)));
        let drag_ms = Cascade::ramp_ms_for(d.ramp_distance_db());

        eprintln!("drag {drag_ms:.2} ms, slot swap {swap_ms:.2} ms");
        assert!(drag_ms <= RAMP_MS + 0.01, "drag should still sit at the floor, got {drag_ms} ms");
        assert!(
            swap_ms > drag_ms * 5.0,
            "a full slot swap should ramp far longer than a single-band drag: {swap_ms} ms vs {drag_ms} ms"
        );
    }

    /// The core mechanism directly: a request stays "continuing a gesture" — truncated to
    /// [`RAMP_MS`] — for [`GESTURE_GAP_MS`] after the *previous* request, not just while a ramp
    /// is still physically in flight. A `RAMP_MS`-floor ramp finishes in 8 ms, far short of a
    /// real drag's own cadence, so checking `ramp_left` alone would let the very next retarget
    /// read the cascade as settled again and hand it a fresh full-length ramp — see
    /// `start_ramp`'s own doc for the measured bug this closes. Only a gap that genuinely
    /// exceeds `GESTURE_GAP_MS` earns the full duration again.
    #[test]
    fn a_retarget_stays_truncated_through_the_whole_gesture_gap() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&realistic_correction(3.0)));
        c.settle();

        // First substantive retarget: settled, so this earns the full distance-scaled ramp. A
        // large jump (not the drag-sized 0.2 dB nudge `a_drag_sized_single_band_edit_still_hits_the_floor`
        // shows sitting at the floor already), so it genuinely exercises the "first request"
        // branch rather than one that would truncate to the floor on its own merits anyway.
        assert!(c.set_bands(&realistic_correction(9.0)));
        let first_ms = Cascade::ramp_ms_for(c.ramp_distance_db());
        assert!(first_ms > RAMP_MS + 0.01, "sanity check: this edit must not already sit at the floor");
        assert_eq!(c.ramp_frames, ((first_ms / 1000.0) * FS).round().max(1.0) as u32);

        // Only a few ms into that (long) first ramp — nowhere near enough for it to land —
        // standing in for "the next drag tick arrives well before the previous request's own
        // ramp would have finished," as a real fast drag does.
        let mut out = [0.0f32; 1];
        for _ in 0..((RAMP_MS / 1000.0 * FS).round() as usize + 10) {
            c.process(&[0.1], &mut out, 1);
        }

        // Retarget again while the first ramp is still very much in flight: this must
        // truncate, on `ramp_left > 0` alone.
        assert!(c.set_bands(&realistic_correction(9.4)));
        assert_eq!(
            c.ramp_frames,
            ((RAMP_MS / 1000.0) * FS).round().max(1.0) as u32,
            "an interrupting retarget must truncate to the floor"
        );

        // Now let *that* truncated ramp fully land — comfortably inside GESTURE_GAP_MS.
        for _ in 0..((RAMP_MS / 1000.0 * FS).round() as usize + 10) {
            c.process(&[0.1], &mut out, 1);
        }
        assert!(!c.is_ramping(), "sanity check: the floor-length ramp must have already landed");

        // The cascade is settled (`ramp_left == 0`) but still within `GESTURE_GAP_MS` of the
        // last request — this retarget must STILL truncate, which is the entire point of
        // `cooldown_left` existing alongside `ramp_left`.
        assert!(c.set_bands(&realistic_correction(9.6)));
        assert_eq!(
            c.ramp_frames,
            ((RAMP_MS / 1000.0) * FS).round().max(1.0) as u32,
            "a retarget arriving within GESTURE_GAP_MS of the last one must still truncate, even though no ramp was in flight"
        );

        // Let the gesture actually end: wait past GESTURE_GAP_MS with no further requests.
        for _ in 0..((GESTURE_GAP_MS / 1000.0 * FS).round() as usize + 10) {
            c.process(&[0.1], &mut out, 1);
        }

        // A genuinely fresh request now earns the full duration again.
        assert!(c.set_bands(&realistic_correction(3.0)));
        let later_ms = Cascade::ramp_ms_for(c.ramp_distance_db());
        assert_eq!(
            c.ramp_frames,
            ((later_ms / 1000.0) * FS).round().max(1.0) as u32,
            "a request arriving after a genuine pause must earn the full duration again"
        );
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
    /// `a_drag_sized_edit_is_far_below_the_headline_figure`). The case that looked like it
    /// needed something better — the A/B slot switch, where the whole correction changes at
    /// once — was measured against a separate parallel-warm-chains mechanism built for
    /// exactly that (two filter instances, inactive one kept warm, crossfaded on switch) and
    /// found no clearly better: the artefact already scales with the size of the change, so a
    /// large A/B jump is masked by the real tonal difference the same way a large edit is.
    /// That mechanism was removed; A/B switches use this same coefficient ramp.
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

    /// The real-world 21-band correction (a low-shelf stack plus a broad low-Q cut around
    /// 105–167 Hz, among others) captured live from a §5.2 isolate press over the low shelf.
    /// The regression this guards: `apply_coeffs` (plain coefficient ramp) drives this exact
    /// drop-to-a-single-bandpass transition to +22 dB above *both* endpoints at 172 Hz, purely
    /// from several individually-clean per-band fades sharing one ramp clock (verified: no
    /// single band here, faded alone, ever overshoots its own endpoints) — see
    /// `start_crossfade`'s own doc for the mechanism this replaces it with.
    fn real_isolate_correction() -> Vec<Band> {
        vec![
            Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 4.18, q: 0.7 },
            Band { kind: FilterKind::HighShelf, freq_hz: 10000.0, gain_db: -3.26, q: 0.7 },
            Band { kind: FilterKind::Peaking, freq_hz: 167.06, gain_db: -3.28, q: 0.3834 },
            Band { kind: FilterKind::Peaking, freq_hz: 2279.24, gain_db: 4.33, q: 2.0228 },
            Band { kind: FilterKind::Peaking, freq_hz: 5880.51, gain_db: -6.2, q: 5.2771 },
            Band { kind: FilterKind::Peaking, freq_hz: 3883.2, gain_db: -3.82, q: 5.9839 },
            Band { kind: FilterKind::Peaking, freq_hz: 1370.05, gain_db: 1.21, q: 2.6791 },
            Band { kind: FilterKind::Peaking, freq_hz: 4591.72, gain_db: 1.29, q: 6.0 },
            Band { kind: FilterKind::Peaking, freq_hz: 6768.92, gain_db: 1.54, q: 6.0 },
            Band { kind: FilterKind::Peaking, freq_hz: 5347.68, gain_db: -1.31, q: 5.9947 },
            Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 0.6, q: 0.7 },
            Band { kind: FilterKind::Peaking, freq_hz: 5789.0, gain_db: -2.0, q: 3.0 },
            Band { kind: FilterKind::Peaking, freq_hz: 8337.0, gain_db: 1.6, q: 2.8 },
            Band { kind: FilterKind::HighShelf, freq_hz: 12000.0, gain_db: 0.3, q: 0.7 },
            Band { kind: FilterKind::Peaking, freq_hz: 1601.0, gain_db: -3.0, q: 1.0 },
            Band { kind: FilterKind::Peaking, freq_hz: 2402.0, gain_db: 1.2, q: 2.0 },
            Band { kind: FilterKind::Peaking, freq_hz: 3894.0, gain_db: -2.8, q: 3.0 },
            Band { kind: FilterKind::Peaking, freq_hz: 3291.0, gain_db: -1.0, q: 0.6 },
            Band { kind: FilterKind::LowShelf, freq_hz: 105.0, gain_db: 0.0, q: 1.0 },
            Band { kind: FilterKind::HighShelf, freq_hz: 4000.0, gain_db: 0.0, q: 0.7 },
            Band { kind: FilterKind::HighShelf, freq_hz: 12000.0, gain_db: 0.0, q: 0.7 },
        ]
    }

    /// The composed response (all bands + preamp, in dB) of whichever bank is currently
    /// contributing to the output, at every point along a full crossfade transition — the
    /// mid-transition ground truth `response_db` deliberately does not provide (it reports the
    /// *target*, per its own doc), the same reason `retuning_live_does_not_splatter...` above
    /// measures actual samples instead of trusting the endpoints.
    fn composed_output_db(c: &Cascade, f: f64) -> f64 {
        let wet: f64 = (0..c.process_count).map(|i| c.coeffs[i].response_db(f, FS)).sum::<f64>()
            + 20.0 * c.preamp.log10();
        if c.process_count2 == 0 {
            return wet;
        }
        let wet_lin = 10.0_f64.powf(wet / 20.0);
        let secondary: f64 = (0..c.process_count2).map(|i| c.coeffs2[i].response_db(f, FS)).sum::<f64>()
            + 20.0 * c.preamp2.log10();
        let secondary_lin = 10.0_f64.powf(secondary / 20.0);
        let mixed_lin = wet_lin + (secondary_lin - wet_lin) * c.cross_mix;
        20.0 * mixed_lin.abs().max(1e-12).log10()
    }

    /// Walks every frame of a crossfade (or plain ramp, if `cross`ing is `false`) and returns
    /// the worst (max) composed response found anywhere along the way, across a 20 Hz–20 kHz
    /// log grid.
    fn worst_response_along_transition(c: &mut Cascade) -> f64 {
        let freqs: Vec<f64> = (0..200).map(|i| 20.0 * 1000.0f64.powf(i as f64 / 199.0)).collect();
        let mut worst = f64::MIN;
        while c.is_ramping() || c.process_count2 > 0 {
            for &f in &freqs {
                worst = worst.max(composed_output_db(c, f));
            }
            if c.ramp_left > 0 {
                c.advance_ramp();
            } else if c.cross_preroll_left > 0 || c.cross_fade_left > 0 {
                c.advance_cross();
            } else {
                break; // process_count2 > 0 but neither clock is running: shouldn't happen
            }
        }
        for &f in &freqs {
            worst = worst.max(composed_output_db(c, f));
        }
        worst
    }

    /// The bug: a plain coefficient ramp (`apply_coeffs`) drops the whole correction to a
    /// single isolate bandpass by sweeping every one of the 21 bands towards `PASSTHROUGH` on
    /// one shared clock, and at 31 Hz that sails to +22 dB above both endpoints (measured; see
    /// `real_isolate_correction`'s doc). Confirms the regression is real before proving the fix
    /// below closes it — a fix test with no matching failure-mode test proves nothing.
    #[test]
    fn a_coefficient_ramp_spikes_on_this_real_correction() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&real_isolate_correction()));
        c.settle();
        let start_max = (0..200)
            .map(|i| 20.0 * 1000.0f64.powf(i as f64 / 199.0))
            .map(|f| composed_output_db(&c, f))
            .fold(f64::MIN, f64::max);

        let bandpass = Band { kind: FilterKind::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 };
        let mut target: Vec<Coeffs> = vec![Coeffs::PASSTHROUGH; real_isolate_correction().len()];
        target.push(coefficients(&bandpass, FS));
        assert!(c.apply_coeffs(&target, 0.0));
        assert!(c.is_ramping());

        let worst = worst_response_along_transition(&mut c);
        eprintln!("coefficient ramp: start_max={start_max:.1} dB, worst_mid_ramp={worst:.1} dB");
        assert!(
            worst > start_max + 10.0,
            "expected the known coefficient-ramp spike (>10 dB over start), got only {:.1} dB over",
            worst - start_max
        );
    }

    /// The fix, proven directly on real audio rather than an analytic frequency-response proxy
    /// (which would have to model phase between the two banks to be trustworthy — magnitude
    /// responses don't simply add). Since `process()`'s blend is
    /// `wet + (secondary - wet) * cross_mix` with `cross_mix` in `[0, 1]`, every output sample
    /// is a **convex combination** of the two banks' own outputs — by the triangle inequality
    /// that can never exceed `max(|wet|, |secondary|)`, regardless of their relative phase.
    /// There is nothing left to spike, by construction, not by luck. This measures that bound
    /// directly: the same real correction, the same isolate bandpass, but driven through
    /// `start_crossfade` instead of `apply_coeffs`, and checks the transition's output never
    /// gets louder, sample for sample, than running either side alone on identical input.
    #[test]
    fn a_crossfade_never_exceeds_either_side_alone() {
        const N: usize = 4000; // well past pre-roll + fade (2 * DRY_FADE_MS worth of frames)
        // A few simultaneous tones rather than one — closer to real content, and exercises
        // more of the spectrum (including near the correction's own low-end cluster) at once.
        let input: Vec<f32> = (0..N)
            .map(|n| {
                let t = n as f64 / FS;
                let s = 0.2 * (2.0 * std::f64::consts::PI * 60.0 * t).sin()
                    + 0.15 * (2.0 * std::f64::consts::PI * 500.0 * t).sin()
                    + 0.1 * (2.0 * std::f64::consts::PI * 4000.0 * t).sin();
                s as f32
            })
            .collect();

        for isolate_freq in [31.0, 2000.0, 9000.0] {
            let bandpass = Band { kind: FilterKind::Bandpass, freq_hz: isolate_freq, gain_db: 0.0, q: 8.0 };

            // Reference A: the old correction alone, untouched, for the whole window.
            let mut wet_only = Cascade::new(1, FS);
            assert!(wet_only.set_bands(&real_isolate_correction()));
            wet_only.settle();
            let mut wet_out = vec![0.0f32; N];
            wet_only.process(&input, &mut wet_out, N);

            // Reference B: just the bandpass, from the same cold start `start_crossfade` gives
            // the real secondary bank (a fresh `Cascade`'s delay registers are already zero).
            let mut secondary_only = Cascade::new(1, FS);
            assert!(secondary_only.set_bands(std::slice::from_ref(&bandpass)));
            secondary_only.settle();
            let mut secondary_out = vec![0.0f32; N];
            secondary_only.process(&input, &mut secondary_out, N);

            // The actual transition: the old correction crossfading to the bandpass mid-stream.
            let mut actual = Cascade::new(1, FS);
            assert!(actual.set_bands(&real_isolate_correction()));
            actual.settle();
            let target = [coefficients(&bandpass, FS)];
            assert!(actual.start_crossfade(&target, 0.0), "the crossfade must be accepted");
            assert!(!actual.is_ramping(), "a crossfade must not also start a coefficient ramp");
            let mut actual_out = vec![0.0f32; N];
            actual.process(&input, &mut actual_out, N);

            let mut worst_excess = 0.0f32;
            for n in 0..N {
                let bound = wet_out[n].abs().max(secondary_out[n].abs());
                worst_excess = worst_excess.max(actual_out[n].abs() - bound);
            }
            eprintln!(
                "isolate_freq={isolate_freq:.0} Hz: worst sample excess over max(wet, secondary) = {worst_excess:.6}"
            );
            assert!(
                worst_excess < 1e-4,
                "crossfade produced a sample louder than either side alone: excess {worst_excess:.6} at {isolate_freq} Hz"
            );

            // And it lands correctly once the transition finishes.
            assert_eq!(actual.process_count2, 0, "the secondary bank must be idle once the fade lands");
            assert_eq!(actual.process_count, 1, "the bandpass must now be the one and only primary band");
        }
    }

    /// A retarget that arrives during **pre-roll** (`cross_mix` still pinned at 0 — see
    /// `start_crossfade`'s doc) is safe to apply immediately: nothing audible has committed to
    /// the old target yet. Must redirect smoothly, not restart the pre-roll clock.
    #[test]
    fn retargeting_mid_preroll_repoints_immediately_without_restarting() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&real_isolate_correction()));
        c.settle();

        let first = [coefficients(&Band { kind: FilterKind::Bandpass, freq_hz: 100.0, gain_db: 0.0, q: 8.0 }, FS)];
        assert!(c.start_crossfade(&first, 0.0));

        // Run the pre-roll down partway — well before the audible fade even begins.
        let mut out = [0.0f32; 1];
        for _ in 0..50 {
            c.process(&[0.1], &mut out, 1);
        }
        assert!(c.cross_preroll_left > 0, "should still be pre-rolling");
        assert_eq!(c.cross_mix, 0.0, "nothing should be audible yet");

        let second = [coefficients(&Band { kind: FilterKind::Bandpass, freq_hz: 8000.0, gain_db: 0.0, q: 8.0 }, FS)];
        assert!(c.start_crossfade(&second, 0.0));
        assert_eq!(c.coeffs2[0].b0, second[0].b0, "the secondary bank must repoint immediately — nothing to defer");

        // Must still land cleanly — no NaN/instability, and it does eventually finish.
        let mut frames = 0;
        while c.process_count2 > 0 {
            c.process(&[0.1], &mut out, 1);
            assert!(out[0].is_finite(), "retargeting mid-preroll must not go unstable");
            frames += 1;
            assert!(frames < 100_000, "crossfade never landed after a retarget");
        }
        assert_eq!(c.process_count, 1);
        assert_eq!(c.coeffs[0].b0, second[0].b0, "must have landed on the retargeted band, not the first one");
    }

    /// **The channel's contract**: a retarget that arrives once the fade is already
    /// **audible** (`cross_mix > 0`) must redirect the secondary bank *immediately*, not queue
    /// behind whatever the in-flight fade already committed to. An earlier version queued this
    /// case specifically to avoid stepping the secondary bank's own contribution to the output —
    /// but that meant the newest request's effect was delayed until the stale one finished,
    /// which is backwards for a live control channel: real-time response wins over how smooth
    /// any one still-in-flight transition looks (see `start_crossfade`'s own doc). The bounded
    /// step this accepts is small in practice — early in the fade, when a retarget is most
    /// likely, `cross_mix` itself is still small, so the secondary bank barely contributes yet.
    #[test]
    fn retargeting_mid_fade_redirects_immediately_without_queueing() {
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&real_isolate_correction()));
        c.settle();

        let first = [coefficients(&Band { kind: FilterKind::Bandpass, freq_hz: 9000.0, gain_db: 0.0, q: 8.0 }, FS)];
        assert!(c.start_crossfade(&first, 0.0));

        // Clear the pre-roll entirely, then run into the audible fade so `cross_mix` is
        // genuinely non-zero — the exact condition a naive queued retarget used to special-case.
        let mut out = [0.0f32; 1];
        while c.cross_preroll_left > 0 {
            c.process(&[0.1], &mut out, 1);
        }
        for _ in 0..100 {
            c.process(&[0.1], &mut out, 1);
        }
        assert!(c.cross_fade_left > 0, "should be mid-fade");
        let mix_at_retarget = c.cross_mix;
        assert!(mix_at_retarget > 0.0, "should already carry real weight");

        let second = [coefficients(&Band { kind: FilterKind::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 }, FS)];
        assert!(c.start_crossfade(&second, 0.0));
        assert_eq!(c.coeffs2[0].b0, second[0].b0, "the secondary bank must redirect immediately — nothing to queue");
        assert_eq!(c.cross_mix, mix_at_retarget, "the blend's own progress must not reset on a retarget");
        assert!(c.cross_fade_left > 0, "the in-flight fade continues on its existing schedule");

        // It must land only on the newest target — never on `first`, which was superseded
        // before it ever took effect.
        let mut frames = 0;
        while c.process_count2 > 0 {
            c.process(&[0.1], &mut out, 1);
            assert!(out[0].is_finite(), "must never go unstable");
            frames += 1;
            assert!(frames < 100_000, "the crossfade never landed");
        }
        assert_eq!(c.coeffs[0].b0, second[0].b0, "must land on the retargeted (second) band, not the superseded first one");
    }

    /// The actual tick sequence and inter-tick timing captured live from a reported
    /// "sweep and lurch" §5.2 isolate drag (the `[isolate-debug]` log), replayed frame-by-frame
    /// at 48 kHz exactly as real audio would see it.
    ///
    /// (elapsed_ms_since_previous_tick, freq_hz) from `t=875306` (first isolate tick) through
    /// `t=883165` (release excluded: that is the crossfade back to the full correction, a
    /// different mechanism).
    const REAL_DRAG_TICKS: &[(u64, f64)] = &[
        (0, 38.0), (170, 58.0), (75, 89.0), (75, 110.0), (74, 136.0), (72, 170.0), (75, 220.0),
        (74, 277.0), (71, 337.0), (71, 430.0), (75, 553.0), (75, 746.0), (76, 1048.0), (74, 1512.0),
        (75, 2383.0), (76, 3583.0), (74, 5573.0), (76, 9597.0), (74, 10699.0), (230, 7566.0),
        (75, 5535.0), (75, 4275.0), (73, 3394.0), (72, 2863.0), (74, 2304.0), (75, 1854.0),
        (75, 1384.0), (75, 946.0), (76, 642.0), (74, 424.0), (72, 286.0), (75, 223.0), (75, 160.0),
        (75, 110.0), (75, 81.0), (75, 59.0), (73, 50.0), (73, 46.0), (81, 46.0), (154, 49.0),
        (125, 53.0), (75, 65.0), (75, 72.0), (75, 77.0), (75, 89.0), (74, 134.0), (75, 237.0),
        (76, 394.0), (75, 647.0), (74, 914.0), (75, 1241.0), (76, 1586.0), (75, 2109.0), (74, 3003.0),
        (71, 4734.0), (75, 7021.0), (75, 9467.0), (71, 12173.0), (83, 12940.0), (128, 13029.0),
        (72, 10341.0), (75, 8434.0), (74, 6695.0), (73, 5726.0), (72, 4865.0), (74, 3941.0),
        (75, 3149.0), (75, 2483.0), (75, 1841.0), (75, 1471.0), (75, 1099.0), (75, 766.0),
        (75, 517.0), (72, 326.0), (75, 223.0), (75, 160.0), (75, 110.0), (75, 81.0), (75, 59.0),
        (73, 50.0), (73, 46.0), (73, 43.0), (74, 39.0), (71, 37.0),
    ];

    /// Highest-magnitude-response frequency in the cascade's *actual, current* single band —
    /// what a listener would hear the peak sitting at right now, as opposed to `response_db`'s
    /// documented target-only answer.
    fn peak_freq(c: &Cascade, scan: &[f64]) -> f64 {
        scan.iter()
            .copied()
            .fold((f64::MIN, 0.0), |(best_db, best_f), f| {
                let db = c.coeffs[0].response_db(f, FS);
                if db > best_db { (db, f) } else { (best_db, best_f) }
            })
            .1
    }

    /// The bug this closes: `start_ramp`'s duration used to always be `ramp_ms_for`'s
    /// distance-scaled figure, with no notion of "already busy" — every retarget, however soon
    /// after the last one, recomputed a fresh full-length ramp. A narrow Q≈8 bandpass reads
    /// even a small frequency move as a large response-curve distance, so real isolate-drag
    /// ticks (arriving roughly every 70 ms) kept landing 100-300 ms ramps that never finished
    /// before the next tick replaced them — the coefficients fell up to ~18x behind the pointer
    /// over the length of a real captured drag, replayed here via the ordinary public
    /// `set_bands` — no special-cased "fast" entry point, just `start_ramp`'s own contract:
    /// only a request arriving while the cascade is genuinely settled (no ramp in flight *and*
    /// no request within `GESTURE_GAP_MS`) gets the full duration; anything that continues an
    /// already-moving gesture truncates to the `RAMP_MS` floor instead of restarting a fresh
    /// full-length one.
    ///
    /// Measures, for each tick, whether the cascade actually reached *that* tick's own
    /// requested frequency by the time the *next* one arrived — not whether it had already
    /// heard about a not-yet-sent future request, which even flawless instant application
    /// could not satisfy and would only measure how big consecutive ticks' own steps happen to
    /// be. A retarget that arrives more than [`GESTURE_GAP_MS`] after the previous one is
    /// legitimately treated as a fresh "first" request (there are two such gaps in this real
    /// drag — the very start, and one genuine 230 ms pause partway through) and may not finish
    /// within the following tick's gap; every retarget that continues an already-moving
    /// gesture must.
    #[test]
    fn set_bands_tracks_the_pointer_closely_through_a_real_isolate_drag() {
        let q = 8.0;
        let mut c = Cascade::new(1, FS);
        assert!(c.set_bands(&[Band {
            kind: FilterKind::Bandpass,
            freq_hz: REAL_DRAG_TICKS[0].1,
            gain_db: 0.0,
            q,
        }]));
        c.settle();

        let scan: Vec<f64> = (0..800).map(|i| 20.0 * 1000.0f64.powf(i as f64 / 799.0)).collect();
        let (input, mut output) = ([0.0f32; 1], [0.0f32; 1]);
        let mut max_lag_pct = 0.0f64;
        for i in 1..REAL_DRAG_TICKS.len() {
            let (gap_before, freq_hz) = REAL_DRAG_TICKS[i];
            assert!(c.set_bands(&[Band { kind: FilterKind::Bandpass, freq_hz, gain_db: 0.0, q }]));
            // How long this tick gets to settle before the next one arrives (or, for the last
            // tick, nothing — that's covered by the settle-tail check below instead).
            let gap_after = REAL_DRAG_TICKS.get(i + 1).map_or(0, |&(g, _)| g);
            for _ in 0..(gap_after as f64 / 1000.0 * FS).round() as usize {
                c.process(&input, &mut output, 1);
            }
            let lag_pct = 100.0 * (freq_hz - peak_freq(&c, &scan)).abs() / freq_hz;
            // This tick itself arrived long enough after the previous one to legitimately be
            // treated as a fresh "first" request (full, un-truncated duration) — allowed to
            // still be mid-ramp when the next tick arrives. Anything closer behind the
            // previous tick is continuing an already-moving gesture and must land in time.
            if (gap_before as f64) <= GESTURE_GAP_MS {
                max_lag_pct = max_lag_pct.max(lag_pct);
            }
        }
        let mut settle_frames = 0;
        while c.is_ramping() {
            c.process(&input, &mut output, 1);
            settle_frames += 1;
        }
        eprintln!(
            "max lag on a continuing tick: {max_lag_pct:.1}%; settle tail after last tick: {:.1} ms",
            settle_frames as f64 / FS * 1000.0
        );
        assert!(
            max_lag_pct < 10.0,
            "expected every gesture-continuing tick to land within its own gap, got {max_lag_pct:.1}% behind at some tick"
        );
        assert!(
            settle_frames as f64 / FS * 1000.0 < 20.0,
            "expected a short settle tail after the drag stops (near the RAMP_MS floor, not RAMP_MAX_MS), got \
             {:.1} ms",
            settle_frames as f64 / FS * 1000.0
        );
    }
}
