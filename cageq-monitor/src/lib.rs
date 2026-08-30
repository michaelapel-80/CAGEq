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
    use super::{MeterUpdate, ScopeUpdate, SpectrumUpdate};
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
    const BASE_FFT_SIZE: usize = 8192;
    /// The real, windowed analysis block (`BASE_FFT_SIZE`-derived) is transformed at this many
    /// times its own length — the rest of the FFT's input is zeros. This is NOT the same thing as
    /// more resolution: resolution (how well two close tones can be told apart) is fixed by the
    /// analysis window's time *duration*, unaffected by this. Zero-padding instead *interpolates*
    /// the transform of that same finite window more finely — a finite-duration signal has a
    /// well-defined continuous Fourier transform, and the unpadded FFT only ever samples it
    /// coarsely; padding computes more exact samples of that identical continuous function, not
    /// new/approximated information. This is the standard technique real-time spectrum analyzers
    /// use to look smooth without a longer (higher-latency) window.
    ///
    /// Concretely: 5.9 Hz bins (BASE_FFT_SIZE alone) let the 240 *log*-spaced display bins outrun
    /// the linear FFT's own resolution below ~200 Hz — several adjacent display bins there end up
    /// reading the exact same linear bin, a genuine plateau the display has to render honestly
    /// (see EqChart/SpectrumScope's dedup) rather than smooth over. At ×4 (≈1.5 Hz bins) that
    /// crossover drops to ~50 Hz, shrinking the affected range by roughly the same factor, for a
    /// modest one-time FFT cost (O(N log N), so ×4 the points costs well under ×4) and zero added
    /// latency — same real samples, same hop cadence, just more (interpolated) output bins.
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
    const SPEC_F_MIN: f32 = 20.0;
    const SPEC_F_MAX: f32 = 20_000.0;
    /// dB floor for empty/silent spectrum bins.
    const SPEC_FLOOR: f32 = -120.0;
    /// Power-spectrum smoothing time constant (seconds) — just enough to settle pure FFT/windowing
    /// noise across a couple of hops (~43 ms each, see fft_hop), not to steady the display over
    /// time: that's the front-end's job now (canvas phosphor persistence, plus interpolating
    /// between spectrum events instead of snapping). A slower constant here used to add its own
    /// multi-hop lag *underneath* the front-end's persistence — two decays compounding into a
    /// sluggish "slow fall" that no front-end tuning could get out from under, since it was baked
    /// into the values themselves before they ever left the sidecar.
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
        pub fn start<F, G, H>(
            endpoint_id: Option<String>,
            scope_viewers: Arc<AtomicUsize>,
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
                    if let Err(e) = capture_loop(endpoint_id, &stop_thread, &scope_viewers, on_update, on_spectrum, on_scope) {
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

    /// A safe, self-terminating pink-noise player — the render counterpart to the loopback. The
    /// app's self-test plays this out the endpoint (through EqAPO) while the loopback captures the
    /// result, to prove end-to-end that the EQ chain is actually applying corrections. Level is
    /// fixed low (audible but safe) and faded in; the caller stops it when the measurement is done.
    pub struct TestSignal {
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    /// Self-test playback level — audible but conservative (extra headroom over the -3 dBFS the dev
    /// `testtone` example allows, since this plays on real users' devices through their EQ).
    const TEST_SIGNAL_DBFS: f32 = -18.0;

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

    /// Render faded-in pink noise out the endpoint at [`TEST_SIGNAL_DBFS`] until `stop`. Shared-mode
    /// render, so it passes through EqAPO like any app's audio (the whole point of the self-test).
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

        let gain = 10f32.powf(TEST_SIGNAL_DBFS / 20.0);
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
    /// the span it was estimated over — is the fix.
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
    /// reading matters more than a perfectly flat tone sweep.
    fn gaussian_power(power: &[f32], lo: f32, hi: f32) -> f32 {
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
            acc / wsum
        } else {
            0.0
        }
    }

    fn to_db(linear: f32) -> f32 {
        if linear <= 0.0 {
            DB_FLOOR
        } else {
            (20.0 * linear.log10()).max(DB_FLOOR)
        }
    }

    fn clamp_lufs(v: f64) -> f32 {
        if v.is_finite() {
            (v as f32).max(LUFS_FLOOR)
        } else {
            LUFS_FLOOR
        }
    }

    /// Post-EQ spectrum analyzer: accumulates mono loopback samples, runs overlapping Hann-windowed
    /// FFTs, lightly averages the power spectrum across hops, and collapses it onto log-frequency
    /// bins. No peak-hold here — the front-end's own persistence covers that job now.
    struct Spectrum {
        fft: Arc<dyn RealToComplex<f32>>,
        analysis_size: usize,      // real, windowed sample count (BASE_FFT_SIZE scaled to rate) —
                                    // governs window duration/hop timing, i.e. true resolution
        fft_hop: usize,            // hop between windows = analysis_size / FFT_OVERLAP_DIV
        in_buf: Vec<f32>,          // realfft input scratch, len analysis_size*ZERO_PAD_FACTOR —
                                    // only the first analysis_size entries ever hold real samples,
                                    // the rest must stay exactly 0.0 (see push(): realfft documents
                                    // input as "garbage after calling", so it's re-zeroed every hop
                                    // rather than trusted to stay zero from init)
        out_buf: Vec<Complex<f32>>, // realfft output scratch (len in_buf.len()/2 + 1)
        scratch: Vec<Complex<f32>>,
        window: Vec<f32>,          // Hann window, len analysis_size — the real samples only
        accum: VecDeque<f32>,      // mono sample accumulator
        avg_power: Vec<f32>,       // smoothed linear power per (padded, interpolated) FFT bin
        ranges: Vec<(f32, f32)>, // per log bin: exact (fractional) linear-bin span — see `gaussian_power`
        smoothing: f32,            // power-average coefficient per hop
        power_scale: f32,          // |X|² -> normalized power so a full-scale sine reads ~0 dBFS
    }

    impl Spectrum {
        fn new(rate: u32) -> Self {
            // Scale the real analysis window up with the rate so its *duration* (≈171 ms) stays
            // constant: analysis_size = BASE × next_pow2(round(rate / 48 kHz)). 48 k→8192,
            // 96 k→16384, 192 k→32768 (44.1/88.2/176.4 round to the same multiples). Keeps the true
            // (unpadded) resolution identical across devices instead of coarsening at high rates.
            let mult = ((rate as f32 / BASE_RATE).round().max(1.0) as usize).next_power_of_two();
            let analysis_size = BASE_FFT_SIZE * mult;
            let fft_hop = analysis_size / FFT_OVERLAP_DIV;
            // The FFT is planned and run at ZERO_PAD_FACTOR times the real window — see that
            // constant's doc for why this is an interpolation of the same window's transform, not
            // additional resolution. Stays a power of two (both factors are), so the transform
            // itself is exactly as cheap per-point as an unpadded one of the same total length.
            let padded_size = analysis_size * ZERO_PAD_FACTOR;

            let fft = RealFftPlanner::<f32>::new().plan_fft_forward(padded_size);
            let in_buf = fft.make_input_vec();
            let out_buf = fft.make_output_vec();
            let scratch = fft.make_scratch_vec();
            // Windows only the real (analysis_size) portion — the padding is zeros regardless of
            // any window coefficient, so extending the window formula over it would be dead work.
            let window: Vec<f32> = (0..analysis_size)
                .map(|n| {
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / analysis_size as f32).cos()
                })
                .collect();

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

            let smoothing = 1.0 - (-(fft_hop as f32 / rate as f32) / SPEC_TAU_SECS).exp();
            // Amplitude normalization: a full-scale sine at a bin centre gives |X| = S1/2 (window
            // coherent gain), so multiply the one-sided amplitude by 2/S1 to read 1.0 → 0 dBFS.
            // In the power domain that's (2/S1)². S1 = sum(window) — unaffected by the zero
            // padding (zeros contribute nothing to the sum either way), so this is still exactly
            // the real (analysis_size) window's own coherent gain, FFT-size independent as before.
            let s1: f32 = window.iter().sum();
            let power_scale = (2.0 / s1).powi(2);
            Spectrum {
                fft,
                analysis_size,
                fft_hop,
                in_buf,
                out_buf,
                scratch,
                window,
                accum: VecDeque::new(),
                avg_power: vec![0.0; n_lin],
                ranges,
                smoothing,
                power_scale,
            }
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
                for _ in 0..self.fft_hop {
                    self.accum.pop_front();
                }
            }
        }

        /// Fade the stored power toward the floor — called on an emit with no fresh audio, so the
        /// spectrum decays during silence instead of freezing at its last values.
        fn decay_idle(&mut self) {
            for p in self.avg_power.iter_mut() {
                *p *= SPEC_IDLE_DECAY;
            }
        }

        /// Collapse the averaged power onto log bins (dB).
        fn snapshot(&mut self, signal: bool) -> SpectrumUpdate {
            let mut db = Vec::with_capacity(N_LOG_BINS);
            for &(lo, hi) in self.ranges.iter() {
                // Gaussian-weighted integral of linear power across the log bin's own span, not
                // the single loudest sample in it — see `gaussian_power`'s doc for the full case
                // history (this replaced `.fold(max)`, then a rectangular/boxcar sum, in that
                // order; both are in git history if the reasoning against either is ever needed
                // again). Verified against synthetic ground truth before landing here — see
                // `cageq-monitor/tests/decimation_spike.rs`, kept.
                let power = gaussian_power(&self.avg_power, lo, hi);
                let cur = if power > 0.0 {
                    (10.0 * (power * self.power_scale).log10()).max(SPEC_FLOOR)
                } else {
                    SPEC_FLOOR
                };
                db.push(round_to(cur, 1));
            }
            SpectrumUpdate { db, signal, f_min: SPEC_F_MIN, f_max: SPEC_F_MAX }
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
            if let Err(e) = run_session(&endpoint_id, stop, scope_viewers, &on_update, &on_spectrum, &on_scope) {
                eprintln!("[cageq-monitor] reopening capture after: {e}");
                // Show the UI an idle state during the gap, then back off before reopening.
                on_update(MeterUpdate {
                    peak_db: DB_FLOOR,
                    rms_db: DB_FLOOR,
                    momentary_lufs: LUFS_FLOOR,
                    short_term_lufs: LUFS_FLOOR,
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
        // replaced a plain sample-peak scan.
        let mut ebu = ebur128::EbuR128::new(
            channels as u32,
            rate,
            ebur128::Mode::M | ebur128::Mode::S | ebur128::Mode::TRUE_PEAK,
        )
        .map_err(|e| format!("ebur128 init: {e:?}"))?;

        let mut spectrum = Spectrum::new(rate);

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

                on_update(MeterUpdate {
                    peak_db,
                    rms_db: to_db(rms_mean_sq.sqrt() as f32),
                    momentary_lufs: clamp_lufs(
                        ebu.loudness_momentary().unwrap_or(f64::NEG_INFINITY),
                    ),
                    short_term_lufs: clamp_lufs(
                        ebu.loudness_shortterm().unwrap_or(f64::NEG_INFINITY),
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

        #[test]
        fn pure_tone_lobe_is_monotonic_each_side_of_peak() {
            let rate = 48_000u32;
            let mut spec = Spectrum::new(rate);
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

        pub fn stop(self) {}
    }
}
