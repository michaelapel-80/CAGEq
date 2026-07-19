//! Integration test for the REAL AutoEq DSP sidecar (sidecar_dsp.py), driven
//! through the actual transport. Unlike the stub round-trip tests, this needs the
//! Python 3.10 venv with autoeq installed; it soft-skips if that venv isn't present
//! (other machines / CI won't have it), so the suite stays green everywhere while
//! actually exercising the real engine on a dev box.
//!
//! Driven at the transport level (a bare Sidecar, no watchdog) on purpose: importing
//! numpy/scipy/autoeq plus the optimisation takes a few seconds, and Sidecar::call
//! has no timeout — the watchdog's short deadlines would false-trip here.

use std::path::PathBuf;

use cageq_sidecar::Sidecar;
use serde_json::{json, Value};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The venv interpreter, or None (soft-skip) if the DSP env isn't set up here.
fn venv_python() -> Option<PathBuf> {
    let p = manifest().join(".venv").join("Scripts").join("python.exe");
    p.exists().then_some(p)
}

fn dsp_script() -> PathBuf {
    manifest().join("python").join("sidecar_dsp.py")
}

/// A synthetic headphone-ish measurement: flat with a +6 dB bump around 3 kHz.
fn synthetic_measurement() -> Vec<Value> {
    let mut points = Vec::new();
    let mut f = 20.0_f64;
    while f <= 20_000.0 {
        let raw = 6.0 * (-0.5 * ((f.log10() - 3000.0_f64.log10()) / 0.06).powi(2)).exp();
        points.push(json!({ "frequency": f, "raw_db": raw }));
        f *= 1.06;
    }
    points
}

#[test]
fn real_dsp_fits_parametric_filters() {
    let Some(python) = venv_python() else {
        eprintln!("skipping real_dsp test: no .venv (run the AutoEq setup to enable)");
        return;
    };

    let mut sc = Sidecar::spawn(&python, &dsp_script()).expect("spawn dsp sidecar");
    sc.ping().expect("ping (waits for the autoeq import)");

    let reply = sc
        .call("calculate_filters", json!({ "device": "Test DAC", "measurement": synthetic_measurement() }))
        .expect("calculate_filters");

    // DeviceConfig shape, 2 shelves + 8 peaking = 10 bands.
    assert_eq!(reply["device"], "Test DAC");
    let filters = reply["filters"].as_array().expect("filters array");
    assert_eq!(filters.len(), 10, "expected 2 shelves + 8 peaking");

    // Every kind is a valid Rust FilterType variant name.
    for f in filters {
        let kind = f["kind"].as_str().unwrap();
        assert!(matches!(kind, "LowShelf" | "HighShelf" | "Peaking"), "bad kind {kind}");
    }

    // The dominant correction is a cut (negative gain) targeting the ~3 kHz bump.
    let strongest = filters
        .iter()
        .min_by(|a, b| a["gain_db"].as_f64().unwrap().partial_cmp(&b["gain_db"].as_f64().unwrap()).unwrap())
        .unwrap();
    assert_eq!(strongest["kind"], "Peaking");
    assert!(strongest["gain_db"].as_f64().unwrap() < -3.0, "expected a real cut: {strongest}");
    let fc = strongest["freq_hz"].as_f64().unwrap();
    assert!((2000.0..4500.0).contains(&fc), "cut should sit near the 3 kHz bump, got {fc}");

    // Preamp is present and non-positive (headroom, never a boost).
    assert!(reply["preamp_db"].as_f64().unwrap() <= 0.0);
}
