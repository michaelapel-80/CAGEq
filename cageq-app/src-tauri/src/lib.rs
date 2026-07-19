use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cageq_core::{
    Applied, AudioDevice, CalcRequest, Core, CoreError, DEFAULT_BASE_PREGAIN_DB, LoudnessSettings,
    Sidecar, WatchdogConfig, detect_eqapo_config_dir, list_render_devices,
};
use serde_json::{json, Map, Value};
use tauri::State;

/// Backend held in Tauri managed state. `Ready` on a successful start; `Failed`
/// keeps the init error string so commands can report it instead of the app being
/// silently dead (e.g. if no Python/sidecar is found).
enum Backend {
    Ready { core: Core, config_dir: PathBuf, config_source: String, sidecar: String },
    Failed(String),
}

#[derive(serde::Serialize)]
struct ApplyResult {
    hash: String,
    device: String,
    cageq_path: String,
    /// The exact text written to cageq.txt — the end-to-end proof for the UI.
    cageq_text: String,
    /// Composed final preamp (§4.0/§4.2): base pre-gain + §4.1 loudness match, capped.
    preamp_db: f64,
    /// §4.2 emergency ceiling bound the level instead of the §4.1 loudness match.
    clipping_warning: bool,
}

#[derive(serde::Serialize)]
struct Status {
    startup: String,
    health: String,
    recoveries: u32,
    config_dir: String,
    /// How config_dir was resolved: the detected EqAPO dir, an override, or dev temp.
    config_source: String,
    /// Which sidecar is live: the real AutoEq DSP or the stub.
    sidecar: String,
}

/// Fit the selected AutoEq `headphone` (a catalogue path) against an optional named
/// `target` via the sidecar, write cageq.txt through cageq-core, and return the hash
/// plus the file content actually written.
#[tauri::command]
fn apply(device: String, headphone: String, target: Option<String>, state: State<Backend>) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let mut inputs = Map::new();
            inputs.insert("headphone".into(), Value::String(headphone));
            if let Some(t) = target {
                inputs.insert("target".into(), Value::String(t));
            }
            let applied = core.apply(CalcRequest { device, inputs }).map_err(|e| e.to_string())?;
            Ok(apply_result(applied, config_dir))
        }
    }
}

/// Build the UI-facing result from an [`Applied`], reading back the exact cageq.txt
/// that was written (the end-to-end proof). Shared by `apply` and `set_loudness`.
fn apply_result(applied: Applied, config_dir: &Path) -> ApplyResult {
    let cageq_path = config_dir.join("cageq.txt");
    let cageq_text = std::fs::read_to_string(&cageq_path).unwrap_or_default();
    ApplyResult {
        hash: applied.hash,
        device: applied.device,
        cageq_path: cageq_path.display().to_string(),
        cageq_text,
        preamp_db: applied.preamp_db,
        clipping_warning: applied.clipping_warning,
    }
}

/// The AutoEq headphone catalogue (built/cached by the sidecar). Relayed as-is.
#[tauri::command]
fn list_headphones(state: State<Backend>) -> Result<Value, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => core.request("list_headphones", json!({})).map_err(|e| e.to_string()),
    }
}

/// Active Windows playback devices to scope the EQ to (§3.0). A local registry read,
/// not a sidecar call — available even if the DSP failed to start.
#[tauri::command]
fn list_devices() -> Vec<AudioDevice> {
    list_render_devices()
}

/// The AutoEq target curves. Relayed as-is.
#[tauri::command]
fn list_targets(state: State<Backend>) -> Result<Value, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => core.request("list_targets", json!({})).map_err(|e| e.to_string()),
    }
}

/// The current §4.0 loudness settings (base pre-gain + mode).
#[tauri::command]
fn get_loudness(state: State<Backend>) -> Result<LoudnessSettings, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => Ok(core.loudness()),
    }
}

/// What `set_loudness` returns: the (clamped, persisted) settings, plus the config
/// re-applied with them — so the UI's shown preamp/cageq.txt updates live.
#[derive(serde::Serialize)]
struct LoudnessUpdate {
    settings: LoudnessSettings,
    /// Present when a config was already applied and re-applying it succeeded.
    applied: Option<ApplyResult>,
}

/// Set the §4.0 loudness settings (base pre-gain + Comparison/FinalVolume mode),
/// persist them, and re-apply the current config so the change takes effect
/// immediately. Base pre-gain is clamped to attenuation only (−40..0 dB): a positive
/// value would be a boost, which the safety model must never allow.
#[tauri::command]
fn set_loudness(settings: LoudnessSettings, state: State<Backend>) -> Result<LoudnessUpdate, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let base = if settings.base_pregain_db.is_finite() {
                settings.base_pregain_db.clamp(-40.0, 0.0)
            } else {
                DEFAULT_BASE_PREGAIN_DB
            };
            let settings = LoudnessSettings { base_pregain_db: base, mode: settings.mode };
            core.set_loudness(settings);
            let _ = save_settings(&AppSettings { loudness: settings }); // best-effort persist
            let applied = core.reapply().and_then(|r| r.ok()).map(|a| apply_result(a, config_dir));
            Ok(LoudnessUpdate { settings, applied })
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
            config_source: "-".into(),
            sidecar: "-".into(),
        },
        Backend::Ready { core, config_dir, config_source, sidecar } => Status {
            startup: format!("{:?}", core.startup_decision()),
            health: format!("{:?}", core.health()),
            recoveries: core.recoveries(),
            config_dir: config_dir.display().to_string(),
            config_source: config_source.clone(),
            sidecar: sidecar.clone(),
        },
    }
}

// --- persisted settings (§3.5, minimal) -----------------------------------

/// The app's persisted settings. A struct (not a bare value) so it can grow without
/// invalidating older files; `#[serde(default)]` fills in anything a prior version
/// didn't write. Currently just the §4.0 loudness settings.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct AppSettings {
    #[serde(default)]
    loudness: LoudnessSettings,
}

/// settings.json location: `%APPDATA%\CAGEq\settings.json`, overridable via
/// CAGEQ_SETTINGS_PATH (used by tests to avoid touching the real profile).
fn settings_path() -> PathBuf {
    if let Ok(p) = env::var("CAGEQ_SETTINGS_PATH") {
        return PathBuf::from(p);
    }
    let base = env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    base.join("CAGEq").join("settings.json")
}

/// Load persisted settings, or defaults if the file is missing/unreadable/corrupt
/// (a bad settings file must never stop the app from starting).
fn load_settings() -> AppSettings {
    load_settings_from(&settings_path())
}

fn load_settings_from(path: &Path) -> AppSettings {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => AppSettings::default(),
    }
}

/// Persist settings (best-effort; creates the parent dir).
fn save_settings(s: &AppSettings) -> std::io::Result<()> {
    save_settings_to(&settings_path(), s)
}

fn save_settings_to(path: &Path, s: &AppSettings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(s).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

// --- backend setup --------------------------------------------------------

fn build_backend() -> Backend {
    let (config_dir, config_source) = resolve_config_dir();
    let (python, script) = resolve_sidecar();
    let sidecar = if script.file_name().is_some_and(|n| n == "sidecar_dsp.py") {
        format!("AutoEq DSP ({})", python.display())
    } else {
        format!("stub ({})", script.display())
    };
    match start_core(&config_dir, python, script) {
        Ok(core) => {
            core.set_loudness(load_settings().loudness); // restore §4.0 settings
            Backend::Ready { core, config_dir, config_source, sidecar }
        }
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

/// Resolve where cageq.txt is written, plus a human label of how it was found (for
/// the UI). Precedence: an explicit `CAGEQ_CONFIG_DIR` override (dev/tests) → the
/// detected Equalizer APO config dir (§3.0, the real target that affects audio) → a
/// dev temp folder, so the app still runs on a machine without EqAPO installed.
fn resolve_config_dir() -> (PathBuf, String) {
    if let Ok(dir) = env::var("CAGEQ_CONFIG_DIR") {
        return (PathBuf::from(dir), "override (CAGEQ_CONFIG_DIR)".into());
    }
    if let Some(dir) = detect_eqapo_config_dir() {
        let label = format!("Equalizer APO — {}", dir.display());
        return (dir, label);
    }
    let dev = std::env::temp_dir().join("cageq-dev-config");
    (dev, "dev temp (Equalizer APO not detected)".into())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(build_backend())
        .invoke_handler(tauri::generate_handler![
            apply,
            status,
            list_headphones,
            list_devices,
            list_targets,
            get_loudness,
            set_loudness
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use cageq_core::LoudnessMode;

    /// A synthetic measurement (used by the offline e2e test so it needs no network):
    /// +5 dB ~3 kHz, -3 dB ~6 kHz, gentle bass rise.
    fn demo_inputs() -> Map<String, Value> {
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
        let mut m = Map::new();
        m.insert("measurement".into(), Value::Array(pts));
        m
    }

    /// Exercises the app's dev-path wiring end to end (no Tauri/GUI): resolve the
    /// sidecar, start a Core, and apply a synthetic measurement, asserting a real
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

    /// The real catalogue path: list headphones, then fit the first one. Only runs
    /// against the real DSP; soft-skips on the stub or a network error.
    #[test]
    fn dev_backend_applies_a_catalogue_headphone() {
        let (python, script) = resolve_sidecar();
        if script.file_name().and_then(|n| n.to_str()) != Some("sidecar_dsp.py") {
            eprintln!("skipping catalogue apply: stub sidecar (no venv)");
            return;
        }
        let dir = std::env::temp_dir().join(format!("cageq-app-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = start_core(&dir, python, script).expect("core should start");

        let list = match core.request("list_headphones", json!({})) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping catalogue apply (network?): {e}");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        let hp = &list["headphones"].as_array().expect("headphones")[0];
        let (name, path) = (hp["name"].as_str().unwrap().to_string(), hp["path"].as_str().unwrap().to_string());

        let mut inputs = Map::new();
        inputs.insert("headphone".into(), Value::String(path));
        core.apply(CalcRequest { device: name.clone(), inputs }).expect("apply a catalogue headphone");

        let text = std::fs::read_to_string(dir.join("cageq.txt")).unwrap();
        assert!(text.contains(&format!("Device: {name}")), "{text}");
        assert!(text.contains("Filter 1:"), "{text}");

        drop(core);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn preamp_line(text: &str) -> String {
        text.lines().find(|l| l.starts_with("Preamp:")).expect("a Preamp line").to_string()
    }

    /// Changing the §4.0 loudness settings and re-applying updates the written preamp
    /// live: FinalVolume (max clipping-free, -G_max_peak) differs from Comparison
    /// (base pre-gain + loudness match) for a non-flat curve.
    #[test]
    fn loudness_mode_changes_the_written_preamp_on_reapply() {
        let dir = std::env::temp_dir().join(format!("cageq-app-loud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (python, script) = resolve_sidecar();
        let core = start_core(&dir, python, script).expect("core should start");

        // Default (Comparison, -9 dB base pre-gain).
        core.apply(CalcRequest { device: "Dev".into(), inputs: demo_inputs() }).expect("apply");
        let comparison = preamp_line(&std::fs::read_to_string(dir.join("cageq.txt")).unwrap());

        // Switch to FinalVolume and re-apply the same config.
        core.set_loudness(LoudnessSettings { base_pregain_db: -9.0, mode: LoudnessMode::FinalVolume });
        core.reapply().expect("something was applied").expect("reapply ok");
        let final_vol = preamp_line(&std::fs::read_to_string(dir.join("cageq.txt")).unwrap());

        assert_ne!(comparison, final_vol, "mode change should move the preamp");

        drop(core);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Persisted settings round-trip through the settings.json file (path-based
    /// helpers, so no global env mutation and the real profile is untouched).
    #[test]
    fn settings_persist_and_reload() {
        let path = std::env::temp_dir().join(format!("cageq-settings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        assert_eq!(load_settings_from(&path).loudness, LoudnessSettings::default(), "missing -> defaults");

        let want = LoudnessSettings { base_pregain_db: -6.0, mode: LoudnessMode::FinalVolume };
        save_settings_to(&path, &AppSettings { loudness: want }).expect("save");
        assert_eq!(load_settings_from(&path).loudness, want, "reloaded value should match saved");

        let _ = std::fs::remove_file(&path);
    }
}
