//! End-to-end orchestrator tests: real Python stub sidecar, real cageq.txt writes.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use cageq_config_writer::{self as cw, BlockState, StartupDecision};
use cageq_core::{
    CalcRequest, Core, CoreError, DEFAULT_BASE_PREGAIN_DB, LoudnessMode, LoudnessSettings, Slot,
};
use cageq_sidecar::{Sidecar, SidecarError};
use cageq_watchdog::{Health, WatchdogConfig};
use serde_json::json;

// --- rig ------------------------------------------------------------------

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

fn healthy_spawner() -> impl Fn() -> Result<Sidecar, SidecarError> + Send + 'static {
    let (py, script) = (find_python(), stub_script());
    move || Sidecar::spawn(&py, &script)
}

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cageq-core-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn dir(&self) -> &std::path::Path {
        &self.0
    }
    fn cageq(&self) -> String {
        fs::read_to_string(self.0.join("cageq.txt")).unwrap_or_default()
    }
    fn config(&self) -> String {
        fs::read_to_string(self.0.join("config.txt")).unwrap_or_default()
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

// --- tests ----------------------------------------------------------------

#[test]
fn slots_switch_by_rewrite_without_refitting() {
    let tmp = TempDir::new("slots");
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();

    // Populate A and B with distinct device names (the stub echoes device).
    core.apply_to_slot(Slot::A, CalcRequest::for_device("Device A")).expect("apply A");
    core.apply_to_slot(Slot::B, CalcRequest::for_device("Device B")).expect("apply B");
    assert_eq!(core.active_slot(), Slot::B);
    assert!(tmp.cageq().contains("Device: Device B"));

    // Switching back to A is a pure re-write — cageq.txt flips, no extra fit.
    let a = core.activate_slot(Slot::A).expect("activate A");
    assert_eq!(a.device, "Device A");
    assert_eq!(core.active_slot(), Slot::A);
    let cageq = tmp.cageq();
    assert!(cageq.contains("Device: Device A"), "{cageq}");
    assert!(cageq.contains("Filter 1:"), "A keeps its filters: {cageq}");

    // Dry writes the no-correction reference: same device scope, but no filters.
    core.activate_slot(Slot::Dry).expect("activate Dry");
    let dry = tmp.cageq();
    assert!(!dry.contains("Filter 1:"), "Dry has no filters: {dry}");

    // Dry isn't editable; and an untouched slot has nothing to activate.
    assert!(matches!(
        core.apply_to_slot(Slot::Dry, CalcRequest::for_device("x")),
        Err(CoreError::DryNotEditable)
    ));
    let empty = TempDir::new("empty");
    let fresh = Core::start(empty.dir(), healthy_spawner(), fast_cfg(), None).unwrap();
    assert!(matches!(fresh.activate_slot(Slot::B), Err(CoreError::EmptySlot(Slot::B))));
}

#[test]
fn copy_slot_duplicates_a_fit_and_activates_the_target() {
    let tmp = TempDir::new("copy");
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();

    core.apply_to_slot(Slot::A, CalcRequest::for_device("Device A")).expect("apply A");

    // Copy A -> B: B now holds A's config and becomes active.
    let copied = core.copy_slot(Slot::A, Slot::B).expect("copy A->B");
    assert_eq!(copied.device, "Device A");
    assert_eq!(core.active_slot(), Slot::B);
    assert!(tmp.cageq().contains("Device: Device A"), "B mirrors A: {}", tmp.cageq());

    // B is independent now: re-applying B doesn't disturb A.
    core.apply_to_slot(Slot::B, CalcRequest::for_device("Device B")).expect("apply B");
    assert_eq!(core.activate_slot(Slot::A).unwrap().device, "Device A");

    // Copying from an empty slot errors; Dry can't take part.
    let fresh = TempDir::new("copy-empty");
    let c2 = Core::start(fresh.dir(), healthy_spawner(), fast_cfg(), None).unwrap();
    assert!(matches!(c2.copy_slot(Slot::A, Slot::B), Err(CoreError::EmptySlot(Slot::A))));
    assert!(matches!(core.copy_slot(Slot::A, Slot::Dry), Err(CoreError::DryNotEditable)));
}

#[test]
fn loudness_increase_ramps_but_decrease_is_direct() {
    let tmp = TempDir::new("ramp");
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();

    // Small base pre-gain so the Comparison->Final increase is a quick ~1 dB ramp.
    core.set_loudness(LoudnessSettings { base_pregain_db: -1.0, mode: LoudnessMode::Comparison });
    core.apply(CalcRequest::for_device("DAC")).expect("apply"); // stub is flat -> preamp -1.0
    assert!(tmp.cageq().contains("Preamp: -1.0 dB"), "{}", tmp.cageq());
    let after_apply = core.applied_count();

    // Comparison -> FinalVolume raises volume (-1 -> 0 dB): ramped over several writes,
    // landing exactly on 0.
    core.update_loudness(LoudnessSettings { base_pregain_db: -1.0, mode: LoudnessMode::FinalVolume })
        .expect("something active")
        .expect("ramp ok");
    assert!(tmp.cageq().contains("Preamp: 0.0 dB"), "{}", tmp.cageq());
    let ramp_writes = core.applied_count() - after_apply;
    assert!(ramp_writes >= 3, "an increase should ramp over several writes, got {ramp_writes}");
    let after_ramp = core.applied_count();

    // FinalVolume -> Comparison lowers volume (0 -> -1 dB): direct, one write.
    core.update_loudness(LoudnessSettings { base_pregain_db: -1.0, mode: LoudnessMode::Comparison })
        .expect("something active")
        .expect("direct ok");
    assert!(tmp.cageq().contains("Preamp: -1.0 dB"), "{}", tmp.cageq());
    assert_eq!(core.applied_count() - after_ramp, 1, "a decrease is a single direct write");
}

#[test]
fn apply_calculates_and_writes_config() {
    let tmp = TempDir::new("apply");
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();

    let applied = core.apply(CalcRequest::for_device("USB DAC")).expect("apply ok");
    assert_eq!(applied.device, "USB DAC");
    assert_eq!(core.applied_count(), 1);

    // The DSP's canned filters made it through to cageq.txt in EqAPO syntax...
    let cageq = tmp.cageq();
    assert!(cageq.contains("Device: USB DAC"), "{cageq}");
    assert!(cageq.contains("Filter 1: ON LSC Fc 105 Hz Gain 3.0 dB Q 0.70"), "{cageq}");
    // The core composes the preamp itself (§4.0/§4.2) and ignores any preamp the
    // sidecar reports; the stub sends no loudness data, so this is just the default
    // base pre-gain (-9.0 dB), and the §4.2 ceiling does not trip on a flat curve.
    assert!(cageq.contains("Preamp: -9.0 dB"), "{cageq}");
    assert_eq!(applied.preamp_db, DEFAULT_BASE_PREGAIN_DB);
    assert!(!applied.clipping_warning);
    // ...and config.txt got the Include block.
    assert!(tmp.config().contains("Include: cageq.txt"));

    // The returned hash matches what the config-writer reads back.
    match cw::read_cageq_state(&tmp.dir().join("cageq.txt")).unwrap() {
        BlockState::Present { actual_hash, .. } => assert_eq!(actual_hash, applied.hash),
        BlockState::Absent => panic!("expected a present cageq.txt"),
    }
}

#[test]
fn recovery_reapplies_last_config_and_leaves_safe_state() {
    let tmp = TempDir::new("reapply");
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();

    core.apply(CalcRequest::for_device("DAC")).unwrap();
    assert_eq!(core.applied_count(), 1);
    assert!(tmp.cageq().contains("Filter 1: ON LSC"));

    // Crash the sidecar. The watchdog trips (writes -120 dB silence) then restarts,
    // and the reconciler must re-apply the real config on top.
    let _ = core.request("exit", json!({ "code": 1 }));

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && core.applied_count() < 2 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(core.applied_count() >= 2, "reconciler should have re-applied after recovery");
    assert!(matches!(core.health(), Health::Running));

    // The real config is back and the safe state has been left.
    let cageq = tmp.cageq();
    assert!(cageq.contains("Filter 1: ON LSC"), "real config restored: {cageq}");
    assert!(!cageq.contains("Preamp: -120.0 dB"), "safe state should be left: {cageq}");
}

#[test]
fn startup_is_first_run_on_a_clean_dir() {
    let tmp = TempDir::new("firstrun");
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();
    assert_eq!(core.startup_decision(), StartupDecision::FirstRun);
}

#[test]
fn startup_trusts_a_matching_remembered_hash() {
    let tmp = TempDir::new("resume");

    // Session 1: apply, remember the hash, shut down (cageq.txt persists on disk).
    let hash = {
        let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), None).unwrap();
        core.apply(CalcRequest::for_device("DAC")).unwrap().hash
    };

    // Session 2: start again with that remembered hash -> the resume state is trusted.
    let core = Core::start(tmp.dir(), healthy_spawner(), fast_cfg(), Some(&hash)).unwrap();
    assert_eq!(core.startup_decision(), StartupDecision::ResumeTrusted);
}
