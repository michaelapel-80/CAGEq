//! Record a dry switch with known timing, so what the audio did can be read off a file
//! instead of argued about.
//!
//! Every synthetic test says the engine's dry switch is sample-continuous — including one
//! mirroring the reported case exactly. The report is a clean hard edge on the scope. This
//! removes both the modelling and the human timing from the question: it starts the loopback
//! recorder, waits, commands dry through the real control channel, waits, restores what was
//! there, and stops — then prints where in the file each switch happened.
//!
//! ```text
//! switchtest '{endpoint-guid}'            # record dry and back, default timings
//! switchtest '{endpoint-guid}' out.wav 400
//! ```
//!
//! Play a steady tone on that endpoint first. The recording is post-EQ loopback, i.e. exactly
//! what left the endpoint.

use std::time::{Duration, Instant};

use cageq_apo::channel::ControlChannel;
use cageq_apo::control::{self, RawCoeffs, ReadOutcome, Snapshot};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(endpoint) = args.first().and_then(|a| cageq_apo::config::normalize_endpoint_id(a))
    else {
        eprintln!("usage: switchtest <endpoint-guid> [out.wav] [segment-ms]");
        eprintln!("  quote the GUID: PowerShell parses an unquoted {{...}} as a script block");
        std::process::exit(2);
    };
    let out = args.get(1).cloned().unwrap_or_else(|| "switch.wav".into());
    let seg: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600);

    // Report the actual error rather than a guess. ERROR_FILE_NOT_FOUND (2) means no APO is
    // locked on that endpoint; ERROR_ACCESS_DENIED (5) means the section is there but this
    // process cannot reach it — completely different problems, and collapsing them into "no
    // channel" is a mistake this codebase has already made once.
    let ch = match ControlChannel::open(&endpoint) {
        Ok(ch) => ch,
        Err(2) => {
            eprintln!("no section CAGEqApo_{endpoint} exists.");
            eprintln!("The APO creates it at LockForProcess, so this means no APO instance is");
            eprintln!("locked on that endpoint — is audio playing on THIS device right now?");
            std::process::exit(1);
        }
        Err(5) => {
            eprintln!("access denied opening CAGEqApo_{endpoint} — the section EXISTS.");
            eprintln!("Same user as whatever else can reach it? Integrity level too low?");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("could not open CAGEqApo_{endpoint}: Win32 error {e}");
            std::process::exit(1);
        }
    };
    println!("opened CAGEqApo_{endpoint}");

    // Capture what is running now, so it can be put back afterwards. Without this the test
    // would leave the endpoint dry, which is a rude thing for a diagnostic to do.
    let mut snap = Snapshot::default();
    let ReadOutcome::Updated(_) = control::try_read(ch.block(), &mut snap) else {
        eprintln!("could not read the current correction; is CAGEq running and applied?");
        std::process::exit(1);
    };
    let wet: Vec<RawCoeffs> = snap.coeffs[..snap.band_count]
        .iter()
        .map(|c| RawCoeffs { b0: c.b0, b1: c.b1, b2: c.b2, a1: c.a1, a2: c.a2 })
        .collect();
    let wet_preamp = snap.preamp_db;
    println!("current: {} band(s), preamp {wet_preamp:.1} dB", wet.len());
    if wet.is_empty() {
        eprintln!("already dry — apply a correction first, or there is nothing to switch from");
        std::process::exit(1);
    }

    // The monitor reads this at capture start. Set before the capture thread exists, which is
    // the only point at which touching the environment is safe.
    unsafe { std::env::set_var("CAGEQ_RECORD", &out) };

    let idle = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let monitor = match cageq_monitor::Monitor::start(None, idle, |_| {}, |_| {}, |_| {}) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("could not start the loopback recorder: {e}");
            std::process::exit(1);
        }
    };

    // The capture thread opens the device and starts the stream before the first sample
    // lands; without this the "dry at" offsets below would be wrong by that startup cost.
    std::thread::sleep(Duration::from_millis(700));
    let t0 = Instant::now();

    let hold = Duration::from_millis(seg);
    std::thread::sleep(hold);

    let at_dry = t0.elapsed();
    // Dry as the app commands it: no filters, and the preamp the Dry slot carries.
    if !ch.publish(-9.0, &[], false) {
        eprintln!("publishing dry was refused");
    }
    std::thread::sleep(hold);

    let at_wet = t0.elapsed();
    if !ch.publish(wet_preamp, &wet, false) {
        eprintln!("restoring the correction was refused");
    }
    std::thread::sleep(hold);

    monitor.stop();
    // Give the capture thread its moment to patch the WAV header; a file whose sizes were
    // never written back will not open at all.
    std::thread::sleep(Duration::from_millis(300));

    println!("wrote {out}");
    println!("  -> dry  at ~{:.0} ms into the recording", at_dry.as_secs_f64() * 1000.0);
    println!("  -> back at ~{:.0} ms", at_wet.as_secs_f64() * 1000.0);
    println!("(offsets are from the first captured sample, +/- one buffer)");
}
