//! Ground-truth check for `crate::loudness` against the live sidecar's own
//! `loudness_target_db` (`tests/fixtures_loudness/*.json`, captured by
//! `fixtures/generate_loudness_fixtures.py`).

use std::path::PathBuf;

use cageq_peq_solver::{curve_peak_db, loudness_target_db};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    name: String,
    input: Input,
    output: Output,
}

#[derive(Deserialize)]
struct Input {
    f: Vec<f64>,
    curve: Vec<f64>,
}

#[derive(Deserialize)]
struct Output {
    g_target_db: f64,
    g_max_peak_db: f64,
}

fn check_fixture(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    let fixture: Fixture = serde_json::from_str(&text).map_err(|e| format!("parsing {path:?}: {e}"))?;

    let g_target = loudness_target_db(&fixture.input.f, &fixture.input.curve, 48_000.0);
    let g_peak = curve_peak_db(&fixture.input.curve);
    eprintln!("{:<26} g_target rust {:>9.6} python {:>9.6}   g_peak rust {:>9.6} python {:>9.6}", fixture.name, g_target, fixture.output.g_target_db, g_peak, fixture.output.g_max_peak_db);

    if (g_target - fixture.output.g_target_db).abs() > 1e-6 {
        return Err(format!("{}: g_target_db {:.8} diverges from python {:.8}", fixture.name, g_target, fixture.output.g_target_db));
    }
    if (g_peak - fixture.output.g_max_peak_db).abs() > 1e-9 {
        return Err(format!("{}: g_max_peak_db {:.8} diverges from python {:.8}", fixture.name, g_peak, fixture.output.g_max_peak_db));
    }
    Ok(())
}

#[test]
fn rust_loudness_matches_python_exactly_on_every_fixture() {
    let dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures_loudness");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading fixtures dir {dir:?} (run fixtures/generate_loudness_fixtures.py first): {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures found in {dir:?} — run fixtures/generate_loudness_fixtures.py first");

    let failures: Vec<String> = paths.iter().filter_map(|p| check_fixture(p).err()).collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
