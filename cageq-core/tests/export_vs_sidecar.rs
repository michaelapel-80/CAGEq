//! Old-vs-new comparison for `Core::fit_export_eq`/`fit_fixed_band_eq` against the
//! sidecar's `fit_export_eq`/`fit_fixed_band_eq` (kept working in `sidecar_dsp.py` as
//! reference implementations — see `export.rs`'s module doc). The reference side
//! spawns a `cageq_sidecar::Sidecar` directly (no `Core`/watchdog involved). Needs the
//! AutoEq venv; soft-skips without it, like `rust_vs_sidecar.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use cageq_core::{filter_curve_db, Core, EqApoBackend, EqBackend, Filter, FilterType};
use cageq_sidecar::Sidecar;
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

/// A deliberately busy composed slot curve — a bass shelf, two peaking bands, a
/// treble shelf — so both fits have real shape to chase, not a trivial flat line.
fn sample_filters() -> Vec<Filter> {
    vec![
        Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 4.0, q: 0.7 },
        Filter { kind: FilterType::Peaking, freq_hz: 500.0, gain_db: -3.0, q: 1.2 },
        Filter { kind: FilterType::Peaking, freq_hz: 3000.0, gain_db: 5.0, q: 1.8 },
        Filter { kind: FilterType::HighShelf, freq_hz: 9000.0, gain_db: -4.0, q: 0.7 },
    ]
}

fn rmse(a: &[f64], b: &[f64]) -> f64 {
    (a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / a.len() as f64).sqrt()
}

/// The reference sidecar (called directly) plus a fresh `Core` (no sidecar of its own)
/// and its scratch temp dir — `None` if there's no local AutoEq venv to soft-skip on.
fn rig() -> Option<(Sidecar, Core, PathBuf)> {
    let python = venv_python()?;
    let script = dsp_script();
    let mut sidecar = Sidecar::spawn(&python, &script).expect("spawn dsp sidecar");
    sidecar.ping().expect("ping (waits for the autoeq import)");

    let tmp = std::env::temp_dir().join(format!("cageq-core-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let backend: Arc<dyn EqBackend> = Arc::new(EqApoBackend::new(&tmp));
    let core = Core::start(backend, None).expect("start core");
    Some((sidecar, core, tmp))
}

#[test]
fn rust_export_eq_matches_the_reference_sidecar() {
    let Some((mut sidecar, core, tmp)) = rig() else {
        eprintln!("skipping export-vs-sidecar check: no .venv (run the AutoEq setup to enable)");
        return;
    };
    let filters = sample_filters();
    let band_count = 8u32;

    let old_reply = sidecar.call("fit_export_eq", json!({"filters": filters, "band_count": band_count})).expect("fit_export_eq");
    let old_filters: Vec<Filter> = serde_json::from_value(old_reply["filters"].clone()).expect("old filters");
    let old_preamp = old_reply["preamp_db"].as_f64().expect("old preamp_db");

    let (new_filters, new_preamp) = core.fit_export_eq(&filters, band_count).expect("rust fit_export_eq");

    let grid = cageq_peq_solver::grid::standard_grid();
    let old_curve = filter_curve_db(&old_filters, &grid);
    let new_curve = filter_curve_db(&new_filters, &grid);
    let curve_rmse = rmse(&old_curve, &new_curve);
    eprintln!("export_eq: preamp old {old_preamp:.4} new {new_preamp:.4}  curve RMSE {curve_rmse:.4} dB");

    assert!(curve_rmse < 1.5, "export_eq curves diverge by {curve_rmse:.4} dB RMSE");
    assert!((old_preamp - new_preamp).abs() < 1.0, "export_eq preamp diverges: old {old_preamp:.4} new {new_preamp:.4}");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn rust_fixed_band_eq_matches_the_reference_sidecar_for_both_presets() {
    let Some((mut sidecar, core, tmp)) = rig() else {
        eprintln!("skipping fixed-band-vs-sidecar check: no .venv (run the AutoEq setup to enable)");
        return;
    };
    let filters = sample_filters();

    for preset in ["10", "31"] {
        let old_reply = sidecar
            .call("fit_fixed_band_eq", json!({"filters": filters, "preset": preset}))
            .unwrap_or_else(|e| panic!("fit_fixed_band_eq preset {preset}: {e}"));
        let old_filters: Vec<Filter> = serde_json::from_value(old_reply["filters"].clone()).expect("old filters");
        let old_preamp = old_reply["preamp_db"].as_f64().expect("old preamp_db");

        let (new_filters, new_preamp) = core.fit_fixed_band_eq(&filters, preset).unwrap_or_else(|e| panic!("rust fit_fixed_band_eq preset {preset}: {e}"));
        assert_eq!(new_filters.len(), old_filters.len(), "preset {preset}: band count mismatch");

        let grid = cageq_peq_solver::grid::standard_grid();
        let old_curve = filter_curve_db(&old_filters, &grid);
        let new_curve = filter_curve_db(&new_filters, &grid);
        let curve_rmse = rmse(&old_curve, &new_curve);
        eprintln!("fixed_band[{preset}]: preamp old {old_preamp:.4} new {new_preamp:.4}  curve RMSE {curve_rmse:.4} dB");

        assert!(curve_rmse < 1.5, "preset {preset}: curves diverge by {curve_rmse:.4} dB RMSE");
        assert!((old_preamp - new_preamp).abs() < 1.0, "preset {preset}: preamp diverges: old {old_preamp:.4} new {new_preamp:.4}");
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
