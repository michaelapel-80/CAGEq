//! `Core::warm_fit` should populate the *same* fit cache `apply_to_slot` reads —
//! otherwise it's warming a cache nothing looks at (the bug this method was added to
//! fix: `cageq-app`'s `warm_fit` command used to call the sidecar's `calculate_filters`
//! directly, which stopped mattering the moment `fit_slot` moved off that RPC).
//!
//! Needs network access to fetch one real measurement CSV (`fetch_curve` isn't
//! network-mockable here); soft-skips on any fetch error, same convention as
//! `cageq-catalog`'s `live_fetch.rs`. No sidecar involved at all — `Core` doesn't spawn
//! one any more, and the fit itself doesn't touch one for a headphone-based request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cageq_core::{CalcRequest, Core, EqApoBackend, EqBackend, Slot};

#[test]
fn warm_fit_populates_the_cache_apply_to_slot_reads() {
    let tmp = std::env::temp_dir().join(format!("cageq-core-warm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let backend: Arc<dyn EqBackend> = Arc::new(EqApoBackend::new(&tmp));
    let core = Core::start(backend, None).expect("start core");

    let headphone = "measurements/oratory1990/data/over-ear/Sennheiser HD 6XX.csv".to_string();

    // Warm it, then apply with the exact same headphone/target/defaults — cache-key
    // parity with `fit.rs`'s `fit_key` (headphone/target/max_gain/peaking_filters/fs)
    // is what makes this a hit rather than a second cold fit.
    core.warm_fit(headphone.clone(), None);

    let mut req = CalcRequest::for_device("DAC");
    req.inputs.insert("headphone".into(), serde_json::Value::String(headphone));

    let started = Instant::now();
    let applied = match core.apply_to_slot(Slot::A, req) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping warm_fit check: apply failed, likely no network access ({e})");
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
    };
    let elapsed = started.elapsed();

    assert!(!applied.filters.is_empty(), "expected a real AutoEq fit, got no filters");
    // A cold SLSQP fit costs ~1-4 s (see fit.rs's module doc); a cache hit is a HashMap
    // lookup plus recombining curves — generous enough to absorb real machine variance
    // while still clearly failing if this silently fell back to re-fitting.
    assert!(elapsed < Duration::from_millis(500), "apply after warm_fit took {elapsed:?} — looks like a cache miss, not a hit");

    let _ = std::fs::remove_dir_all(&tmp);
}
