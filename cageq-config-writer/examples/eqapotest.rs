//! Record an Equalizer APO switch, the same way `switchtest` records ours — so the two can be
//! compared with the same tool instead of by impression.
//!
//! CAGEq's own APO is reported as audibly worse than EqAPO on a 50 Hz sine, for both Dry<->A
//! and A<->B, and as a *swell* rather than a click. Shortening our crossfade did not change it,
//! so the cause is not the fade. This captures what EqAPO actually does, to be measured with
//! `wavscan` against the same numbers.
//!
//! ```text
//! eqapotest                    # dry and back, default timings
//! eqapotest eqapo.wav 600
//! ```
//!
//! Requires Equalizer APO installed and driving the endpoint, with a correction applied. Play
//! a steady tone first. The switch is performed the way EqAPO's own reload is triggered — by
//! rewriting cageq.txt — so the transition measured is EqAPO's, not ours.

use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = args.first().cloned().unwrap_or_else(|| "eqapo.wav".into());
    let seg: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(600);

    let Some(dir) = cageq_config_writer::detect_eqapo_config_dir() else {
        eprintln!("Equalizer APO not found (no ConfigPath or InstallPath in the registry)");
        std::process::exit(1);
    };
    let path = dir.join(cageq_config_writer::CAGEQ_FILENAME);
    let Ok(original) = std::fs::read_to_string(&path) else {
        eprintln!("could not read {} — has CAGEq applied a correction through EqAPO?", path.display());
        std::process::exit(1);
    };
    if original.lines().filter(|l| l.contains("Filter")).count() == 0 {
        eprintln!("{} has no filters — there is nothing to switch away from.", path.display());
        std::process::exit(1);
    }
    println!("using {}", path.display());

    // Set before the capture thread exists, which is the only point at which touching the
    // environment is safe.
    unsafe { std::env::set_var("CAGEQ_RECORD", &out) };
    let idle = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let monitor = match cageq_monitor::Monitor::start(None, idle, false, |_| {}, |_| {}, |_| {}) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("could not start the loopback recorder: {e}");
            std::process::exit(1);
        }
    };

    // The capture thread opens the device before the first sample lands.
    std::thread::sleep(Duration::from_millis(700));
    let t0 = Instant::now();
    let hold = Duration::from_millis(seg);
    std::thread::sleep(hold);

    // Dry, as CAGEq's EqAPO backend writes it: same device scope, preamp only, no filters.
    // EqAPO dedups identical *parsed* configs, so this has to differ in substance — it does.
    let device_line = original
        .lines()
        .find(|l| l.trim_start().starts_with("Device:"))
        .unwrap_or("Device: all");
    let dry = format!("{device_line}\r\nPreamp: -9.0 dB\r\n");

    let at_dry = t0.elapsed();
    if let Err(e) = std::fs::write(&path, dry.as_bytes()) {
        eprintln!("could not write dry config: {e}");
    }
    std::thread::sleep(hold);

    // Put back exactly what was there, byte for byte.
    let at_wet = t0.elapsed();
    if let Err(e) = std::fs::write(&path, original.as_bytes()) {
        eprintln!("could not restore {}: {e}", path.display());
        eprintln!("the original content is lost from this process — restore from CAGEq");
    }
    std::thread::sleep(hold);

    monitor.stop();
    // Let the capture thread patch the WAV header; a file whose sizes were never written back
    // will not open at all.
    std::thread::sleep(Duration::from_millis(300));

    println!("wrote {out}");
    println!("  -> dry  at ~{:.0} ms into the recording", at_dry.as_secs_f64() * 1000.0);
    println!("  -> back at ~{:.0} ms", at_wet.as_secs_f64() * 1000.0);
    println!("compare with:  wavscan {out}");
}
