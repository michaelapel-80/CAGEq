use std::path::{Path, PathBuf};
use std::process::Command;

use cageq_core::{CalcRequest, Core, CoreError, Sidecar, WatchdogConfig};
use tauri::State;

/// Backend held in Tauri managed state. `Ready` on a successful start; `Failed`
/// keeps the init error string so commands can report it instead of the app being
/// silently dead (e.g. if no Python is found).
enum Backend {
    Ready { core: Core, config_dir: PathBuf },
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
}

/// Calculate filters for `device` via the (stub) sidecar, write cageq.txt through
/// cageq-core, and return the hash plus the file content actually written.
#[tauri::command]
fn apply(device: String, state: State<Backend>) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir } => {
            let applied = core.apply(CalcRequest::for_device(device)).map_err(|e| e.to_string())?;
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

/// Backend status for the UI: startup verdict, watchdog health, recovery count.
#[tauri::command]
fn status(state: State<Backend>) -> Status {
    match state.inner() {
        Backend::Failed(e) => Status {
            startup: format!("init failed: {e}"),
            health: "-".into(),
            recoveries: 0,
            config_dir: "-".into(),
        },
        Backend::Ready { core, config_dir } => Status {
            startup: format!("{:?}", core.startup_decision()),
            health: format!("{:?}", core.health()),
            recoveries: core.recoveries(),
            config_dir: config_dir.display().to_string(),
        },
    }
}

// --- dev-path backend setup (stub sidecar) --------------------------------

fn build_backend() -> Backend {
    let config_dir = dev_config_dir();
    match start_core(&config_dir) {
        Ok(core) => Backend::Ready { core, config_dir },
        Err(e) => Backend::Failed(format!("backend init failed: {e}")),
    }
}

fn start_core(config_dir: &Path) -> Result<Core, CoreError> {
    let _ = std::fs::create_dir_all(config_dir);
    let python = resolve_python();
    let script = sidecar_script();
    // The watchdog uses this to spawn the initial sidecar and every restart.
    let spawn_fn = move || Sidecar::spawn(&python, &script);
    Core::start(config_dir, spawn_fn, WatchdogConfig::default(), None)
}

/// Where cageq.txt is written in dev. Overridable via CAGEQ_CONFIG_DIR; defaults to
/// a temp folder — NOT EqAPO's real config dir (locating that is a separate step).
fn dev_config_dir() -> PathBuf {
    std::env::var("CAGEQ_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("cageq-dev-config"))
}

/// The Python DSP stub, in dev. Overridable via CAGEQ_SIDECAR_SCRIPT; defaults to
/// the sibling crate's script relative to this crate's source (dev tree only — a
/// bundled release would ship the sidecar as a resource instead).
fn sidecar_script() -> PathBuf {
    if let Ok(p) = std::env::var("CAGEQ_SIDECAR_SCRIPT") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("cageq-sidecar")
        .join("python")
        .join("sidecar_stub.py")
}

/// Resolve a real Python interpreter: CAGEQ_PYTHON, else the Windows `py` launcher's
/// target, else bare `python`. (Nested ifs rather than a let-chain: this crate is
/// edition 2021.)
fn resolve_python() -> PathBuf {
    if let Ok(p) = std::env::var("CAGEQ_PYTHON") {
        return PathBuf::from(p);
    }
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

    /// Exercises the app's dev-path wiring end to end (no Tauri/GUI): resolve_python
    /// + sidecar_script must locate a real interpreter and the stub, and a Core must
    /// start against them and write a real cageq.txt. Needs Python available.
    #[test]
    fn dev_backend_applies_against_the_stub() {
        let dir = std::env::temp_dir().join(format!("cageq-app-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let core = start_core(&dir).expect("core should start (needs Python + the stub script)");
        let applied =
            core.apply(CalcRequest::for_device("Test DAC")).expect("apply should compute + write");

        let text = std::fs::read_to_string(dir.join("cageq.txt")).unwrap();
        assert!(text.contains("Device: Test DAC"), "{text}");
        assert!(text.contains("Filter 1: ON LSC"), "{text}");
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
