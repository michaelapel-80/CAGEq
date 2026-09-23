//! Ground-truth check against the real AutoEq optimizer (`peq.py`, captured into
//! `tests/fixtures/*.json` by `fixtures/generate_fixtures.py`) — not just the
//! individual formulas, which `src/filter.rs`/`src/peaks.rs` already unit-test in
//! isolation. See the crate-level doc comment: nothing in this crate should be trusted
//! to replace the sidecar until this suite is green.
//!
//! Needs the `slsqp` feature (the default) — there is no optimizer to test without it,
//! so this file (and therefore its dependency on `serde`/`serde_json`) is compiled out
//! entirely under `--no-default-features`.
#![cfg(feature = "slsqp")]

use std::path::{Path, PathBuf};

use cageq_peq_solver::{Band, BandKind, Solver};
use serde::Deserialize;

#[derive(Deserialize)]
struct InputBand {
    #[serde(rename = "type")]
    kind: String,
    fc: Option<f64>,
    q: Option<f64>,
    gain: Option<f64>,
}

#[derive(Deserialize)]
struct OutputBand {
    #[serde(rename = "type")]
    kind: String,
    fc: f64,
    q: f64,
    gain: f64,
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    input: FixtureInput,
    output: FixtureOutput,
}

#[derive(Deserialize)]
struct FixtureInput {
    f: Vec<f64>,
    fs: f64,
    target: Vec<f64>,
    bands: Vec<InputBand>,
}

#[derive(Deserialize)]
struct FixtureOutput {
    bands: Vec<OutputBand>,
    loss: f64,
}

fn kind_of(s: &str) -> BandKind {
    match s {
        "PEAKING" => BandKind::Peaking,
        "LOW_SHELF" => BandKind::LowShelf,
        "HIGH_SHELF" => BandKind::HighShelf,
        other => panic!("unknown band type {other}"),
    }
}

/// `generate_fixtures.py`'s `cageq_config()` only ever produces fully-free bands (no
/// fc/q/gain given) or bands with fc+q fixed and gain free — the two shapes
/// `cageq_default_bands` itself builds. Anything else means the fixture generator
/// changed shape without this test being updated to match.
fn band_from_input(b: &InputBand) -> Band {
    let kind = kind_of(&b.kind);
    match (b.fc, b.q, b.gain) {
        (Some(fc), Some(q), None) => Band::fixed_fc_q(kind, fc, q),
        (None, None, None) => Band::free(kind),
        _ => panic!("unexpected fixture band shape: fc={:?} q={:?} gain={:?}", b.fc, b.q, b.gain),
    }
}

fn python_fitted_curve(f: &[f64], fs: f64, bands: &[OutputBand]) -> Vec<f64> {
    let bands: Vec<Band> = bands
        .iter()
        .map(|b| {
            let kind = kind_of(&b.kind);
            Band { fc: b.fc, q: b.q, gain: b.gain, optimize_fc: false, optimize_q: false, optimize_gain: false, ..Band::free(kind) }
        })
        .collect();
    Solver::new(f.to_vec(), fs, bands, vec![0.0; f.len()]).fr()
}

fn rmse(a: &[f64], b: &[f64]) -> f64 {
    (a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / a.len() as f64).sqrt()
}

fn check_fixture(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    let fixture: Fixture = serde_json::from_str(&text).map_err(|e| format!("parsing {path:?}: {e}"))?;

    let bands: Vec<Band> = fixture.input.bands.iter().map(band_from_input).collect();
    let mut solver = Solver::new(fixture.input.f.clone(), fixture.input.fs, bands, fixture.input.target.clone());
    let report = solver.optimize().map_err(|e| format!("{}: optimize failed: {e}", fixture.name))?;

    let rust_fr = solver.fr();
    let python_fr = python_fitted_curve(&fixture.input.f, fixture.input.fs, &fixture.output.bands);
    let rust_rmse = rmse(&rust_fr, &fixture.input.target);
    let python_rmse = rmse(&python_fr, &fixture.input.target);
    eprintln!(
        "{:<22} rust loss {:>10.6}  python loss {:>10.6}   rust rmse {:>7.4}  python rmse {:>7.4}",
        fixture.name, report.autoeq_loss, fixture.output.loss, rust_rmse, python_rmse
    );

    // Not bit-exact filter parameters — a different SLSQP implementation need not land
    // on the identical point of a non-convex loss (see optimizer.rs's module doc on the
    // early-stop/best-tracking divergence). What has to hold is fit *quality*: the loss
    // Rust reaches should be in the same ballpark as Python's, not dramatically worse.
    // 50% slack plus a small floor absorbs near-zero-loss cases (`flat.json`) where a
    // tiny absolute difference is a huge ratio.
    let loss_ceiling = fixture.output.loss * 1.5 + 1e-4;
    // `autoeq_loss`, not `loss`: the Rust solve also minimises a cancellation penalty that
    // Python AutoEq has no counterpart for, so only AutoEq's own loss compares like with like.
    if report.autoeq_loss > loss_ceiling {
        return Err(format!(
            "{}: rust loss {:.6} exceeds python loss {:.6} by more than the 1.5x+1e-4 slack",
            fixture.name, report.autoeq_loss, fixture.output.loss
        ));
    }

    // Cross-check from the curve side too: Rust's fitted response should track the
    // target about as well as Python's fitted response does.
    if rust_rmse > python_rmse * 1.5 + 0.05 {
        return Err(format!(
            "{}: rust curve RMSE {:.4} dB exceeds python's {:.4} dB by more than the 1.5x+0.05dB slack",
            fixture.name, rust_rmse, python_rmse
        ));
    }
    Ok(())
}

#[test]
fn rust_solver_matches_python_fit_quality_on_every_fixture() {
    let dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading fixtures dir {dir:?} (run fixtures/generate_fixtures.py first): {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures found in {dir:?} — run fixtures/generate_fixtures.py first");

    let failures: Vec<String> = paths.iter().filter_map(|p| check_fixture(p).err()).collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
