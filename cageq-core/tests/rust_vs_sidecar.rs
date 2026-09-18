//! Old-vs-new comparison: the sidecar's `calculate_filters` (kept working in
//! `sidecar_dsp.py` as the reference implementation, even though `cageq-core` no longer
//! calls it in production — see `fit.rs`'s module doc) against the new Rust-driven
//! pipeline, on the same real input, through the real `autoeq` package.
//!
//! Needs the AutoEq venv; soft-skips without it, like `biquad_crosscheck.rs`. Uses a
//! directly-supplied synthetic `measurement` array (not a named headphone), so this
//! needs no network access and is fully reproducible.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cageq_core::{filter_curve_db, CalcRequest, Core, EqApoBackend, EqBackend, Slot};
use cageq_sidecar::Sidecar;
use cageq_watchdog::WatchdogConfig;
use serde_json::json;

fn sidecar_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("cageq-sidecar")
}

fn venv_python() -> Option<PathBuf> {
    let p = sidecar_manifest().join(".venv").join("Scripts").join("python.exe");
    p.exists().then_some(p)
}

fn dsp_script() -> PathBuf {
    sidecar_manifest().join("python").join("sidecar_dsp.py")
}

/// Same real-world timings `cageq-app` uses (`watchdog_cfg` in
/// `cageq-app/src-tauri/src/lib.rs`) — a real fit costs a Python cold-start plus
/// ~1-2 s, so a short test-only deadline would false-trip the watchdog mid-comparison.
fn watchdog_cfg() -> WatchdogConfig {
    WatchdogConfig {
        idle_interval: Duration::from_secs(10),
        idle_response: Duration::from_secs(5),
        busy_response: Duration::from_secs(25),
        tick: Duration::from_millis(200),
        restart_backoffs: vec![Duration::from_secs(2)],
    }
}

/// A deliberately non-trivial synthetic deviation (a bass bump, a midrange dip, a
/// treble peak) on a coarser-than-standard grid — realistic in shape and density for an
/// actual headphone measurement, so both pipelines have real work to do.
fn synthetic_measurement() -> serde_json::Value {
    let mut points = Vec::new();
    let mut f: f64 = 20.0;
    while f <= 20_000.0 {
        let log_f = f.log2();
        let raw = 4.0 * (-((log_f - 80f64.log2()).powi(2)) / (2.0 * 0.3 * 0.3)).exp()
            - 3.0 * (-((log_f - 3000f64.log2()).powi(2)) / (2.0 * 0.25 * 0.25)).exp()
            + 2.0 * (-((log_f - 8000f64.log2()).powi(2)) / (2.0 * 0.2 * 0.2)).exp();
        points.push(json!({"frequency": f, "raw_db": raw}));
        f *= 1.03;
    }
    serde_json::Value::Array(points)
}

fn rmse(a: &[f64], b: &[f64]) -> f64 {
    (a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / a.len() as f64).sqrt()
}

#[test]
fn rust_pipeline_matches_the_reference_sidecar_calculate_filters() {
    let Some(python) = venv_python() else {
        eprintln!("skipping rust-vs-sidecar check: no .venv (run the AutoEq setup to enable)");
        return;
    };
    let script = dsp_script();

    let tmp_dir = std::env::temp_dir().join(format!("cageq-core-rvs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let backend: Arc<dyn EqBackend> = Arc::new(EqApoBackend::new(&tmp_dir));

    let spawn = {
        let (py, sc) = (python.clone(), script.clone());
        move || Sidecar::spawn(&py, &sc)
    };
    let core = Core::start(backend, spawn, watchdog_cfg(), None).expect("start core");

    let measurement = synthetic_measurement();
    let (max_gain, peaking_filters) = (6.0, 8);

    // Reference: the live sidecar's own calculate_filters, called directly (the code
    // path cageq-core used before fit.rs existed).
    let old_params = json!({
        "device": "REF",
        "measurement": measurement,
        "max_gain": max_gain,
        "peaking_filters": peaking_filters,
    });
    let old_reply = core.request_with_deadline("calculate_filters", old_params, Duration::from_secs(20)).expect("calculate_filters");
    let old_filters: Vec<cageq_core::Filter> = serde_json::from_value(old_reply["filters"].clone()).expect("old filters");
    let old_g_target = old_reply["g_target_db"].as_f64().expect("old g_target_db");
    let old_g_peak = old_reply["g_max_peak_db"].as_f64().expect("old g_max_peak_db");

    // New: the same request through the real Core::apply_to_slot -> fit.rs ->
    // cageq-peq-solver, exercising fetch_raw_curves + the Rust FR-prep/fit/loudness
    // pipeline end to end, including the real write to cageq.txt.
    let mut req = CalcRequest::for_device("NEW");
    req.inputs.insert("measurement".into(), measurement);
    req.inputs.insert("max_gain".into(), json!(max_gain));
    req.inputs.insert("peaking_filters".into(), json!(peaking_filters));
    let applied = core.apply_to_slot(Slot::A, req).expect("apply via new pipeline");

    // Not bit-exact filters — a different SLSQP implementation need not land on the
    // identical point of a non-convex loss (same reasoning as `solver_fixtures.rs`).
    // Compare the actual cascades' shape and the derived loudness/peak instead.
    let grid = cageq_peq_solver::grid::standard_grid();
    let old_curve = filter_curve_db(&old_filters, &grid);
    let new_curve = filter_curve_db(&applied.filters, &grid);
    let curve_rmse = rmse(&old_curve, &new_curve);

    eprintln!("g_target_db  old {old_g_target:.4}  new {:.4}", applied.g_target_db);
    eprintln!("g_max_peak_db old {old_g_peak:.4}  new {:.4}", applied.g_max_peak_db);
    eprintln!("fitted-curve RMSE old-vs-new: {curve_rmse:.4} dB");

    assert!(curve_rmse < 1.5, "old and new fitted curves diverge by {curve_rmse:.4} dB RMSE (want < 1.5)");
    assert!((old_g_target - applied.g_target_db).abs() < 1.0, "loudness target diverges: old {old_g_target:.4} new {:.4}", applied.g_target_db);
    assert!((old_g_peak - applied.g_max_peak_db).abs() < 1.0, "peak diverges: old {old_g_peak:.4} new {:.4}", applied.g_max_peak_db);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
