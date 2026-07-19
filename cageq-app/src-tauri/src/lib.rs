use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cageq_core::{CalcRequest, Core, CoreError, Sidecar, WatchdogConfig};
use serde_json::{json, Map, Value};
use tauri::State;

/// Backend held in Tauri managed state. `Ready` on a successful start; `Failed`
/// keeps the init error string so commands can report it instead of the app being
/// silently dead (e.g. if no Python/sidecar is found).
enum Backend {
    Ready { core: Core, config_dir: PathBuf, sidecar: String },
    Failed(String),
}

#[derive(serde::Serialize)]
struct ApplyResult {
    hash: String,
    device: String,
    cageq_path: String,
    /// The exact text written to cageq.txt — the end-to-end proof for the UI.
    cageq_text: String,
}

#[derive(serde::Serialize)]
struct Status {
    startup: String,
    health: String,
    recoveries: u32,
    config_dir: String,
    /// Which sidecar is live: the real AutoEq DSP or the stub.
    sidecar: String,
}

/// Calculate filters for `device` via the sidecar and write cageq.txt through
/// cageq-core, returning the hash plus the file content actually written.
///
/// Until the measurement-import UI exists, this feeds the engine a built-in **demo
/// measurement** — so against the real DSP the result is genuine AutoEq output for a
/// synthetic curve, and against the stub it's ignored.
#[tauri::command]
fn apply(device: String, state: State<Backend>) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let request = CalcRequest { device, inputs: demo_inputs() };
            let applied = core.apply(request).map_err(|e| e.to_string())?;
            let cageq_path = config_dir.join("cageq.txt");
            let cageq_text = std::fs::read_to_string(&cageq_path).unwrap_or_default();
            Ok(ApplyResult {
                hash: applied.hash,
                device: applied.device,
                cageq_path: cageq_path.display().to_string(),
                cageq_text,
            })
        }
    }
}

/// Backend status for the UI: startup verdict, watchdog health, recovery count,
/// and which sidecar is live.
#[tauri::command]
fn status(state: State<Backend>) -> Status {
    match state.inner() {
        Backend::Failed(e) => Status {
            startup: format!("init failed: {e}"),
            health: "-".into(),
            recoveries: 0,
            config_dir: "-".into(),
            sidecar: "-".into(),
        },
        Backend::Ready { core, config_dir, sidecar } => Status {
            startup: format!("{:?}", core.startup_decision()),
            health: format!("{:?}", core.health()),
            recoveries: core.recoveries(),
            config_dir: config_dir.display().to_string(),
            sidecar: sidecar.clone(),
        },
    }
}

// --- demo measurement (until the REW import UI exists) ---------------------

/// A synthetic headphone-ish deviation: a +5 dB bump ~3 kHz, a -3 dB dip ~6 kHz, and
/// a gentle bass rise — enough for AutoEq to produce interesting corrections.
fn sample_measurement() -> Vec<Value> {
    let mut pts = Vec::new();
    let mut f = 20.0_f64;
    while f <= 20_000.0 {
        let lg = f.log10();
        let bump3k = 5.0 * (-0.5 * ((lg - 3000.0_f64.log10()) / 0.08).powi(2)).exp();
        let dip6k = -3.0 * (-0.5 * ((lg - 6000.0_f64.log10()) / 0.06).powi(2)).exp();
        let bass = 3.0 * (-0.5 * ((lg - 45.0_f64.log10()) / 0.35).powi(2)).exp();
        pts.push(json!({ "frequency": f, "raw_db": bump3k + dip6k + bass }));
        f *= 1.05;
    }
    pts
}

fn demo_inputs() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("measurement".into(), Value::Array(sample_measurement()));
    m
}

// --- backend setup --------------------------------------------------------

fn build_backend() -> Backend {
    let config_dir = dev_config_dir();
    let (python, script) = resolve_sidecar();
    let sidecar = if script.file_name().is_some_and(|n| n == "sidecar_dsp.py") {
        format!("AutoEq DSP ({})", python.display())
    } else {
        format!("stub ({})", script.display())
    };
    match start_core(&config_dir, python, script) {
        Ok(core) => Backend::Ready { core, config_dir, sidecar },
        Err(e) => Backend::Failed(format!("backend init failed: {e}")),
    }
}

fn start_core(config_dir: &Path, python: PathBuf, script: PathBuf) -> Result<Core, CoreError> {
    let _ = std::fs::create_dir_all(config_dir);
    let spawn_fn = move || Sidecar::spawn(&python, &script);
    Core::start(config_dir, spawn_fn, watchdog_cfg(), None)
}

/// Lenient timeouts: the real DSP sidecar spends a few seconds importing
/// numpy/scipy/autoeq at startup and ~1-2 s per fit, so short deadlines would
/// false-trip the watchdog.
fn watchdog_cfg() -> WatchdogConfig {
    WatchdogConfig {
        idle_interval: Duration::from_secs(10),
        idle_response: Duration::from_secs(5),
        busy_response: Duration::from_secs(25),
        tick: Duration::from_millis(200),
        restart_backoffs: vec![Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(10)],
    }
}

/// Where cageq.txt is written in dev. Overridable via CAGEQ_CONFIG_DIR; defaults to
/// a temp folder — NOT EqAPO's real config dir (locating that is a separate step).
fn dev_config_dir() -> PathBuf {
    env::var("CAGEQ_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("cageq-dev-config"))
}

fn sidecar_root() -> PathBuf {
    // this crate is cageq-app/src-tauri; the sidecar crate is a sibling of cageq-app.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("cageq-sidecar")
}

/// Resolve (interpreter, script) as a consistent pair. Prefers the real AutoEq DSP
/// (the Python 3.10 venv + sidecar_dsp.py) when both are present; otherwise falls
/// back to a `py`-resolved interpreter + the dependency-free stub. Env vars
/// CAGEQ_PYTHON / CAGEQ_SIDECAR_SCRIPT override either half.
fn resolve_sidecar() -> (PathBuf, PathBuf) {
    let root = sidecar_root();
    let venv = root.join(".venv").join("Scripts").join("python.exe");
    let dsp = root.join("python").join("sidecar_dsp.py");
    let use_real = venv.exists() && dsp.exists();

    let python = env::var("CAGEQ_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|_| if use_real { venv } else { resolve_python() });
    let script = env::var("CAGEQ_SIDECAR_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| if use_real { dsp } else { root.join("python").join("sidecar_stub.py") });
    (python, script)
}

/// Fallback interpreter resolution: the Windows `py` launcher's target, else bare
/// `python`. (Nested ifs rather than a let-chain: this crate is edition 2021.)
fn resolve_python() -> PathBuf {
    if let Ok(out) = Command::new("py").args(["-c", "import sys;print(sys.executable)"]).output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    PathBuf::from("python")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the app's dev-path wiring end to end (no Tauri/GUI): resolve the
    /// sidecar, start a Core, and apply the demo measurement, asserting a real
    /// cageq.txt is written. Runs against whichever sidecar resolves (real DSP if the
    /// venv is present, else the stub). Needs a working Python.
    #[test]
    fn dev_backend_applies_end_to_end() {
        let dir = std::env::temp_dir().join(format!("cageq-app-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (python, script) = resolve_sidecar();
        let core = start_core(&dir, python, script).expect("core should start");
        let request = CalcRequest { device: "Test DAC".into(), inputs: demo_inputs() };
        let applied = core.apply(request).expect("apply should compute + write");

        let text = std::fs::read_to_string(dir.join("cageq.txt")).unwrap();
        assert!(text.contains("Device: Test DAC"), "{text}");
        assert!(text.contains("Filter 1:"), "at least one filter written: {text}");
        assert!(!applied.hash.is_empty());

        drop(core);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(build_backend())
        .invoke_handler(tauri::generate_handler![apply, status])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
