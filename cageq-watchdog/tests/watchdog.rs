//! End-to-end watchdog tests. Each spawns a real Python stub (via cageq-sidecar),
//! wraps it in a Supervisor with *small* deadlines, and provokes one fault, then
//! asserts (a) the trip reason and (b) that the safe state actually hit disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cageq_sidecar::Sidecar;
use cageq_watchdog::{Mode, Supervisor, SupervisorError, TripReason, WatchdogConfig};
use serde_json::json;

// --- test rig -------------------------------------------------------------

/// The shared stub lives in the sidecar crate; reach it via the workspace root.
fn stub_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("cageq-sidecar")
        .join("python")
        .join("sidecar_stub.py")
}

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

/// RAII temp dir standing in for EqAPO's config directory.
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cageq-wd-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn cageq_txt(&self) -> PathBuf {
        self.0.join("cageq.txt")
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Short deadlines so a run is milliseconds, not seconds.
fn fast_cfg() -> WatchdogConfig {
    WatchdogConfig {
        idle_interval: Duration::from_millis(300),
        idle_response: Duration::from_millis(250),
        busy_response: Duration::from_millis(300),
        tick: Duration::from_millis(50),
    }
}

fn supervise(cageq: &Path) -> Supervisor {
    let sidecar = Sidecar::spawn(&find_python(), &stub_script()).expect("spawn stub");
    Supervisor::start(sidecar, fast_cfg(), cageq)
}

fn wrote_safe_state(path: &Path) -> bool {
    fs::read_to_string(path).map(|s| s.contains("Preamp: -120.0 dB")).unwrap_or(false)
}

// --- tests ----------------------------------------------------------------

#[test]
fn healthy_sidecar_never_trips() {
    let tmp = TempDir::new("healthy");
    let sup = supervise(&tmp.cageq_txt());

    // A normal request answers well inside the busy deadline.
    let v = sup.call("calculate_filters", json!({ "device": "DAC" })).expect("call ok");
    assert_eq!(v["device"], "DAC");

    // Sit idle across several heartbeat intervals; the pings must all succeed.
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(sup.tripped(), None, "healthy sidecar should not trip");
    assert!(!tmp.cageq_txt().exists(), "no safe state should have been written");
}

#[test]
fn busy_timeout_trips_and_writes_safe_state() {
    let tmp = TempDir::new("busy");
    let sup = supervise(&tmp.cageq_txt());

    // A call that sleeps well past busy_response (300 ms): the monitor must trip.
    let err = sup.call("sleep_ms", json!({ "ms": 1500 })).unwrap_err();
    match err {
        SupervisorError::Tripped(TripReason::Unresponsive { mode: Mode::Busy, .. }) => {}
        other => panic!("expected Tripped(Unresponsive{{Busy}}), got {other:?}"),
    }
    assert!(wrote_safe_state(&tmp.cageq_txt()), "safe state must be on disk after a trip");
}

#[test]
fn sidecar_crash_trips_with_exited() {
    let tmp = TempDir::new("crash");
    let sup = supervise(&tmp.cageq_txt());

    // `exit` terminates the child without replying -> EOF -> Exited.
    let _ = sup.call("exit", json!({ "code": 1 }));
    assert_eq!(sup.tripped(), Some(TripReason::SidecarExited));
    assert!(wrote_safe_state(&tmp.cageq_txt()));

    // Once tripped, further calls fail fast rather than touching the dead process.
    match sup.call("ping", json!(null)) {
        Err(SupervisorError::Tripped(TripReason::SidecarExited)) => {}
        other => panic!("expected fast Tripped after a trip, got {other:?}"),
    }
}
