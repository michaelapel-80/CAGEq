//! Post-EQ loudness monitoring via WASAPI render-endpoint loopback (§5.3c).
//!
//! Independent of the custom-APO work: loopback taps the mix at the render endpoint, which is
//! already *after* EqualizerAPO in the pipeline — so it's the post-EQ signal, exactly what
//! validates the auto-LUFS preamp ("did the loudness match land?").
//!
//! A [`Monitor`] owns a capture thread that opens loopback on a chosen endpoint, computes
//! peak/RMS (ourselves) and BS.1770 K-weighted momentary/short-term LUFS (via the reference
//! `ebur128` port — getting the K-weighting + gating right by hand at arbitrary sample rates is
//! a correctness rabbit hole not worth re-treading), and reports a [`MeterUpdate`] ~20×/s.
//!
//! Real capture is Windows-only; on other platforms [`Monitor::start`] returns an error and the
//! crate is a no-op stub, so the workspace still builds cross-platform.

use serde::Serialize;

pub mod signal;

/// One meter reading, pushed to the UI ~20×/s. Plain data, so it's platform-independent.
#[derive(Clone, Debug, Serialize)]
pub struct MeterUpdate {
    /// Peak level, dBTP (BS.1770 oversampled true peak — catches inter-sample overs a plain
    /// sample-peak scan would miss, so this can read slightly above 0 on hot/limited material) — a
    /// PPM-style follower on top of that (instant attack, brief hold, release), floored at -120.
    pub peak_db: f32,
    /// True RMS level (VU-integrated), dBFS, floored at -120.
    pub rms_db: f32,
    /// BS.1770 K-weighted momentary loudness (400 ms window), LUFS (floored at -70).
    pub momentary_lufs: f32,
    /// BS.1770 K-weighted short-term loudness (3 s window), LUFS (floored at -70).
    pub short_term_lufs: f32,
    /// BS.1770 gated integrated loudness — the whole measurement's overall level, EBU R128's
    /// headline number — LUFS (floored at -70). Accumulates for as long as the capture session
    /// stays open (a slot/A-B/Dry switch does *not* reset it, only a device/rate change does, or
    /// the reset control below), same window `loudness_range` gates against.
    pub integrated_lufs: f32,
    /// EBU Tech 3342 loudness range (LRA): the spread, in LU, between the 10th and 95th
    /// percentile of gated short-term loudness across the whole measurement — a real statistical
    /// dynamics measure, not a crest-factor stat. `0.0` (not negative, never NaN) before there's
    /// enough history for a meaningful gate, same as `integrated_lufs` above.
    pub loudness_range: f32,
    /// The loudest true peak (dBTP) seen since the last reset — an all-time high-water mark, not
    /// the PPM-style decaying `peak_db` above. Floored at -120.
    pub true_peak_max_db: f32,
    /// `false` when the endpoint produced no audio this window — the UI shows an idle state
    /// rather than a misleading `-120`/`-70`.
    pub signal: bool,
    /// Phosphor-persistence histogram of the level bar: per-segment brightness 0..1, from the
    /// quietest segment (index 0) up. A segment lights when the level covers it and decays
    /// otherwise, so always-covered low segments stay bright and peaks leave a fading afterglow —
    /// the meter's glow. Empty while idle.
    pub bins: Vec<f32>,
    /// The endpoint's real, uncapped shared-mode mix sample rate (Hz) — i.e. Windows' configured
    /// playback rate for this device. `0` while the session is (re)opening. The UI surfaces it
    /// next to the device picker (filter.md §8, read-only format display) for its original
    /// purpose: matching the *device's* configured rate to the source material's own rate to
    /// avoid Windows resampling. Deliberately NOT `run_session`'s own (possibly lower —
    /// `CAPTURE_RATE_CAP`) internal analysis rate: that's what the loopback capture and every
    /// on-screen scope/spectrum/meter reading is actually computed from, but reporting it here
    /// instead of the true device rate would make this readout lie about what device rate you
    /// actually need to match, which defeats its whole purpose.
    pub sample_rate: u32,
}

/// A post-EQ spectrum snapshot (loopback FFT), emitted a few times a second on its own channel.
/// Log-frequency bins so it drops straight onto the EQ chart's log axis; the source spectrum is
/// derivable later by dividing out the known filter response. Plain data → platform-independent.
#[derive(Clone, Debug, Serialize)]
pub struct SpectrumUpdate {
    /// Smoothed magnitude per log-frequency bin, dB (relative), from low freq (index 0) up.
    pub db: Vec<f32>,
    /// One-to-one with `db`, but the true linear-FFT level at each bin's span — the *max* of the
    /// (temporally-smoothed) linear power spectrum over `[lo, hi]`, not `gaussian_power`'s
    /// density-weighted average. `db` is the right thing to *draw*: a real spectral-density shape,
    /// correct for broadband content, that accepts a swept tone reading up to ~19dB low at HF as
    /// the tradeoff (see `gaussian_power`'s doc). That droop is real dilution, though, not just a
    /// cosmetic slope — a tone's whole energy sits in 1-2 linear bins, and averaging those with an
    /// increasingly wide window of near-silent neighbours understates its actual level. Taking the
    /// max over the same span instead reports whichever linear bin actually saw the tone,
    /// undiluted. Only meant for reading a *specific* frequency's level (a detected peak, the
    /// hover cursor) — as a curve it would be a poor density estimator (biased high, noisy) for
    /// broadband material, which is exactly why it's a second field and not a `db` replacement.
    pub peak_db: Vec<f32>,
    /// `db`, but *before* the density-normalization fix (`gaussian_power`'s own doc, "A FOURTH
    /// correction") — the same Gaussian-weighted *sum* that function computes on the way to its
    /// own density average (`acc`, its second return value), calibrated with its own constant
    /// (`Spectrum::raw_power_scale`, Parseval/S2-based — *not* `power_scale`, which is built for
    /// reading a single peak bin and overshoots by several dB if reused for a summed quantity like
    /// this one, measured live before that got corrected). Two genuinely different, both-correct-
    /// in-their-own-convention readings result: `db` is power *spectral density* (right for
    /// broadband/noise-floor claims — pink noise reads the correct -3 dB/octave); `db_raw`
    /// approximates the *total* power captured over each display bin's span instead, which is
    /// Parseval-consistent for a concentrated source (recovers a swept tone's true level with none
    /// of `db`'s dilution droop — matches `peak_db` at the same bin to within ~1dB, verified for
    /// several frequencies) — and, for broadband content, reads pink noise flat, the conventional
    /// RTA display. Computed unconditionally (cheap — reuses the same `gaussian_power` call
    /// already made for `db`, just an extra multiply-and-log step) so SpectrumScope's Tilt toggle
    /// needs no backend coordination, the same "always compute both, let each view pick" pattern
    /// `db_lin` below already established.
    pub db_raw: Vec<f32>,
    /// `db`/`peak_db`'s exact counterparts on a *linear*-frequency axis instead (constant-Hz
    /// spacing spanning the same [`f_min`, `f_max`], `N_LIN_BINS` bins — see that constant's own
    /// doc). For SpectrumScope's optional linear-axis mode: a harmonic series reads as an
    /// evenly-*spaced* comb there, unlike on the log axis, where it bunches at the bottom and
    /// spreads at the top. Computed unconditionally (cheap relative to the log reduction, whose
    /// cost is dominated by its widest, high-frequency Gaussian windows — a linear axis has no
    /// such widening) rather than gated behind a toggle, so EqChart (always log) and
    /// SpectrumScope (either) can each just read whichever fields they want.
    pub db_lin: Vec<f32>,
    pub peak_db_lin: Vec<f32>,
    /// `db_lin`'s counterpart to `db_raw` — see that field's own doc.
    pub db_lin_raw: Vec<f32>,
    /// `false` when the endpoint produced no audio this window (same silence test as
    /// [`MeterUpdate::signal`]). The analyzer view blanks its beam on this instead of painting the
    /// idle-decay sweep: without it, the ~35 dB/s slide to the floor is *actively redrawn* into
    /// the phosphor trail for seconds after the music stops — which reads as burn-in, and had been
    /// chased as a (nonexistent) trail-decay bug through several rendering rewrites before the
    /// rebuild-after-hard-clear behavior gave away that the content was being repainted, not stuck.
    pub signal: bool,
    /// Centre frequency of bin 0 (Hz); bins are geometrically spaced up to `f_max`.
    pub f_min: f32,
    /// Centre frequency of the last bin (Hz).
    pub f_max: f32,
    /// Detected spectral peaks, ascending by frequency, already sub-bin-refined — found directly
    /// on the raw linear power spectrum (uniform, frequency-independent resolution), not on `db`'s
    /// log-binned curve. See `find_peaks`'s own doc for why: `db`'s Gaussian-density smoothing
    /// widens with frequency, which dilutes and reshapes a high-frequency tone enough that
    /// sub-bin interpolation on it produced wildly wrong frequencies at the top of the spectrum —
    /// the bug this field exists to fix by not doing peak-finding on that curve at all.
    pub peaks: Vec<SpectrumPeak>,
}

/// One detected spectral peak — see [`SpectrumUpdate::peaks`].
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SpectrumPeak {
    pub hz: f32,
    pub db: f32,
}

/// A stereo vectorscope window (loopback X-Y goniometer): decimated (left, right) sample pairs
/// for the front-end phosphor scope. `xy` is interleaved `l0, r0, l1, r1, …` at raw sample
/// amplitude (≈ -1..1); a mono endpoint sends `l == r`. Empty while idle. Platform-independent.
#[derive(Clone, Debug, Serialize)]
pub struct ScopeUpdate {
    pub xy: Vec<f32>,
    /// `false` when the endpoint produced no audio this window — the UI idles the scope.
    pub signal: bool,
    /// The endpoint's mix sample rate (Hz) — the rate these samples (and thus the applied EQ) are
    /// at, so the scope's inverse-filter ("undistort") mode can build coefficients at the same fs.
    pub rate: u32,
}

#[cfg(windows)]
pub use windows_impl::{Monitor, TestSignal};

#[cfg(not(windows))]
pub use stub::{Monitor, TestSignal};

/// The Windows default render endpoint's WASAPI id (`{0.0.0.00000000}.{guid}`), or `None` off
/// Windows / on failure. Used to choose the right `ms-settings:` deep-link: the app's device id
/// (the registry GUID) is the suffix of this, so a `contains` match tells whether the selected
/// device *is* the default (filter.md §8). Runs the COM query on its own thread so it doesn't
/// disturb the caller's apartment.
#[cfg(windows)]
pub fn default_render_id() -> Option<String> {
    std::thread::spawn(windows_impl::default_render_id).join().ok().flatten()
}
#[cfg(not(windows))]
pub fn default_render_id() -> Option<String> {
    None
}

#[cfg(windows)]
mod windows_impl {
    use super::{MeterUpdate, ScopeUpdate, SpectrumPeak, SpectrumUpdate};
    use std::collections::VecDeque;
    use std::error::Error;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use realfft::num_complex::Complex;
    use realfft::{RealFftPlanner, RealToComplex};
    use wasapi::{
        initialize_mta, AudioClient, Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
    };

    /// Floor on the shared-mode stream buffer's duration (100ns units — 100ms), used by both
    /// `render_pink` and `run_session`'s own `StreamMode::PollingShared`. The device's own bare
    /// `get_device_period()` default is a *latency* setting, not a safety margin — some
    /// devices/drivers report a shorter default period at higher sample rates (lower configured
    /// latency there), and polling mode is the `wasapi` crate's own documented "more prone to
    /// glitches when running at low latency" mode (event-driven mode avoids that, but isn't worth
    /// the extra API surface for either of these). Neither loopback capture nor the self-test's
    /// pink noise has any reason to chase low latency — a fixed, generous floor trades startup
    /// delay (both already fade in) for real headroom. Same fix, same reasoning, as
    /// `cageq-monitor/examples/testtone.rs`'s own `BUFFER_FLOOR_HNS` — ported here since that fix
    /// only covered the dev example, not this library's own two `PollingShared` sites.
    const BUFFER_FLOOR_HNS: i64 = 1_000_000;

    /// `get_device_period()`'s own default, floored at [`BUFFER_FLOOR_HNS`] — see that constant's
    /// own doc for why the bare default isn't safe to hand `StreamMode::PollingShared` as-is.
    fn safe_buffer_hns(audio_client: &AudioClient) -> Result<i64, Box<dyn Error>> {
        let (def_period, _min_period) = audio_client.get_device_period()?;
        Ok(def_period.max(BUFFER_FLOOR_HNS))
    }

    /// Ceiling on the rate `run_session` ever asks the capture endpoint for, regardless of the
    /// device's own (possibly much higher) native mix rate — nothing in this crate's analysis
    /// (spectrum/scope/meter) needs more than this to be useful. `autoconvert: true` (already set)
    /// makes Windows' own shared-mode engine resample the loopback down to this transparently —
    /// the same AUTOCONVERT mechanism `cageq-monitor/examples/testtone.rs`'s `--rate` flag uses
    /// deliberately in the opposite direction (forcing a *source* rate below the device's, to test
    /// the resampler).
    ///
    /// First tried at 96 kHz — fixed silently-dropped samples at *extreme* device rates
    /// (192/384 kHz), where the whole capture loop's own per-second cost (the spectrum FFT
    /// especially — its analysis window scales up with rate, and it's recomputed on every hop, so
    /// its cost grows faster than linearly with rate) had clearly outrun real time. But testing at
    /// 96 kHz then found a moderate 88.2 kHz device rate *also* wasn't safe: the loop was measured
    /// (a temporary counter in `TimeScope.tsx`) actually managing only ~44 scope emits/sec against
    /// the intended 60 (`SCOPE_INTERVAL`), ~23ms real cadence instead of 16ms — enough for more
    /// than `SCOPE_MAX_POINTS` samples to pile up between emits, silently truncating the excess and
    /// splicing a real discontinuity into `TimeScope`'s ring buffer (visible only once the display
    /// window was wide enough to span one of those seams — narrow ms/div settings could dodge it by
    /// chance).
    ///
    /// That measurement was taken in a **debug build**, though — release measured ~13x lower CPU
    /// for the same session (~4% of a single core in debug vs ~0.3% in release, live-measured), the
    /// usual gap for tight numeric loops (FFT, biquad filtering) with debug's lack of inlining/
    /// vectorization. 96 kHz may well be genuinely safe in release, where this whole analysis
    /// pipeline was always going to actually run — restored to 96 kHz so that's testable at all
    /// (a lower cap here would silently mask a release build that never even attempts the rate in
    /// question). If a release build still shows the symptom (scope-only glitch, clean spectrum,
    /// `n` pinned at exactly `SCOPE_MAX_POINTS`) at 96 kHz, lower this — not `SCOPE_MAX_POINTS` — to
    /// whatever rate that same build actually keeps up with.
    const CAPTURE_RATE_CAP: u32 = 96_000;

    /// dB floor for peak/RMS so a silent endpoint reports a finite number, not -inf.
    const DB_FLOOR: f32 = -120.0;
    /// LUFS floor (below the BS.1770 absolute gate) for the same reason.
    const LUFS_FLOOR: f32 = -70.0;
    /// Emit cadence (~60 fps). Decoupled from the metering ballistics below, which are time-based
    /// — so the fast rate makes the meter smooth, not twitchy (BS.1770 sets window *lengths*, not
    /// a refresh rate; the LUFS windows stay 400 ms / 3 s inside ebur128 regardless).
    const TICK: Duration = Duration::from_millis(16);
    /// Peak meter release: linear dB/s after the hold expires (attack is instant). PPM-like.
    const PEAK_RELEASE_DB_PER_SEC: f32 = 20.0;
    /// Hold a fresh peak this long before it starts releasing, so transients stay readable.
    const PEAK_HOLD: Duration = Duration::from_millis(350);
    /// True-RMS integration for the reference line (symmetric one-pole, VU-like ~300 ms). The
    /// glowing bar itself is the peak's phosphor-decay envelope; this is just the honest RMS
    /// marker drawn on top of it.
    const RMS_TAU_SECS: f64 = 0.3;
    /// No frames for at least this long ⇒ the endpoint is idle (report `signal: false`).
    const SILENCE_GAP: Duration = Duration::from_millis(300);
    /// Level-bar range (dBFS): the phosphor histogram's segments span BAR_MIN_DB..0.
    const BAR_MIN_DB: f32 = -40.0;
    /// Number of segments in the phosphor level histogram (bar resolution).
    const N_BINS: usize = 64;
    /// Phosphor persistence: per-segment brightness decay time constant (seconds) — the afterglow.
    const PHOSPHOR_DECAY_SECS: f32 = 1.2;
    /// Fraction of the way from the block RMS toward its peak used as the histogram coverage level
    /// — a touch of peak so transients poke above the energy fill without saturating the bar.
    const PEAK_BLEND: f32 = 1.0;
    /// Brightness ramp: a segment reaches full brightness when the level sits this many dB above
    /// it, fading to dark at the level. Sets the fill's tonal range — smaller = steeper/punchier,
    /// larger = a gentler glow.
    const GLOW_SPAN_DB: f32 = 3.0;

    // --- spectrum analyzer (post-EQ loopback FFT) ---
    /// Base *analysis window* size at ≤48 kHz (≈171 ms — decent low-end for resonance hunting).
    /// Scaled up with the sample rate in [`Spectrum::new`] so the window *duration* — and thus the
    /// true frequency resolution, Δf = rate/analysis_size, i.e. how well two close tones can be
    /// told apart — stays constant at 96/192 kHz instead of halving/quartering. We only ever
    /// display up to 20 kHz, so analysing the whole (wider) band and ignoring the bins above 20 kHz
    /// is simpler and alias-free vs. decimating the input.
    ///
    /// This is the *real* window length, not the FFT transform length — see `ZERO_PAD_FACTOR`
    /// below for why those are no longer the same number.
    ///
    /// The default, not the only option — one of three explicit tiers the UI's FFT-size slider
    /// picks between (`Spectrum::new`'s `base_fft_size` argument, passed straight through from
    /// the frontend — see [`MED_FFT_SIZE`]/[`HIGH_RES_FFT_SIZE`] for the tradeoff a larger window
    /// buys, and, just as important, doesn't cost).
    const BASE_FFT_SIZE: usize = 8192;
    /// Middle tier — 2x [`BASE_FFT_SIZE`], ≈341 ms at ≤48 kHz, ≈1.5 Hz raw bins instead of ≈2.9 Hz
    /// (both at [`ZERO_PAD_FACTOR`]). This exact size was tried once as the *default* and rejected
    /// ("neither here nor there" — see [`HIGH_RES_FFT_SIZE`]'s doc for the full reasoning that
    /// judgment rests on), but that's a verdict about the default, not about offering it as a
    /// real, explicit choice between [`BASE_FFT_SIZE`] and [`HIGH_RES_FFT_SIZE`] for whoever wants
    /// it — which is what the slider's middle position is. `Spectrum::new` treats this the same as
    /// [`HIGH_RES_FFT_SIZE`] for the resolution-dependent peak-detection tuning (anything above
    /// `BASE_FFT_SIZE` counts as "high res" there — see its own `high_res` local).
    const MED_FFT_SIZE: usize = 16384;
    /// Top tier — 4x [`BASE_FFT_SIZE`], ≈683 ms at ≤48 kHz, ≈0.73 Hz raw bins instead of ≈2.9 Hz
    /// (both at [`ZERO_PAD_FACTOR`]).
    ///
    /// Not a CPU tradeoff at all any more, in fact — not just "nearly free". Total FFT cost/sec is
    /// `O(M log M) × hops/sec` where `hops/sec = rate / (analysis_size / FFT_OVERLAP_DIV)`, and
    /// `M` is `padded_size` (see that local's own doc for the fixed-budget mechanism): at any
    /// given sample rate, `M` is now the *same* fixed budget at all three tiers (padding just
    /// shrinks as the real window grows into it), so raising the tier doesn't grow `M` at all —
    /// only `analysis_size` grows, which *shrinks* `hops/sec`. A higher tier therefore runs the
    /// *same-cost* FFT *less often* — genuinely cheaper per second, not merely close to free
    /// (baseline is ~0.3% of one core at Base, per [`CAPTURE_RATE_CAP`]'s own measurement — CPU
    /// was never the constraint here, at any of the three sizes, before or after this). Higher
    /// sample rates are the real cost driver instead: `mult` scales `M` directly (see
    /// `padded_size`'s own doc), linearly, with no such cancellation. `FFT_OVERLAP_DIV` is a
    /// genuinely different knob from either: more overlap buys smoother/faster-updating output at
    /// whichever resolution is already chosen, linearly in CPU — it cannot buy more resolution
    /// itself (that's fixed by window *duration* alone, a Fourier uncertainty-principle floor, not
    /// an implementation gap `ZERO_PAD_FACTOR` or overlap can paper over).
    ///
    /// The actual price is smearing content that changes within the window's own duration —
    /// fine for hunting a stationary headphone/room resonance, worse for anything transient (a
    /// fast sweep, a percussive test tone) — which is exactly why this and [`MED_FFT_SIZE`] are
    /// opt-in slider positions (defaulting to [`BASE_FFT_SIZE`]) rather than replacing it outright:
    /// the smearing cost is paid on *everything*, all the time, for a resolution win that only
    /// matters when specifically hunting a narrow low-frequency feature.
    const HIGH_RES_FFT_SIZE: usize = 32768;
    /// Defines the *fixed* interpolation budget every tier shares: `BASE_FFT_SIZE ×
    /// ZERO_PAD_FACTOR × mult` (see `Spectrum::new`'s `padded_size`) is the total FFT transform
    /// length at [`BASE_FFT_SIZE`], and [`MED_FFT_SIZE`]/[`HIGH_RES_FFT_SIZE`] reuse that exact
    /// same budget rather than multiplying their own, larger real window by this factor again —
    /// see `padded_size`'s own doc for why growing both independently double-pays for the same
    /// thing. This is NOT the same thing as more resolution: resolution (how well two close tones
    /// can be told apart) is fixed by the analysis window's time *duration*, unaffected by this.
    /// Zero-padding instead *interpolates* the transform of that same finite window more finely
    /// — a finite-duration signal has a well-defined continuous Fourier transform, and the
    /// unpadded FFT only ever samples it coarsely; padding computes more exact samples of that
    /// identical continuous function, not new/approximated information. This is the standard
    /// technique real-time spectrum analyzers use to look smooth without a longer (higher-
    /// latency) window.
    ///
    /// Concretely: 5.9 Hz bins (BASE_FFT_SIZE alone) let the 240 *log*-spaced display bins outrun
    /// the linear FFT's own resolution below ~200 Hz — several adjacent display bins there end up
    /// reading the exact same linear bin, a genuine plateau the display has to render honestly
    /// (see EqChart/SpectrumScope's dedup) rather than smooth over. At ×4 (≈1.5 Hz bins) that
    /// crossover drops to ~50 Hz, shrinking the affected range by roughly the same factor, for a
    /// modest one-time FFT cost (O(N log N), so ×4 the points costs well under ×4) and zero added
    /// latency — same real samples, same hop cadence, just more (interpolated) output bins. That
    /// ~1.5 Hz figure is also what the *budget* is pegged to, at any tier — see `padded_size`.
    const ZERO_PAD_FACTOR: usize = 4;
    /// Floor on `gaussian_power`'s sigma, in the same padded-linear-bin units `ranges` already
    /// uses — see that function's doc for the full case history this closes out. Width-matching
    /// sigma to each log bin's own local span (the un-floored formula) correctly tames hard
    /// bin-membership roughness on dense content, but near the low-frequency crossover that local
    /// span is only a bin or so wide — too narrow to also smooth over the analysis window's own
    /// sidelobe structure, a *fixed-Hz* artifact unrelated to the local decimation ratio.
    ///
    /// A first attempt set this to `4.0 * ZERO_PAD_FACTOR` (16), reasoning "the Hann mainlobe
    /// width, in padded-bin units" — wrong, because a Gaussian's effective reach is `±4σ`
    /// (`gaussian_power` builds `radius` from `sigma * 4.0`), so that set the *sigma* to the
    /// mainlobe width and the actual kernel reach to ~4x the mainlobe width. In practice that
    /// smeared away all frequency resolution below ~100 Hz — confirmed directly in
    /// `cageq-monitor/tests/decimation_spike.rs`'s `floor_sweep_preserves_low_frequency_resolution`
    /// test: at floor=16, even two tones a full octave apart (60/120 Hz) no longer show a
    /// distinguishable dip between their peaks.
    ///
    /// `2.0` is the corrected, spike-verified value: comfortably covers the known sidelobe-null
    /// spacing (~2 unpadded bins) while leaving real closely-spaced content resolved with wide
    /// margin — the tightest pair tested (40/60 Hz, 0.58 octave apart) still shows a 7.67 dB dip
    /// at floor=2.0 (vs. 0.15 dB — effectively merged — at floor=6.0, and total collapse by 16).
    /// Costs some smoothness versus the more aggressive floors (60 Hz near-peak roughness only
    /// drops from 13.65 dB to 7.09 dB, not down to ~1 dB) — a deliberate trade favoring resolution,
    /// per direct user feedback that the heavier floor was "too heavy handed... 0 frequency
    /// resolution below 100Hz."
    const GAUSSIAN_FLOOR_SIGMA_BINS: f32 = 2.0;
    /// Uniform multiplier on `gaussian_power`'s natural (pre-floor) half-width — a small, constant
    /// extra smoothing margin, independent of the floor above (which is a `max()` and so can only
    /// ever help the low end; it never engages at high frequency, where the natural width-matched
    /// sigma already dwarfs any reasonable floor). Verified in the same spike to have no
    /// measurable downside up to at least 1.5x (an isolated 8 kHz tone's -6dB width stays flat at
    /// 232.6 Hz through 1.5x, only widening at 2.0x) — `1.2` is a deliberately small "smidge",
    /// per direct user feedback asking for "additional smoothing (again, just a smidge)" at the
    /// high end specifically.
    const GAUSSIAN_WIDTH_MULT: f32 = 1.2;
    /// The rate the base size is tuned for; higher rates scale the FFT proportionally.
    const BASE_RATE: f32 = 48_000.0;
    /// Hop is a quarter of the (per-rate) FFT size → 75% overlap, Welch-style averaging.
    const FFT_OVERLAP_DIV: usize = 4;
    /// Number of log-frequency display bins spanning [SPEC_F_MIN, SPEC_F_MAX].
    const N_LOG_BINS: usize = 240;
    /// Number of *linear*-frequency display bins spanning the same [SPEC_F_MIN, SPEC_F_MAX] range
    /// — `SpectrumUpdate::db_lin`/`peak_db_lin`, for SpectrumScope's optional linear-axis mode
    /// (constant-Hz spacing, unlike `N_LOG_BINS`'s constant-*percentage* spacing — better for
    /// reading a harmonic series as the evenly-spaced comb it actually is). Matches `N_LOG_BINS`
    /// for a first cut; purely a display-resolution knob, independently tunable later.
    const N_LIN_BINS: usize = 240;
    const SPEC_F_MIN: f32 = 20.0;
    const SPEC_F_MAX: f32 = 20_000.0;
    /// dB floor for empty/silent spectrum bins — just a `log(0)`/finite-number guard, not a claim
    /// about where the analyzer's own precision runs out. Both charts clip their drawing to a
    /// fixed, independent -90 dBFS (`SPEC_TOP_DB - SPEC_DYN` in `SpectrumScope.tsx`/`EqChart.tsx`),
    /// so nothing this deep is ever visible in the drawn curve, peak markers, or `findPeaks`' own
    /// silence gate — this floor only bounds `db`/`peak_db`'s numeric value, and is set deliberately
    /// far below the visible range so the hover/peak readout can report a genuine sub-floor
    /// measurement (e.g. `peak_db`'s undiluted per-bin reading, which correctly scales with the
    /// real analysis window via FFT processing gain for incoherent content — see `max_power`'s
    /// doc) instead of being clamped to a number that looks like a limit but isn't one.
    const SPEC_FLOOR: f32 = -210.0;

    // --- peak-finding (`find_peaks`) ------------------------------------------------------
    // Ported from `SpectrumScope.tsx`'s former client-side `findPeaks`/`interpolatePeak` (values
    // unchanged) — moved here because that function ran on `db`'s log-binned, Gaussian-*density*
    // smoothed curve, whose smoothing sigma widens with frequency (see `gaussian_power`'s own
    // doc). That's fine for drawing a broadband shape, but a high-frequency tone gets smeared
    // into a broad, shallow, not-reliably-parabolic hump across many log bins, each smoothed with
    // a *different*-width kernel — sub-bin interpolation on that produced wildly wrong
    // frequencies at the top of the spectrum. Finding peaks here instead, directly on
    // `avg_power` (uniform, frequency-*independent* linear-bin resolution, before it's ever
    // collapsed into log bins), sidesteps the problem at its source rather than compensating for
    // it after the data is already lossy.

    /// Up to this many peaks reported per snapshot.
    const PEAK_COUNT: usize = 5;
    /// A candidate's dB must exceed both its flanking valleys by at least this much to count as a
    /// real, standalone peak rather than a shoulder riding on a bigger one.
    const PEAK_MIN_PROMINENCE_DB: f32 = 6.0;
    /// How far apart (in octaves) two reported peaks must be — otherwise one broad resonance's
    /// own ripples could fill every remaining slot. In octaves rather than a flat Hz/percent gap
    /// so it means the same thing at 100 Hz and 10 kHz.
    const PEAK_MIN_SEPARATION_OCTAVES: f32 = 1.0;
    /// Tighter minimum spacing at high resolution, where genuinely close content is actually
    /// resolved and the default 1-octave gate would needlessly hide it. Live-tuned against the
    /// real readout, not derived.
    const PEAK_MIN_SEPARATION_OCTAVES_HIGH_RES: f32 = 0.25;
    /// A candidate more than this many dB below the loudest thing in the frame is inaudible
    /// against it and gated out — real content the ear can't use in context, not a false peak
    /// worth relaxing the gate for.
    const PEAK_MAX_RANGE_DB: f32 = 30.0;
    /// Looser range at high resolution: `SPEC_TAU_SECS` (cageq-monitor's power-spectrum smoothing
    /// time constant) is deliberately fixed rather than scaled to the longer hop at high-res, so a
    /// real, quiet, transient partial is likelier to get momentarily cut off by the tighter
    /// default gate. Live-tuned, not derived, like `PEAK_MAX_RANGE_DB` itself originally was.
    const PEAK_MAX_RANGE_DB_HIGH_RES: f32 = 60.0;
    /// How far (in cents — 1200ths of an octave) a candidate may drift from an exact integer
    /// multiple of a lower peak and still fold into its harmonic series, rather than being an
    /// unrelated peak that happens to land nearby.
    const HARMONIC_TOLERANCE_CENTS: f32 = 45.0;
    /// Backstop cap on which partial number a candidate may fold as, derived from the display's
    /// own [SPEC_F_MIN]/[SPEC_F_MAX] range (`20_000 / 20 = 1000`) rather than an instrument-timbre
    /// guess: no real candidate's `n` can ever exceed that ratio for the lowest possible root
    /// anyway, so this only guards against the range ever changing — `HARMONIC_TOLERANCE_CENTS`
    /// and the prominence/audibility gates above already do the real filtering.
    const HARMONIC_MAX_N: u32 = 1000;
    /// Absolute "nothing real happening" gate: a frame whose loudest bin doesn't even reach this
    /// mirrors the frontend's own visible-plot floor (`SPEC_TOP_DB - SPEC_DYN` in
    /// `SpectrumScope.tsx`/`EqChart.tsx`, both `0 - 90`) — kept in sync by hand, the same
    /// convention this codebase already uses for other cross-language mirrored constants. Without
    /// it, WASAPI keeps delivering (all near-zero) frames as long as a stream is open, so pure
    /// digital silence would still have *a* loudest bin and could still report "peaks" that are
    /// really just the noise floor's own ripple.
    const PEAK_SILENCE_FLOOR_DB: f32 = -90.0;

    /// Power-spectrum smoothing time constant (seconds) — just enough to settle pure FFT/windowing
    /// noise across a couple of hops (~43 ms each at the default window, see fft_hop), not to
    /// steady the display over time: that's the front-end's job now (canvas phosphor persistence,
    /// plus interpolating between spectrum events instead of snapping). A slower constant here
    /// used to add its own multi-hop lag *underneath* the front-end's persistence — two decays
    /// compounding into a sluggish "slow fall" that no front-end tuning could get out from under,
    /// since it was baked into the values themselves before they ever left the sidecar.
    ///
    /// **Deliberately a fixed number of seconds, not a fraction of the hop** — tried the latter
    /// (`SPEC_TAU_HOPS = 0.469`, reproducing this exact 0.02s/43ms ratio at the default window,
    /// scaling proportionally at `HIGH_RES_FFT_SIZE`'s ~171 ms hop) on the reasoning that it kept
    /// the *relative* damping-per-hop constant across window sizes, the same principle that
    /// correctly governs `GAUSSIAN_FLOOR_SIGMA_BINS`. Reverted after live use: at high-res that
    /// works out to an ≈80 ms time constant, and it "feels incredibly slow" — perceived
    /// sluggishness tracks *absolute* response latency, not a ratio to the update cadence, unlike
    /// the Gaussian floor's case (there, the physical thing being covered — the Hann window's own
    /// sidelobe spacing — itself shrinks in Hz as the window lengthens, so scaling proportionally
    /// is physically correct; here, nothing about human time-perception scales with FFT hop size).
    /// The fixed 0.02s does mean *less* relative smoothing at high-res (barely more than one raw
    /// hop's own noise gets through, confirmed live as "slightly steppy... but still felt ok") —
    /// an accepted tradeoff, steppy-but-responsive beating smooth-but-laggy.
    const SPEC_TAU_SECS: f32 = 0.02;
    /// Spectrum emit cadence — 60 fps, matching the level meter. The FFT is heavier than the
    /// meter but the fold + emit is cheap enough that the full rate reads noticeably smoother.
    const SPECTRUM_INTERVAL: Duration = Duration::from_millis(16);
    /// Power multiplier applied per emit while genuinely silent (endpoint idle) — fades the stored
    /// spectrum toward the floor instead of freezing it lit (≈35 dB/s at the 16 ms cadence).
    const SPEC_IDLE_DECAY: f32 = 0.88;

    // --- stereo vectorscope (loopback X-Y goniometer) ---
    /// Vectorscope emit cadence (60 fps — the front-end phosphor persistence does the smoothing).
    const SCOPE_INTERVAL: Duration = Duration::from_millis(16);
    /// Safety margin `SCOPE_MAX_POINTS` carries over the *raw* `CAPTURE_RATE_CAP × SCOPE_INTERVAL`
    /// product — covers the capture loop's own cadence slipping past `SCOPE_INTERVAL` under load
    /// (see `CAPTURE_RATE_CAP`'s own doc: measured ~1.36x slower than intended in one debug-build
    /// test), not just raw device throughput at a perfectly-timed 16 ms cadence.
    const SCOPE_MAX_POINTS_MARGIN: usize = 3;
    /// Max (l, r) pairs sent per emit — the *contiguous tail* of the window, in order and **not**
    /// stride-decimated, so the front-end can connect them into a continuous beam trace. Decimation
    /// would shred the drawn Lissajous figures of oscilloscope-music.
    ///
    /// Must comfortably cover what a full `SCOPE_INTERVAL` window *actually* generates — this is a
    /// genuine cap, not just a display-density choice: whatever's generated beyond it is silently
    /// dropped by `tail_pairs` (only the newest `SCOPE_MAX_POINTS` survive), which `TimeScope.tsx`'s
    /// ring buffer then splices to the *next* emit as if no time had passed — a real discontinuity
    /// in an otherwise-continuous stream, not a cosmetic one (reported live as a *perfectly stable,
    /// reproducible* non-sine shape on a pure sine input — deterministic because the same phase gets
    /// dropped every single emit at a fixed cadence/tone-frequency pair, not random glitching; also
    /// visible on Vectorscope, which reads this identical truncated stream).
    ///
    /// **Derived from `CAPTURE_RATE_CAP`, not hand-picked, deliberately**: this used to be a bare
    /// `2048`, reasoned as "enough now that capture is capped at 96 kHz" — true at the time, but
    /// nothing *tied* the two constants together, so raising `CAPTURE_RATE_CAP` alone (exactly what
    /// happened testing whether 384 kHz holds up in a release build) silently reintroduced this
    /// exact truncation bug. Deriving it here makes that coupling structural instead of a comment
    /// someone has to remember to keep in sync.
    const SCOPE_MAX_POINTS: usize = (CAPTURE_RATE_CAP as u128 * SCOPE_INTERVAL.as_millis() / 1000) as usize * SCOPE_MAX_POINTS_MARGIN;

    /// A running loopback monitor. Dropping it (or calling [`Monitor::stop`]) ends the thread.
    pub struct Monitor {
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Monitor {
        /// Start monitoring. `endpoint_id` is the app's device id (the MMDevices registry endpoint
        /// GUID); the matching WASAPI endpoint is found by suffix (its id is `{0.0.0.0…}.{guid}`),
        /// falling back to the default render endpoint when `None` or unmatched. `on_update` is
        /// called from the capture thread ~20×/s.
        /// `scope_viewers` is a shared count of open vectorscope views (inline + pop-out window);
        /// the loopback only accumulates and emits the (heavier) `scope` stream while it's > 0, so
        /// nothing's serialized when no one is watching the scope.
        ///
        /// `fft_size` selects the spectrum analyzer's window size — one of `BASE_FFT_SIZE`,
        /// `MED_FFT_SIZE`, or `HIGH_RES_FFT_SIZE` (see those constants' own docs for the
        /// tradeoff), read live off the shared atomic every read (see `Spectrum::reconfigure`) —
        /// changing it does *not* restart the monitor, same as `harmonic_fold` below, which
        /// toggles `find_peaks`' harmonic folding live the same way. Both are shared with the
        /// Tauri layer (`HarmonicFoldState`/`SpecFftSizeState`) so they survive a monitor restart
        /// (e.g. a device change) too.
        ///
        /// `reset_lufs` is the same shape again: set it once (from the Tauri layer's
        /// `LufsResetState`) to restart the Integrated/Loudness Range/Peak Max measurement on the
        /// very next tick, without touching momentary/short-term or restarting the monitor —
        /// checked-and-cleared inside `run_session`'s own loop.
        pub fn start<F, G, H>(
            endpoint_id: Option<String>,
            scope_viewers: Arc<AtomicUsize>,
            fft_size: Arc<AtomicUsize>,
            harmonic_fold: Arc<AtomicBool>,
            reset_lufs: Arc<AtomicBool>,
            on_update: F,
            on_spectrum: G,
            on_scope: H,
        ) -> Result<Monitor, String>
        where
            F: Fn(MeterUpdate) + Send + 'static,
            G: Fn(SpectrumUpdate) + Send + 'static,
            H: Fn(ScopeUpdate) + Send + 'static,
        {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = thread::Builder::new()
                .name("cageq-loopback".into())
                .spawn(move || {
                    if let Err(e) = capture_loop(
                        endpoint_id,
                        &stop_thread,
                        &scope_viewers,
                        fft_size,
                        harmonic_fold,
                        reset_lufs,
                        on_update,
                        on_spectrum,
                        on_scope,
                    ) {
                        // A failed monitor simply yields no updates; surface why for debugging.
                        eprintln!("[cageq-monitor] capture ended: {e}");
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(Monitor { stop, handle: Some(handle) })
        }

        /// Signal the capture thread to stop and wait for it to unwind.
        pub fn stop(mut self) {
            self.shutdown();
        }

        fn shutdown(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// A self-terminating render (playback) signal, one of two flavors: [`start`](Self::start), a
    /// safe fixed pink-noise player (the render counterpart to the loopback — the app's Self-test
    /// plays this through EqAPO while the loopback captures the result, to prove end-to-end that
    /// the EQ chain is actually applying corrections), or [`start_generator`](Self::start_generator),
    /// the general-purpose test-tone generator behind the in-app window. Either way, level is
    /// faded in and the caller stops it when done.
    pub struct TestSignal {
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl TestSignal {
        /// Start pink noise on `endpoint_id` (or the default render endpoint) until [`stop`].
        pub fn start(endpoint_id: Option<String>) -> Result<TestSignal, String> {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = thread::Builder::new()
                .name("cageq-testsignal".into())
                .spawn(move || {
                    if let Err(e) = render_pink(endpoint_id, &stop_thread) {
                        eprintln!("[cageq-monitor] test signal ended: {e}");
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(TestSignal { stop, handle: Some(handle) })
        }

        pub fn stop(mut self) {
            self.shutdown();
        }

        /// Start the general-purpose test-tone generator (the in-app window, `cageq-app`'s
        /// `start_test_generator` command) on `endpoint_id` (or the default render endpoint)
        /// until [`stop`](Self::stop). Independent render path from `start`'s fixed self-test
        /// pink noise — see `render_generator`'s own doc for why they're not merged.
        pub fn start_generator(
            endpoint_id: Option<String>,
            params: crate::signal::GeneratorParams,
        ) -> Result<TestSignal, String> {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = thread::Builder::new()
                .name("cageq-testgenerator".into())
                .spawn(move || {
                    if let Err(e) = render_generator(endpoint_id, params, &stop_thread) {
                        eprintln!("[cageq-monitor] test generator ended: {e}");
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(TestSignal { stop, handle: Some(handle) })
        }

        fn shutdown(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    impl Drop for TestSignal {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// Render faded-in pink noise out the endpoint at [`crate::signal::SAFE_PLAYBACK_CEILING_DBFS`]
    /// until `stop`. Shared-mode render, so it passes through EqAPO like any app's audio (the
    /// whole point of the self-test).
    fn render_pink(endpoint_id: Option<String>, stop: &AtomicBool) -> Result<(), Box<dyn Error>> {
        initialize_mta().ok()?;
        let enumerator = DeviceEnumerator::new()?;
        let device = resolve_device(&enumerator, &endpoint_id)?;
        let mut audio_client = device.get_iaudioclient()?;
        let mix = audio_client.get_mixformat()?;
        let rate = mix.get_samplespersec();
        let channels = mix.get_nchannels();
        let desired =
            WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
        let block_align = desired.get_blockalign() as usize;
        let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: safe_buffer_hns(&audio_client)? };
        audio_client.initialize_client(&desired, &Direction::Render, &mode)?;
        let render = audio_client.get_audiorenderclient()?;

        let gain = 10f32.powf(crate::signal::SAFE_PLAYBACK_CEILING_DBFS / 20.0);
        let fade = (0.12 * rate as f32) as u64; // 120 ms fade-in — no startup pop
        audio_client.start_stream()?;

        let mut frame: u64 = 0;
        let mut rng: u32 = 0x2545_f491; // xorshift white source
        let (mut b0, mut b1, mut b2) = (0f32, 0f32, 0f32); // Paul Kellet economy pink filter
        let mut buf: Vec<u8> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let space = audio_client.get_available_space_in_frames()? as usize;
            if space == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            buf.clear();
            for _ in 0..space {
                let env = if frame < fade { frame as f32 / fade as f32 } else { 1.0 };
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                let white = (rng as f32 / u32::MAX as f32) * 2.0 - 1.0;
                b0 = 0.99765 * b0 + white * 0.0990460;
                b1 = 0.96300 * b1 + white * 0.2965164;
                b2 = 0.57000 * b2 + white * 1.0526913;
                let s = ((b0 + b1 + b2 + white * 0.1848) * 0.11 * gain * env).clamp(-0.891, 0.891);
                let bytes = s.to_le_bytes();
                for _ in 0..channels {
                    buf.extend_from_slice(&bytes);
                }
                frame += 1;
            }
            let frames = buf.len() / block_align;
            if frames > 0 {
                render.write_to_device(frames, &buf, None)?;
            }
        }
        let _ = audio_client.stop_stream();
        Ok(())
    }

    /// Render `params.signal` out the endpoint until `stop`, honoring `params.seconds` as an
    /// auto-stop with a fade-out. Mirrors `testtone.rs`'s own main loop (device/format/stream
    /// setup, fade in/out envelope, wavetable indexing for `Tone`/`Isp`, `PinkNoise` for
    /// `Pink`/`White`) but stop-flag-driven instead of Ctrl+C-driven, and silent (no `eprintln!`
    /// diagnostics — those are CLI-only concerns `testtone.rs` keeps for itself).
    ///
    /// Kept independent from [`render_pink`] rather than generalizing that function to cover both:
    /// `render_pink` backs the app's Self-test feature (real users rely on its exact fixed
    /// behavior — pink noise, -18 dBFS, no seconds/rate/level parameters), and threading a much
    /// larger parameter surface through it risks changing that behavior by accident. Two render
    /// loops sharing `resolve_device`/`safe_buffer_hns` (device/buffer setup) and, for the new
    /// path only, `crate::signal`'s synthesis (see that module's own doc for why *that* part is
    /// shared) is a smaller blast radius than one loop trying to serve both.
    fn render_generator(
        endpoint_id: Option<String>,
        params: crate::signal::GeneratorParams,
        stop: &AtomicBool,
    ) -> Result<(), Box<dyn Error>> {
        use crate::signal::{
            am_sample, build_wavetable, chirp_phase, fm_phase, wavetable_sample, wavetable_step, PinkNoise, Signal,
        };

        initialize_mta().ok()?;
        let enumerator = DeviceEnumerator::new()?;
        let device = resolve_device(&enumerator, &endpoint_id)?;
        let mut audio_client = device.get_iaudioclient()?;
        let mix = audio_client.get_mixformat()?;
        let mix_rate = mix.get_samplespersec();
        let channels = mix.get_nchannels();
        // Source rate: the device mix rate unless forced — when it differs, AUTOCONVERT below
        // makes the engine resample up/down to the mix rate (the whole point of `rate_override`).
        let rate = params.rate_override.unwrap_or(mix_rate);
        let desired = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
        let block_align = desired.get_blockalign() as usize;
        let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: safe_buffer_hns(&audio_client)? };
        audio_client.initialize_client(&desired, &Direction::Render, &mode)?;
        let render = audio_client.get_audiorenderclient()?;

        let gain = 10f32.powf(params.level_dbfs / 20.0);
        let total_frames: Option<u64> = params.seconds.map(|s| (s * rate as f32) as u64);
        let fade_frames = (0.12 * rate as f32) as u64; // 120 ms fade in/out — kills startup/stop pops
        let table = build_wavetable(params.signal, rate);
        let step = wavetable_step(params.signal, rate);
        let mut phase: f64 = 0.0;
        let mut noise = PinkNoise::new();
        // Chirp's own repeat-cycle length in frames — see the `Signal::Chirp` arm below for why
        // it repeats via a local per-cycle fade rather than carrying an accumulated phase across
        // the wrap. Unused (stays 1) for every other signal.
        let chirp_cycle_frames = if let Signal::Chirp { duration_secs, .. } = params.signal {
            ((duration_secs * rate as f64).round() as u64).max(1)
        } else {
            1
        };

        audio_client.start_stream()?;
        let mut frame: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        'play: while !stop.load(Ordering::Relaxed) {
            if let Some(total) = total_frames {
                if frame >= total {
                    break;
                }
            }
            let space = audio_client.get_available_space_in_frames()? as usize;
            if space == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            buf.clear();
            for _ in 0..space {
                if stop.load(Ordering::Relaxed) {
                    break 'play;
                }
                if let Some(total) = total_frames {
                    if frame >= total {
                        break;
                    }
                }
                let mut env = if frame < fade_frames { frame as f32 / fade_frames as f32 } else { 1.0 };
                if let Some(total) = total_frames {
                    let rem = total.saturating_sub(frame);
                    if rem < fade_frames {
                        env = env.min(rem as f32 / fade_frames as f32);
                    }
                }
                let mono = match params.signal {
                    Signal::Tone { .. } | Signal::Isp { .. } => {
                        let s = wavetable_sample(&table, phase) * gain;
                        phase = (phase + step).rem_euclid(table.len() as f64);
                        s
                    }
                    Signal::White => noise.next_white() * gain,
                    Signal::Pink => noise.next_pink() * gain,
                    Signal::Chirp { f0, f1, duration_secs, log } => {
                        let frame_in_cycle = frame % chirp_cycle_frames;
                        let t = frame_in_cycle as f64 / rate as f64;
                        let s = chirp_phase(f0, f1, duration_secs, log, t).sin() as f32 * gain;
                        // Local fade in/out at every repeat boundary, same `fade_frames` pattern
                        // as the session-level `env` above but measured against the cycle instead
                        // of the whole session — a naive phase wrap generally isn't continuous
                        // and would otherwise click every `duration_secs` (see `Signal::Chirp`'s
                        // own doc).
                        let mut cycle_env = if frame_in_cycle < fade_frames {
                            frame_in_cycle as f32 / fade_frames as f32
                        } else {
                            1.0
                        };
                        let rem_in_cycle = chirp_cycle_frames.saturating_sub(frame_in_cycle);
                        if rem_in_cycle < fade_frames {
                            cycle_env = cycle_env.min(rem_in_cycle as f32 / fade_frames as f32);
                        }
                        s * cycle_env
                    }
                    Signal::Am { carrier_hz, mod_hz, depth } => {
                        let t = frame as f64 / rate as f64;
                        am_sample(carrier_hz, mod_hz, depth, t) as f32 * gain
                    }
                    Signal::Fm { carrier_hz, mod_hz, deviation_hz } => {
                        let t = frame as f64 / rate as f64;
                        fm_phase(carrier_hz, mod_hz, deviation_hz, t).sin() as f32 * gain
                    }
                };
                let s = (mono * env).clamp(-params.sample_ceil, params.sample_ceil);
                let bytes = s.to_le_bytes();
                for _ in 0..channels {
                    buf.extend_from_slice(&bytes);
                }
                frame += 1;
            }
            let frames = buf.len() / block_align;
            if frames > 0 {
                render.write_to_device(frames, &buf, None)?;
            }
        }

        // Let the last buffer drain before tearing down, so a --seconds fade-out is actually heard.
        thread::sleep(Duration::from_millis(200));
        let _ = audio_client.stop_stream();
        Ok(())
    }

    /// Round to a fixed number of decimal digits before it goes out over IPC. `f32`'s shortest
    /// round-trip text (what serde_json emits) can run to 8-9 significant digits even though only
    /// ~1-2 decimals are ever meaningful here — at ~60Hz across a few hundred bins that difference
    /// is real, sustained JSON-parse garbage on the JS side (a WebView2 GC-pressure finding, not a
    /// display concern), so we quantize outgoing values instead of the physically-precise ones.
    fn round_to(v: f32, decimals: i32) -> f32 {
        let scale = 10f32.powi(decimals);
        (v * scale).round() / scale
    }

    /// Collapse `power` (one value per linear FFT bin) onto a single log-display-bin reading, over
    /// the bin's own exact (fractional) span `[lo, hi]` — a Gaussian-weighted integral, not a
    /// rectangular (boxcar) one and not the single loudest sample in the range.
    ///
    /// This is the third reduction this bin has used, and the case for each replacement is worth
    /// keeping (verified against synthetic ground truth in `cageq-monitor/tests/decimation_spike.rs`
    /// each time, not live-guessed):
    ///  1. `.fold(max)` (original): at the high end a single display bin spans dozens to hundreds
    ///     of linear bins (more so since `ZERO_PAD_FACTOR` densified them), and independently
    ///     peak-picking each one from a mostly-unrelated neighbouring window produces adjacent
    ///     display bins whose reported level barely correlates — a jagged, uncorrelated zigzag on
    ///     dense/busy content.
    ///  2. A rectangular (boxcar) integral over `[lo, hi]` fixed that, and is also the *more*
    ///     correct read for an isolated tone by Parseval's theorem (recovers the tone's whole main
    ///     lobe rather than one sample of it, immune to "scalloping loss"). But a box's inclusion
    ///     rule is all-or-nothing: a discrete spectral feature (a harmonic partial, FFT sidelobe
    ///     structure) sitting near a boundary counts *entirely* toward whichever side it falls on,
    ///     so as that hard boundary sweeps past dense discrete content from bin to bin, each
    ///     feature flips in or out abruptly — the spike's own measurement shows this literally
    ///     quantizes the output to `log10(integer count of partials included)`, jumping between
    ///     discrete levels even though the underlying content barely changes.
    ///  3. Gaussian removes the hard edge: a feature near a boundary contributes partially to BOTH
    ///     neighbouring bins, blended smoothly, so the same sweep produces a smoothly-varying total
    ///     instead of a jagged one — measured 3.7x smoother on a synthetic harmonic series, with
    ///     the max/box/gaussian ordering coming out monotonic exactly as predicted.
    ///
    /// One theory the spike *ruled out*, worth not re-trying: this is NOT classical decimation
    /// aliasing in the continuous-signal sense, and a box's sidelobes are not "leaking" outside
    /// bin content in — a rectangular weight over already-computed frequency-domain power values
    /// has *zero* response outside its own exact range, by construction (unlike a window applied
    /// to time-domain data before an FFT, which does have real sidelobes — the Hann-window case
    /// this same investigation found in `SpectrumUpdate`'s own history). The problem was always
    /// hard bin-membership on discrete content, not out-of-band leakage.
    ///
    /// Sigma is set to the bin's own nominal half-width (`(hi-lo)/2`), i.e. matched to the *local*
    /// decimation ratio exactly the way `[lo, hi]` already is — wide in linear-bin terms low in
    /// the spectrum, narrow high up — scaled by `GAUSSIAN_WIDTH_MULT` (a small constant "smidge" of
    /// extra smoothing everywhere) and then floored at `GAUSSIAN_FLOOR_SIGMA_BINS` (see that
    /// constant's own doc for why the width-matched value alone isn't enough at the low end: near
    /// the low-frequency crossover the local span is too narrow to smooth over the analysis
    /// window's own, fixed-Hz sidelobe structure, a second, different-scale problem from the one
    /// width-matching alone solves).
    ///
    /// A FOURTH correction, reported live against real pink noise ("perfectly equal magnitude...
    /// reading high"): this used to be scaled by `(hi-lo)`, matching the boxcar integral's "total
    /// power over this span" units. That's exactly the bug — reported live and reproduced with a
    /// direct FabFilter Pro-Q screenshot (pink noise sloping down ~-3dB/octave, not flat). Real
    /// analyzers display power SPECTRAL DENSITY (power per Hz), not power sitting over the display
    /// bin's own bandwidth: a log bin's bandwidth grows with frequency (1 linear bin at 20Hz, ~287
    /// at 14.5kHz — `cageq-monitor/tests/decimation_spike.rs`'s `old_max_vs_new_gaussian_on_pink_noise`),
    /// so a `*(hi-lo)` reading grows right along with it for any broadband signal — flat instead of
    /// the correct -3dB/octave for pink noise, confirmed to ~0.02dB of theory once removed
    /// (`gaussian_floor_bias_on_pink_noise`, `width_matched_density_final_check`). Simply dropping
    /// the `*(hi-lo)` factor — reporting the Gaussian-weighted AVERAGE, not the average scaled up by
    /// the span it was estimated over — is the fix. The pre-normalized *sum* (`acc`, everything
    /// below divides by `wsum` to get; this function's second return value) is worth keeping
    /// around rather than discarding, though — see `SpectrumUpdate::db_raw`'s own doc for why: it's
    /// what a later `db_raw` display mode reconstructs "un-density-normalized" power from, and
    /// measured live to be a genuinely unbiased total for a concentrated source (unlike re-scaling
    /// the average back up by the bin's own nominal span, which reads a systematic several dB high
    /// — `sigma` isn't literally `(hi-lo)/2`, it's that scaled by `GAUSSIAN_WIDTH_MULT` and then
    /// floored, so multiplying by the nominal span doesn't actually invert this function's own
    /// weighting).
    ///
    /// This does cost something for an isolated TONE: sigma still widens with frequency (needed —
    /// `decimation_spike.rs`'s `debug_12khz_window_scan` shows a sigma that *doesn't* track the
    /// bin's own width can miss real content sitting away from the bin's centre entirely, ~76 bins
    /// off in that measured case), and averaging a tone's few genuinely-loud bins together with an
    /// increasingly wide window of near-silent neighbours dilutes it — a real, ~19dB droop measured
    /// from 60Hz to 18kHz for a swept full-scale tone. This is the textbook resolution-bandwidth
    /// tradeoff of ANY constant-Q/fractional-octave analyzer (confirmed against a real reference
    /// technique — Tylka & Choueiri, JAES, "fractional-octave smoothing" — not fixable without a
    /// genuinely different architecture, e.g. multi-resolution FFT). Accepted per direct user
    /// steer: broadband content is the common case here, and a correct noise floor/pink-noise
    /// reading matters more than a perfectly flat tone sweep — `db_raw` (using this function's
    /// `acc` return) is what routes around it for whoever wants that instead.
    ///
    /// Returns `(density, raw)` — `acc/wsum` (the density this function exists for) and `acc`
    /// itself (the pre-normalization weighted sum, `db_raw`'s own input).
    fn gaussian_power(power: &[f32], lo: f32, hi: f32) -> (f32, f32) {
        let lo = lo.max(0.0);
        let hi = hi.min(power.len().saturating_sub(1) as f32);
        let center = (lo + hi) / 2.0;
        let half_width = ((hi - lo) / 2.0) * GAUSSIAN_WIDTH_MULT;
        let sigma = half_width.max(GAUSSIAN_FLOOR_SIGMA_BINS);
        let radius = (sigma * 4.0).ceil() as i32;
        let c = center.round() as i32;
        let lo_i = (c - radius).max(0) as usize;
        let hi_i = ((c + radius) as usize).min(power.len().saturating_sub(1));
        let mut acc = 0.0f32;
        let mut wsum = 0.0f32;
        for (i, &p) in power.iter().enumerate().take(hi_i + 1).skip(lo_i) {
            let d = i as f32 - center;
            let w = (-0.5 * (d / sigma).powi(2)).exp();
            acc += p * w;
            wsum += w;
        }
        if wsum > 0.0 {
            (acc / wsum, acc)
        } else {
            (0.0, 0.0)
        }
    }

    /// Companion to `gaussian_power`, for `SpectrumUpdate::peak_db` — the plain max of `power`
    /// over the bin's own exact (fractional) span `[lo, hi]`, rounded outward to whole linear
    /// bins rather than weighted/interpolated at the edges. Unlike `gaussian_power`, correctness
    /// here isn't about a smooth *shape*: this only ever gets read at a single reported frequency
    /// (a peak, the cursor), so there's no adjacent-bin sweep to jump between and nothing for a
    /// hard edge to visibly break.
    fn max_power(power: &[f32], lo: f32, hi: f32) -> f32 {
        let lo_i = lo.max(0.0).floor() as usize;
        let hi_i = (hi.max(0.0).ceil() as usize).min(power.len().saturating_sub(1));
        power[lo_i..=hi_i].iter().copied().fold(0.0f32, f32::max)
    }

    fn to_db(linear: f32) -> f32 {
        if linear <= 0.0 {
            DB_FLOOR
        } else {
            (20.0 * linear.log10()).max(DB_FLOOR)
        }
    }

    /// Up to [PEAK_COUNT] distinct spectral peaks in `power[0..n)` (raw, uniformly-spaced linear
    /// FFT bins), ported from the frontend's former client-side `findPeaks` (see the module doc
    /// above `PEAK_COUNT` for why it moved here): local maxima, prominent enough to be a real
    /// peak rather than FFT noise (`PEAK_MIN_PROMINENCE_DB`), loud enough to be real content
    /// rather than noise-floor ripple (`max_range_db`), spaced far enough apart that they aren't
    /// all just one resonance's shoulder (`min_separation_octaves`), and — when `fold_harmonics`
    /// is set — not an integer-ratio harmonic of a lower peak that's also present (`harmonic_of`).
    /// Returns the *largest* qualifying peaks, then reorders them to ascending frequency —
    /// picking by magnitude and presenting by frequency are different steps on purpose, so a
    /// strong low-frequency hum and a quieter but still-qualifying high note both land in the
    /// order a reader scans the axis, not loudest-first. Drops the original's plateau/tied-run
    /// walk: that only existed for `db`'s display-side 1-decimal rounding, and raw `f32` power
    /// values essentially never tie exactly.
    fn find_peaks(
        power: &[f32],
        power_scale: f32,
        bin_hz: f32,
        fold_harmonics: bool,
        min_separation_octaves: f32,
        max_range_db: f32,
    ) -> Vec<SpectrumPeak> {
        let n = power.len();
        if n < 3 {
            return Vec::new();
        }
        let v: Vec<f32> = power
            .iter()
            .map(|&p| if p > 0.0 { 10.0 * (p * power_scale).log10() } else { f32::NEG_INFINITY })
            .collect();

        let mut loudest = f32::NEG_INFINITY;
        for &x in &v {
            if x > loudest {
                loudest = x;
            }
        }
        // Absolute silence gate — see PEAK_SILENCE_FLOOR_DB's own doc. WASAPI keeps delivering
        // (all near-zero) frames as long as a stream is open, so this can't rely on `signal`.
        if loudest < PEAK_SILENCE_FLOOR_DB {
            return Vec::new();
        }

        // 1) Local maxima.
        let mut candidates: Vec<(usize, f32)> = Vec::new();
        for i in 1..n - 1 {
            if v[i] > v[i - 1] && v[i] >= v[i + 1] {
                candidates.push((i, v[i]));
            }
        }

        // 2) Prominence: walk outward from each candidate until the ground rises back above it
        // (or the array ends), tracking the lowest point crossed each way. A shoulder bump never
        // finds a valley deep enough before running into the bigger peak it's riding on; a
        // standalone peak does.
        let prominent: Vec<(usize, f32)> = candidates
            .into_iter()
            .filter(|&(i, val)| {
                let mut left_min = val;
                let mut j = i;
                while j > 0 && v[j - 1] <= val {
                    j -= 1;
                    left_min = left_min.min(v[j]);
                }
                let mut right_min = val;
                let mut k = i;
                while k + 1 < n && v[k + 1] <= val {
                    k += 1;
                    right_min = right_min.min(v[k]);
                }
                val - left_min.max(right_min) >= PEAK_MIN_PROMINENCE_DB
            })
            .collect();

        // 2.5) Noise-floor gate: prominence alone can't tell a real quiet feature from the
        // floor's own statistical ripple — this can, since it's relative to the loudest thing
        // actually in the frame rather than each candidate's own immediate neighbours.
        let audible: Vec<(usize, f32)> =
            prominent.into_iter().filter(|&(_, val)| loudest - val <= max_range_db).collect();

        // 3) Harmonic folding: ascending frequency (bin index already is, for this uniformly-
        // spaced array), so a later, higher partial can fold into a root added just before it in
        // this same pass.
        let mut roots: Vec<(usize, f32)> = Vec::new();
        for &(i, val) in &audible {
            let f = bin_hz * i as f32;
            let is_harmonic =
                fold_harmonics && roots.iter().any(|&(ri, _)| harmonic_of(f, bin_hz * ri as f32));
            if !is_harmonic {
                roots.push((i, val));
            }
        }

        // 4) Greedy pick by magnitude, skipping anything too close (in octaves) to an already-
        // picked peak — otherwise one broad resonance's own ripples could fill every remaining
        // slot.
        roots.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut picked: Vec<(usize, f32)> = Vec::new();
        for &(i, val) in &roots {
            if picked.len() >= PEAK_COUNT {
                break;
            }
            let f = bin_hz * i as f32;
            let too_close = picked.iter().any(|&(pi, _)| {
                let pf = bin_hz * pi as f32;
                (f / pf).log2().abs() < min_separation_octaves
            });
            if too_close {
                continue;
            }
            picked.push((i, val));
        }

        // 5) Presented by frequency, not the magnitude order they were picked in. Interpolated
        // last, after every index-based comparison above is done with the coarse integer bin —
        // those decisions don't need sub-bin precision, only the final reported frequency/level do.
        picked.sort_by(|a, b| a.0.cmp(&b.0));
        picked
            .into_iter()
            .map(|(i, _)| {
                let (ri, rv) = interpolate_peak(&v, i, n);
                SpectrumPeak { hz: bin_hz * ri, db: rv }
            })
            .collect()
    }

    /// Parabolic (quadratic) interpolation across the three linear bins straddling a peak at
    /// integer index `i`, refining both its frequency and level to sub-bin precision — the
    /// standard technique for sub-bin FFT peak/pitch estimation, principled here because these
    /// are actual (Hann-windowed, zero-padded-for-interpolation) FFT bins with a smooth,
    /// well-behaved main lobe near an isolated tone. This used to run on `db`'s log-binned,
    /// cross-bin-varying-kernel curve instead (see the module doc above `PEAK_COUNT`), where that
    /// smoothness assumption broke down at high frequency and produced wildly wrong frequencies —
    /// the bug `find_peaks` exists to fix by not running on that curve at all. Returns `i`
    /// untouched at an array edge or where the fit is degenerate (denominator ~0, a genuinely
    /// flat top).
    fn interpolate_peak(v: &[f32], i: usize, n: usize) -> (f32, f32) {
        if i == 0 || i >= n - 1 {
            return (i as f32, v[i]);
        }
        let ym1 = v[i - 1];
        let y0 = v[i];
        let yp1 = v[i + 1];
        let denom = ym1 - 2.0 * y0 + yp1;
        if denom.abs() < 1e-9 {
            return (i as f32, y0);
        }
        let d = (0.5 * (ym1 - yp1) / denom).clamp(-0.5, 0.5);
        (i as f32 + d, y0 - 0.25 * (ym1 - yp1) * d)
    }

    /// Is `f` an integer multiple (2nd..[HARMONIC_MAX_N]th partial) of `root`, within
    /// [HARMONIC_TOLERANCE_CENTS]? Used by `find_peaks` to fold a peak into a lower one's
    /// harmonic series. Deliberately only tests the pairwise ratio between two *actually
    /// detected* peaks — it doesn't try to infer an absent fundamental from its partials.
    fn harmonic_of(f: f32, root: f32) -> bool {
        if f <= root {
            return false;
        }
        let n = (f / root).round();
        if n < 2.0 || n > HARMONIC_MAX_N as f32 {
            return false;
        }
        let cents = 1200.0 * (f / (n * root)).log2();
        cents.abs() < HARMONIC_TOLERANCE_CENTS
    }

    fn clamp_lufs(v: f64) -> f32 {
        if v.is_finite() {
            (v as f32).max(LUFS_FLOOR)
        } else {
            LUFS_FLOOR
        }
    }

    /// Same idea as `clamp_lufs`, but for a LU *width* (loudness range) rather than an absolute
    /// LUFS level — 0.0 is the natural floor (no spread yet), not -70; `ebur128` itself already
    /// never returns negative or NaN here, this just guards the non-finite case the same way.
    fn clamp_lu(v: f64) -> f32 {
        if v.is_finite() {
            (v as f32).max(0.0)
        } else {
            0.0
        }
    }

    /// Post-EQ spectrum analyzer: accumulates mono loopback samples, runs overlapping Hann-windowed
    /// FFTs, lightly averages the power spectrum across hops, and collapses it onto log-frequency
    /// bins. No peak-hold here — the front-end's own persistence covers that job now.
    struct Spectrum {
        fft: Arc<dyn RealToComplex<f32>>,
        base_fft_size: usize,      // current, already-snapped tier — reconfigure()'s change guard
        analysis_size: usize,      // real, windowed sample count (BASE_FFT_SIZE scaled to rate) —
                                    // governs window duration/hop timing, i.e. true resolution
        fft_hop: usize,            // hop between windows = analysis_size / FFT_OVERLAP_DIV
        in_buf: Vec<f32>,          // realfft input scratch, len padded_size (see that local's own
                                    // doc — the fixed per-rate budget, not analysis_size*ZERO_PAD_FACTOR
                                    // at every tier) — only the first analysis_size entries ever hold real samples,
                                    // the rest must stay exactly 0.0 (see push(): realfft documents
                                    // input as "garbage after calling", so it's re-zeroed every hop
                                    // rather than trusted to stay zero from init)
        out_buf: Vec<Complex<f32>>, // realfft output scratch (len in_buf.len()/2 + 1)
        scratch: Vec<Complex<f32>>,
        window: Vec<f32>,          // Hann window, len analysis_size — the real samples only
        accum: VecDeque<f32>,      // mono sample accumulator
        avg_power: Vec<f32>,       // smoothed linear power per (padded, interpolated) FFT bin
        ranges: Vec<(f32, f32)>, // per log bin: exact (fractional) linear-bin span — see `gaussian_power`
        ranges_lin: Vec<(f32, f32)>, // per linear-axis bin: same, constant-Hz spacing instead
        smoothing: f32,            // power-average coefficient per hop
        power_scale: f32,          // |X|² -> normalized power so a full-scale sine reads ~0 dBFS
        raw_power_scale: f32,      // db_raw's own calibration constant — see its own doc, and
                                    // gaussian_power's ("A FOURTH correction"), for why it's a
                                    // different constant from power_scale (S2/energetic-gain
                                    // Parseval calibration, not S1/coherent-gain single-bin)
        padded_size: usize,        // rate-only (see `new`'s own local of the same name) — kept as
                                    // a field, unlike that local, only because `reconfigure` needs
                                    // it again to recompute `raw_power_scale` on a tier change
        bin_hz: f32,               // Hz per linear (padded) FFT bin — uniform, unlike a log bin's
        harmonic_fold: Arc<AtomicBool>, // live-toggleable; see `find_peaks`'s own doc
        min_sep_octaves: f32,      // resolution-dependent PEAK_MIN_SEPARATION_OCTAVES(_HIGH_RES)
        max_range_db: f32,         // resolution-dependent PEAK_MAX_RANGE_DB(_HIGH_RES)
    }

    /// The tier-dependent half of `Spectrum::new`'s computation — everything that depends on
    /// `base_fft_size` (the FFT-size slider's tier) at a fixed `rate`. Shared by `Spectrum::new`
    /// and `Spectrum::reconfigure`, the latter of which live-changes exactly these fields without
    /// touching the FFT plan/`padded_size`/bin ranges — see that method's own doc for why those
    /// don't belong here (they're rate-only, not tier-dependent).
    struct TierParams {
        base_fft_size: usize, // snapped to the nearest real tier
        mult: usize,
        analysis_size: usize,
        fft_hop: usize,
        window: Vec<f32>,
        smoothing: f32,
        power_scale: f32,
        // S2 (energetic gain, Σw[n]²) — `db_raw`'s own calibration input, alongside the
        // tier-invariant `padded_size` neither `new` nor `reconfigure` have in scope here (see
        // `power_scale`'s own comment above). Combined into the actual `raw_power_scale` constant
        // by whichever of those two callers has `padded_size` on hand.
        s2: f32,
        min_sep_octaves: f32,
        max_range_db: f32,
    }

    impl Spectrum {
        /// `base_fft_size` is one of [`BASE_FFT_SIZE`]/[`MED_FFT_SIZE`]/[`HIGH_RES_FFT_SIZE`] — the
        /// app's 3-position FFT-size slider. Also selects which of `find_peaks`'s
        /// resolution-dependent gates apply (anything above `BASE_FFT_SIZE` counts as "high res"
        /// there — see `high_res` below), the same way it already selects the FFT size itself.
        fn tier_params(rate: u32, base_fft_size: usize) -> TierParams {
            // Snap to the nearest of the three real tiers — defensive against whatever crosses the
            // Tauri IPC boundary (the frontend's slider only ever sends one of the three exactly,
            // but this is the one place that assumption would actually matter: an arbitrary huge
            // value here means an arbitrarily huge FFT plan/allocation in `Spectrum::new`).
            let base_fft_size = [BASE_FFT_SIZE, MED_FFT_SIZE, HIGH_RES_FFT_SIZE]
                .into_iter()
                .min_by_key(|&sz| (sz as i64 - base_fft_size as i64).abs())
                .unwrap();
            // Scale the real analysis window up with the rate so its *duration* (≈171 ms at the
            // default base size) stays constant: analysis_size = base × next_pow2(round(rate /
            // 48 kHz)). At BASE_FFT_SIZE: 48 k→8192, 96 k→16384, 192 k→32768 (44.1/88.2/176.4
            // round to the same multiples). Keeps the true (unpadded) resolution identical across
            // devices instead of coarsening at high rates.
            let mult = ((rate as f32 / BASE_RATE).round().max(1.0) as usize).next_power_of_two();
            let analysis_size = base_fft_size * mult;
            let fft_hop = analysis_size / FFT_OVERLAP_DIV;
            // Windows only the real (analysis_size) portion — the padding is zeros regardless of
            // any window coefficient, so extending the window formula over it would be dead work.
            let window: Vec<f32> = (0..analysis_size)
                .map(|n| {
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / analysis_size as f32).cos()
                })
                .collect();
            let smoothing = 1.0 - (-(fft_hop as f32 / rate as f32) / SPEC_TAU_SECS).exp();
            // Amplitude normalization: a full-scale sine at a bin centre gives |X| = S1/2 (window
            // coherent gain), so multiply the one-sided amplitude by 2/S1 to read 1.0 → 0 dBFS. In
            // the power domain that's (2/S1)². S1 = sum(window) — unaffected by the zero padding
            // (zeros contribute nothing to the sum either way), so this is still exactly the real
            // (analysis_size) window's own coherent gain, FFT-size independent as before.
            let s1: f32 = window.iter().sum();
            let power_scale = (2.0 / s1).powi(2);
            // Input to `db_raw`'s own calibration constant (`raw_power_scale`, computed by `new`/
            // `reconfigure` once `padded_size` is in scope — see `TierParams::s2`'s own doc) — see
            // `gaussian_power`'s doc ("A FOURTH correction") for why it needs S2 (energetic gain,
            // `Σw[n]²`) rather than S1 (coherent gain) above: derived from Parseval's theorem
            // rather than the single-peak-bin relation `power_scale` is built on.
            let s2: f32 = window.iter().map(|w| w * w).sum();
            // Medium and High both count as "high res" for tuning purposes — only Base gets the
            // tighter defaults. `>`, not `== HIGH_RES_FFT_SIZE`, so this doesn't need updating if
            // another tier is ever added.
            let high_res = base_fft_size > BASE_FFT_SIZE;
            let min_sep_octaves =
                if high_res { PEAK_MIN_SEPARATION_OCTAVES_HIGH_RES } else { PEAK_MIN_SEPARATION_OCTAVES };
            let max_range_db =
                if high_res { PEAK_MAX_RANGE_DB_HIGH_RES } else { PEAK_MAX_RANGE_DB };
            TierParams {
                base_fft_size,
                mult,
                analysis_size,
                fft_hop,
                window,
                smoothing,
                power_scale,
                s2,
                min_sep_octaves,
                max_range_db,
            }
        }

        /// `base_fft_size` is one of [`BASE_FFT_SIZE`]/[`MED_FFT_SIZE`]/[`HIGH_RES_FFT_SIZE`] — the
        /// app's 3-position FFT-size slider, passed straight through — see those constants' own
        /// docs for what a larger one trades away. `harmonic_fold` is shared with the Tauri layer
        /// (`HarmonicFoldState`) so toggling it takes effect immediately, no restart — same as
        /// `base_fft_size` itself now, via `reconfigure` (see that method's own doc).
        fn new(rate: u32, base_fft_size: usize, harmonic_fold: Arc<AtomicBool>) -> Self {
            let tp = Self::tier_params(rate, base_fft_size);
            // The FFT is planned and run at a fixed total length — BASE_FFT_SIZE's own
            // ZERO_PAD_FACTOR×mult budget (see that constant's doc) — not analysis_size's own
            // ZERO_PAD_FACTOR× every time: at BASE_FFT_SIZE this is exactly analysis_size ×
            // ZERO_PAD_FACTOR as before (real + padding both scale with mult identically), but at
            // MED_FFT_SIZE/HIGH_RES_FFT_SIZE the larger real window increasingly eats into that
            // same fixed budget instead of multiplying it further — padding shrinks as real
            // resolution grows, rather than both growing together and interpolating the same
            // ~1.5 Hz-at-48kHz crossover redundantly. `max` covers HIGH_RES_FFT_SIZE, whose real
            // window already meets (not exceeds) the budget, i.e. zero padding, not negative.
            // Still always a power of two (every term is), so this transform is exactly as cheap
            // per-point as an unpadded one of the same total length, same as before. Rate-only —
            // unlike everything in `TierParams`, never changes on a `reconfigure`.
            let padded_size = tp.analysis_size.max(BASE_FFT_SIZE * ZERO_PAD_FACTOR * tp.mult);
            // `db_raw`'s calibration constant (see its own doc, and `gaussian_power`'s "A FOURTH
            // correction"): Parseval's theorem relates a windowed tone's *total* power, summed
            // across the *whole* (padded) DFT's one-sided bins, to `S2 = Σw[n]²` — not `S1` —
            // giving `amplitude² ≈ (4 / (padded_size × S2)) × Σ|X[k]|²` (excluding the negligible
            // DC/Nyquist edge terms for a tone away from them). This is a different derivation
            // from `power_scale`'s own single-peak-bin coherent-gain relation just above, and a
            // different constant — reusing `power_scale` for a *summed* (not single-bin) reading
            // is exactly what overshot when first tried (see git history/commit message for the
            // measured bias this fixes).
            let raw_power_scale = 4.0 / (padded_size as f32 * tp.s2);

            let fft = RealFftPlanner::<f32>::new().plan_fft_forward(padded_size);
            let in_buf = fft.make_input_vec();
            let out_buf = fft.make_output_vec();
            let scratch = fft.make_scratch_vec();

            let n_lin = padded_size / 2 + 1;
            let bin_hz = rate as f32 / padded_size as f32;
            let ratio = (SPEC_F_MAX / SPEC_F_MIN).powf(1.0 / (N_LOG_BINS as f32 - 1.0));
            let half = ratio.sqrt();
            // Exact (fractional) linear-bin boundaries, deliberately NOT rounded to integers —
            // `gaussian_power` (below) derives each bin's Gaussian centre and width directly from
            // these, so an integer-rounded boundary would just reintroduce the same "bin count
            // jumps as the true fractional width crosses an integer" artifact the Gaussian exists
            // to avoid (see that function's doc).
            let mut ranges = Vec::with_capacity(N_LOG_BINS);
            for i in 0..N_LOG_BINS {
                let fc = SPEC_F_MIN * ratio.powi(i as i32);
                let lo = ((fc / half) / bin_hz).max(0.0);
                let hi = ((fc * half) / bin_hz).min((n_lin - 1) as f32);
                ranges.push((lo, hi));
            }
            // Same idea, constant-Hz width instead of constant-percentage: each bin owns the
            // half-width window around its own evenly-spaced centre.
            let lin_bin_width = (SPEC_F_MAX - SPEC_F_MIN) / (N_LIN_BINS as f32 - 1.0);
            let mut ranges_lin = Vec::with_capacity(N_LIN_BINS);
            for i in 0..N_LIN_BINS {
                let fc = SPEC_F_MIN + lin_bin_width * i as f32;
                let lo = ((fc - lin_bin_width / 2.0) / bin_hz).max(0.0);
                let hi = ((fc + lin_bin_width / 2.0) / bin_hz).min((n_lin - 1) as f32);
                ranges_lin.push((lo, hi));
            }

            Spectrum {
                fft,
                base_fft_size: tp.base_fft_size,
                analysis_size: tp.analysis_size,
                fft_hop: tp.fft_hop,
                in_buf,
                out_buf,
                scratch,
                window: tp.window,
                accum: VecDeque::new(),
                avg_power: vec![0.0; n_lin],
                ranges,
                ranges_lin,
                smoothing: tp.smoothing,
                power_scale: tp.power_scale,
                raw_power_scale,
                padded_size,
                bin_hz,
                harmonic_fold,
                min_sep_octaves: tp.min_sep_octaves,
                max_range_db: tp.max_range_db,
            }
        }

        /// Live-reconfigure the analysis window when the shared fft-size tier changes — called
        /// every read from `run_session`'s main loop (see `capture_loop`). Unlike a rate change
        /// (which invalidates the whole WASAPI session and rebuilds `Spectrum` from scratch), a
        /// tier change never touches the FFT plan, `padded_size`, or the bin `ranges`/`ranges_lin`
        /// (all rate-only, not tier-dependent — see `padded_size`'s own doc in `new`), so this only
        /// replaces the real-window-derived fields from `TierParams`. `avg_power` is left as-is:
        /// its bins mean the same thing either way, so the running average just blends across the
        /// change instead of resetting.
        fn reconfigure(&mut self, rate: u32, base_fft_size: usize) {
            let snapped = [BASE_FFT_SIZE, MED_FFT_SIZE, HIGH_RES_FFT_SIZE]
                .into_iter()
                .min_by_key(|&sz| (sz as i64 - base_fft_size as i64).abs())
                .unwrap();
            if snapped == self.base_fft_size {
                return; // already at the requested tier — the common case, checked on every read
            }
            let tp = Self::tier_params(rate, snapped);
            self.base_fft_size = tp.base_fft_size;
            self.analysis_size = tp.analysis_size;
            self.fft_hop = tp.fft_hop;
            self.window = tp.window;
            self.smoothing = tp.smoothing;
            self.power_scale = tp.power_scale;
            // padded_size is tier-invariant (unlike everything else here) so self.padded_size,
            // set once in `new`, is still correct — only S2 (from the new tier's own window)
            // changed. See `new`'s own comment on `raw_power_scale` for the formula.
            self.raw_power_scale = 4.0 / (self.padded_size as f32 * tp.s2);
            self.min_sep_octaves = tp.min_sep_octaves;
            self.max_range_db = tp.max_range_db;
        }

        /// Feed mono samples; runs an FFT for every full hop and folds it into the running average.
        fn push(&mut self, mono: &[f32]) {
            self.accum.extend(mono.iter().copied());
            while self.accum.len() >= self.analysis_size {
                for (i, w) in self.window.iter().enumerate() {
                    self.in_buf[i] = self.accum[i] * w;
                }
                // The zero-padded tail. realfft documents `process`/`process_with_scratch`'s input
                // as "garbage after calling" (it's reused as working space), so this can't be
                // zeroed once at init and trusted to stay that way — re-zeroed every hop instead.
                // Cheap: FFT_OVERLAP_DIV=4 means this runs at most 4x/hop-worth of audio, and it's
                // a plain fill, not per-sample work.
                self.in_buf[self.analysis_size..].fill(0.0);
                if self
                    .fft
                    .process_with_scratch(&mut self.in_buf, &mut self.out_buf, &mut self.scratch)
                    .is_ok()
                {
                    let a = self.smoothing;
                    for (avg, c) in self.avg_power.iter_mut().zip(self.out_buf.iter()) {
                        *avg += a * (c.norm_sqr() - *avg);
                    }
                }
                // One bulk removal instead of `fft_hop` individual `pop_front` calls — same net
                // effect (each call's own bookkeeping — wraparound index math, capacity checks —
                // repeated per element adds up at the larger window tiers, especially unoptimized;
                // `drain` does it once). The `Drain` iterator removes its whole range on drop even
                // though its yielded items are never read here.
                self.accum.drain(..self.fft_hop);
            }
        }

        /// Fade the stored power toward the floor — called on an emit with no fresh audio, so the
        /// spectrum decays during silence instead of freezing at its last values.
        fn decay_idle(&mut self) {
            for p in self.avg_power.iter_mut() {
                *p *= SPEC_IDLE_DECAY;
            }
        }

        /// Collapse `avg_power` onto `ranges` (dB) — shared by both the log and linear axis
        /// reductions in `snapshot`, which differ only in how `ranges` itself was built
        /// (`Spectrum::new`'s `ranges` vs `ranges_lin`); this half doesn't know or care which.
        /// Returns `(db, db_raw, peak_db)` — see `SpectrumUpdate::db_raw`'s own doc for what it is
        /// and why it's worth a second field alongside `db`.
        fn reduce(avg_power: &[f32], power_scale: f32, raw_power_scale: f32, ranges: &[(f32, f32)]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
            let mut db = Vec::with_capacity(ranges.len());
            let mut db_raw = Vec::with_capacity(ranges.len());
            let mut peak_db = Vec::with_capacity(ranges.len());
            for &(lo, hi) in ranges {
                // Gaussian-weighted integral of linear power across the bin's own span, not
                // the single loudest sample in it — see `gaussian_power`'s doc for the full case
                // history (this replaced `.fold(max)`, then a rectangular/boxcar sum, in that
                // order; both are in git history if the reasoning against either is ever needed
                // again). Verified against synthetic ground truth before landing here — see
                // `cageq-monitor/tests/decimation_spike.rs`, kept.
                let (power, raw_power) = gaussian_power(avg_power, lo, hi);
                let cur = if power > 0.0 {
                    (10.0 * (power * power_scale).log10()).max(SPEC_FLOOR)
                } else {
                    SPEC_FLOOR
                };
                db.push(round_to(cur, 1));

                // `db_raw`: the pre-normalization weighted sum `gaussian_power` computed on the
                // way to its own density average — see that function's own doc ("A FOURTH
                // correction") for why this, calibrated with `raw_power_scale` (Parseval/S2-based
                // — see `Spectrum::new`'s own comment on it), not `power_scale`: `power_scale` is
                // built for reading a *single* peak bin (`peak_db`/`db` both use it correctly for
                // that), and reusing it for a *summed* quantity like this one overshoots by
                // several dB for a concentrated source — measured live against `peak_db` at the
                // same bin before this was corrected.
                let raw_cur = if raw_power > 0.0 {
                    (10.0 * (raw_power * raw_power_scale).log10()).max(SPEC_FLOOR)
                } else {
                    SPEC_FLOOR
                };
                db_raw.push(round_to(raw_cur, 1));

                // Same span, but the true (undiluted) level — see `peak_db`'s own doc on
                // `SpectrumUpdate` for why this is a second reduction rather than reusing `power`.
                let pk = max_power(avg_power, lo, hi);
                let pk_cur = if pk > 0.0 {
                    (10.0 * (pk * power_scale).log10()).max(SPEC_FLOOR)
                } else {
                    SPEC_FLOOR
                };
                peak_db.push(round_to(pk_cur, 1));
            }
            (db, db_raw, peak_db)
        }

        /// Collapse the averaged power onto both the log and linear display bins (dB).
        fn snapshot(&mut self, signal: bool) -> SpectrumUpdate {
            let (db, db_raw, peak_db) =
                Self::reduce(&self.avg_power, self.power_scale, self.raw_power_scale, &self.ranges);
            let (db_lin, db_lin_raw, peak_db_lin) =
                Self::reduce(&self.avg_power, self.power_scale, self.raw_power_scale, &self.ranges_lin);
            let peaks = find_peaks(
                &self.avg_power,
                self.power_scale,
                self.bin_hz,
                self.harmonic_fold.load(Ordering::Relaxed),
                self.min_sep_octaves,
                self.max_range_db,
            );
            SpectrumUpdate {
                db,
                peak_db,
                db_raw,
                db_lin,
                peak_db_lin,
                db_lin_raw,
                signal,
                f_min: SPEC_F_MIN,
                f_max: SPEC_F_MAX,
                peaks,
            }
        }
    }

    /// Resolve the app's endpoint id (registry GUID) to a WASAPI render device, else default.
    fn resolve_device(
        enumerator: &DeviceEnumerator,
        endpoint_id: &Option<String>,
    ) -> Result<Device, Box<dyn Error>> {
        if let Some(id) = endpoint_id {
            let want = id.to_ascii_lowercase();
            for dev in &enumerator.get_device_collection(&Direction::Render)? {
                let dev = dev?;
                if let Ok(dev_id) = dev.get_id() {
                    if dev_id.to_ascii_lowercase().contains(&want) {
                        return Ok(dev);
                    }
                }
            }
        }
        Ok(enumerator.get_default_device(&Direction::Render)?)
    }

    /// The default render endpoint's id — for the deep-link default check. Own-thread COM init
    /// (see the public wrapper) so it's safe to call from any Tauri command thread.
    pub(super) fn default_render_id() -> Option<String> {
        let _ = initialize_mta(); // fresh thread → no COM yet; ignore (enumerator fails to None if not)
        let enumerator = DeviceEnumerator::new().ok()?;
        let dev = enumerator.get_default_device(&Direction::Render).ok()?;
        dev.get_id().ok()
    }


    /// Minimal 32-bit-float WAV writer, for the loopback recorder below.
    ///
    /// Hand-rolled rather than pulling in a crate: it is a 44-byte header and raw samples, and
    /// this exists to settle arguments about what the audio actually did — a dependency for
    /// that would be out of proportion.
    struct WavRecorder {
        file: std::fs::File,
        bytes: u32,
        channels: u16,
        rate: u32,
    }

    impl WavRecorder {
        fn new(path: &std::path::Path, rate: u32, channels: u16) -> std::io::Result<WavRecorder> {
            use std::io::Write;
            let mut file = std::fs::File::create(path)?;
            // Sizes are patched in `finish`; zeros for now.
            file.write_all(&[0u8; 44])?;
            Ok(WavRecorder { file, bytes: 0, channels, rate })
        }

        fn write(&mut self, interleaved: &[f32]) {
            use std::io::Write;
            // A recorder that kills the capture thread would be worse than no recorder, so a
            // write failure is dropped rather than propagated onto the audio path.
            let mut buf = Vec::with_capacity(interleaved.len() * 4);
            for s in interleaved {
                buf.extend_from_slice(&s.to_le_bytes());
            }
            if self.file.write_all(&buf).is_ok() {
                self.bytes = self.bytes.saturating_add(buf.len() as u32);
            }
        }

        fn finish(mut self) {
            use std::io::{Seek, SeekFrom, Write};
            let block_align = self.channels * 4;
            let mut h = Vec::with_capacity(44);
            h.extend_from_slice(b"RIFF");
            h.extend_from_slice(&(36 + self.bytes).to_le_bytes());
            h.extend_from_slice(b"WAVEfmt ");
            h.extend_from_slice(&16u32.to_le_bytes());
            h.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
            h.extend_from_slice(&self.channels.to_le_bytes());
            h.extend_from_slice(&self.rate.to_le_bytes());
            h.extend_from_slice(&(self.rate * block_align as u32).to_le_bytes());
            h.extend_from_slice(&block_align.to_le_bytes());
            h.extend_from_slice(&32u16.to_le_bytes()); // bits per sample
            h.extend_from_slice(b"data");
            h.extend_from_slice(&self.bytes.to_le_bytes());
            let _ = self.file.seek(SeekFrom::Start(0));
            let _ = self.file.write_all(&h);
            let _ = self.file.flush();
        }
    }
    fn capture_loop<F, G, H>(
        endpoint_id: Option<String>,
        stop: &AtomicBool,
        scope_viewers: &AtomicUsize,
        fft_size: Arc<AtomicUsize>,
        harmonic_fold: Arc<AtomicBool>,
        reset_lufs: Arc<AtomicBool>,
        on_update: F,
        on_spectrum: G,
        on_scope: H,
    ) -> Result<(), Box<dyn Error>>
    where
        F: Fn(MeterUpdate),
        G: Fn(SpectrumUpdate),
        H: Fn(ScopeUpdate),
    {
        initialize_mta().ok()?;

        // Supervisor: (re)open the endpoint and meter until it invalidates or we're told to stop.
        // A shared-mode format change (sample-rate switch, an exclusive-mode app grabbing the
        // device, a default-device change) invalidates the capture client — WASAPI surfaces that
        // as an error from the next read. Rather than ending the thread (which froze the meter
        // until the user toggled it off/on), tear down and reopen: get_mixformat re-reads the new
        // rate and the loudness state is rebuilt for it, so a rate change self-heals.
        while !stop.load(Ordering::Relaxed) {
            if let Err(e) = run_session(
                &endpoint_id,
                stop,
                scope_viewers,
                &fft_size,
                &harmonic_fold,
                &reset_lufs,
                &on_update,
                &on_spectrum,
                &on_scope,
            ) {
                eprintln!("[cageq-monitor] reopening capture after: {e}");
                // Show the UI an idle state during the gap, then back off before reopening. A
                // reopen already rebuilds the ebur128 instance from scratch (see run_session), so
                // Integrated/LRA/Peak Max reset here too — same floor as the fresh-instance state.
                on_update(MeterUpdate {
                    peak_db: DB_FLOOR,
                    rms_db: DB_FLOOR,
                    momentary_lufs: LUFS_FLOOR,
                    short_term_lufs: LUFS_FLOOR,
                    integrated_lufs: LUFS_FLOOR,
                    loudness_range: 0.0,
                    true_peak_max_db: DB_FLOOR,
                    signal: false,
                    bins: Vec::new(),
                    sample_rate: 0, // unknown until the session reopens and re-reads the mix format
                });
                on_scope(ScopeUpdate { xy: Vec::new(), signal: false, rate: 0 });
                sleep_unless_stopped(stop, Duration::from_millis(500));
            }
        }
        Ok(())
    }

    /// Take the most recent (contiguous) `max_pairs` from interleaved `(l, r)` data, order intact —
    /// so consecutive samples stay adjacent and the front-end can join them into the beam trace.
    /// No decimation (that would break oscilloscope-music figures); an already-small window passes
    /// through whole.
    fn tail_pairs(lr: &[f32], max_pairs: usize) -> Vec<f32> {
        let pairs = lr.len() / 2;
        if pairs == 0 || max_pairs == 0 {
            return Vec::new();
        }
        let take = pairs.min(max_pairs);
        lr[(pairs - take) * 2..pairs * 2].to_vec()
    }

    /// Sleep up to `dur`, returning early if a stop is requested.
    fn sleep_unless_stopped(stop: &AtomicBool, dur: Duration) {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// One capture session: open loopback on the (re-resolved) endpoint at its current mix format
    /// and meter until stopped (returns `Ok`) or the stream errors/invalidates (returns `Err`, so
    /// the supervisor reopens — e.g. after a sample-rate change).
    fn run_session<F, G, H>(
        endpoint_id: &Option<String>,
        stop: &AtomicBool,
        scope_viewers: &AtomicUsize,
        fft_size: &Arc<AtomicUsize>,
        harmonic_fold: &Arc<AtomicBool>,
        reset_lufs: &Arc<AtomicBool>,
        on_update: &F,
        on_spectrum: &G,
        on_scope: &H,
    ) -> Result<(), Box<dyn Error>>
    where
        F: Fn(MeterUpdate),
        G: Fn(SpectrumUpdate),
        H: Fn(ScopeUpdate),
    {
        let enumerator = DeviceEnumerator::new()?;
        let device = resolve_device(&enumerator, endpoint_id)?;
        let mut audio_client = device.get_iaudioclient()?;

        // Capture at the endpoint's shared-mode mix rate/channels (capped — see
        // CAPTURE_RATE_CAP's own doc), but ask for f32 with autoconvert so we always parse a
        // known sample type. `device_rate` is the endpoint's real, uncapped rate — kept
        // separately so `MeterUpdate.sample_rate` can still report Windows' actual configured
        // rate to the UI (its own doc's whole point: "match your device sample rate to the
        // source sample rate") rather than silently reporting the internal analysis cap instead.
        let mix = audio_client.get_mixformat()?;
        let device_rate = mix.get_samplespersec();
        let rate = device_rate.min(CAPTURE_RATE_CAP);
        let channels = mix.get_nchannels();
        let desired =
            WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
        let bytes_per_frame = desired.get_blockalign() as usize;

        let mode =
            StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: safe_buffer_hns(&audio_client)? };
        audio_client.initialize_client(&desired, &Direction::Capture, &mode)?;
        let capture = audio_client.get_audiocaptureclient()?;

        // K-weighted loudness at the *actual* endpoint rate (ebur128 recomputes coefficients).
        // TRUE_PEAK (which subsumes SAMPLE_PEAK's own bits) also gets us BS.1770 oversampled true
        // peak "for free" off the same instance — see block_peak's own doc below for why that
        // replaced a plain sample-peak scan. I | LRA add Integrated loudness and EBU Tech 3342
        // Loudness Range — both real gated statistics `ebur128` already implements, not
        // reimplemented here; `TRUE_PEAK` (already needed above) also gives Peak Max "for free"
        // via `EbuR128::true_peak`'s own running maximum, no extra mode needed for that one.
        let mut ebu = ebur128::EbuR128::new(
            channels as u32,
            rate,
            ebur128::Mode::M | ebur128::Mode::S | ebur128::Mode::TRUE_PEAK | ebur128::Mode::I | ebur128::Mode::LRA,
        )
        .map_err(|e| format!("ebur128 init: {e:?}"))?;
        // A fresh instance has nothing to reset; drop any stale request left over from before this
        // session opened (e.g. a reset clicked while the endpoint was mid-reopen) rather than
        // firing it pointlessly on the first tick below.
        reset_lufs.store(false, Ordering::Relaxed);

        let mut spectrum = Spectrum::new(rate, fft_size.load(Ordering::Relaxed), harmonic_fold.clone());

        audio_client.start_stream()?;

        // Raw loopback recorder, off unless CAGEQ_RECORD names a file. Captures exactly what
        // the endpoint produced, post-EQ, sample for sample — the only way to settle what a
        // transition actually did rather than what a synthetic model says it should have.
        // Deliberately env-gated: it costs a running app nothing and cannot destabilise the
        // monitor for anyone not asking for it.
        let mut recorder = std::env::var_os("CAGEQ_RECORD").and_then(|p| {
            let path = std::path::PathBuf::from(p);
            match WavRecorder::new(&path, rate, channels as u16) {
                Ok(r) => {
                    eprintln!("[cageq-monitor] recording loopback to {}", path.display());
                    Some(r)
                }
                Err(e) => {
                    eprintln!("[cageq-monitor] could not record to {}: {e}", path.display());
                    None
                }
            }
        });

        let mut queue: VecDeque<u8> = VecDeque::new();
        let mut frames: Vec<f32> = Vec::new(); // interleaved, reused each read for ebur128
        let mut mono: Vec<f32> = Vec::new(); // per-read mono downmix, fed to the FFT
        let mut scope_lr: Vec<f32> = Vec::new(); // (l, r) pairs since the last scope emit
        // Per-tick block accumulators (reset every emit). `block_peak` is BS.1770 true peak
        // (linear, can exceed 1.0 — i.e. positive dBTP — on real inter-sample overs), not a plain
        // sample-peak scan: a reconstruction filter can overshoot between samples on hot/limited
        // material, which a max-abs-of-samples scan can't see at all — ebur128's oversampled filter
        // (already running for the LUFS numbers on this same instance) catches it. Folded in per
        // read below rather than read once per tick, since `ebu.prev_true_peak` only covers the
        // frames from the single most recent `add_frames` call, and a tick can span several reads.
        let mut block_peak = 0.0f32;
        let mut block_sum_sq = 0.0f64;
        let mut block_count: u64 = 0;
        // Persistent, time-based meter ballistics (carried across ticks).
        let mut peak_db = DB_FLOOR; // held/decaying peak, dBFS
        let mut peak_hold_until = Instant::now();
        let mut rms_mean_sq = 0.0f64; // one-pole-smoothed mean square
        let mut intensity = vec![0.0f32; N_BINS]; // phosphor histogram brightness per segment
        let mut last_tick = Instant::now();
        let mut last_spectrum = Instant::now();
        let mut last_scope = Instant::now();
        let mut last_signal = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            let before = queue.len();
            capture.read_from_device_to_deque(&mut queue)?;
            let got_data = queue.len() > before;
            if got_data {
                last_signal = Instant::now();
            }
            // Only do the (heavier) vectorscope work when a scope view is actually open.
            let scope_on = scope_viewers.load(Ordering::Relaxed) > 0;
            if !scope_on && !scope_lr.is_empty() {
                scope_lr.clear();
            }

            frames.clear();
            mono.clear();
            while queue.len() >= bytes_per_frame {
                let mut frame_sum = 0.0f32;
                let mut ch0 = 0.0f32; // first channel (L) for the vectorscope
                let mut ch1 = 0.0f32; // second channel (R); stays == L for a mono endpoint
                for c in 0..channels {
                    let b = [
                        queue.pop_front().unwrap(),
                        queue.pop_front().unwrap(),
                        queue.pop_front().unwrap(),
                        queue.pop_front().unwrap(),
                    ];
                    let s = f32::from_le_bytes(b);
                    if c == 0 {
                        ch0 = s;
                        ch1 = s;
                    } else if c == 1 {
                        ch1 = s;
                    }
                    frames.push(s);
                    frame_sum += s;
                    block_sum_sq += (s as f64) * (s as f64);
                    block_count += 1;
                }
                mono.push(frame_sum / channels as f32);
                if scope_on {
                    scope_lr.push(ch0);
                    scope_lr.push(ch1);
                }
            }
            if let Some(r) = recorder.as_mut() {
                r.write(&frames);
            }

            if !frames.is_empty() && ebu.add_frames_f32(&frames).is_ok() {
                // This read's true peak (max(sample_peak, true_peak) internally — see
                // `prev_true_peak`'s own doc), folded into the tick-spanning accumulator above.
                for c in 0..channels as u32 {
                    if let Ok(tp) = ebu.prev_true_peak(c) {
                        let tp = tp as f32;
                        if tp > block_peak {
                            block_peak = tp;
                        }
                    }
                }
            }
            // Cheap no-op when the tier hasn't changed (see `reconfigure`'s own doc) — checked
            // unconditionally, even on a silent/empty read, so a slider drag takes effect
            // immediately regardless of whether audio happens to be playing.
            spectrum.reconfigure(rate, fft_size.load(Ordering::Relaxed));
            if !mono.is_empty() {
                spectrum.push(&mono);
            }

            if last_tick.elapsed() >= TICK {
                let dt = last_tick.elapsed().as_secs_f32();
                let now = Instant::now();

                // Peak: instant attack, brief hold, then linear-dB release — a PPM-style follower
                // that reads cleanly at 60 fps instead of flickering on a raw per-tick max.
                let block_peak_db = to_db(block_peak);
                if block_peak_db >= peak_db {
                    peak_db = block_peak_db;
                    peak_hold_until = now + PEAK_HOLD;
                } else if now >= peak_hold_until {
                    peak_db = (peak_db - PEAK_RELEASE_DB_PER_SEC * dt).max(DB_FLOOR);
                }

                // True RMS for the reference line: a plain symmetric integrator (VU-like). The
                // bar's glow and decay come from the phosphor histogram below, not from here.
                let block_ms = if block_count > 0 { block_sum_sq / block_count as f64 } else { 0.0 };
                let alpha = 1.0 - (-(dt as f64) / RMS_TAU_SECS).exp();
                rms_mean_sq += alpha * (block_ms - rms_mean_sq);

                // Phosphor histogram. Coverage level is mostly the block RMS (energy) — peaks sit
                // near 0 dBFS on most material and would saturate the bar, whereas energy shows
                // where the sound actually sits — with a little peak blended in so transients poke
                // up. Each segment's *target* brightness ramps with how far the level sits above it
                // (deeper = brighter), so the fill has real tonal range instead of a flat block;
                // segments hold their brightest recent value and decay from it (the afterglow), so
                // a passing peak leaves a fading trail above the current level.
                let rms_now_db = to_db(block_ms.sqrt() as f32);
                let cover_db = rms_now_db + PEAK_BLEND * (block_peak_db - rms_now_db);
                let decay = (-dt / PHOSPHOR_DECAY_SECS).exp();
                for (i, cell) in intensity.iter_mut().enumerate() {
                    let seg_db = BAR_MIN_DB * (1.0 - (i as f32 + 0.5) / N_BINS as f32);
                    let target = ((cover_db - seg_db) / GLOW_SPAN_DB).clamp(0.0, 1.0);
                    *cell = (*cell * decay).max(target);
                }

                // A pending reset (§ meter's "restart measurement" control) restarts Integrated/
                // Loudness Range/Peak Max together with momentary/short-term/true-peak filter
                // state — `EbuR128::reset()` is a full reset, so this takes effect for every field
                // below at once, on this very tick.
                if reset_lufs.swap(false, Ordering::Relaxed) {
                    ebu.reset();
                }

                on_update(MeterUpdate {
                    peak_db,
                    rms_db: to_db(rms_mean_sq.sqrt() as f32),
                    momentary_lufs: clamp_lufs(
                        ebu.loudness_momentary().unwrap_or(f64::NEG_INFINITY),
                    ),
                    short_term_lufs: clamp_lufs(
                        ebu.loudness_shortterm().unwrap_or(f64::NEG_INFINITY),
                    ),
                    integrated_lufs: clamp_lufs(ebu.loudness_global().unwrap_or(f64::NEG_INFINITY)),
                    loudness_range: clamp_lu(ebu.loudness_range().unwrap_or(f64::NAN)),
                    true_peak_max_db: to_db(
                        (0..channels as u32)
                            .map(|c| ebu.true_peak(c).unwrap_or(0.0) as f32)
                            .fold(0.0f32, f32::max),
                    ),
                    signal: last_signal.elapsed() < SILENCE_GAP,
                    bins: intensity.iter().map(|&v| round_to(v, 3)).collect(),
                    sample_rate: device_rate,
                });

                block_peak = 0.0;
                block_sum_sq = 0.0;
                block_count = 0;
                last_tick = now;
            }

            // Spectrum on its own (slower) cadence — the FFTs themselves ran in `push` above. Only
            // decay toward the floor on genuine silence (endpoint idle), not merely "no new hop
            // finished in the last SPECTRUM_INTERVAL" — the hop is ~43 ms (see fft_hop) and the emit
            // tick is 16 ms, so during perfectly normal continuous playback most ticks land between
            // hops; treating that gap as "idle" faded the display in a spurious sawtooth even with
            // real audio flowing. Silence detection reuses the same `last_signal`/SILENCE_GAP the
            // meter's own `signal` field uses just below, for the same "endpoint has gone idle" test.
            if last_spectrum.elapsed() >= SPECTRUM_INTERVAL {
                let signal = last_signal.elapsed() < SILENCE_GAP;
                if !signal {
                    spectrum.decay_idle();
                }
                on_spectrum(spectrum.snapshot(signal));
                last_spectrum = Instant::now();
            }

            // Vectorscope: emit the contiguous (l, r) window (see tail_pairs) — but only while a
            // scope view is open, so the heavier stream costs nothing when nobody's watching.
            if scope_on && last_scope.elapsed() >= SCOPE_INTERVAL {
                let signal = last_signal.elapsed() < SILENCE_GAP;
                on_scope(ScopeUpdate {
                    xy: if signal { tail_pairs(&scope_lr, SCOPE_MAX_POINTS) } else { Vec::new() },
                    signal,
                    rate,
                });
                scope_lr.clear();
                last_scope = Instant::now();
            }

            // Nothing ready — yield briefly so we don't spin a core polling an idle endpoint.
            if !got_data {
                thread::sleep(Duration::from_millis(5));
            }
        }

        // Patch the WAV header before anything else: a recording whose sizes were never
        // written back is a file no tool will open, wasting the whole capture.
        if let Some(r) = recorder.take() {
            r.finish();
            eprintln!("[cageq-monitor] recording closed");
        }

        let _ = audio_client.stop_stream();
        Ok(())
    }

    #[cfg(test)]
    mod spectrum_reduction {
        // A regression guard on the actual production `Spectrum` pipeline (not the isolated
        // `tests/decimation_spike.rs`, which validated the reduction *method* against synthetic
        // ground truth before it was ported here) — a pure tone's spectrum should be one smooth,
        // monotonic lobe on each side of its peak, with no bumps or reversals. This is exactly the
        // property `.fold(max)` and a plain boxcar sum each failed at some point this investigation
        // (see `gaussian_power`'s doc for the full case history) — asserted here, not just printed,
        // so a future change to the reduction that reintroduces either failure mode fails a test
        // instead of needing a live screenshot to notice again.
        use super::*;

        /// The whole point of `padded_size`'s fixed-budget formula (see its own doc): at a fixed
        /// sample rate, the FFT transform length — and so its CPU cost — must be *identical*
        /// across all three window tiers, not grow with them the way a flat `analysis_size ×
        /// ZERO_PAD_FACTOR` would. `avg_power.len() == padded_size/2 + 1`, so its length is a
        /// direct, inspectable proxy for `padded_size` from outside `Spectrum::new`.
        #[test]
        fn padded_transform_size_is_constant_across_tiers_at_a_fixed_rate() {
            let rate = 48_000u32;
            let fold = || Arc::new(AtomicBool::new(false));
            let base = Spectrum::new(rate, BASE_FFT_SIZE, fold()).avg_power.len();
            let med = Spectrum::new(rate, MED_FFT_SIZE, fold()).avg_power.len();
            let high = Spectrum::new(rate, HIGH_RES_FFT_SIZE, fold()).avg_power.len();
            assert_eq!(base, med, "Med should reuse Base's exact transform budget");
            assert_eq!(base, high, "High should reuse Base's exact transform budget too");
        }

        /// The budget itself still scales with the sample-rate `mult`, same as before this
        /// change — only the *tier* stopped growing the transform, not the rate.
        #[test]
        fn padded_transform_size_still_scales_with_sample_rate() {
            let fold = || Arc::new(AtomicBool::new(false));
            let at_48k = Spectrum::new(48_000, BASE_FFT_SIZE, fold()).avg_power.len();
            let at_96k = Spectrum::new(96_000, BASE_FFT_SIZE, fold()).avg_power.len();
            assert!(at_96k > at_48k, "96 kHz ({at_96k}) should budget more than 48 kHz ({at_48k})");
        }

        /// The whole point of the live-reconfigure fix: a tier change in place must move
        /// `analysis_size`/`window` to the new tier's values while leaving `avg_power.len()` (the
        /// `padded_size` proxy — see the tests above) untouched, i.e. no FFT replan happens.
        #[test]
        fn reconfigure_changes_tier_in_place_without_replanning_the_fft() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            let padded_before = spec.avg_power.len();
            assert_eq!(spec.analysis_size, BASE_FFT_SIZE);

            spec.reconfigure(rate, HIGH_RES_FFT_SIZE);
            assert_eq!(spec.analysis_size, HIGH_RES_FFT_SIZE, "tier should have taken effect");
            assert_eq!(spec.window.len(), HIGH_RES_FFT_SIZE);
            assert_eq!(spec.avg_power.len(), padded_before, "padded_size must not change");

            // Reconfiguring to the tier it's already at is a documented no-op.
            spec.reconfigure(rate, HIGH_RES_FFT_SIZE);
            assert_eq!(spec.analysis_size, HIGH_RES_FFT_SIZE);
            assert_eq!(spec.avg_power.len(), padded_before);

            spec.reconfigure(rate, BASE_FFT_SIZE);
            assert_eq!(spec.analysis_size, BASE_FFT_SIZE, "should be able to reconfigure back down");
            assert_eq!(spec.avg_power.len(), padded_before);
        }

        #[test]
        fn pure_tone_lobe_is_monotonic_each_side_of_peak() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            let freq = 60.0f32;
            let amp = 0.5f32;
            let total_samples = rate as usize * 2; // 2s — well past SPEC_TAU_SECS settling
            let mut phase = 0.0f64;
            let step = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
            let mut buf = vec![0.0f32; 4096];
            let mut fed = 0;
            while fed < total_samples {
                let n = buf.len().min(total_samples - fed);
                for s in buf.iter_mut().take(n) {
                    *s = amp * phase.sin() as f32;
                    phase += step;
                }
                spec.push(&buf[..n]);
                fed += n;
            }

            let update = spec.snapshot(true);
            let peak_i = update
                .db
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .map(|(i, _)| i)
                .unwrap();

            // Monotonically non-increasing moving outward from the peak, checked over a window
            // wide enough to cover the sidelobe-null region a boxcar/max reduction showed
            // artifacts in (±20 bins was comfortably past it in the original diagnosis).
            let window = 20usize;
            let mut prev = update.db[peak_i];
            for i in (peak_i.saturating_sub(window)..peak_i).rev() {
                assert!(
                    update.db[i] <= prev + 0.05, // small tolerance for floating-point/rounding noise
                    "non-monotonic on the falling-frequency side at bin {i}: {} > {prev} (peak at bin {peak_i})",
                    update.db[i]
                );
                prev = update.db[i];
            }
            prev = update.db[peak_i];
            for i in (peak_i + 1)..(peak_i + window).min(update.db.len()) {
                assert!(
                    update.db[i] <= prev + 0.05,
                    "non-monotonic on the rising-frequency side at bin {i}: {} > {prev} (peak at bin {peak_i})",
                    update.db[i]
                );
                prev = update.db[i];
            }
        }

        /// The reason `peak_db` exists at all: `db`'s density-normalized reduction genuinely
        /// dilutes an isolated tone at HF (the accepted ~19dB droop — see `gaussian_power`'s doc,
        /// and `decimation_spike.rs`'s `production_formula_tone_and_pink_noise_behavior` for the
        /// measured curve). A full-scale 12kHz tone is well up that droop; `max_power`'s job is to
        /// still report it at ~0dBFS despite `db` reporting it well below that at the same bin.
        #[test]
        fn peak_db_recovers_a_full_scale_tone_that_db_droops() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            let freq = 12_000.0f32;
            let amp = 1.0f32;
            let total_samples = rate as usize * 2; // 2s — well past SPEC_TAU_SECS settling
            let mut phase = 0.0f64;
            let step = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
            let mut buf = vec![0.0f32; 4096];
            let mut fed = 0;
            while fed < total_samples {
                let n = buf.len().min(total_samples - fed);
                for s in buf.iter_mut().take(n) {
                    *s = amp * phase.sin() as f32;
                    phase += step;
                }
                spec.push(&buf[..n]);
                fed += n;
            }

            let update = spec.snapshot(true);
            let peak_i = update
                .db
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .map(|(i, _)| i)
                .unwrap();

            // The droop this whole field exists to route around: `db` at the tone's own peak bin
            // reads well below 0dBFS despite the input being a full-scale sine.
            assert!(
                update.db[peak_i] < -10.0,
                "expected the documented HF droop in db at 12kHz, got {} dB — if this no longer \
                 droops, gaussian_power's tradeoff changed and peak_db's rationale should be \
                 re-checked, not just this assertion loosened",
                update.db[peak_i]
            );
            // peak_db at the same bin should read close to the true 0dBFS level instead — a
            // generous tolerance covers residual scalloping loss, not the ~10dB+ of density
            // dilution `db` shows at this frequency.
            assert!(
                update.peak_db[peak_i] > -1.5,
                "peak_db should recover the true tone level near 0dBFS, got {} dB (db read {} dB \
                 at the same bin)",
                update.peak_db[peak_i],
                update.db[peak_i]
            );
        }

        /// The actual claim behind moving Tilt server-side (see `SpectrumUpdate::db_raw`'s own
        /// doc): the Parseval/S2-calibrated Gaussian-weighted sum recovers a swept tone's level
        /// without `db`'s dilution droop — unlike a fixed additive dB/octave slope (the frontend
        /// approximation this replaced, which couldn't correct a non-slope dilution effect) or
        /// re-scaling the average by the bin's own nominal span (the first server-side attempt,
        /// which reused `power_scale` — a single-peak-bin calibration — for a summed quantity and
        /// measured a systematic +5 to +7dB overshoot live before this was corrected). Checked at
        /// several frequencies, not just one: the S1-vs-S2 bug this pins was a near-constant
        /// offset, which a single-frequency test could pass by accident if the tolerance happened
        /// to swallow it.
        #[test]
        fn db_raw_recovers_a_swept_tone_without_dbs_dilution_droop() {
            let rate = 48_000u32;
            for freq in [200.0f32, 1000.0, 3000.0, 6000.0, 12000.0, 18000.0] {
                let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
                let amp = 1.0f32;
                let total_samples = rate as usize * 2;
                let mut phase = 0.0f64;
                let step = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
                let mut buf = vec![0.0f32; 4096];
                let mut fed = 0;
                while fed < total_samples {
                    let n = buf.len().min(total_samples - fed);
                    for s in buf.iter_mut().take(n) {
                        *s = amp * phase.sin() as f32;
                        phase += step;
                    }
                    spec.push(&buf[..n]);
                    fed += n;
                }

                let update = spec.snapshot(true);
                let peak_i = update
                    .db
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                    .map(|(i, _)| i)
                    .unwrap();

                if freq >= 1000.0 {
                    assert!(
                        update.db[peak_i] < -5.0,
                        "expected db to still show a real HF droop at {freq}Hz, got {} dB",
                        update.db[peak_i]
                    );
                }
                // Tight, two-sided: db_raw should land close to peak_db's own true-0dBFS reading
                // at every frequency, not just clear some generous one-sided floor — an S1/S2
                // calibration mistake reads as a systematic offset in exactly this check.
                let diff = (update.db_raw[peak_i] - update.peak_db[peak_i]).abs();
                assert!(
                    diff < 2.0,
                    "db_raw should track peak_db closely at {freq}Hz — db_raw {} dB, peak_db {} \
                     dB, diff {diff:.2} dB (db read {} dB at the same bin)",
                    update.db_raw[peak_i],
                    update.peak_db[peak_i],
                    update.db[peak_i]
                );
            }
        }

        /// The other half of the same claim: `db_raw` should still read pink noise flat (the
        /// pre-density-normalization, conventional-RTA behavior it's reviving on purpose — see
        /// `SpectrumUpdate::db_raw`'s own doc), even though it now also fixes tones. Measured as
        /// the *slope* of a linear fit against bin index, not the raw spread (max-min): pink
        /// noise is genuine random noise, so individual bins carry real sample-to-sample variance
        /// on top of the systematic tilt this test actually cares about, and — since `db_raw` is
        /// just `db`'s own per-bin power rescaled by a *deterministic* per-bin span — that random
        /// component is identical in both, only the systematic slope differs. A spread-based check
        /// would conflate the two and wash out the improvement; the slope isolates it.
        #[test]
        fn db_raw_reads_pink_noise_flatter_than_db() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            let mut noise = crate::signal::PinkNoise::new();
            let total_samples = rate as usize * 4; // longer settle — broadband, not a single tone
            let mut buf = vec![0.0f32; 4096];
            let mut fed = 0;
            while fed < total_samples {
                let n = buf.len().min(total_samples - fed);
                for s in buf.iter_mut().take(n) {
                    *s = noise.next_pink() * 0.5;
                }
                spec.push(&buf[..n]);
                fed += n;
            }

            let update = spec.snapshot(true);
            // Mid-range bins only (roughly 100Hz-10kHz at this SPEC_F_MIN/F_MAX span) — comfortably
            // away from the floor at either end, where a pink source has real, settled energy.
            let lo = update.db.len() / 6;
            let hi = update.db.len() * 5 / 6;
            // Ordinary-least-squares slope of `v[lo..hi]` against its own bin index — bins are
            // log-spaced at a constant frequency ratio, so a constant dB/octave tilt is already a
            // constant dB/*bin* slope here, no log-frequency conversion needed.
            let slope = |v: &[f32]| -> f32 {
                let seg = &v[lo..hi];
                let n = seg.len() as f32;
                let xbar = (n - 1.0) / 2.0;
                let ybar = seg.iter().sum::<f32>() / n;
                let mut sxy = 0.0f32;
                let mut sxx = 0.0f32;
                for (i, &y) in seg.iter().enumerate() {
                    let x = i as f32 - xbar;
                    sxy += x * (y - ybar);
                    sxx += x * x;
                }
                sxy / sxx
            };
            let db_slope = slope(&update.db);
            let raw_slope = slope(&update.db_raw);
            assert!(db_slope < -0.05, "expected db to show a real declining tilt, got slope {db_slope:.4} dB/bin");
            // A generous fraction, not near-zero: pink noise is genuine random noise, so the
            // regression itself carries sampling scatter on top of whatever systematic tilt
            // remains — this only needs to show db_raw's tilt is clearly, substantially smaller,
            // not that it's a perfect zero.
            assert!(
                raw_slope.abs() < db_slope.abs() * 0.5,
                "db_raw's tilt should be substantially smaller than db's — db slope {db_slope:.4} dB/bin, \
                 db_raw slope {raw_slope:.4} dB/bin"
            );
        }
    }

    #[cfg(test)]
    mod peak_finding {
        use super::*;

        /// Feed `spec` a full-scale sine at `freq` Hz for 2s (well past `SPEC_TAU_SECS`
        /// settling), matching `spectrum_reduction`'s own harness.
        fn feed_tone(spec: &mut Spectrum, rate: u32, freq: f32) {
            let total_samples = rate as usize * 2;
            let mut phase = 0.0f64;
            let step = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
            let mut buf = vec![0.0f32; 4096];
            let mut fed = 0;
            while fed < total_samples {
                let n = buf.len().min(total_samples - fed);
                for s in buf.iter_mut().take(n) {
                    *s = phase.sin() as f32;
                    phase += step;
                }
                spec.push(&buf[..n]);
                fed += n;
            }
        }

        /// The actual regression case: a tone at a frequency with no special relationship to the
        /// bin grid (not a "nice" round number). The bug this whole feature fixes only shows up
        /// at the top of the spectrum, where the old log-bin approach's fractional-bin error
        /// translated to hundreds of Hz — this asserts the refined frequency lands within a few
        /// linear-bin-widths of the truth instead.
        #[test]
        fn an_awkward_high_frequency_tone_is_found_accurately() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            let freq = 15_437.0f32;
            feed_tone(&mut spec, rate, freq);

            let peaks = spec.snapshot(true).peaks;
            assert_eq!(peaks.len(), 1, "expected exactly one peak, got {peaks:?}");
            let bin_hz = rate as f32 / (BASE_FFT_SIZE as f32 * ZERO_PAD_FACTOR as f32);
            let err = (peaks[0].hz - freq).abs();
            assert!(
                err < bin_hz * 5.0,
                "peak reported at {} Hz, {err} Hz off {freq} Hz (bin width {bin_hz} Hz) — \
                 log-bin peak-finding used to be off by hundreds of Hz here",
                peaks[0].hz
            );
        }

        /// Same check at a plain, bin-grid-friendly frequency — a sanity baseline the awkward
        /// case above is compared against conceptually, not derived from.
        #[test]
        fn a_round_frequency_tone_is_found_accurately() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            let freq = 1_000.0f32;
            feed_tone(&mut spec, rate, freq);

            let peaks = spec.snapshot(true).peaks;
            assert_eq!(peaks.len(), 1, "expected exactly one peak, got {peaks:?}");
            let bin_hz = rate as f32 / (BASE_FFT_SIZE as f32 * ZERO_PAD_FACTOR as f32);
            assert!((peaks[0].hz - freq).abs() < bin_hz * 5.0);
        }

        #[test]
        fn true_silence_reports_no_peaks() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate, BASE_FFT_SIZE, Arc::new(AtomicBool::new(false)));
            spec.push(&vec![0.0f32; rate as usize * 2]);
            assert!(spec.snapshot(true).peaks.is_empty());
        }

        /// Below this, `find_peaks` operates directly on synthetic power arrays rather than a
        /// real `Spectrum` — precise control over exact bin values, for the algorithmic cases
        /// that don't need a real FFT to exercise (silence-gate math aside, already covered above
        /// against the real pipeline).
        const TEST_BIN_HZ: f32 = 10.0;

        /// Build a synthetic power spectrum of mostly-floor bins with isolated single-bin peaks
        /// at the given (bin index, dB) pairs — `power_scale = 1.0` throughout, so dB is exactly
        /// `10*log10(power)`.
        fn synth(n: usize, peaks_db: &[(usize, f32)]) -> Vec<f32> {
            let floor_db = -150.0f32;
            let mut v = vec![10f32.powf(floor_db / 10.0); n];
            for &(i, db) in peaks_db {
                v[i] = 10f32.powf(db / 10.0);
            }
            v
        }

        #[test]
        fn harmonic_folding_collapses_a_series_to_its_root() {
            // Root at bin 100 (1000 Hz) plus its 2nd/3rd/4th partials, all loud and prominent.
            // Near-zero minimum separation so this test isolates harmonic folding from the
            // (separately tested) octave-separation gate.
            let power = synth(2000, &[(100, -10.0), (200, -12.0), (300, -14.0), (400, -16.0)]);
            let folded = find_peaks(&power, 1.0, TEST_BIN_HZ, true, 0.001, 60.0);
            assert_eq!(folded.len(), 1, "harmonics should fold into the one root, got {folded:?}");
            assert!((folded[0].hz - 1000.0).abs() < TEST_BIN_HZ * 2.0);

            let unfolded = find_peaks(&power, 1.0, TEST_BIN_HZ, false, 0.001, 60.0);
            assert_eq!(unfolded.len(), 4, "with folding off, all four should stand alone");
        }

        #[test]
        fn peaks_closer_than_the_minimum_separation_are_dropped() {
            // Two peaks 0.1 octave apart (bins 100 and 107 ~ 1000/1070 Hz) — well inside a
            // 1-octave minimum separation, so only the louder one should survive.
            let power = synth(2000, &[(100, -10.0), (107, -20.0)]);
            let peaks = find_peaks(&power, 1.0, TEST_BIN_HZ, false, 1.0, 60.0);
            assert_eq!(peaks.len(), 1);
            assert!((peaks[0].hz - 1000.0).abs() < TEST_BIN_HZ * 2.0);
        }

        #[test]
        fn at_most_peak_count_peaks_are_reported() {
            // Eight well-separated, equally loud peaks (1 octave apart, right at the default
            // separation gate — none of them collide with it) — only PEAK_COUNT should come back.
            let peaks_db: Vec<(usize, f32)> = (0..8).map(|k| (100 * 2usize.pow(k), -10.0)).collect();
            let power = synth(20_000, &peaks_db);
            let found = find_peaks(&power, 1.0, TEST_BIN_HZ, false, 1.0, 60.0);
            assert_eq!(found.len(), PEAK_COUNT);
        }

        #[test]
        fn a_quiet_candidate_far_below_the_loudest_is_gated_out() {
            // Two peaks 45dB apart, past PEAK_MAX_RANGE_DB's default 30dB — only the loud one
            // should qualify.
            let power = synth(2000, &[(100, -10.0), (300, -55.0)]);
            let peaks = find_peaks(&power, 1.0, TEST_BIN_HZ, false, 1.0, PEAK_MAX_RANGE_DB);
            assert_eq!(peaks.len(), 1);
            assert!((peaks[0].hz - 1000.0).abs() < TEST_BIN_HZ * 2.0);
        }
    }

    /// First loudness-side tests in this crate — everything up to here only exercised
    /// `Spectrum`/`find_peaks`. Drives `ebur128::EbuR128` directly (no WASAPI needed, same
    /// "test the underlying machinery" approach `tests/decimation_spike.rs` already takes for
    /// `Spectrum`), so this is really a test of *this crate's own integration* (which mode flags
    /// are enabled, that a reset actually reaches every field) rather than re-proving `ebur128`'s
    /// own Tech 3342 math, which is out of scope here.
    #[cfg(test)]
    mod loudness {
        use super::*;

        const TEST_RATE: u32 = 48_000;

        /// A full-scale sine at `freq` Hz, `secs` long, interleaved mono-as-stereo (both channels
        /// identical) — `EbuR128` wants real frame data, not a bare mono stream, for a 2-channel
        /// instance. `gain` scales linear amplitude (1.0 = 0 dBFS).
        fn tone_frames(freq: f32, secs: f32, gain: f32) -> Vec<f32> {
            let n = (TEST_RATE as f32 * secs) as usize;
            let step = 2.0 * std::f64::consts::PI * freq as f64 / TEST_RATE as f64;
            let mut out = Vec::with_capacity(n * 2);
            for i in 0..n {
                let s = (gain as f64 * (step * i as f64).sin()) as f32;
                out.push(s);
                out.push(s);
            }
            out
        }

        fn new_ebu() -> ebur128::EbuR128 {
            ebur128::EbuR128::new(
                2,
                TEST_RATE,
                ebur128::Mode::M | ebur128::Mode::S | ebur128::Mode::TRUE_PEAK | ebur128::Mode::I | ebur128::Mode::LRA,
            )
            .expect("ebur128 init")
        }

        /// A programme alternating between a loud and a much quieter passage should show a real,
        /// positive Loudness Range — the whole reason LRA exists (a track that's just constant
        /// loudness the whole way through has ~0 LU, one that alternates loud/quiet has a wide
        /// spread) — and Integrated should land somewhere between the two blocks' own levels, not
        /// at either extreme.
        #[test]
        fn alternating_loud_and_quiet_blocks_show_a_real_loudness_range() {
            let mut ebu = new_ebu();
            for _ in 0..6 {
                ebu.add_frames_f32(&tone_frames(1000.0, 3.0, 0.8)).unwrap(); // loud
                ebu.add_frames_f32(&tone_frames(1000.0, 3.0, 0.05)).unwrap(); // quiet
            }

            let integrated = ebu.loudness_global().unwrap();
            let lra = ebu.loudness_range().unwrap();
            assert!(integrated.is_finite(), "expected a real integrated reading, got {integrated}");
            assert!(lra > 3.0, "alternating loud/quiet should show a real LRA spread, got {lra} LU");
        }

        /// A programme at one constant level should show a small Loudness Range — the negative
        /// case for the test above, so a real spread isn't just LRA always reading high.
        #[test]
        fn constant_level_shows_a_small_loudness_range() {
            let mut ebu = new_ebu();
            for _ in 0..6 {
                ebu.add_frames_f32(&tone_frames(1000.0, 3.0, 0.3)).unwrap();
            }
            let lra = ebu.loudness_range().unwrap();
            assert!(lra < 1.0, "a constant-level programme should show ~0 LRA, got {lra} LU");
        }

        /// `EbuR128::reset()` — what the meter's reset control calls — should drop Integrated back
        /// to "no data" (-inf, clamped to LUFS_FLOOR by `clamp_lufs`), Loudness Range back to 0,
        /// and the running true-peak maximum back to silence, all together, in one call.
        #[test]
        fn reset_clears_integrated_range_and_peak_max_together() {
            let mut ebu = new_ebu();
            ebu.add_frames_f32(&tone_frames(1000.0, 3.0, 0.9)).unwrap();
            ebu.add_frames_f32(&tone_frames(1000.0, 3.0, 0.05)).unwrap();

            assert!(ebu.loudness_global().unwrap().is_finite());
            assert!(ebu.true_peak(0).unwrap() > 0.0, "should have a real peak before reset");

            ebu.reset();

            assert_eq!(clamp_lufs(ebu.loudness_global().unwrap_or(f64::NEG_INFINITY)), LUFS_FLOOR);
            assert_eq!(clamp_lu(ebu.loudness_range().unwrap_or(f64::NAN)), 0.0);
            assert_eq!(ebu.true_peak(0).unwrap(), 0.0, "true-peak max should be zeroed by reset");
        }
    }
}

#[cfg(not(windows))]
mod stub {
    use super::{MeterUpdate, ScopeUpdate, SpectrumUpdate};

    /// Non-Windows stub: loopback monitoring needs WASAPI, so [`Monitor::start`] just errors.
    pub struct Monitor;

    impl Monitor {
        pub fn start<F, G, H>(
            _endpoint_id: Option<String>,
            _scope_viewers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            _fft_size: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            _harmonic_fold: std::sync::Arc<std::sync::atomic::AtomicBool>,
            _reset_lufs: std::sync::Arc<std::sync::atomic::AtomicBool>,
            _on_update: F,
            _on_spectrum: G,
            _on_scope: H,
        ) -> Result<Monitor, String>
        where
            F: Fn(MeterUpdate) + Send + 'static,
            G: Fn(SpectrumUpdate) + Send + 'static,
            H: Fn(ScopeUpdate) + Send + 'static,
        {
            Err("loopback monitoring is only available on Windows".to_string())
        }

        pub fn stop(self) {}
    }

    /// Non-Windows stub for the self-test signal player (WASAPI render is Windows-only).
    pub struct TestSignal;

    impl TestSignal {
        pub fn start(_endpoint_id: Option<String>) -> Result<TestSignal, String> {
            Err("test-signal playback is only available on Windows".to_string())
        }

        pub fn start_generator(
            _endpoint_id: Option<String>,
            _params: crate::signal::GeneratorParams,
        ) -> Result<TestSignal, String> {
            Err("test-signal playback is only available on Windows".to_string())
        }

        pub fn stop(self) {}
    }
}
