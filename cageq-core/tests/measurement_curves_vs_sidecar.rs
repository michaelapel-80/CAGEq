//! Old-vs-new comparison for `Core::measurement_curves` against the sidecar's own
//! `measurement_curves` (kept working in `sidecar_dsp.py` as the reference — see
//! `catalog.rs`'s module doc). The reference side spawns a `cageq_sidecar::Sidecar`
//! directly (no `Core`/watchdog involved). Uses a real headphone/target from the
//! AutoEq catalogue, so this needs network access; soft-skips on any error.

use std::path::PathBuf;
use std::sync::Arc;

use cageq_core::{Core, EqApoBackend, EqBackend};
use cageq_sidecar::Sidecar;
use serde_json::json;

fn sidecar_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("cageq-sidecar")
}

fn venv_python() -> Option<PathBuf> {
    let p = sidecar_manifest().join(".venv").join("Scripts").join("python.exe");
    p.exists().then_some(p)
}

#[test]
fn rust_measurement_curves_matches_the_reference_sidecar() {
    let Some(python) = venv_python() else {
        eprintln!("skipping measurement_curves check: no .venv (run the AutoEq setup to enable)");
        return;
    };
    let script = sidecar_manifest().join("python").join("sidecar_dsp.py");

    let tmp = std::env::temp_dir().join(format!("cageq-core-meascurves-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let backend: Arc<dyn EqBackend> = Arc::new(EqApoBackend::new(&tmp));
    let core = Core::start(backend, None).expect("start core");

    let headphone = "measurements/oratory1990/data/over-ear/Sennheiser HD 6XX.csv";
    let target = "targets/Harman over-ear 2018.csv";

    let mut sidecar = Sidecar::spawn(&python, &script).expect("spawn dsp sidecar");
    sidecar.ping().expect("ping (waits for the autoeq import)");
    let old_reply = match sidecar.call("measurement_curves", json!({"headphone": headphone, "target": target})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping measurement_curves check: no network access ({e})");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
    };
    let old_raw: Vec<(f64, f64)> = old_reply["raw_curve"].as_array().unwrap().iter().map(|p| (p["f"].as_f64().unwrap(), p["db"].as_f64().unwrap())).collect();
    let old_target: Vec<(f64, f64)> = old_reply["target_curve"].as_array().unwrap().iter().map(|p| (p["f"].as_f64().unwrap(), p["db"].as_f64().unwrap())).collect();

    let (new_raw, new_target) = core.measurement_curves(headphone, Some(target)).expect("rust measurement_curves");
    let new_raw: Vec<(f64, f64)> = new_raw.iter().map(|p| (p.f, p.db)).collect();
    let new_target: Vec<(f64, f64)> = new_target.iter().map(|p| (p.f, p.db)).collect();

    assert_eq!(old_raw.len(), new_raw.len(), "raw_curve point count mismatch");
    assert_eq!(old_target.len(), new_target.len(), "target_curve point count mismatch");

    let max_diff = |a: &[(f64, f64)], b: &[(f64, f64)]| -> f64 { a.iter().zip(b).map(|((_, ay), (_, by))| (ay - by).abs()).fold(0.0, f64::max) };
    let raw_diff = max_diff(&old_raw, &new_raw);
    let target_diff = max_diff(&old_target, &new_target);
    eprintln!("raw_curve max diff: {raw_diff:.6} dB, target_curve max diff: {target_diff:.6} dB");

    assert!(raw_diff < 1e-2, "raw_curve diverges from python by {raw_diff:.6} dB");
    assert!(target_diff < 1e-2, "target_curve diverges from python by {target_diff:.6} dB");

    let _ = std::fs::remove_dir_all(&tmp);
}
