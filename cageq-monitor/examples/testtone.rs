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
//!   cargo run -p cageq-monitor --example testtone -- --level -24
//!   cargo run -p cageq-monitor --example testtone -- --seconds 10     # auto-stop w/ fade-out
//!   cargo run -p cageq-monitor --example testtone -- --device "Phonitor"  # match by name
//!
//! Safety: the level is clamped to ≤ -3 dBFS, every sample is hard-limited just below full scale,
//! and it fades in (and out, with --seconds) — so even with EQ boosts stacked on top it can't
//! blast. Ctrl+C stops it (abrupt; use --seconds for a clean fade-out).

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use wasapi::{
        initialize_mta, Direction, SampleType, StreamMode, WaveFormat,
    };

    // --- args (dependency-free parsing) ------------------------------------------------------
    let mut sine_hz: Option<f32> = None;
    let mut level_dbfs: f32 = -20.0;
    let mut seconds: Option<f32> = None;
    let mut device_match: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sine" => sine_hz = Some(args.next().ok_or("--sine needs a frequency")?.parse()?),
            "--level" => level_dbfs = args.next().ok_or("--level needs a value")?.parse()?,
            "--seconds" => seconds = Some(args.next().ok_or("--seconds needs a value")?.parse()?),
            "--device" => device_match = Some(args.next().ok_or("--device needs a name")?),
            "-h" | "--help" => {
                eprintln!("usage: testtone [--sine <hz>] [--level <dbfs>] [--seconds <n>] [--device <name-substr>]");
                return Ok(());
            }
            other => return Err(format!("unknown arg: {other}").into()),
        }
    }
    // Clamp the level well below full scale so a boosted EQ can't drive it into a blast.
    const LEVEL_CEIL_DBFS: f32 = -3.0;
    if level_dbfs > LEVEL_CEIL_DBFS {
        eprintln!("[testtone] level {level_dbfs} dBFS is above the {LEVEL_CEIL_DBFS} dBFS safety ceiling — clamping.");
        level_dbfs = LEVEL_CEIL_DBFS;
    }
    let gain = 10f32.powf(level_dbfs.clamp(-80.0, LEVEL_CEIL_DBFS) / 20.0);
    // Final per-sample ceiling (~-1 dBFS) so pink-noise/EQ peaks never reach digital clip.
    const SAMPLE_CEIL: f32 = 0.891;

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
    let rate = mix.get_samplespersec();
    let channels = mix.get_nchannels();
    let desired = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
    let block_align = desired.get_blockalign() as usize;

    let (def_period, _min_period) = audio_client.get_device_period()?;
    let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: def_period };
    audio_client.initialize_client(&desired, &Direction::Render, &mode)?;
    let render = audio_client.get_audiorenderclient()?;

    let total_frames: Option<u64> = seconds.map(|s| (s * rate as f32) as u64);
    let fade_frames = (0.12 * rate as f32) as u64; // 120 ms fade in/out — kills startup/stop pops
    match sine_hz {
        Some(hz) => eprintln!("[testtone] {hz} Hz sine @ {level_dbfs} dBFS, {rate} Hz / {channels} ch"),
        None => eprintln!("[testtone] pink noise @ ~{level_dbfs} dBFS, {rate} Hz / {channels} ch"),
    }
    eprintln!("[testtone] Ctrl+C to stop.");

    audio_client.start_stream()?;

    // Signal state.
    let mut frame: u64 = 0;
    let mut rng: u32 = 0x2545_f491; // xorshift white-noise source (no rand dependency needed)
    let (mut b0, mut b1, mut b2) = (0f32, 0f32, 0f32); // Paul Kellet economy pink-noise filter
    let two_pi_f = std::f32::consts::TAU * sine_hz.unwrap_or(0.0);
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

            let mono = match sine_hz {
                Some(_) => (two_pi_f * frame as f32 / rate as f32).sin() * gain,
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
            let s = (mono * env).clamp(-SAMPLE_CEIL, SAMPLE_CEIL);
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
