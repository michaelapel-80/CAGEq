//! Dev test-signal generator — plays a known signal out a render endpoint via WASAPI shared
//! mode, so the running app's loopback spectrum/meters can be verified end-to-end. The signal
//! passes through Equalizer APO on the way out, and the app's spectrum is *source-referred* (it
//! back-subtracts the applied EQ magnitude per bin), so steady pink noise reads flat and stays
//! flat no matter which filters are applied — a direct check that the pre-filter/source
//! reconstruction is correct (a flat source stays flat under any EQ). A sine is the calibrated
//! spot-check for bin position/level (also EQ-invariant, for the same reason).
//!
//! Run it alongside CAGEq (it plays, the app captures):
//!   cargo run -p cageq-monitor --example testtone                     # pink noise, -20 dBFS
//!   cargo run -p cageq-monitor --example testtone -- --sine 1000      # 1 kHz sine
//!   cargo run -p cageq-monitor --example testtone -- --square 220     # 220 Hz square
//!   cargo run -p cageq-monitor --example testtone -- --triangle 220   # 220 Hz triangle
//!   cargo run -p cageq-monitor --example testtone -- --sawtooth 220   # 220 Hz sawtooth
//!   cargo run -p cageq-monitor --example testtone -- --level -24
//!   cargo run -p cageq-monitor --example testtone -- --seconds 10     # auto-stop w/ fade-out
//!   cargo run -p cageq-monitor --example testtone -- --device "Phonitor"  # match by name
//!   cargo run -p cageq-monitor --example testtone -- --sine 12000 --rate 44100  # test Windows' resampler
//!
//! `--sine`/`--square`/`--triangle`/`--sawtooth` are mutually exclusive (one tone at a time);
//! omitting all of them plays pink noise. The three non-sine shapes are synthesised as an exact
//! band-limited Fourier sum (harmonics only up to just below Nyquist, at each shape's textbook
//! amplitude — 1/k for square/sawtooth, 1/k² for triangle), not generated naively (e.g. a hard
//! comparison for square, a wrapped ramp for sawtooth) — a naive version has harmonic content out
//! to infinity, so anything above Nyquist folds back down and contaminates the very spectrum
//! shape this tool exists to let you verify against a known-correct reference. So the loopback
//! spectrum for, say, `--square 220` should show *only* clean odd harmonics falling off at
//! -6 dB/octave up to the Nyquist-adjacent cutoff, and nothing else.
//!
//! `--rate <hz>` forces a *source* rate different from the device's — Windows' shared-mode
//! resampler (AUTOCONVERT) then converts it up/down to the device rate, so any resampling images/
//! aliasing show up in the loopback spectrum. Pair with a tone flag and a Dry slot to isolate it.
//!
//! Safety: the level is clamped to ≤ -3 dBFS, every sample is hard-limited just below full scale,
//! and it fades in (and out, with --seconds) — so even with EQ boosts stacked on top it can't
//! blast. `--unsafe` lifts both clamps for a deliberate full-scale (0 dBFS) torture test. Ctrl+C
//! stops it (abrupt; use --seconds for a clean fade-out).

#[cfg(windows)]
#[derive(Clone, Copy)]
enum Waveform {
    Sine,
    Square,
    Triangle,
    Sawtooth,
}

#[cfg(windows)]
impl Waveform {
    fn name(self) -> &'static str {
        match self {
            Waveform::Sine => "sine",
            Waveform::Square => "square",
            Waveform::Triangle => "triangle",
            Waveform::Sawtooth => "sawtooth",
        }
    }

    /// One sample of this shape at phase `theta` (radians, any real value — `sin` wraps it),
    /// summing harmonics 1..=`k_max` at each shape's textbook Fourier amplitude. Peak amplitude is
    /// ~1 (plus a few percent of Gibbs overshoot right at an edge for square/sawtooth, same as any
    /// finite-harmonic approximation of a discontinuous waveform — left to the caller's existing
    /// `gain`/`sample_ceil` handling, exactly like a sine's own ~1 peak already is).
    fn sample(self, theta: f32, k_max: u32) -> f32 {
        const FRAC_4_PI: f32 = 4.0 / std::f32::consts::PI;
        const FRAC_2_PI: f32 = 2.0 / std::f32::consts::PI;
        const FRAC_8_PI2: f32 = 8.0 / (std::f32::consts::PI * std::f32::consts::PI);
        match self {
            Waveform::Sine => theta.sin(),
            // Odd harmonics only, amplitude 1/k — the textbook square-wave series.
            Waveform::Square => {
                let mut acc = 0.0f32;
                let mut k = 1u32;
                while k <= k_max {
                    acc += (k as f32 * theta).sin() / k as f32;
                    k += 2;
                }
                acc * FRAC_4_PI
            }
            // All harmonics, amplitude 1/k, alternating sign — the textbook (rising) sawtooth series.
            Waveform::Sawtooth => {
                let mut acc = 0.0f32;
                let mut sign = 1.0f32;
                for k in 1..=k_max {
                    acc += sign * (k as f32 * theta).sin() / k as f32;
                    sign = -sign;
                }
                acc * FRAC_2_PI
            }
            // Odd harmonics only, amplitude 1/k² (converges much faster than square/sawtooth — no
            // audible discontinuity in the waveform itself, just a slope change, so far less Gibbs
            // ringing and a visibly steeper roll-off on screen: -12 dB/octave vs -6).
            Waveform::Triangle => {
                let mut acc = 0.0f32;
                let mut k = 1u32;
                let mut sign = 1.0f32;
                while k <= k_max {
                    acc += sign * (k as f32 * theta).sin() / (k as f32 * k as f32);
                    sign = -sign;
                    k += 2;
                }
                acc * FRAC_8_PI2
            }
        }
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use wasapi::{
        initialize_mta, Direction, SampleType, StreamMode, WaveFormat,
    };

    // --- args (dependency-free parsing) ------------------------------------------------------
    let mut tone: Option<(Waveform, f32)> = None;
    let mut level_dbfs: f32 = -20.0;
    let mut seconds: Option<f32> = None;
    let mut device_match: Option<String> = None;
    let mut rate_override: Option<u32> = None;
    let mut unsafe_mode = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        // Shared by all four tone flags: only one shape at a time, so a leftover --square from a
        // copy-pasted command line can't silently combine with a new --sine instead of erroring.
        let mut set_tone = |wf: Waveform, hz: f32| -> Result<(), Box<dyn std::error::Error>> {
            if tone.is_some() {
                return Err("only one of --sine/--square/--triangle/--sawtooth may be given".into());
            }
            tone = Some((wf, hz));
            Ok(())
        };
        match a.as_str() {
            "--sine" => set_tone(Waveform::Sine, args.next().ok_or("--sine needs a frequency")?.parse()?)?,
            "--square" => set_tone(Waveform::Square, args.next().ok_or("--square needs a frequency")?.parse()?)?,
            "--triangle" => set_tone(Waveform::Triangle, args.next().ok_or("--triangle needs a frequency")?.parse()?)?,
            "--sawtooth" => set_tone(Waveform::Sawtooth, args.next().ok_or("--sawtooth needs a frequency")?.parse()?)?,
            "--level" => level_dbfs = args.next().ok_or("--level needs a value")?.parse()?,
            "--seconds" => seconds = Some(args.next().ok_or("--seconds needs a value")?.parse()?),
            "--device" => device_match = Some(args.next().ok_or("--device needs a name")?),
            // Force a source sample rate ≠ the device rate → Windows' shared-mode resampler
            // converts it (AUTOCONVERT), so the loopback shows the resampling artifacts. Pair with
            // a tone flag and a Dry slot to isolate the resampler.
            "--rate" => rate_override = Some(args.next().ok_or("--rate needs a value")?.parse()?),
            // Lift the -3 dBFS safety ceiling and the per-sample limiter to allow a full-scale
            // (0 dBFS) torture test. Opt-in and deliberate — mind your ears and gear.
            "--unsafe" => unsafe_mode = true,
            "-h" | "--help" => {
                eprintln!("usage: testtone [--sine|--square|--triangle|--sawtooth <hz>] [--level <dbfs>] [--rate <hz>] [--seconds <n>] [--device <name-substr>] [--unsafe]");
                return Ok(());
            }
            other => return Err(format!("unknown arg: {other}").into()),
        }
    }
    if let Some(r) = rate_override {
        if !(8_000..=768_000).contains(&r) {
            return Err(format!("--rate {r} is out of the 8000..=768000 range").into());
        }
    }
    // Clamp the level below full scale so a boosted EQ can't drive it into a blast — unless the
    // caller opts into a full-scale (0 dBFS) torture test with --unsafe.
    let level_ceil_dbfs: f32 = if unsafe_mode { 0.0 } else { -3.0 };
    if unsafe_mode {
        eprintln!("[testtone] --unsafe: full-scale (0 dBFS) allowed and the per-sample limiter is off — mind your ears/gear.");
    }
    if level_dbfs > level_ceil_dbfs {
        eprintln!("[testtone] level {level_dbfs} dBFS is above the {level_ceil_dbfs} dBFS ceiling — clamping.");
        level_dbfs = level_ceil_dbfs;
    }
    let gain = 10f32.powf(level_dbfs.clamp(-80.0, level_ceil_dbfs) / 20.0);
    // Final per-sample ceiling: ~-1 dBFS normally (keeps pink/EQ peaks off the clip rail), full
    // scale under --unsafe so a 0 dBFS sine passes through untouched.
    let sample_ceil: f32 = if unsafe_mode { 1.0 } else { 0.891 };

    initialize_mta().ok()?;
    let enumerator = wasapi::DeviceEnumerator::new()?;

    // Target: a render endpoint whose friendly name contains --device, else the default.
    let device = match &device_match {
        Some(want) => {
            let want = want.to_ascii_lowercase();
            let mut found = None;
            for dev in &enumerator.get_device_collection(&Direction::Render)? {
                let dev = dev?;
                if dev.get_friendlyname().map(|n| n.to_ascii_lowercase().contains(&want)).unwrap_or(false) {
                    found = Some(dev);
                    break;
                }
            }
            found.ok_or_else(|| format!("no render device matching \"{want}\""))?
        }
        None => enumerator.get_default_device(&Direction::Render)?,
    };
    eprintln!("[testtone] device: {}", device.get_friendlyname().unwrap_or_else(|_| "<unknown>".into()));

    let mut audio_client = device.get_iaudioclient()?;
    let mix = audio_client.get_mixformat()?;
    let mix_rate = mix.get_samplespersec();
    let channels = mix.get_nchannels();
    // Source rate: the device mix rate unless forced. When it differs, AUTOCONVERT below makes the
    // engine resample us up/down to the mix rate — the whole point of --rate.
    let rate = rate_override.unwrap_or(mix_rate);
    let desired = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
    let block_align = desired.get_blockalign() as usize;

    let (def_period, _min_period) = audio_client.get_device_period()?;
    let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: def_period };
    audio_client
        .initialize_client(&desired, &Direction::Render, &mode)
        .map_err(|e| format!("initialize_client at {rate} Hz failed ({e:?}) — the endpoint may not accept this source rate"))?;
    let render = audio_client.get_audiorenderclient()?;

    let total_frames: Option<u64> = seconds.map(|s| (s * rate as f32) as u64);
    let fade_frames = (0.12 * rate as f32) as u64; // 120 ms fade in/out — kills startup/stop pops
    let resample = if rate != mix_rate { format!(" → resampled to {mix_rate} Hz by Windows") } else { String::new() };
    match tone {
        Some((wf, hz)) => eprintln!("[testtone] {hz} Hz {} @ {level_dbfs} dBFS, source {rate} Hz / {channels} ch{resample}", wf.name()),
        None => eprintln!("[testtone] pink noise @ ~{level_dbfs} dBFS, source {rate} Hz / {channels} ch{resample}"),
    }
    eprintln!("[testtone] Ctrl+C to stop.");

    audio_client.start_stream()?;

    // Signal state.
    let mut frame: u64 = 0;
    let mut rng: u32 = 0x2545_f491; // xorshift white-noise source (no rand dependency needed)
    let (mut b0, mut b1, mut b2) = (0f32, 0f32, 0f32); // Paul Kellet economy pink-noise filter
    // Tone phase as a *wrapped incremental accumulator*, not `sin(2π·f·frame/rate)`: computing the
    // latter from an ever-growing `frame` in f32 loses precision in binades, dirtying the tone
    // step-wise over time (a confound that looks like the resampler degrading). Wrapping keeps the
    // argument in [0, 2π) so it stays precise indefinitely — and since every harmonic's contribution
    // below is computed as `sin(k * phase)` fresh from that same bounded, precise value each sample
    // (not its own accumulated phase), they inherit the same precision guarantee for free.
    let phase_inc = std::f32::consts::TAU * tone.map(|(_, hz)| hz).unwrap_or(0.0) / rate as f32;
    let mut phase = 0f32;
    // Highest harmonic to sum, kept a few percent below true Nyquist rather than right up against
    // it — see the file header doc for why band-limiting matters here at all (a naive square/
    // triangle/sawtooth has harmonics to infinity, which would alias back down and contaminate the
    // very spectrum this tool exists to let you check against a known-correct shape).
    let k_max: u32 = match tone {
        Some((_, hz)) if hz > 0.0 => (((rate as f32 * 0.48) / hz).floor().max(1.0)) as u32,
        _ => 1,
    };
    let mut buf: Vec<u8> = Vec::new();

    'play: loop {
        if let Some(total) = total_frames {
            if frame >= total {
                break;
            }
        }
        let space = audio_client.get_available_space_in_frames()? as usize;
        if space == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        buf.clear();
        for _ in 0..space {
            if let Some(total) = total_frames {
                if frame >= total {
                    break;
                }
            }
            // Fade envelope: ramp in over the first `fade_frames`, and (when finite) out over the last.
            let mut env = if frame < fade_frames { frame as f32 / fade_frames as f32 } else { 1.0 };
            if let Some(total) = total_frames {
                let rem = total.saturating_sub(frame);
                if rem < fade_frames {
                    env = env.min(rem as f32 / fade_frames as f32);
                }
            }

            let mono = match tone {
                Some((wf, _)) => {
                    let s = wf.sample(phase, k_max) * gain;
                    phase += phase_inc;
                    if phase >= std::f32::consts::TAU {
                        phase -= std::f32::consts::TAU;
                    }
                    s
                }
                None => {
                    // White (uniform [-1,1]) → Paul Kellet's economy pink filter.
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    let white = (rng as f32 / u32::MAX as f32) * 2.0 - 1.0;
                    b0 = 0.99765 * b0 + white * 0.0990460;
                    b1 = 0.96300 * b1 + white * 0.2965164;
                    b2 = 0.57000 * b2 + white * 1.0526913;
                    // Sum + direct term, normalised (~unit amplitude) then scaled to the level.
                    (b0 + b1 + b2 + white * 0.1848) * 0.11 * gain
                }
            };
            let s = (mono * env).clamp(-sample_ceil, sample_ceil);
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
        if buf.is_empty() {
            break 'play;
        }
    }

    // Let the last buffer drain before tearing down, so the fade-out is actually heard.
    std::thread::sleep(Duration::from_millis(200));
    audio_client.stop_stream()?;
    eprintln!("[testtone] done.");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("testtone is Windows-only (WASAPI render).");
}
