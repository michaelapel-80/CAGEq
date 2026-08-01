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
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MeterUpdate {
    /// Peak level over the last tick, dBFS (floored at -120).
    pub peak_db: f32,
    /// RMS level over the last tick, dBFS (floored at -120).
    pub rms_db: f32,
    /// BS.1770 K-weighted momentary loudness (400 ms window), LUFS (floored at -70).
    pub momentary_lufs: f32,
    /// BS.1770 K-weighted short-term loudness (3 s window), LUFS (floored at -70).
    pub short_term_lufs: f32,
    /// `false` when the endpoint produced no audio this window — the UI shows an idle state
    /// rather than a misleading `-120`/`-70`.
    pub signal: bool,
}

#[cfg(windows)]
pub use windows_impl::Monitor;

#[cfg(not(windows))]
pub use stub::Monitor;

#[cfg(windows)]
mod windows_impl {
    use super::MeterUpdate;
    use std::collections::VecDeque;
    use std::error::Error;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use wasapi::{
        initialize_mta, Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
    };

    /// dB floor for peak/RMS so a silent endpoint reports a finite number, not -inf.
    const DB_FLOOR: f32 = -120.0;
    /// LUFS floor (below the BS.1770 absolute gate) for the same reason.
    const LUFS_FLOOR: f32 = -70.0;
    /// UI cadence. Peak/RMS are measured over this window; LUFS windows are internal to ebur128.
    const TICK: Duration = Duration::from_millis(50);
    /// No frames for at least this long ⇒ the endpoint is idle (report `signal: false`).
    const SILENCE_GAP: Duration = Duration::from_millis(300);

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
        pub fn start<F>(endpoint_id: Option<String>, on_update: F) -> Result<Monitor, String>
        where
            F: Fn(MeterUpdate) + Send + 'static,
        {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let handle = thread::Builder::new()
                .name("cageq-loopback".into())
                .spawn(move || {
                    if let Err(e) = capture_loop(endpoint_id, &stop_thread, on_update) {
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

    fn capture_loop<F>(
        endpoint_id: Option<String>,
        stop: &AtomicBool,
        on_update: F,
    ) -> Result<(), Box<dyn Error>>
    where
        F: Fn(MeterUpdate),
    {
        initialize_mta().ok()?;

        // Supervisor: (re)open the endpoint and meter until it invalidates or we're told to stop.
        // A shared-mode format change (sample-rate switch, an exclusive-mode app grabbing the
        // device, a default-device change) invalidates the capture client — WASAPI surfaces that
        // as an error from the next read. Rather than ending the thread (which froze the meter
        // until the user toggled it off/on), tear down and reopen: get_mixformat re-reads the new
        // rate and the loudness state is rebuilt for it, so a rate change self-heals.
        while !stop.load(Ordering::Relaxed) {
            if let Err(e) = run_session(&endpoint_id, stop, &on_update) {
                eprintln!("[cageq-monitor] reopening capture after: {e}");
                // Show the UI an idle state during the gap, then back off before reopening.
                on_update(MeterUpdate {
                    peak_db: DB_FLOOR,
                    rms_db: DB_FLOOR,
                    momentary_lufs: LUFS_FLOOR,
                    short_term_lufs: LUFS_FLOOR,
                    signal: false,
                });
                sleep_unless_stopped(stop, Duration::from_millis(500));
            }
        }
        Ok(())
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
    fn run_session<F>(
        endpoint_id: &Option<String>,
        stop: &AtomicBool,
        on_update: &F,
    ) -> Result<(), Box<dyn Error>>
    where
        F: Fn(MeterUpdate),
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

        audio_client.start_stream()?;

        let mut queue: VecDeque<u8> = VecDeque::new();
        let mut frames: Vec<f32> = Vec::new(); // interleaved, reused each tick for ebur128
        let mut peak = 0.0f32;
        let mut sum_sq = 0.0f64;
        let mut sample_count: u64 = 0;
        let mut last_tick = Instant::now();
        let mut last_signal = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            let before = queue.len();
            capture.read_from_device_to_deque(&mut queue)?;
            let got_data = queue.len() > before;
            if got_data {
                last_signal = Instant::now();
            }

            frames.clear();
            while queue.len() >= bytes_per_frame {
                for _ in 0..channels {
                    let b = [
                        queue.pop_front().unwrap(),
                        queue.pop_front().unwrap(),
                        queue.pop_front().unwrap(),
                        queue.pop_front().unwrap(),
                    ];
                    let s = f32::from_le_bytes(b);
                    frames.push(s);
                    let a = s.abs();
                    if a > peak {
                        peak = a;
                    }
                    sum_sq += (s as f64) * (s as f64);
                    sample_count += 1;
                }
            }
            if !frames.is_empty() {
                let _ = ebu.add_frames_f32(&frames);
            }

            if last_tick.elapsed() >= TICK {
                let signal = sample_count > 0 && last_signal.elapsed() < SILENCE_GAP;
                let rms = if sample_count > 0 {
                    (sum_sq / sample_count as f64).sqrt() as f32
                } else {
                    0.0
                };
                on_update(MeterUpdate {
                    peak_db: to_db(peak),
                    rms_db: to_db(rms),
                    momentary_lufs: clamp_lufs(
                        ebu.loudness_momentary().unwrap_or(f64::NEG_INFINITY),
                    ),
                    short_term_lufs: clamp_lufs(
                        ebu.loudness_shortterm().unwrap_or(f64::NEG_INFINITY),
                    ),
                    signal,
                });
                peak = 0.0;
                sum_sq = 0.0;
                sample_count = 0;
                last_tick = Instant::now();
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
    use super::MeterUpdate;

    /// Non-Windows stub: loopback monitoring needs WASAPI, so [`Monitor::start`] just errors.
    pub struct Monitor;

    impl Monitor {
        pub fn start<F>(_endpoint_id: Option<String>, _on_update: F) -> Result<Monitor, String>
        where
            F: Fn(MeterUpdate) + Send + 'static,
        {
            Err("loopback monitoring is only available on Windows".to_string())
        }

        pub fn stop(self) {}
    }
}
