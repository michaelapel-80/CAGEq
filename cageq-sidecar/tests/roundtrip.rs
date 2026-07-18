//! End-to-end transport tests: spawn the real Python stub and talk to it.
//!
//! Unlike the config-writer's pure unit tests, these need a working Python. The
//! sidecar is a Python component by design, so requiring an interpreter here is
//! fair. Discovery order: `CAGEQ_PYTHON` env override -> the Windows `py` launcher
//! resolving the real interpreter -> `python3`/`python` on PATH (validated, so we
//! don't fall for the Microsoft Store stub aliases that only print a nag).

use std::path::PathBuf;
use std::process::Command;

use cageq_sidecar::{Sidecar, SidecarError};
use serde_json::{json, Value};

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python").join("sidecar_stub.py")
}

/// Best-effort: run `<cand> -c "print(1)"` and confirm it actually prints "1"
/// (the Store stubs exit nonzero / print a nag instead).
fn works(cmd: &str) -> bool {
    Command::new(cmd)
        .args(["-c", "print(1)"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

fn find_python() -> PathBuf {
    if let Ok(p) = std::env::var("CAGEQ_PYTHON") {
        return PathBuf::from(p);
    }
    // Prefer the real interpreter the `py` launcher points at (Windows).
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

fn spawn() -> Sidecar {
    Sidecar::spawn(&find_python(), &script_path()).expect("spawn sidecar stub")
}

#[test]
fn ping_round_trips() {
    let mut sc = spawn();
    sc.ping().expect("ping should get {pong:true}");
}

#[test]
fn calculate_filters_returns_deviceconfig_shape() {
    let mut sc = spawn();
    let v: Value = sc.call("calculate_filters", json!({ "device": "Test DAC" })).unwrap();
    // The device we asked for is echoed, and the canned filter set comes back in
    // the DeviceConfig shape the config-writer consumes.
    assert_eq!(v["device"], "Test DAC");
    assert_eq!(v["preamp_db"], -6.5);
    let filters = v["filters"].as_array().expect("filters array");
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0]["kind"], "LowShelf");
    assert_eq!(filters[1]["kind"], "Peaking");
}

#[test]
fn many_calls_reuse_one_process_and_ids_advance() {
    // The whole point of a *persistent* sidecar: no per-call process spawn.
    let mut sc = spawn();
    for i in 0..5 {
        let v = sc.call("calculate_filters", json!({ "device": format!("dev{i}") })).unwrap();
        assert_eq!(v["device"], format!("dev{i}"));
    }
}

#[test]
fn unknown_method_maps_to_remote_error() {
    let mut sc = spawn();
    match sc.call("does_not_exist", Value::Null) {
        Err(SidecarError::Remote { code, .. }) => assert_eq!(code, -32601),
        other => panic!("expected a Remote(-32601) error, got {other:?}"),
    }
}

#[test]
fn shutdown_then_drop_is_clean() {
    let sc = spawn();
    sc.shutdown(); // asks the stub to exit; Drop reaps whatever is left
}
