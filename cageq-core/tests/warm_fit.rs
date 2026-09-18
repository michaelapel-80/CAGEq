//! `Core::warm_fit` should populate the *same* fit cache `apply_to_slot` reads —
//! otherwise it's warming a cache nothing looks at (the bug this method was added to
//! fix: `cageq-app`'s `warm_fit` command used to call the sidecar's `calculate_filters`
//! directly, which stopped mattering the moment `fit_slot` moved off that RPC).
//!
//! Needs network access to fetch one real measurement CSV (`fetch_curve` isn't
//! network-mockable here); soft-skips on any fetch error, same convention as
//! `rust_vs_sidecar.rs`/`cageq-catalog`'s `live_fetch.rs`. Uses the lightweight Python
//! stub, not the real AutoEq sidecar — the fit itself no longer touches the sidecar at
//! all for a headphone-based request, so there's nothing for the real one to add here.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cageq_core::{CalcRequest, Core, EqApoBackend, EqBackend, Slot};
use cageq_sidecar::{Sidecar, SidecarError};
use cageq_watchdog::WatchdogConfig;

fn stub_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("cageq-sidecar").join("python").join("sidecar_stub.py")
}

fn works(cmd: &str) -> bool {
    Command::new(cmd).args(["-c", "print(1)"]).output().map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1").unwrap_or(false)
}

fn find_python() -> PathBuf {
    if let Ok(p) = std::env::var("CAGEQ_PYTHON") {
        return PathBuf::from(p);
    }
    if let Ok(out) = Command::new("py").args(["-c", "import sys;print(sys.executable)"]).output()
        && out.status.success()
    {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    for cand in ["python3", "python"] {
        if works(cand) {
            return PathBuf::from(cand);
        }
    }
    panic!("no working Python found; set CAGEQ_PYTHON to a python.exe");
}

fn healthy_spawner() -> impl Fn() -> Result<Sidecar, SidecarError> + Send + 'static {
    let (py, script) = (find_python(), stub_script());
    move || Sidecar::spawn(&py, &script)
}

fn fast_cfg() -> WatchdogConfig {
    WatchdogConfig {
        idle_interval: Duration::from_millis(300),
        idle_response: Duration::from_millis(250),
        busy_response: Duration::from_millis(300),
        tick: Duration::from_millis(40),
        restart_backoffs: vec![Duration::from_millis(30); 3],
    }
}

#[test]
fn warm_fit_populates_the_cache_apply_to_slot_reads() {
    let tmp = std::env::temp_dir().join(format!("cageq-core-warm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let backend: Arc<dyn EqBackend> = Arc::new(EqApoBackend::new(&tmp));
    let core = Core::start(backend, healthy_spawner(), fast_cfg(), None).expect("start core");

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
