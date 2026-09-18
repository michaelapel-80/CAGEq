//! Exhaustive ground-truth check for `csv_parse::parse_csv` against every single file
//! actually checked into AutoEq's `measurements/`/`targets/` directories — not a
//! sample. Fingerprints (`tests/fixtures/*_fingerprint.jsonl`) captured by
//! `tests/fixtures/dump_csv_fingerprints.py` against the real `autoeq.csv.parse_csv`.
//!
//! Needs a local AutoEq checkout (`$AUTOEQ_REPO_DIR`, default
//! `../../../GitHub/AutoEq` relative to this crate — i.e. a sibling of the `CAGE`
//! checkout); soft-skips without one, like the sidecar-venv checks elsewhere.

use std::path::{Path, PathBuf};

use cageq_catalog::parse_csv;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fingerprint {
    path: String,
    ok: bool,
    n: Option<usize>,
    f0: Option<f64>,
    f_last: Option<f64>,
    raw_sum: Option<f64>,
    raw_sumsq: Option<f64>,
}

fn autoeq_repo_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AUTOEQ_REPO_DIR") {
        return Some(PathBuf::from(p));
    }
    let guess = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("GitHub").join("AutoEq");
    guess.join("measurements").exists().then_some(guess)
}

fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

/// Checks one fingerprinted file, returning `Err(message)` on any mismatch — never
/// panicking directly, so the caller can collect every failure across the whole
/// corpus in one run instead of stopping at the first.
fn check_one(repo: &Path, fp: &Fingerprint) -> Result<(), String> {
    let full_path = repo.join(&fp.path);
    let bytes = std::fs::read(&full_path).map_err(|e| format!("{}: reading file: {e}", fp.path))?;
    let text = decode(&bytes);
    let result = parse_csv(text.trim());

    match (fp.ok, result) {
        (false, Err(_)) => Ok(()), // both sides refuse this file (e.g. the one multi-column reference target) — agreement
        (false, Ok((f, _))) => Err(format!("{}: python refused this file but rust parsed {} points", fp.path, f.len())),
        (true, Err(e)) => Err(format!("{}: python parsed it but rust errored: {e}", fp.path)),
        (true, Ok((f, raw))) => {
            let n = fp.n.unwrap();
            if f.len() != n || raw.len() != n {
                return Err(format!("{}: point count mismatch: python {n} vs rust f={} raw={}", fp.path, f.len(), raw.len()));
            }
            let (f0, f_last) = (fp.f0.unwrap(), fp.f_last.unwrap());
            if (f[0] - f0).abs() > 1e-6 || (f[n - 1] - f_last).abs() > 1e-6 {
                return Err(format!("{}: endpoint mismatch: python [{f0}, {f_last}] vs rust [{}, {}]", fp.path, f[0], f[n - 1]));
            }
            let raw_sum: f64 = raw.iter().sum();
            let raw_sumsq: f64 = raw.iter().map(|v| v * v).sum();
            // A fingerprint over ~hundreds of dB-scale values accumulates float error
            // in the 1e-6-ish range even between two *correct* summation orders —
            // 1e-3 is well above that noise floor and still tight enough to catch a
            // wrong column, a unit slip, or a dropped/duplicated row.
            if (raw_sum - fp.raw_sum.unwrap()).abs() > 1e-3 || (raw_sumsq - fp.raw_sumsq.unwrap()).abs() > 1e-3 {
                return Err(format!("{}: raw-value fingerprint mismatch: python sum={} sumsq={} vs rust sum={raw_sum} sumsq={raw_sumsq}", fp.path, fp.raw_sum.unwrap(), fp.raw_sumsq.unwrap()));
            }
            Ok(())
        }
    }
}

fn run_corpus(repo: &Path, fingerprint_file: &str) -> (usize, Vec<String>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(fingerprint_file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?} (run dump_csv_fingerprints.py first): {e}"));
    let mut checked = 0;
    let mut failures = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fp: Fingerprint = serde_json::from_str(line).unwrap_or_else(|e| panic!("parsing a fingerprint line: {e}"));
        checked += 1;
        if let Err(msg) = check_one(repo, &fp) {
            failures.push(msg);
        }
    }
    (checked, failures)
}

#[test]
fn rust_parser_matches_python_on_every_measurement_file() {
    let Some(repo) = autoeq_repo_dir() else {
        eprintln!("skipping full-corpus check: no local AutoEq checkout (set AUTOEQ_REPO_DIR)");
        return;
    };
    let (checked, failures) = run_corpus(&repo, "measurements_fingerprint.jsonl");
    eprintln!("checked {checked} measurement files, {} failures", failures.len());
    assert!(failures.is_empty(), "{} of {checked} measurement files diverged:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn rust_parser_matches_python_on_every_target_file() {
    let Some(repo) = autoeq_repo_dir() else {
        eprintln!("skipping full-corpus check: no local AutoEq checkout (set AUTOEQ_REPO_DIR)");
        return;
    };
    let (checked, failures) = run_corpus(&repo, "targets_fingerprint.jsonl");
    eprintln!("checked {checked} target files, {} failures", failures.len());
    assert!(failures.is_empty(), "{} of {checked} target files diverged:\n{}", failures.len(), failures.join("\n"));
}
