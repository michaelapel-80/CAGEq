//! Dev test-signal generator — plays a known signal out a render endpoint via WASAPI shared
//! mode, so the running app's loopback spectrum/meters can be verified end-to-end. The signal
//! passes through Equalizer APO on the way out, and the app's spectrum is *source-referred* (it
//! back-subtracts the applied EQ magnitude per bin), so steady pink noise reads flat and stays
//! flat no matter which filters are applied — a direct check that the pre-filter/source
//! reconstruction is correct (a flat source stays flat under any EQ). A sine is the calibrated
//! spot-check for bin position/level (also EQ-invariant, for the same reason).
//!
//! The actual signal synthesis (`Waveform`, `Signal`, the wavetable builder, `ISP_MAX_OVER_DB`)
//! lives in `cageq_monitor::signal` — shared with the in-app test-tone generator window
//! (`cageq-app`'s `start_test_generator` command) so the two "drivers" can't independently drift
//! against each other. This file keeps only what's CLI-specific: arg parsing, the
//! `--unsafe`/level-ceiling policy, `eprintln!` diagnostics, and the interactive render loop
//! (Ctrl+C-driven, with a `--seconds` auto-stop).
//!
//! Run it alongside CAGEq (it plays, the app captures):
//!   cargo run -p cageq-monitor --example testtone                     # pink noise, -20 dBFS
//!   cargo run -p cageq-monitor --example testtone -- --pink           # same, explicit
//!   cargo run -p cageq-monitor --example testtone -- --white          # white noise
//!   cargo run -p cageq-monitor --example testtone -- --sine 1000      # 1 kHz sine
//!   cargo run -p cageq-monitor --example testtone -- --square 220     # 220 Hz square
//!   cargo run -p cageq-monitor --example testtone -- --triangle 220   # 220 Hz triangle
//!   cargo run -p cageq-monitor --example testtone -- --sawtooth 220   # 220 Hz sawtooth
//!   cargo run -p cageq-monitor --example testtone -- --pulse 100      # 100 Hz pulse train (10% duty)
//!   cargo run -p cageq-monitor --example testtone -- --level -24
//!   cargo run -p cageq-monitor --example testtone -- --seconds 10     # auto-stop w/ fade-out
//!   cargo run -p cageq-monitor --example testtone -- --device "Phonitor"  # match by name
//!   cargo run -p cageq-monitor --example testtone -- --sine 12000 --rate 44100  # test Windows' resampler
//!   cargo run -p cageq-monitor --example testtone -- --isp 3.0103 --unsafe  # +3 dBTP true-peak over
//!
//! `--sine`/`--square`/`--triangle`/`--sawtooth`/`--pulse`/`--pink`/`--white`/`--isp` are all
//! mutually exclusive (one signal at a time); omitting every one of them plays pink noise, same as
//! passing `--pink` explicitly.
//!
//! `--isp <db-over>` is a different kind of test signal from the rest: not a spectrum-shape check,
//! but a *true-peak* one. Every other signal here is judged purely by its sample values, but a
//! sample stream at or below 0 dBFS can still reconstruct, on a real DAC, to an analog waveform
//! that peaks *above* 0 dBFS between samples — the "inter-sample peak" (ISP) a true-peak meter
//! catches and a sample-peak meter can't. `--isp` builds the textbook example: a sine at exactly
//! Fs/4 with a 45°-phase offset, so consecutive samples land at ±sin(45°) = ±1/√2 while the
//! continuous waveform's own peak — sitting exactly at the midpoint between two samples, by
//! construction — reaches ±1. That's a factor of √2 between sample peak and true peak, i.e.
//! 20·log10(√2) = 10·log10(2) ≈ 3.0103 dB, exactly and independent of level or which (correctly
//! bandlimited) reconstruction filter the DAC uses — every one of them reproduces the same
//! original sinusoid. `<db-over>` is how far above 0 dBFS the *true* peak should land (0..=3.0103,
//! clamped); the sample-domain level needed to hit it (`db-over - 3.0103`, always ≤ 0 dBFS) is
//! derived automatically and `--level` is ignored. Real mastered/clipped material can occasionally
//! exceed 3.0103 dB of ISP, but that's this generator's ceiling: the largest overshoot a single
//! pure tone can produce deterministically, reconstruction-filter-agnostic. See ITU-R BS.1770
//! Annex 2 / EBU Tech 3341 for the same construction used as the standard true-peak-meter
//! calibration example.
//!
//! Pink and white noise are also the calibrated pair for the Monitor pane's
//! spectrum *display* (not just the source-referred EQ check above): pink noise's power spectral
//! density is -3dB/octave by definition (equal energy per octave — the textbook reason it's used
//! as a reference signal at all), so it should read as a straight, flat-DIAGONAL line log-log;
//! white noise's PSD is flat by definition, so it should read as a flat-HORIZONTAL line. Together
//! they're a direct, no-guessing check that the spectrum's own per-bin reduction is doing
//! power-per-Hz (density) rather than power-summed-over-a-widening-band, the exact bug
//! `cageq-monitor`'s `gaussian_power` had until it was caught live against a real analyzer showing
//! the correct pink slope and CAGEq showing flat instead. The four non-sine tone shapes are
//! synthesised as an exact band-limited Fourier sum (harmonics only up to just below Nyquist, at
//! each shape's textbook amplitude — 1/k for square/sawtooth, 1/k² for triangle, 2·d·sinc(k·d) for
//! pulse at its own fixed duty cycle `d` — see `Waveform::sample`'s own doc for why pulse's
//! spectrum shape differs from the other three), not generated naively (e.g. a hard comparison for
//! square, a wrapped ramp for sawtooth) — a naive version has harmonic content out to infinity, so
//! anything above Nyquist folds back down and contaminates the very spectrum shape this tool
//! exists to let you verify against a known-correct reference. So the loopback spectrum for, say,
//! `--square 220` should show *only* clean odd harmonics falling off at -6 dB/octave up to the
//! Nyquist-adjacent cutoff, and nothing else — `--pulse 220` should show *every* harmonic at close
//! to equal amplitude out to roughly the duty cycle's own reciprocal, then rolling off.
//!
//! `--rate <hz>` forces a *source* rate different from the device's — Windows' shared-mode
//! resampler (AUTOCONVERT) then converts it up/down to the device rate, so any resampling images/
//! aliasing show up in the loopback spectrum. Pair with a tone flag and a Dry slot to isolate it.
//!
//! Safety: the level is clamped to ≤ -3 dBFS, every sample is hard-limited just below full scale,
//! and it fades in (and out, with --seconds) — so even with EQ boosts stacked on top it can't
//! blast. `--unsafe` lifts both clamps for a deliberate full-scale (0 dBFS) torture test. Ctrl+C
//! stops it (abrupt; use --seconds for a clean fade-out).
//!
//! `--isp` always requires `--unsafe`: even at its gentlest (`--isp 0`, a true peak sitting right
//! at 0 dBTP) the sample-domain level it needs is -3.0103 dBFS — already past the default -3 dBFS
//! ceiling — and every `--isp` value above 0 needs a sample level closer to 0 dBFS still. There's
//! no in-between "safe" `--isp` setting the way there is for the other signals.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cageq_monitor::signal::{build_wavetable, ISP_MAX_OVER_DB, PinkNoise, Signal, Waveform};
    use std::time::Duration;
    use wasapi::{initialize_mta, Direction, SampleType, StreamMode, WaveFormat};

    // --- args (dependency-free parsing) ------------------------------------------------------
    let mut signal: Option<Signal> = None;
    let mut level_dbfs: f32 = -20.0;
    let mut seconds: Option<f32> = None;
    let mut device_match: Option<String> = None;
    let mut rate_override: Option<u32> = None;
    let mut unsafe_mode = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        // Shared by every signal flag: only one at a time, so a leftover --square from a
        // copy-pasted command line can't silently combine with a new --sine (or --pink) instead of
        // erroring.
        let mut set_signal = |sig: Signal| -> Result<(), Box<dyn std::error::Error>> {
            if signal.is_some() {
                return Err("only one of --sine/--square/--triangle/--sawtooth/--pulse/--pink/--white/--isp may be given".into());
            }
            signal = Some(sig);
            Ok(())
        };
        match a.as_str() {
            "--sine" => set_signal(Signal::Tone { waveform: Waveform::Sine, hz: args.next().ok_or("--sine needs a frequency")?.parse()? })?,
            "--square" => set_signal(Signal::Tone { waveform: Waveform::Square, hz: args.next().ok_or("--square needs a frequency")?.parse()? })?,
            "--triangle" => set_signal(Signal::Tone { waveform: Waveform::Triangle, hz: args.next().ok_or("--triangle needs a frequency")?.parse()? })?,
            "--sawtooth" => set_signal(Signal::Tone { waveform: Waveform::Sawtooth, hz: args.next().ok_or("--sawtooth needs a frequency")?.parse()? })?,
            "--pulse" => set_signal(Signal::Tone { waveform: Waveform::Pulse, hz: args.next().ok_or("--pulse needs a frequency")?.parse()? })?,
            "--pink" => set_signal(Signal::Pink)?,
            "--white" => set_signal(Signal::White)?,
            "--isp" => set_signal(Signal::Isp { db_over: args.next().ok_or("--isp needs a dB-over-0-dBFS true-peak target (0..=3.0103)")?.parse()? })?,
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
                eprintln!("usage: testtone [--sine|--square|--triangle|--sawtooth|--pulse <hz>] [--pink|--white] [--isp <db-over> --unsafe] [--level <dbfs>] [--rate <hz>] [--seconds <n>] [--device <name-substr>] [--unsafe]");
                return Ok(());
            }
            other => return Err(format!("unknown arg: {other}").into()),
        }
    }
    // No signal flag at all still means pink noise — same as it always has, just now also
    // reachable explicitly via --pink.
    let mut signal = signal.unwrap_or(Signal::Pink);
    if let Some(r) = rate_override {
        if !(8_000..=768_000).contains(&r) {
            return Err(format!("--rate {r} is out of the 8000..=768000 range").into());
        }
    }
    // §ISP: derive the sample-domain level from the requested true-peak overshoot (file header
    // doc), and refuse outright without --unsafe — there is no "safe" --isp setting (see the
    // safety doc above), so this is checked before the generic ceiling logic even gets a chance
    // to just clamp it down to something quieter and silently defeat the whole point.
    if let Signal::Isp { db_over: requested_over } = signal {
        if !unsafe_mode {
            return Err("--isp requires --unsafe — it deliberately drives the sample peak up near 0 dBFS to construct a true-peak-over (see the file header doc)".into());
        }
        let over = requested_over.clamp(0.0, ISP_MAX_OVER_DB);
        if (over - requested_over).abs() > 1e-9 {
            eprintln!(
                "[testtone] --isp {requested_over} is outside 0..={ISP_MAX_OVER_DB:.4} (the exact max for this construction) — clamping to {over:.4}."
            );
        }
        // Reassign with the clamped value so every downstream use (status line, wavetable) sees
        // the same number `level_dbfs` below was actually derived from.
        //
        // `level_dbfs` is set to `over` directly, NOT `over - ISP_MAX_OVER_DB` (an earlier,
        // wrong version of this did the latter). The table already carries the ±1/√2 amplitude
        // from the 45° phase offset, and the continuous sine's own peak is exactly the applied
        // *gain* regardless of which points get sampled — so gain-in-dB (`level_dbfs`) IS the
        // true peak in dBTP, directly, with no further offset. It's the discrete SAMPLE peak
        // that sits 3.0103 dB *below* `level_dbfs` (computed just for the log line below), not
        // the other way round. Confirmed against `--isp 3.0103 --unsafe`: `level_dbfs` = 3.0103
        // → gain = √2 → sample peak = √2 · (1/√2) = 1.0 (exactly 0 dBFS, the textbook case) →
        // true peak = √2 · 1.0 = +3.0103 dBTP, matching the request.
        signal = Signal::Isp { db_over: over };
        level_dbfs = over as f32;
        let sample_peak_dbfs = over - ISP_MAX_OVER_DB;
        eprintln!(
            "[testtone] --isp: true (reconstructed) peak targeted at {over:.4} dBTP, sample peak {sample_peak_dbfs:.4} dBFS; --level is ignored for this signal."
        );
    }
    // Clamp the level below full scale so a boosted EQ can't drive it into a blast — unless the
    // caller opts into a full-scale (0 dBFS) torture test with --unsafe. `--isp` is exempt: its
    // own clamp above (`0.0..=ISP_MAX_OVER_DB`) already bounds it, and there `level_dbfs` is the
    // *true*-peak target, which legitimately sits above 0 dBFS while the sample values it
    // actually produces never do (they top out at exactly 0 dBFS right at `--isp 3.0103`, per
    // the derivation above) — clamping it the same way as every other signal would silently
    // force every `--isp` request back down to the 0 dBTP boundary case, which is the exact bug
    // this exemption fixes.
    let level_ceil_dbfs: f32 = if unsafe_mode { 0.0 } else { -3.0 };
    if unsafe_mode {
        eprintln!("[testtone] --unsafe: full-scale (0 dBFS) allowed and the per-sample limiter is off — mind your ears/gear.");
    }
    let is_isp = matches!(signal, Signal::Isp { .. });
    if !is_isp && level_dbfs > level_ceil_dbfs {
        eprintln!("[testtone] level {level_dbfs} dBFS is above the {level_ceil_dbfs} dBFS ceiling — clamping.");
        level_dbfs = level_ceil_dbfs;
    }
    let gain = 10f32.powf((if is_isp { level_dbfs } else { level_dbfs.clamp(-80.0, level_ceil_dbfs) }) / 20.0);
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

    // Buffer duration: at least 100ms (BUFFER_FLOOR_HNS), not just the device's own bare
    // `get_device_period()` default. That default is a *latency* setting — some devices/drivers
    // report a shorter default period at higher sample rates (lower configured latency there),
    // and this stream is `StreamMode::PollingShared`, which the `wasapi` crate's own docs on
    // `initialize_client` call out as "less efficient and more prone to glitches when running at
    // low latency" (event-driven mode avoids that, but isn't worth the extra API surface here).
    // Reported live: every waveform, including plain sine (i.e. not a per-sample CPU-cost issue —
    // sine's own `Waveform::sample` is a single `sin()` call, no k_max loop to blow up), glitched
    // specifically above 96 kHz with no `--rate` override (so no AUTOCONVERT resampling involved
    // either — the device's own native mix rate was already >96kHz). A dev test-signal generator
    // has no reason to chase low latency at all — a fixed, generous floor trades a bit of startup/
    // stop delay (already covered by the fade in/out) for headroom against exactly this.
    const BUFFER_FLOOR_HNS: i64 = 1_000_000; // 100ms, in 100ns units
    let (def_period, _min_period) = audio_client.get_device_period()?;
    let buffer_duration_hns = def_period.max(BUFFER_FLOOR_HNS);
    let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns };
    audio_client
        .initialize_client(&desired, &Direction::Render, &mode)
        .map_err(|e| format!("initialize_client at {rate} Hz failed ({e:?}) — the endpoint may not accept this source rate"))?;
    let render = audio_client.get_audiorenderclient()?;
    eprintln!(
        "[testtone] device period: default {:.1}ms, using {:.1}ms, actual buffer {} frames",
        def_period as f64 / 10_000.0,
        buffer_duration_hns as f64 / 10_000.0,
        audio_client.get_buffer_size().unwrap_or(0)
    );

    let total_frames: Option<u64> = seconds.map(|s| (s * rate as f32) as u64);
    let fade_frames = (0.12 * rate as f32) as u64; // 120 ms fade in/out — kills startup/stop pops
    let resample = if rate != mix_rate { format!(" → resampled to {mix_rate} Hz by Windows") } else { String::new() };
    match signal {
        Signal::Tone { waveform, hz } => eprintln!("[testtone] {hz} Hz {} @ {level_dbfs} dBFS, source {rate} Hz / {channels} ch{resample}", waveform.name()),
        Signal::Pink => eprintln!("[testtone] pink noise @ ~{level_dbfs} dBFS, source {rate} Hz / {channels} ch{resample}"),
        Signal::White => eprintln!("[testtone] white noise @ ~{level_dbfs} dBFS, source {rate} Hz / {channels} ch{resample}"),
        Signal::Isp { db_over } => eprintln!(
            "[testtone] ISP torture: Fs/4 45°-phase sine, sample peak {:.4} dBFS → true peak {db_over:.4} dBTP, source {rate} Hz / {channels} ch{resample}",
            db_over - ISP_MAX_OVER_DB
        ),
    }
    eprintln!("[testtone] Ctrl+C to stop.");

    // Wavetable (Tone/Isp) — built once, walked by index; Pink/White are generated per-sample
    // below via `PinkNoise` instead (see `build_wavetable`'s own doc for why they don't share
    // this). Both come from `cageq_monitor::signal` now — see this file's header doc for why.
    let table = build_wavetable(signal, rate);
    let table_len = table.len();
    let mut table_idx: usize = 0;
    let mut noise = PinkNoise::new();

    // Deliberately after the (potentially slow — see `build_wavetable`'s own doc) wavetable build
    // above, not before: starting the stream first would leave the freshly-opened device buffer
    // starving while the table computes, an instant underrun/glitch right at startup instead of
    // the steady-state one this whole precomputation exists to avoid.
    audio_client.start_stream()?;
    let mut buf: Vec<u8> = Vec::new();

    let mut frame: u64 = 0;
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

            let mono = match signal {
                Signal::Tone { .. } | Signal::Isp { .. } => {
                    let s = table[table_idx] * gain;
                    table_idx += 1;
                    if table_idx >= table_len {
                        table_idx = 0;
                    }
                    s
                }
                Signal::White => noise.next_white() * gain,
                Signal::Pink => noise.next_pink() * gain,
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
