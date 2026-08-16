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
    /// Peak level, dBFS — a PPM-style follower (instant attack, brief hold, release), floored at -120.
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
    /// The endpoint's shared-mode mix sample rate (Hz) — i.e. Windows' configured playback rate
    /// for this device, which the loopback runs at. `0` while the session is (re)opening. The UI
    /// surfaces it next to the device picker (filter.md §8, read-only format display).
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
        initialize_mta, Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
    };

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
    /// Base FFT size at ≤48 kHz (≈5.9 Hz bins / 171 ms window — decent low-end for resonance
    /// hunting). Scaled up with the sample rate in [`Spectrum::new`] so the analysis *window
    /// duration* — and thus the low-frequency resolution — stays constant at 96/192 kHz instead of
    /// halving/quartering. We only ever display up to 20 kHz, so analysing the whole (wider) band
    /// and ignoring the bins above 20 kHz is simpler and alias-free vs. decimating the input.
    const BASE_FFT_SIZE: usize = 8192;
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
    /// Max (l, r) pairs sent per emit — the *contiguous tail* of the window, in order and **not**
    /// stride-decimated, so the front-end can connect them into a continuous beam trace. Decimation
    /// would shred the drawn Lissajous figures of oscilloscope-music. A 16 ms window at ≤48 kHz
    /// (~768 pairs) fits under this; higher rates send their most recent SCOPE_MAX_POINTS.
    const SCOPE_MAX_POINTS: usize = 2048;

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
        let (def_period, _min_period) = audio_client.get_device_period()?;
        let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: def_period };
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
        fft_size: usize,           // per-rate FFT length (BASE_FFT_SIZE scaled to the sample rate)
        fft_hop: usize,            // hop between windows = fft_size / FFT_OVERLAP_DIV
        in_buf: Vec<f32>,          // realfft input scratch (len fft_size)
        out_buf: Vec<Complex<f32>>, // realfft output scratch (len fft_size/2 + 1)
        scratch: Vec<Complex<f32>>,
        window: Vec<f32>,          // Hann window
        accum: VecDeque<f32>,      // mono sample accumulator
        avg_power: Vec<f32>,       // smoothed linear power per FFT bin
        ranges: Vec<(usize, usize)>, // per log bin: inclusive linear-bin span
        smoothing: f32,            // power-average coefficient per hop
        power_scale: f32,          // |X|² -> normalized power so a full-scale sine reads ~0 dBFS
    }

    impl Spectrum {
        fn new(rate: u32) -> Self {
            // Scale the FFT length up with the rate so the window *duration* (≈171 ms) stays
            // constant: FFT_SIZE = BASE × next_pow2(round(rate / 48 kHz)). 48 k→8192, 96 k→16384,
            // 192 k→32768 (44.1/88.2/176.4 round to the same multiples). Keeps `bin_hz` — and the
            // low-frequency resolution — identical across devices instead of coarsening at high rates.
            let mult = ((rate as f32 / BASE_RATE).round().max(1.0) as usize).next_power_of_two();
            let fft_size = BASE_FFT_SIZE * mult;
            let fft_hop = fft_size / FFT_OVERLAP_DIV;

            let fft = RealFftPlanner::<f32>::new().plan_fft_forward(fft_size);
            let in_buf = fft.make_input_vec();
            let out_buf = fft.make_output_vec();
            let scratch = fft.make_scratch_vec();
            let window: Vec<f32> = (0..fft_size)
                .map(|n| {
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / fft_size as f32).cos()
                })
                .collect();

            let n_lin = fft_size / 2 + 1;
            let bin_hz = rate as f32 / fft_size as f32;
            let ratio = (SPEC_F_MAX / SPEC_F_MIN).powf(1.0 / (N_LOG_BINS as f32 - 1.0));
            let half = ratio.sqrt();
            let mut ranges = Vec::with_capacity(N_LOG_BINS);
            for i in 0..N_LOG_BINS {
                let fc = SPEC_F_MIN * ratio.powi(i as i32);
                let lo = ((fc / half) / bin_hz).floor().max(0.0) as usize;
                let hi = (((fc * half) / bin_hz).ceil() as usize).min(n_lin - 1);
                ranges.push((lo.min(hi), hi));
            }

            let smoothing = 1.0 - (-(fft_hop as f32 / rate as f32) / SPEC_TAU_SECS).exp();
            // Amplitude normalization: a full-scale sine at a bin centre gives |X| = S1/2 (window
            // coherent gain), so multiply the one-sided amplitude by 2/S1 to read 1.0 → 0 dBFS.
            // In the power domain that's (2/S1)². S1 = sum(window). This makes the level absolute
            // and FFT-size independent, so the display can use a fixed scale.
            let s1: f32 = window.iter().sum();
            let power_scale = (2.0 / s1).powi(2);
            Spectrum {
                fft,
                fft_size,
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
            while self.accum.len() >= self.fft_size {
                for (i, w) in self.window.iter().enumerate() {
                    self.in_buf[i] = self.accum[i] * w;
                }
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
                // Peak linear bin in this log bin, normalized to dBFS — so a narrow resonance reads
                // its true level regardless of the (wider, at HF) log bin, on an absolute scale.
                let power = self.avg_power[lo..=hi].iter().copied().fold(0.0f32, f32::max);
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

        // Capture at the endpoint's shared-mode mix rate/channels, but ask for f32 with
        // autoconvert so we always parse a known sample type.
        let mix = audio_client.get_mixformat()?;
        let rate = mix.get_samplespersec();
        let channels = mix.get_nchannels();
        let desired =
            WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
        let bytes_per_frame = desired.get_blockalign() as usize;

        let (def_period, _min_period) = audio_client.get_device_period()?;
        let mode =
            StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: def_period };
        audio_client.initialize_client(&desired, &Direction::Capture, &mode)?;
        let capture = audio_client.get_audiocaptureclient()?;

        // K-weighted loudness at the *actual* endpoint rate (ebur128 recomputes coefficients).
        let mut ebu = ebur128::EbuR128::new(
            channels as u32,
            rate,
            ebur128::Mode::M | ebur128::Mode::S | ebur128::Mode::SAMPLE_PEAK,
        )
        .map_err(|e| format!("ebur128 init: {e:?}"))?;

        let mut spectrum = Spectrum::new(rate);

        audio_client.start_stream()?;

        let mut queue: VecDeque<u8> = VecDeque::new();
        let mut frames: Vec<f32> = Vec::new(); // interleaved, reused each read for ebur128
        let mut mono: Vec<f32> = Vec::new(); // per-read mono downmix, fed to the FFT
        let mut scope_lr: Vec<f32> = Vec::new(); // (l, r) pairs since the last scope emit
        // Per-tick block accumulators (reset every emit).
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
                    let a = s.abs();
                    if a > block_peak {
                        block_peak = a;
                    }
                    block_sum_sq += (s as f64) * (s as f64);
                    block_count += 1;
                }
                mono.push(frame_sum / channels as f32);
                if scope_on {
                    scope_lr.push(ch0);
                    scope_lr.push(ch1);
                }
            }
            if !frames.is_empty() {
                let _ = ebu.add_frames_f32(&frames);
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
                    sample_rate: rate,
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

        let _ = audio_client.stop_stream();
        Ok(())
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
