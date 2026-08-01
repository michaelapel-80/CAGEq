//! §5.3c post-EQ monitoring — WASAPI loopback capture SPIKE (throwaway).
//!
//! Goal: prove the load-bearing unknowns before building metrics/UI:
//!   1. can we open loopback on the render endpoint and negotiate a format?
//!   2. do frames actually flow (and go quiet, not hang, on silence)?
//!   3. are the numbers sane (near 0 dBFS on loud material, deep negative on quiet)?
//!
//! It opens the *default* render endpoint's loopback (endpoint-id mapping to the app's
//! selected device comes later) and prints peak/RMS dBFS ~10×/s for ~8 s, then exits.
//!
//! Loopback via the `wasapi` crate = get the Render device, initialize it in the *Capture*
//! direction with a Shared stream mode; the crate then sets AUDCLNT_STREAMFLAGS_LOOPBACK for
//! us. We use **polling** (not event) mode on purpose: event-driven loopback delivers no
//! events while the endpoint is idle, so a meter would stall on silence — polling just reads
//! zero frames and moves on.

use std::collections::VecDeque;
use std::error::Error;
use std::time::{Duration, Instant};

use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

type Res<T> = Result<T, Box<dyn Error>>;

const RUN_SECS: u64 = 8;
const REPORT_EVERY: Duration = Duration::from_millis(100);
const DB_FLOOR: f32 = -120.0;

fn dbfs(linear: f32) -> f32 {
    if linear <= 0.0 {
        DB_FLOOR
    } else {
        (20.0 * linear.log10()).max(DB_FLOOR)
    }
}

fn main() -> Res<()> {
    initialize_mta().ok()?;

    let enumerator = DeviceEnumerator::new()?;
    // The render endpoint — loopback taps the mix going *to* it (post-EQ, since EqualizerAPO
    // sits in this pipeline). Later this becomes the app's selected device by id.
    let device = enumerator.get_default_device(&Direction::Render)?;
    let name = device.get_friendlyname().unwrap_or_else(|_| "<unknown>".into());

    let mut audio_client = device.get_iaudioclient()?;

    // Match the endpoint's shared-mode mix rate/channels, but ask for f32 samples with
    // autoconvert so we always parse a known format regardless of the native store type.
    let mix = audio_client.get_mixformat()?;
    let rate = mix.get_samplespersec();
    let channels = mix.get_nchannels();
    println!(
        "endpoint: {name}\nmix format: {} Hz, {} ch, {}-bit {:?}",
        rate,
        channels,
        mix.get_bitspersample(),
        mix.get_subformat()
    );

    let desired = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, channels as usize, None);
    let bytes_per_frame = desired.get_blockalign() as usize;

    let (def_period, _min_period) = audio_client.get_device_period()?;
    let mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: def_period,
    };
    audio_client.initialize_client(&desired, &Direction::Capture, &mode)?;

    let capture = audio_client.get_audiocaptureclient()?;
    audio_client.start_stream()?;
    println!("capturing loopback for {RUN_SECS}s (play some audio to see levels)…\n");

    let mut queue: VecDeque<u8> = VecDeque::new();
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    let mut sample_count: u64 = 0;
    let mut had_signal = false;

    let start = Instant::now();
    let mut last_report = Instant::now();

    while start.elapsed() < Duration::from_secs(RUN_SECS) {
        // Drain whatever WASAPI has ready this tick (each call is one packet, 0 when idle).
        let before = queue.len();
        capture.read_from_device_to_deque(&mut queue)?;
        let got_data = queue.len() > before;

        // Consume whole frames; track peak (max |sample| across channels) and mean-square.
        while queue.len() >= bytes_per_frame {
            for _ in 0..channels {
                let b = [
                    queue.pop_front().unwrap(),
                    queue.pop_front().unwrap(),
                    queue.pop_front().unwrap(),
                    queue.pop_front().unwrap(),
                ];
                let s = f32::from_le_bytes(b);
                let a = s.abs();
                if a > peak {
                    peak = a;
                }
                sum_sq += (s as f64) * (s as f64);
                sample_count += 1;
            }
            had_signal = true;
        }

        if last_report.elapsed() >= REPORT_EVERY {
            if sample_count > 0 {
                let rms = (sum_sq / sample_count as f64).sqrt() as f32;
                println!("peak {:>7.1} dBFS   rms {:>7.1} dBFS", dbfs(peak), dbfs(rms));
            } else {
                println!("peak    --- dBFS   rms    --- dBFS   (no frames — silent endpoint?)");
            }
            peak = 0.0;
            sum_sq = 0.0;
            sample_count = 0;
            last_report = Instant::now();
        }

        if !got_data {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    audio_client.stop_stream()?;
    println!(
        "\ndone. frames flowed: {}",
        if had_signal { "yes" } else { "no (endpoint was idle the whole time)" }
    );
    Ok(())
}
