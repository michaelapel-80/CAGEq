//! A real network fetch against raw.githubusercontent — `csv_parse`/the full corpus
//! check (`full_corpus.rs`) validate parsing against local files only; this is the one
//! test that exercises `fetch_curve`'s actual HTTP + disk-cache path end to end.
//! Soft-skips on any network error rather than failing, since this machine's network
//! access isn't something this test suite should assume.

#[test]
fn fetches_and_caches_a_real_target_curve() {
    // A small, stable file unlikely to be renamed/removed.
    let path = "targets/Diffuse field 5128.csv";

    let cache_dir = std::env::temp_dir().join(format!("cageq-catalog-live-{}", std::process::id()));
    // SAFETY: single-threaded test process at this point (no other threads read env).
    unsafe { std::env::set_var("CAGEQ_CACHE_DIR", &cache_dir) };

    let first = match cageq_catalog::fetch_curve(path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping live fetch check: no network access ({e})");
            let _ = std::fs::remove_dir_all(&cache_dir);
            return;
        }
    };
    assert!(!first.0.is_empty(), "expected a non-empty frequency curve");
    assert_eq!(first.0.len(), first.1.len());

    // Second call should hit the on-disk cache, not the network — same content either way.
    let second = cageq_catalog::fetch_curve(path).expect("cached fetch");
    assert_eq!(first.0, second.0);
    assert_eq!(first.1, second.1);

    let _ = std::fs::remove_dir_all(&cache_dir);
}
