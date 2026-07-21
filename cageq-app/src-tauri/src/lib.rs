use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cageq_core::{
    Applied, AudioDevice, CalcRequest, Core, CoreError, DEFAULT_BASE_PREGAIN_DB, Filter, Health,
    LoudnessSettings, Sidecar, Slot, WatchdogConfig, detect_eqapo_config_dir, list_render_devices,
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
    /// The bands written (AutoEq fit + custom filters) — the §5.2 chart draws these.
    filters: Vec<Filter>,
}

#[derive(serde::Serialize)]
struct Status {
    /// §3.0 startup verdict: FirstRun / ResumeTrusted / SafeStateStillActive / ExternallyModified.
    startup: String,
    /// Watchdog health detail (Debug of the Health enum).
    health: String,
    /// Coarse health for UI branching: "Running" | "Recovering" | "Terminal" | "-".
    health_kind: String,
    recoveries: u32,
    config_dir: String,
    /// How config_dir was resolved: the detected EqAPO dir, an override, or dev temp.
    config_source: String,
    /// Which sidecar is live: the real AutoEq DSP or the stub.
    sidecar: String,
}

fn health_kind(h: &Health) -> &'static str {
    match h {
        Health::Running => "Running",
        Health::Recovering { .. } => "Recovering",
        Health::Terminal { .. } => "Terminal",
    }
}

/// Fit the selected AutoEq `headphone` (a catalogue path) against an optional named
/// `target` into comparison `slot` (A or B), write cageq.txt through cageq-core, and
/// return the hash plus the file content actually written.
#[tauri::command]
fn apply(
    device: String,
    headphone: String,
    target: Option<String>,
    slot: Slot,
    custom_filters: Vec<Filter>,
    state: State<Backend>,
) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let mut inputs = Map::new();
            inputs.insert("headphone".into(), Value::String(headphone.clone()));
            if let Some(t) = &target {
                inputs.insert("target".into(), Value::String(t.clone()));
            }
            // §3.4 manual filters stacked on the AutoEq fit; the sidecar combines them
            // into the curve that drives the loudness/clipping policy.
            if !custom_filters.is_empty() {
                let cf = serde_json::to_value(&custom_filters).map_err(|e| e.to_string())?;
                inputs.insert("custom_filters".into(), cf);
            }
            let applied = core.apply_to_slot(slot, CalcRequest { device, inputs }).map_err(|e| e.to_string())?;
            // Remember what was applied so the pickers pre-fill on the next launch.
            update_settings(|s| s.selection = Selection { headphone: Some(headphone), target });
            Ok(apply_result(applied, config_dir))
        }
    }
}

/// The last-applied headphone/target (catalogue paths) to pre-fill the pickers on
/// startup. Empty on a clean install.
#[tauri::command]
fn get_selection() -> Selection {
    load_settings().selection
}

/// Switch the active comparison slot (A/B/Dry) and write its cached config — instant,
/// no re-fit (filter.md §5.2). Returns the newly-written config for the UI.
#[tauri::command]
fn activate_slot(slot: Slot, state: State<Backend>) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let applied = core.activate_slot(slot).map_err(|e| e.to_string())?;
            Ok(apply_result(applied, config_dir))
        }
    }
}

/// Copy slot `from` onto slot `to` (A/B) and make `to` active — a starting point for
/// a variant (filter.md §5.2). Returns the newly-written config.
#[tauri::command]
fn copy_slot(from: Slot, to: Slot, state: State<Backend>) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let applied = core.copy_slot(from, to).map_err(|e| e.to_string())?;
            Ok(apply_result(applied, config_dir))
        }
    }
}

/// Tell the core which output device every slot is scoped to, so Dry can be written
/// before any fit exists. Called when the user picks a device.
#[tauri::command]
fn set_device(device: String, state: State<Backend>) -> Result<(), String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => {
            core.set_device(device);
            Ok(())
        }
    }
}

/// Build the UI-facing result from an [`Applied`], reading back the exact cageq.txt
/// that was written (the end-to-end proof). Shared by every write path. Persists the
/// hash so the next startup's §3.0 integrity check has something to compare against.
fn apply_result(applied: Applied, config_dir: &Path) -> ApplyResult {
    update_settings(|s| s.last_hash = Some(applied.hash.clone()));
    let cageq_path = config_dir.join("cageq.txt");
    let cageq_text = std::fs::read_to_string(&cageq_path).unwrap_or_default();
    ApplyResult {
        hash: applied.hash,
        device: applied.device,
        cageq_path: cageq_path.display().to_string(),
        cageq_text,
        preamp_db: applied.preamp_db,
        clipping_warning: applied.clipping_warning,
        filters: applied.filters,
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
            update_settings(|s| s.loudness = settings); // persist without wiping the selection
            // update_loudness sets the settings and pushes them live — ramping a volume
            // increase (§7.5), or writing directly for a decrease. Blocks for the ramp.
            let applied = core.update_loudness(settings).and_then(|r| r.ok()).map(|a| apply_result(a, config_dir));
            Ok(LoudnessUpdate { settings, applied })
        }
    }
}

/// Preview the dB the preamp would change if `settings` were applied to the active
/// config (positive = louder) — for the §7.5 confirm dialog. `None` if nothing active.
#[tauri::command]
fn preview_loudness(settings: LoudnessSettings, state: State<Backend>) -> Option<f64> {
    match state.inner() {
        Backend::Ready { core, .. } => core.preview_preamp_delta(settings),
        Backend::Failed(_) => None,
    }
}

/// Whether the §7.5 "switching to Final volume" confirmation is enabled.
#[tauri::command]
fn get_confirm_final_volume() -> bool {
    load_settings().confirm_final_volume
}

/// Enable/disable the §7.5 confirmation ("don't ask again" clears it; a UI checkbox
/// re-enables it).
#[tauri::command]
fn set_confirm_final_volume(enabled: bool) {
    update_settings(|s| s.confirm_final_volume = enabled);
}

/// Backend status for the UI: startup verdict, watchdog health, recovery count,
/// and which sidecar is live.
#[tauri::command]
fn status(state: State<Backend>) -> Status {
    match state.inner() {
        Backend::Failed(e) => Status {
            startup: format!("init failed: {e}"),
            health: "-".into(),
            health_kind: "-".into(),
            recoveries: 0,
            config_dir: "-".into(),
            config_source: "-".into(),
            sidecar: "-".into(),
        },
        Backend::Ready { core, config_dir, config_source, sidecar } => {
            let health = core.health();
            Status {
                startup: format!("{:?}", core.startup_decision()),
                health_kind: health_kind(&health).into(),
                health: format!("{health:?}"),
                recoveries: core.recoveries(),
                config_dir: config_dir.display().to_string(),
                config_source: config_source.clone(),
                sidecar: sidecar.clone(),
            }
        }
    }
}

/// Request one manual recovery attempt out of the watchdog `Terminal` state (§7.2) —
/// from the UI's "Retry" button. The watchdog acts asynchronously; poll `status`.
#[tauri::command]
fn retry(state: State<Backend>) -> Result<(), String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => {
            core.retry();
            Ok(())
        }
    }
}

// --- persisted settings (§3.5, minimal) -----------------------------------

/// The app's persisted settings. A struct (not a bare value) so it can grow without
/// invalidating older files; `#[serde(default)]` fills in anything a prior version
/// didn't write.
#[derive(serde::Serialize, serde::Deserialize)]
struct AppSettings {
    #[serde(default)]
    loudness: LoudnessSettings,
    /// Last-used headphone/target, restored into the pickers on the next launch.
    #[serde(default)]
    selection: Selection,
    /// Content hash of the last cageq.txt CAGEq wrote (§3.0). Compared on the next
    /// startup to tell "unchanged" from "safe-state still active" / "externally edited".
    #[serde(default)]
    last_hash: Option<String>,
    /// §7.5 point 1: confirm before switching to Finale Lautstärke (the volume jump).
    /// Defaults on; the dialog's "don't ask again" clears it, a UI checkbox re-enables.
    #[serde(default = "default_true")]
    confirm_final_volume: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            loudness: LoudnessSettings::default(),
            selection: Selection::default(),
            last_hash: None,
            confirm_final_volume: true,
        }
    }
}

/// The last-applied headphone and target, by their catalogue paths (stable ids).
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Selection {
    #[serde(default)]
    headphone: Option<String>,
    #[serde(default)]
    target: Option<String>,
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

/// Update one field of the persisted settings without clobbering the others
/// (load-modify-save). Best-effort; a failed write just loses the update.
fn update_settings(edit: impl FnOnce(&mut AppSettings)) {
    let mut s = load_settings();
    edit(&mut s);
    let _ = save_settings(&s);
}

// --- backend setup --------------------------------------------------------

/// How to launch the DSP sidecar.
enum SidecarSource {
    /// The self-contained PyInstaller-frozen executable (bundled release build) — no
    /// Python needed on the machine.
    Frozen(PathBuf),
    /// A Python interpreter + a script (dev venv, an env override, or the stub).
    Python { python: PathBuf, script: PathBuf },
}

/// `bundled_sidecar` is the frozen sidecar exe inside the app's resources (release), or
/// `None` in dev.
fn build_backend(bundled_sidecar: Option<PathBuf>) -> Backend {
    let (config_dir, config_source) = resolve_config_dir();
    let (source, sidecar) = resolve_sidecar(bundled_sidecar.as_deref());
    let settings = load_settings();
    match start_core(&config_dir, source, settings.last_hash.as_deref()) {
        Ok(core) => {
            core.set_loudness(settings.loudness); // restore §4.0 settings
            Backend::Ready { core, config_dir, config_source, sidecar }
        }
        Err(e) => Backend::Failed(format!("backend init failed: {e}")),
    }
}

fn start_core(
    config_dir: &Path,
    source: SidecarSource,
    expected_hash: Option<&str>,
) -> Result<Core, CoreError> {
    let _ = std::fs::create_dir_all(config_dir);
    let spawn_fn = move || match &source {
        SidecarSource::Frozen(exe) => Sidecar::spawn_program(exe),
        SidecarSource::Python { python, script } => Sidecar::spawn(python, script),
    };
    Core::start(config_dir, spawn_fn, watchdog_cfg(), expected_hash)
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

/// Resolve how to launch the sidecar, plus a human label for the UI. Precedence:
///   1. `CAGEQ_PYTHON` / `CAGEQ_SIDECAR_SCRIPT` env override (dev/tests/other machine),
///   2. the bundled frozen executable (self-contained release),
///   3. the dev venv + `sidecar_dsp.py`,
///   4. a `py`-resolved interpreter + the dependency-free stub.
fn resolve_sidecar(bundled: Option<&Path>) -> (SidecarSource, String) {
    let root = sidecar_root();
    let env_python = env::var("CAGEQ_PYTHON").ok();
    let env_script = env::var("CAGEQ_SIDECAR_SCRIPT").ok();

    // 1. Explicit override — a Python interpreter + script.
    if env_python.is_some() || env_script.is_some() {
        let python = env_python.map(PathBuf::from).unwrap_or_else(resolve_python);
        let script =
            env_script.map(PathBuf::from).unwrap_or_else(|| root.join("python").join("sidecar_dsp.py"));
        let label = format!("AutoEq DSP, override ({})", python.display());
        return (SidecarSource::Python { python, script }, label);
    }

    // 2. Bundled frozen executable (release).
    if let Some(exe) = bundled {
        if exe.exists() {
            return (SidecarSource::Frozen(exe.to_path_buf()), format!("AutoEq DSP, bundled ({})", exe.display()));
        }
    }

    // 3. Dev venv + real script.
    let venv = root.join(".venv").join("Scripts").join("python.exe");
    let dsp = root.join("python").join("sidecar_dsp.py");
    if venv.exists() && dsp.exists() {
        let label = format!("AutoEq DSP, dev venv ({})", venv.display());
        return (SidecarSource::Python { python: venv, script: dsp }, label);
    }

    // 4. Fallback: the dependency-free stub.
    let script = root.join("python").join("sidecar_stub.py");
    (SidecarSource::Python { python: resolve_python(), script: script.clone() }, format!("stub ({})", script.display()))
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
    let mut builder = tauri::Builder::default();

    // §2: enforce a single instance so two CAGEq processes never race on cageq.txt.
    // Must be registered first (plugin docs). A second launch focuses the existing
    // window instead of starting a rival writer, then exits.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }));
    }

    builder
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // The frozen DSP sidecar ships as a bundled resource (see tauri.conf.json);
            // its path needs the app handle, so build the backend here rather than in
            // `.manage(...)`. In dev this path won't exist and resolve_sidecar falls
            // back to the venv.
            use tauri::Manager;
            let bundled = app
                .path()
                .resource_dir()
                .ok()
                .map(|r| r.join("sidecar").join("cageq-sidecar.exe"));
            app.manage(build_backend(bundled));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            apply,
            activate_slot,
            copy_slot,
            set_device,
            status,
            list_headphones,
            list_devices,
            list_targets,
            get_loudness,
            set_loudness,
            preview_loudness,
            get_confirm_final_volume,
            set_confirm_final_volume,
            get_selection,
            retry
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

        let (source, _) = resolve_sidecar(None);
        let core = start_core(&dir, source, None).expect("core should start");
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
        let (source, _) = resolve_sidecar(None);
        let is_real = match &source {
            SidecarSource::Frozen(_) => true,
            SidecarSource::Python { script, .. } => {
                script.file_name().and_then(|n| n.to_str()) == Some("sidecar_dsp.py")
            }
        };
        if !is_real {
            eprintln!("skipping catalogue apply: stub sidecar (no venv)");
            return;
        }
        let dir = std::env::temp_dir().join(format!("cageq-app-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = start_core(&dir, source, None).expect("core should start");

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
        let (source, _) = resolve_sidecar(None);
        let core = start_core(&dir, source, None).expect("core should start");

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
        let selection = Selection { headphone: Some("measurements/x.csv".into()), target: Some("targets/y.csv".into()) };
        save_settings_to(
            &path,
            &AppSettings {
                loudness: want,
                selection: selection.clone(),
                last_hash: Some("abc123".into()),
                confirm_final_volume: false,
            },
        )
        .expect("save");
        let reloaded = load_settings_from(&path);
        assert_eq!(reloaded.loudness, want, "reloaded loudness should match saved");
        assert_eq!(reloaded.selection.headphone, selection.headphone, "reloaded headphone should match");
        assert_eq!(reloaded.selection.target, selection.target, "reloaded target should match");
        assert_eq!(reloaded.last_hash.as_deref(), Some("abc123"), "reloaded hash should match");
        assert!(!reloaded.confirm_final_volume, "reloaded confirm flag should match saved (false)");
        // A missing field (old settings.json) defaults the confirm flag ON.
        std::fs::write(&path, r#"{"loudness":{"base_pregain_db":-9.0,"mode":"Comparison"}}"#).unwrap();
        assert!(load_settings_from(&path).confirm_final_volume, "missing confirm flag defaults to true");

        let _ = std::fs::remove_file(&path);
    }
}
