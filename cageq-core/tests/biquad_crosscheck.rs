//! Cross-language guard for the three copies of the biquad response model.
//!
//! The curve maths lives in three places on purpose (filter.md §5.3a): AutoEq's
//! `peq.py` (the fit, the reference), `biquad.ts` (the §5.2 chart), and `morph.rs` (the
//! tonal-morph metric, which must run without the sidecar — a slot switch can't call
//! Python, and if the sidecar is unhealthy there's nothing to call). Three copies means
//! three things can silently disagree about what's being written and drawn.
//!
//! `biquad.ts` was verified numerically identical to `peq.py` when it was written; this
//! pins the Rust copy against the *live* `peq.py` (through the real sidecar's
//! `filter_response`), point-for-point on a shared grid — so Rust ↔ Python is guarded
//! here and TS ↔ Python covers the third edge transitively.
//!
//! Needs the AutoEq venv; soft-skips without it (like the sidecar's own real-DSP test)
//! so CI and other machines stay green.

use std::path::PathBuf;

use cageq_core::{Filter, FilterType, Sidecar};
use serde_json::json;

fn sidecar_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("cageq-sidecar")
}

/// The AutoEq venv interpreter, or None (soft-skip) if the DSP env isn't set up here.
fn venv_python() -> Option<PathBuf> {
    let p = sidecar_manifest().join(".venv").join("Scripts").join("python.exe");
    p.exists().then_some(p)
}

fn dsp_script() -> PathBuf {
    sidecar_manifest().join("python").join("sidecar_dsp.py")
}

/// A log-spaced grid, matching the audio-standard axis both copies target.
fn log_grid(points: usize, f_min: f64, f_max: f64) -> Vec<f64> {
    let ratio = (f_max / f_min).ln();
    (0..points).map(|i| f_min * (ratio * i as f64 / (points - 1) as f64).exp()).collect()
}

#[test]
fn rust_biquad_matches_the_reference_python_peq() {
    let Some(python) = venv_python() else {
        eprintln!("skipping biquad cross-check: no .venv (run the AutoEq setup to enable)");
        return;
    };
    let mut sc = Sidecar::spawn(&python, &dsp_script()).expect("spawn dsp sidecar");
    sc.ping().expect("ping (waits for the autoeq import)");

    // A deliberately varied set: both shelves, peaks high and low, wide and narrow Q,
    // boosts and cuts — anything the tonal morph might interpolate through.
    let peak = |freq_hz: f64, gain_db: f64, q: f64| Filter { kind: FilterType::Peaking, freq_hz, gain_db, q };
    let bands = vec![
        Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 5.0, q: 0.7 },
        Filter { kind: FilterType::HighShelf, freq_hz: 4000.0, gain_db: 4.0, q: 0.7 },
        peak(60.0, 3.0, 0.5),
        peak(1000.0, 6.0, 1.0),
        peak(3000.0, -4.0, 2.5),
        peak(12000.0, -6.0, 4.0),
    ];
    let freqs = log_grid(96, 20.0, 20_000.0);

    // Reference: the live AutoEq peq.py, through the real sidecar.
    let reply = sc
        .call("filter_response", json!({ "filters": bands, "freqs": freqs, "fs": 48000 }))
        .expect("filter_response");
    let py: Vec<f64> = reply["db"]
        .as_array()
        .expect("db array")
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();

    // Our copy.
    let rust = cageq_core::filter_curve_db(&bands, &freqs);
    assert_eq!(py.len(), rust.len(), "grid length mismatch");

    // Same model both ways, so agreement is exact down to the sidecar's 6-decimal
    // rounding (~5e-7 dB measured). 1e-4 sits ~200x above that floor yet far below any
    // real formula divergence (a wrong Q convention or shelf form is a dB-scale error).
    let mut worst = 0.0_f64;
    let mut worst_at = 0.0;
    for (i, (&p, &r)) in py.iter().zip(&rust).enumerate() {
        let d = (p - r).abs();
        if d > worst {
            worst = d;
            worst_at = freqs[i];
        }
    }
    assert!(worst < 1e-4, "Rust biquad diverges from peq.py by {worst:.6} dB at {worst_at:.0} Hz");

    // Sanity that the reference isn't trivially flat (which would make the check vacuous).
    let span = py.iter().cloned().fold(f64::MIN, f64::max) - py.iter().cloned().fold(f64::MAX, f64::min);
    assert!(span > 5.0, "reference curve should actually vary (got {span:.2} dB span)");
}
