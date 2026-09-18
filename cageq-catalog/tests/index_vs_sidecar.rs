//! Old-vs-new comparison for `cageq_catalog::index` against the live sidecar's own
//! `build_index`/`list_targets` (`tests/fixtures/*_fingerprint.json`, captured by
//! `tests/fixtures/dump_index_fingerprint.py` against the real GitHub API — not a
//! mock). Both this test and the fixture dump hit the real API on their own runs, so
//! this also serves as a smoke test that the catalogue hasn't changed shape.
//!
//! Needs network access; soft-skips on any fetch error. Uses a throwaway cache dir so
//! this always does a real fresh build.

use std::collections::HashSet;
use std::path::PathBuf;

use cageq_catalog::index::{build_index, list_targets, HeadphoneEntry, TargetEntry};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

fn fresh_cache_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cageq-catalog-index-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn rust_headphone_index_matches_the_reference_sidecar() {
    let fixture_path = fixtures_dir().join("headphones_fingerprint.json");
    let Ok(text) = std::fs::read_to_string(&fixture_path) else {
        eprintln!("skipping index check: no fixture (run dump_index_fingerprint.py first)");
        return;
    };
    let expected: Vec<HeadphoneEntry> = serde_json::from_str(&text).expect("parsing fingerprint");

    let cache = fresh_cache_dir("hp");
    // SAFETY: single-threaded test process at this point.
    unsafe { std::env::set_var("CAGEQ_CACHE_DIR", &cache) };
    let actual = match build_index(true) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping index check: no network access ({e})");
            let _ = std::fs::remove_dir_all(&cache);
            return;
        }
    };

    eprintln!("python: {} headphones, rust: {} headphones", expected.len(), actual.len());
    let expected_set: HashSet<_> = expected.iter().map(|h| (&h.source, &h.form_factor, &h.name, &h.path, &h.rig)).collect();
    let actual_set: HashSet<_> = actual.iter().map(|h| (&h.source, &h.form_factor, &h.name, &h.path, &h.rig)).collect();

    let missing: Vec<_> = expected_set.difference(&actual_set).take(10).collect();
    let extra: Vec<_> = actual_set.difference(&expected_set).take(10).collect();
    assert!(missing.is_empty() && extra.is_empty(), "headphone index diverges — missing (python has, rust doesn't, first 10): {missing:?}\nextra (rust has, python doesn't, first 10): {extra:?}");

    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn rust_target_list_matches_the_reference_sidecar() {
    let fixture_path = fixtures_dir().join("targets_index_fingerprint.json");
    let Ok(text) = std::fs::read_to_string(&fixture_path) else {
        eprintln!("skipping index check: no fixture (run dump_index_fingerprint.py first)");
        return;
    };
    let expected: Vec<TargetEntry> = serde_json::from_str(&text).expect("parsing fingerprint");

    let cache = fresh_cache_dir("targets");
    unsafe { std::env::set_var("CAGEQ_CACHE_DIR", &cache) };
    let actual = match list_targets(true) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping target-list check: no network access ({e})");
            let _ = std::fs::remove_dir_all(&cache);
            return;
        }
    };

    eprintln!("python: {} targets, rust: {} targets", expected.len(), actual.len());
    let expected_set: HashSet<_> = expected.iter().map(|t| (&t.name, &t.path)).collect();
    let actual_set: HashSet<_> = actual.iter().map(|t| (&t.name, &t.path)).collect();
    assert_eq!(expected_set, actual_set, "target list diverges from the reference sidecar");

    let _ = std::fs::remove_dir_all(&cache);
}
