//! End-to-end watchdog + recovery tests. Each spawns real Python stub processes
//! (via cageq-sidecar) with small deadlines/backoffs, provokes a fault, and asserts
//! on the health state machine and the on-disk safe state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cageq_sidecar::{Sidecar, SidecarError};
use cageq_watchdog::{Health, Supervisor, SupervisorError, WatchdogConfig};
use serde_json::json;

// --- test rig -------------------------------------------------------------

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

/// A spawn closure that always produces a healthy stub sidecar.
fn healthy_spawner() -> impl Fn() -> Result<Sidecar, SidecarError> + Send + 'static {
    let (py, script) = (find_python(), stub_script());
    move || Sidecar::spawn(&py, &script)
}

/// A spawn closure that succeeds only on the invocations for which `ok(index)` is
/// true (0-based, counting every call including the initial one), else returns a
/// spawn error. Lets tests script "restart keeps failing" and "succeeds on retry".
fn counting_spawner(ok: impl Fn(usize) -> bool + Send + 'static) -> impl Fn() -> Result<Sidecar, SidecarError> + Send + 'static {
    let (py, script) = (find_python(), stub_script());
    let n = Arc::new(AtomicUsize::new(0));
    move || {
        let i = n.fetch_add(1, Ordering::SeqCst);
        if ok(i) {
            Sidecar::spawn(&py, &script)
        } else {
            Err(SidecarError::Spawn(std::io::Error::new(std::io::ErrorKind::NotFound, "simulated spawn failure")))
        }
    }
}

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

/// Small deadlines/backoffs so a run is ms, not the 5s/2s/15s + 2/5/10s defaults.
fn fast_cfg() -> WatchdogConfig {
    WatchdogConfig {
        idle_interval: Duration::from_millis(300),
        idle_response: Duration::from_millis(250),
        busy_response: Duration::from_millis(200),
        tick: Duration::from_millis(40),
        restart_backoffs: vec![Duration::from_millis(30); 3],
    }
}

fn wrote_safe_state(path: &Path) -> bool {
    fs::read_to_string(path).map(|s| s.contains("Preamp: -120.0 dB")).unwrap_or(false)
}

// --- tests ----------------------------------------------------------------

#[test]
fn healthy_sidecar_never_trips() {
    let tmp = TempDir::new("healthy");
    let sup = Supervisor::start(healthy_spawner(), fast_cfg(), tmp.cageq_txt()).unwrap();

    let v = sup.call("calculate_filters", json!({ "device": "DAC" })).expect("call ok");
    assert_eq!(v["device"], "DAC");

    std::thread::sleep(Duration::from_millis(1000)); // several idle heartbeats
    assert_eq!(sup.health(), Health::Running);
    assert_eq!(sup.recoveries(), 0);
    assert!(!tmp.cageq_txt().exists(), "no safe state should have been written");
}

#[test]
fn crash_trips_then_auto_recovers() {
    let tmp = TempDir::new("crash");
    let sup = Supervisor::start(healthy_spawner(), fast_cfg(), tmp.cageq_txt()).unwrap();

    // A crash: the call fails, the watchdog writes silence, then restarts.
    let _ = sup.call("exit", json!({ "code": 1 }));
    let h = sup.wait_until(|h| matches!(h, Health::Running), Duration::from_secs(3));
    assert!(matches!(h, Health::Running), "should auto-recover to Running, got {h:?}");
    assert!(sup.recoveries() >= 1);
    assert!(wrote_safe_state(&tmp.cageq_txt()), "safe state must have been written on the trip");

    // The fresh sidecar serves calls again.
    let v = sup.call("calculate_filters", json!({ "device": "DAC2" })).unwrap();
    assert_eq!(v["device"], "DAC2");
}

#[test]
fn hang_is_killed_and_recovers_without_waiting_it_out() {
    let tmp = TempDir::new("hang");
    let sup = Supervisor::start(healthy_spawner(), fast_cfg(), tmp.cageq_txt()).unwrap();

    let start = Instant::now();
    // Sleeps 5 s, but busy_response is 200 ms: the monitor must *kill* the hung
    // child (cross-thread) and recover long before 5 s would pass.
    let _ = sup.call("sleep_ms", json!({ "ms": 5000 }));
    let h = sup.wait_until(|h| matches!(h, Health::Running), Duration::from_secs(3));

    assert!(matches!(h, Health::Running), "should recover after killing the hang, got {h:?}");
    assert!(start.elapsed() < Duration::from_secs(4), "recovered without waiting out the 5 s hang");
    assert!(sup.recoveries() >= 1);
    assert!(sup.call("ping", json!(null)).is_ok());
}

#[test]
fn idle_crash_is_detected_event_based() {
    let tmp = TempDir::new("idlecrash");
    // Long idle_interval so the driver stays blocked in recv when the process dies:
    // only the event-based exit waiter can notice it promptly. Long backoff so
    // recovery doesn't race the assertion.
    let cfg = WatchdogConfig {
        idle_interval: Duration::from_secs(10),
        idle_response: Duration::from_secs(2),
        busy_response: Duration::from_secs(10),
        tick: Duration::from_millis(40),
        restart_backoffs: vec![Duration::from_secs(30)],
    };
    let sup = Supervisor::start(healthy_spawner(), cfg, tmp.cageq_txt()).unwrap();

    // Replies immediately, then the process crashes ~300 ms later while we're idle.
    sup.call("die_after_ms", json!({ "ms": 300, "code": 1 })).expect("schedule ok");

    // The safe state must appear well before the 10 s idle heartbeat would fire —
    // that is only possible via the OS-handle waiter.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !wrote_safe_state(&tmp.cageq_txt()) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(wrote_safe_state(&tmp.cageq_txt()), "idle crash should mute promptly via the exit waiter");
    assert!(matches!(sup.health(), Health::Recovering { .. } | Health::Terminal { .. }));
}

#[test]
fn persistent_restart_failure_goes_terminal() {
    let tmp = TempDir::new("terminal");
    // Only the initial spawn (index 0) works; every restart attempt fails.
    let sup = Supervisor::start(counting_spawner(|i| i == 0), fast_cfg(), tmp.cageq_txt()).unwrap();

    let _ = sup.call("exit", json!({ "code": 1 }));
    let h = sup.wait_until(|h| matches!(h, Health::Terminal { .. }), Duration::from_secs(3));
    assert!(matches!(h, Health::Terminal { .. }), "3 failed restarts -> Terminal, got {h:?}");

    // A terminal supervisor rejects work fast, without touching a sidecar.
    match sup.call("ping", json!(null)) {
        Err(SupervisorError::Tripped(_)) => {}
        other => panic!("terminal supervisor should reject calls, got {other:?}"),
    }
}

#[test]
fn manual_retry_recovers_from_terminal() {
    let tmp = TempDir::new("manual");
    // ok on the initial spawn (0) and the manual attempt (4); the 3 automatic
    // attempts (1,2,3) fail, so we reach Terminal first.
    let sup = Supervisor::start(counting_spawner(|i| i == 0 || i >= 4), fast_cfg(), tmp.cageq_txt()).unwrap();

    let _ = sup.call("exit", json!({ "code": 1 }));
    let h = sup.wait_until(|h| matches!(h, Health::Terminal { .. }), Duration::from_secs(3));
    assert!(matches!(h, Health::Terminal { .. }), "expected Terminal first, got {h:?}");

    sup.retry();
    let h = sup.wait_until(|h| matches!(h, Health::Running), Duration::from_secs(3));
    assert!(matches!(h, Health::Running), "manual retry should heal, got {h:?}");
    assert!(sup.call("ping", json!(null)).is_ok());
}
