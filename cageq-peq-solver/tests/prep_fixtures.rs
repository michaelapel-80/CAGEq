//! Ground-truth check for `crate::prep::prepare` and `crate::equalize::equalize`
//! against the real AutoEq FR-prep chain (`tests/fixtures_prep/*.json`, captured by
//! `fixtures/generate_prep_fixtures.py`). Companion to `solver_fixtures.rs`, which does
//! the same for the optimizer.

use std::path::PathBuf;

use cageq_peq_solver::{equalize, prepare};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    name: String,
    input: Input,
    output: Output,
}

#[derive(Deserialize)]
struct Input {
    measurement_f: Vec<f64>,
    measurement_raw: Vec<Option<f64>>,
    target_f: Vec<f64>,
    target_raw: Vec<f64>,
}

#[derive(Deserialize)]
struct Output {
    f: Vec<f64>,
    raw: Vec<f64>,
    target: Vec<f64>,
    error: Vec<f64>,
    smoothed: Vec<f64>,
    error_smoothed: Vec<f64>,
    equalization: Vec<f64>,
}

/// AutoEq's `DEFAULT_MAX_SLOPE`/CAGEq's `max_gain` default — the two parameters
/// `fixtures/generate_prep_fixtures.py`'s `fr.equalize(max_gain=6.0)` call uses.
const MAX_SLOPE: f64 = 18.0;
const MAX_GAIN: f64 = 6.0;

fn max_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max)
}

fn check_fixture(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    let fixture: Fixture = serde_json::from_str(&text).map_err(|e| format!("parsing {path:?}: {e}"))?;

    let measurement_raw: Vec<f64> = fixture.input.measurement_raw.iter().map(|v| v.unwrap_or(f64::NAN)).collect();
    let prepped = prepare(&fixture.input.measurement_f, &measurement_raw, &fixture.input.target_f, &fixture.input.target_raw);

    if prepped.f.len() != fixture.output.f.len() {
        return Err(format!("{}: grid length mismatch: rust {} vs python {}", fixture.name, prepped.f.len(), fixture.output.f.len()));
    }

    // All of this is deterministic, direct-formula math (linear interpolation, a
    // closed-form Savitzky-Golay filter) — unlike the optimizer, there's no
    // non-convex search to land differently, so parity is checked tightly rather than
    // with a fit-quality-style tolerance band.
    for (label, rust, python) in [
        ("f", &prepped.f, &fixture.output.f),
        ("raw", &prepped.raw, &fixture.output.raw),
        ("target", &prepped.target, &fixture.output.target),
        ("error", &prepped.error, &fixture.output.error),
        ("smoothed", &prepped.smoothed, &fixture.output.smoothed),
        ("error_smoothed", &prepped.error_smoothed, &fixture.output.error_smoothed),
    ] {
        let diff = max_abs_diff(rust, python);
        eprintln!("{:<22} {:<15} max abs diff {:.8}", fixture.name, label, diff);
        if diff > 1e-6 {
            return Err(format!("{}: '{}' diverges from python by {:.8} (>1e-6)", fixture.name, label, diff));
        }
    }

    let rust_equalization = equalize(&prepped.f, &prepped.error_smoothed, MAX_SLOPE, MAX_GAIN);
    let diff = max_abs_diff(&rust_equalization, &fixture.output.equalization);
    eprintln!("{:<22} {:<15} max abs diff {:.8}", fixture.name, "equalization", diff);
    if diff > 1e-6 {
        return Err(format!("{}: 'equalization' diverges from python by {:.8} (>1e-6)", fixture.name, diff));
    }
    Ok(())
}

#[test]
fn rust_prep_matches_python_exactly_on_every_fixture() {
    let dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures_prep");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading fixtures dir {dir:?} (run fixtures/generate_prep_fixtures.py first): {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures found in {dir:?} — run fixtures/generate_prep_fixtures.py first");

    let failures: Vec<String> = paths.iter().filter_map(|p| check_fixture(p).err()).collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
