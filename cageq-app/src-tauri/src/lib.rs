use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cageq_core::{
    Applied, AudioDevice, CalcRequest, Core, CoreError, CurvePoint, DEFAULT_BASE_PREGAIN_DB, Filter,
    Health, LoudnessSettings, Sidecar, Slot, WatchdogConfig, detect_eqapo_config_dir,
    list_render_devices,
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

/// Holds the running §5.3c loopback monitor so start/stop commands can replace or end it.
/// Capture is Windows-only (`cageq_monitor::Monitor::start` errors elsewhere).
#[derive(Default)]
struct MonitorState(std::sync::Mutex<Option<cageq_monitor::Monitor>>);

/// Holds the running self-test signal player (pink noise), so start/stop can replace or end it.
#[derive(Default)]
struct TestSignalState(std::sync::Mutex<Option<cageq_monitor::TestSignal>>);

/// Number of open vectorscope views (inline chart view + the pop-out window). The loopback only
/// accumulates/emits the heavier `scope` stream while this is > 0. Shared into the monitor so it
/// survives monitor restarts (device changes); toggled by `set_scope_viewer`.
#[derive(Default)]
struct ScopeViewers(std::sync::Arc<std::sync::atomic::AtomicUsize>);

/// §5.3c data plane: per-stream `Channel` subscribers. These streams used to ride `app.emit`,
/// but Tauri delivers each backend→frontend *event* by evaluating a script in the webview — at
/// this app's sustained ~120 events/s (meter + spectrum at 60 fps each) that churned memory in
/// WebView2 at ~100 MiB/min, faster than its GC kept up (the "out of memory" crash class).
/// `tauri::ipc::Channel` rides the raw IPC pipe instead — the documented transport for exactly
/// this kind of streaming. Each webview registers one channel per stream for its whole lifetime
/// (see frontend `streams.ts`). Arcs so the monitor's capture-thread closures can hold them
/// across monitor restarts (device changes), same pattern as `ScopeViewers`.
///
/// Each entry is keyed by the **label of the webview that registered it**, because that label is
/// the only usable liveness signal on desktop. This used to lean on `Channel::send` failing once
/// the webview was gone — it never does: `send` bottoms out in `Webview::eval` →
/// `send_user_message`, which returns `Ok(())` as long as the *event loop* proxy is alive, whether
/// or not the target webview still exists. The result was a real, unbounded leak in the host
/// process (not in WebView2) after the pop-out scope window closed: a `ScopeUpdate` is ~20 KB of
/// JSON, over Tauri's 8 KiB direct-eval threshold, so every frame took the fetch path — parked in
/// Tauri's global `ChannelDataIpcQueue` map and evicted only when the webview actually fetches it.
/// A dead webview never fetches, and nothing else ever evicts, so ~60 fps × 20 KB ≈ 1 MB/s piled
/// up forever (`Meter` pins `set_scope_viewer` at ≥1, so the stream never stops on its own).
/// [`drop_subs`] on `WindowEvent::Destroyed` is the cleanup; `register` also replaces a same-label
/// entry so a webview reload (same label, fresh channel) can't strand the old one either.
///
/// Window destruction is not the only way a consumer can stop draining, though — a crashed WebView2
/// renderer, a wedged main thread or a JS exception all leave the *window* alive, so no `Destroyed`
/// event fires and the same pile-up resumes at full rate. `alive` is the backstop: each webview
/// beats once a second (`stream_heartbeat`), and [`fan_out`] simply doesn't send to a label that has
/// gone quiet. Bounding the queue directly isn't an option — `ChannelDataIpcQueue` is not exported
/// from `tauri::ipc`, and its only accessor is Tauri's internal fetch command — so the guard has to
/// sit upstream of `send`. Nor can the payload just be kept under the 8 KiB direct-eval threshold
/// (which uses no queue at all): `SCOPE_MAX_POINTS` is 2048 pairs ≈ 45 KB of JSON, and fitting
/// would mean ≲350 pairs, less than half the trace density the scope draws.
type Subs<T> = std::sync::Arc<std::sync::Mutex<Vec<(String, tauri::ipc::Channel<T>)>>>;

/// Last heartbeat per webview label — see [`StreamSubs`] and `stream_heartbeat`.
type Alive = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>;

/// How long a webview may stay silent before [`fan_out`] stops sending to it. Deliberately
/// generous relative to the 1 Hz beat: a false positive is nearly free (frames are skipped for a
/// beat, and the phosphor views already tolerate that — they resume the moment it checks back in),
/// while a false *negative* leaks at the full payload rate.
const SUB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Default)]
struct StreamSubs {
    meter: Subs<cageq_monitor::MeterUpdate>,
    spectrum: Subs<cageq_monitor::SpectrumUpdate>,
    scope: Subs<cageq_monitor::ScopeUpdate>,
    alive: Alive,
}

/// Send one stream payload to every subscriber that is still draining its channel. The `is_ok`
/// prune is a belt-and-braces guard only (it fires on mobile, where the channel is a real callback
/// that can fail) — desktop liveness is the heartbeat's job, see [`StreamSubs`].
fn fan_out<T: Clone + serde::Serialize>(
    subs: &std::sync::Mutex<Vec<(String, tauri::ipc::Channel<T>)>>,
    alive: &Alive,
    value: T,
) {
    let now = std::time::Instant::now();
    let Ok(beats) = alive.lock() else { return };
    if let Ok(mut list) = subs.lock() {
        list.retain(|(label, ch)| {
            let draining = beats.get(label).is_some_and(|t| now - *t <= SUB_TIMEOUT);
            // Gone quiet: crashed, wedged, or just hidden (Chromium throttles timers in hidden
            // windows, which is fine — those views aren't painting either, and they pick straight
            // back up on the next beat). Skip the send rather than park a payload nobody will
            // fetch, but keep the entry, so this is fully reversible.
            !draining || ch.send(value.clone()).is_ok()
        });
    }
}

/// Record a liveness beat for `label`.
fn beat(alive: &Alive, label: &str) {
    if let Ok(mut beats) = alive.lock() {
        beats.insert(label.to_string(), std::time::Instant::now());
    }
}

/// Register `label`'s channel for one stream, replacing any it had already (webview reload).
fn register<T>(subs: &Subs<T>, label: &str, channel: tauri::ipc::Channel<T>) {
    if let Ok(mut list) = subs.lock() {
        list.retain(|(l, _)| l != label);
        list.push((label.to_string(), channel));
    }
}

/// Drop every stream channel belonging to a webview that no longer exists. Called from the
/// app-wide `WindowEvent::Destroyed` handler — see [`StreamSubs`] for why this can't be inferred
/// from send failures.
fn drop_subs(subs: &StreamSubs, label: &str) {
    fn prune<T>(list: &Subs<T>, label: &str) {
        if let Ok(mut l) = list.lock() {
            l.retain(|(k, _)| k != label);
        }
    }
    prune(&subs.meter, label);
    prune(&subs.spectrum, label);
    prune(&subs.scope, label);
    if let Ok(mut beats) = subs.alive.lock() {
        beats.remove(label);
    }
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
    /// §5.2 chart reference: the ideal correction the AutoEq fit targets (empty for Dry).
    reference_curve: Vec<CurvePoint>,
    /// §4.1 loudness target + §4.2 curve peak — so the UI can compute the pre-gain
    /// that gives the loudness match headroom when the clipping ceiling binds.
    g_target_db: f64,
    g_max_peak_db: f64,
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

/// The §3.5 resume blob (active slot, device, per-slot inputs) saved last session, or
/// `null` on a clean install. The frontend owns its shape; we return it verbatim.
#[tauri::command]
fn get_resume() -> Option<Value> {
    load_settings().resume
}

/// Persist the §3.5 resume blob. Called by the frontend whenever the editable session
/// state changes, so the next launch can restore and re-apply it.
#[tauri::command]
fn set_resume(resume: Value) {
    update_settings(|s| s.resume = Some(resume));
}

/// The §3.5 preset library (saved session presets + filter templates) from a previous
/// run, or `null` on a clean install. Like the resume blob, an opaque UI-owned shape the
/// backend stores and returns verbatim.
#[tauri::command]
fn get_library() -> Option<Value> {
    load_settings().library
}

/// Persist the §3.5 preset library. Called by the frontend whenever the user saves or
/// deletes a preset / filter template.
#[tauri::command]
fn set_library(library: Value) {
    update_settings(|s| s.library = Some(library));
}

/// Seed a slot with a fit **persisted from a previous session** (§3.5), no sidecar call.
/// The launch-from-cache path: the frontend saved each slot's last composed bands + curve
/// quantities, so startup can restore the exact EQ without waiting on the ~1–2 s cold fit.
/// Seeds the cache only (no write, no activation) — the caller seeds both slots, then
/// `activate_slot` on the last-active slot writes it instantly. `filters`/`reference_curve`
/// round-trip the same shapes `apply` returns.
#[tauri::command]
fn seed_slot(
    slot: Slot,
    device: String,
    filters: Vec<Filter>,
    g_target_db: f64,
    g_max_peak_db: f64,
    reference_curve: Vec<CurvePoint>,
    state: State<Backend>,
) -> Result<(), String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => core
            .seed_slot(slot, device, filters, g_target_db, g_max_peak_db, reference_curve)
            .map_err(|e| e.to_string()),
    }
}

/// Warm the sidecar's AutoEq fit cache for `headphone`/`target` (§5.2 cache) in the
/// background, so the first tone edit after a launch-from-cache seed (§3.5) is instant
/// rather than paying the cold fit. Fire-and-forget: the reply is discarded (populating
/// the sidecar's in-process cache is the whole point). No write and no slot change, so it
/// can never race with a user edit.
#[tauri::command]
fn warm_fit(device: String, headphone: String, target: Option<String>, state: State<Backend>) -> Result<(), String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => {
            let mut inputs = Map::new();
            inputs.insert("headphone".into(), Value::String(headphone));
            if let Some(t) = target {
                inputs.insert("target".into(), Value::String(t));
            }
            let _ = core.request("calculate_filters", serde_json::to_value(CalcRequest { device, inputs }).map_err(|e| e.to_string())?);
            Ok(())
        }
    }
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

/// §5.2 isolate: write a bandpass-only config for the active slot (audition one band's region).
/// Transient — the frontend re-applies the real correction to clear it.
#[tauri::command]
fn isolate(freq_hz: f64, q: f64, state: State<Backend>) -> Result<ApplyResult, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, config_dir, .. } => {
            let applied = core.apply_isolate(freq_hz, q).map_err(|e| e.to_string())?;
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
        reference_curve: applied.reference_curve,
        g_target_db: applied.g_target_db,
        g_max_peak_db: applied.g_max_peak_db,
    }
}

/// The AutoEq headphone catalogue (built/cached by the sidecar). Relayed as-is.
#[tauri::command]
fn list_headphones(state: State<Backend>) -> Result<Value, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => core
            .request_with_deadline("list_headphones", json!({}), CATALOGUE_BUSY_RESPONSE)
            .map_err(|e| e.to_string()),
    }
}

/// Active Windows playback devices to scope the EQ to (§3.0). A local registry read,
/// not a sidecar call — available even if the DSP failed to start.
#[tauri::command]
fn list_devices() -> Vec<AudioDevice> {
    list_render_devices()
}

/// Active directives in EqAPO's config.txt outside CAGEq's own include block — the foreign
/// filters (e.g. the fresh-install default preamp) that stack on top of every CAGEq correction.
/// Returned for a UI preview so the user can choose to neutralize them.
#[tauri::command]
fn config_foreign_directives(state: State<Backend>) -> Result<Vec<String>, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { config_dir, .. } => {
            cageq_core::foreign_config_directives(&config_dir.join("config.txt")).map_err(|e| e.to_string())
        }
    }
}

/// Comment out those foreign directives (reversible marker prefix), so only CAGEq's correction
/// applies. Returns whether config.txt changed.
#[tauri::command]
fn disable_foreign_config(state: State<Backend>) -> Result<bool, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { config_dir, .. } => {
            cageq_core::disable_foreign_config(&config_dir.join("config.txt")).map_err(|e| e.to_string())
        }
    }
}

/// Undo [`disable_foreign_config`] — strip the disable markers, restoring the original directives.
#[tauri::command]
fn restore_foreign_config(state: State<Backend>) -> Result<bool, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { config_dir, .. } => {
            cageq_core::restore_foreign_config(&config_dir.join("config.txt")).map_err(|e| e.to_string())
        }
    }
}

/// §5.3c: start post-EQ loudness monitoring on `device` (the selected endpoint's id, or `None`
/// for the default render endpoint). Opens WASAPI loopback and streams `MeterUpdate`s (and the
/// spectrum/scope streams) to every channel registered via `subscribe_*` — see [`StreamSubs`] for
/// why channels, not events. Replaces any monitor already running (e.g. after a device change).
#[tauri::command]
fn start_monitor(
    device: Option<String>,
    state: State<MonitorState>,
    scope_viewers: State<ScopeViewers>,
    subs: State<StreamSubs>,
) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(existing) = guard.take() {
        existing.stop();
    }
    let meter_subs = subs.meter.clone();
    let spectrum_subs = subs.spectrum.clone();
    let scope_subs = subs.scope.clone();
    let (meter_alive, spectrum_alive, scope_alive) =
        (subs.alive.clone(), subs.alive.clone(), subs.alive.clone());
    let monitor = cageq_monitor::Monitor::start(
        device,
        scope_viewers.0.clone(),
        move |update| fan_out(&meter_subs, &meter_alive, update),
        move |spectrum| fan_out(&spectrum_subs, &spectrum_alive, spectrum),
        move |scope| fan_out(&scope_subs, &scope_alive, scope),
    )?;
    *guard = Some(monitor);
    Ok(())
}

/// Register this webview's meter-stream channel — once per webview lifetime (frontend
/// `streams.ts` guards against re-registering); dropped when that webview is destroyed
/// (`drop_subs`), keyed by its label.
#[tauri::command]
fn subscribe_meter(
    webview: tauri::Webview,
    channel: tauri::ipc::Channel<cageq_monitor::MeterUpdate>,
    subs: State<StreamSubs>,
) {
    beat(&subs.alive, webview.label());
    register(&subs.meter, webview.label(), channel);
}

/// Register this webview's spectrum-stream channel (see `subscribe_meter`).
#[tauri::command]
fn subscribe_spectrum(
    webview: tauri::Webview,
    channel: tauri::ipc::Channel<cageq_monitor::SpectrumUpdate>,
    subs: State<StreamSubs>,
) {
    beat(&subs.alive, webview.label());
    register(&subs.spectrum, webview.label(), channel);
}

/// Register this webview's scope-stream channel (see `subscribe_meter`). Whether the scope
/// stream carries data at all stays gated by `set_scope_viewer`, orthogonal to the transport.
#[tauri::command]
fn subscribe_scope(
    webview: tauri::Webview,
    channel: tauri::ipc::Channel<cageq_monitor::ScopeUpdate>,
    subs: State<StreamSubs>,
) {
    beat(&subs.alive, webview.label());
    register(&subs.scope, webview.label(), channel);
}

/// Liveness beat from a webview's stream bus (frontend `streams.ts`, ~1 Hz). A webview that stops
/// beating stops being sent to — see [`StreamSubs`] for why this backstop exists on top of the
/// `WindowEvent::Destroyed` cleanup.
#[tauri::command]
fn stream_heartbeat(webview: tauri::Webview, subs: State<StreamSubs>) {
    beat(&subs.alive, webview.label());
}

/// A vectorscope view opened (`active = true`) or closed (`false`). Refcounted so the loopback
/// emits the `scope` stream only while at least one view (inline or the pop-out window) is open.
#[tauri::command]
fn set_scope_viewer(active: bool, scope_viewers: State<ScopeViewers>) {
    use std::sync::atomic::Ordering;
    if active {
        scope_viewers.0.fetch_add(1, Ordering::Relaxed);
    } else {
        // Saturating decrement — never underflow if a stray "close" arrives.
        let _ = scope_viewers
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(1)));
    }
}

/// §5.3c: stop loopback monitoring (idempotent — no-op if nothing is running).
#[tauri::command]
fn stop_monitor(state: State<MonitorState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(existing) = guard.take() {
        existing.stop();
    }
    Ok(())
}

/// Self-test: start pink noise on `device` (or the default endpoint) so it plays out through
/// EqAPO while the loopback monitor captures the result — the frontend compares the captured
/// spectrum shape against the applied EQ curve to prove corrections reach the output. Replaces any
/// test signal already playing.
#[tauri::command]
fn start_test_signal(device: Option<String>, state: State<TestSignalState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(existing) = guard.take() {
        existing.stop();
    }
    *guard = Some(cageq_monitor::TestSignal::start(device)?);
    Ok(())
}

/// Stop the self-test signal (idempotent — no-op if nothing is playing).
#[tauri::command]
fn stop_test_signal(state: State<TestSignalState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(existing) = guard.take() {
        existing.stop();
    }
    Ok(())
}

/// Open the modern Windows Sound settings for the selected output, so the user can change its
/// playback format (sample rate / bit depth). CAGEq only *reads* the format (via the loopback's
/// mix rate) — the device-format property is read-only per Microsoft, so we point at the native
/// tool rather than reimplement it (filter.md §8). When the selected device is the Windows
/// default, jump straight to its properties page (format dropdown); otherwise open the general
/// device list (there's no documented deep-link to a specific non-default device).
#[tauri::command]
fn open_output_settings(device: Option<String>) -> Result<(), String> {
    let is_default = match &device {
        None => true,
        Some(id) => cageq_monitor::default_render_id()
            .map(|d| d.to_ascii_lowercase().contains(&id.to_ascii_lowercase()))
            .unwrap_or(false),
    };
    let uri = if is_default {
        "ms-settings:sound-defaultoutputproperties"
    } else {
        "ms-settings:sound-devices"
    };
    open_uri(uri)
}

/// Fire-and-forget open of a URI via the OS shell. Windows only carries the `ms-settings:`
/// scheme this feature uses; elsewhere it's a no-op.
#[cfg(windows)]
fn open_uri(uri: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // `cmd /C start "" <uri>` — the empty "" is start's window-title arg, so the URI isn't
    // swallowed as the title. Handles the ms-settings: scheme that ShellExecute registers.
    // CREATE_NO_WINDOW keeps the transient cmd from flashing a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("cmd")
        .args(["/C", "start", "", uri])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
#[cfg(not(windows))]
fn open_uri(_uri: &str) -> Result<(), String> {
    Ok(())
}

/// The raw headphone measurement + target curves (centered, shared dBr reference) for the
/// §5.2 nerd overlays. Measurement/target-only, so it's fetched on its own rather than
/// threaded through the apply/slot pipeline. Relayed as-is (`{ raw_curve, target_curve }`).
#[tauri::command]
fn measurement_curves(headphone: String, target: Option<String>, state: State<Backend>) -> Result<Value, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => {
            let mut params = Map::new();
            params.insert("headphone".into(), Value::String(headphone));
            if let Some(t) = target {
                params.insert("target".into(), Value::String(t));
            }
            core.request("measurement_curves", Value::Object(params)).map_err(|e| e.to_string())
        }
    }
}

/// The AutoEq target curves. Relayed as-is.
#[tauri::command]
fn list_targets(state: State<Backend>) -> Result<Value, String> {
    match state.inner() {
        Backend::Failed(e) => Err(e.clone()),
        Backend::Ready { core, .. } => core
            .request_with_deadline("list_targets", json!({}), CATALOGUE_BUSY_RESPONSE)
            .map_err(|e| e.to_string()),
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
    /// §3.5 resume state — the last editable session (active slot, device, per-slot
    /// inputs) so the app restores and re-applies on the next launch. An opaque,
    /// UI-owned JSON blob (shape defined by the frontend): the backend only persists and
    /// returns it, so adding a field there never needs a Rust change.
    #[serde(default)]
    resume: Option<Value>,
    /// §3.5 preset library — the user's saved session presets and filter templates. Same
    /// opaque UI-owned-blob treatment as `resume` (persist + return verbatim).
    #[serde(default)]
    library: Option<Value>,
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
            resume: None,
            library: None,
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
    // Set once, before the sidecar (or any restart of it) is ever spawned — a child process
    // inherits the parent's environment, so this alone is enough for every launch/restart to see it.
    set_cache_dir_env(&resolve_cache_dir());
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

/// Busy deadline for `list_headphones`/`list_targets` specifically, in place of
/// `watchdog_cfg()`'s fit-tuned `busy_response` (25 s). A cold catalogue build fetches every
/// distinct AutoEq source's name_index.tsv, now in parallel (see sidecar_dsp.py) rather than one
/// at a time, but network conditions vary a lot more than a compute-bound fit's runtime does —
/// 25 s was tight enough that a rebuild could blow it, which made the watchdog conclude the
/// sidecar had hung and kill it *mid-fetch*, turning "slow" into a restart loop. This is only ever
/// reached on a cold cache (build_index/list_targets both skip straight to the cached-on-disk
/// result otherwise), so the cost of a generous ceiling here is rare and one-off, not per-request.
const CATALOGUE_BUSY_RESPONSE: Duration = Duration::from_secs(120);

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

/// Where the sidecar caches downloaded AutoEq data (the headphone/target catalogue plus every
/// fetched measurement/target CSV, sidecar_dsp.py's `CAGEQ_CACHE_DIR`). Precedence: an explicit
/// override (dev/tests) → `%LOCALAPPDATA%\CAGEq\cache` → a dev temp folder off-Windows. Must be a
/// stable, persistent location — it used to default (inside the sidecar) to the OS temp dir
/// whenever this env var was unset, which is exactly what always happened, since nothing here
/// ever set it. Temp is fair game for Storage Sense/disk-cleanup tools to wipe, so a slot the user
/// had already used — and reasonably expects cached — would silently re-hit GitHub after any such
/// cleanup. `%LOCALAPPDATA%` is the same kind of per-user, persistent-until-uninstall location the
/// rest of the app data lives in, so caches survive reboots and cleanup sweeps like they should.
fn resolve_cache_dir() -> PathBuf {
    if let Ok(dir) = env::var("CAGEQ_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(local) = env::var("LOCALAPPDATA") {
        return PathBuf::from(local).join("CAGEq").join("cache");
    }
    std::env::temp_dir().join("cageq-cache") // non-Windows dev fallback
}

/// Export `CAGEQ_CACHE_DIR` for the sidecar child to inherit. `set_var` is `unsafe` (it can race
/// another thread's `env::var` read) — sound here because this runs once, synchronously, before
/// `run()` spawns any thread that might read the environment concurrently.
fn set_cache_dir_env(dir: &Path) {
    unsafe { env::set_var("CAGEQ_CACHE_DIR", dir) };
}

fn sidecar_root() -> PathBuf {
    // this crate is cageq-app/src-tauri; the sidecar crate is a sibling of cageq-app.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("cageq-sidecar")
}

/// Resolve how to launch the sidecar, plus a human label for the UI. Precedence:
///   1. `CAGEQ_PYTHON` / `CAGEQ_SIDECAR_SCRIPT` env override (tests / other machine),
///   2. **debug build:** the dev venv + `sidecar_dsp.py`, else the bundled exe;
///      **release build:** the bundled frozen exe, else the dev venv,
///   3. a `py`-resolved interpreter + the dependency-free stub.
///
/// The profile-dependent order matters: Tauri stages bundled resources into
/// `target/debug/` for `tauri dev` too, so a frozen bundle would otherwise shadow the
/// live script and silently ignore every edit to `sidecar_dsp.py`. In a dev build the
/// live script must win; in a release build (no source tree) the bundle must.
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

    // 2. Bundled frozen exe vs live dev venv, ordered by build profile.
    let venv = root.join(".venv").join("Scripts").join("python.exe");
    let dsp = root.join("python").join("sidecar_dsp.py");
    let frozen = bundled.filter(|e| e.exists()).map(|e| {
        (SidecarSource::Frozen(e.to_path_buf()), format!("AutoEq DSP, bundled ({})", e.display()))
    });
    let dev = (venv.exists() && dsp.exists()).then(|| {
        let label = format!("AutoEq DSP, dev venv ({})", venv.display());
        (SidecarSource::Python { python: venv.clone(), script: dsp.clone() }, label)
    });
    let picked = if cfg!(debug_assertions) { dev.or(frozen) } else { frozen.or(dev) };
    if let Some(picked) = picked {
        return picked;
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
        // Release a destroyed webview's stream channels. Without this the closed pop-out scope
        // window's channel stays subscribed forever and its ~20 KB/frame payloads pile up in
        // Tauri's `ChannelDataIpcQueue` — a ~1 MB/s leak in *this* process, see `StreamSubs`.
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                use tauri::Manager;
                drop_subs(&window.state::<StreamSubs>(), window.label());
            }
        })
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
            app.manage(MonitorState::default());
            app.manage(TestSignalState::default());
            app.manage(ScopeViewers::default());
            app.manage(StreamSubs::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            apply,
            seed_slot,
            warm_fit,
            activate_slot,
            isolate,
            copy_slot,
            set_device,
            status,
            list_headphones,
            list_devices,
            config_foreign_directives,
            disable_foreign_config,
            restore_foreign_config,
            start_monitor,
            stop_monitor,
            set_scope_viewer,
            subscribe_meter,
            subscribe_spectrum,
            subscribe_scope,
            stream_heartbeat,
            start_test_signal,
            stop_test_signal,
            open_output_settings,
            list_targets,
            measurement_curves,
            get_loudness,
            set_loudness,
            preview_loudness,
            get_confirm_final_volume,
            set_confirm_final_volume,
            get_selection,
            get_resume,
            set_resume,
            get_library,
            set_library,
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
                resume: Some(serde_json::json!({ "activeSlot": "B", "deviceId": "dev-1" })),
                library: Some(serde_json::json!({ "presets": [{ "id": "p1", "name": "Warm & Relaxed" }], "templates": [] })),
            },
        )
        .expect("save");
        let reloaded = load_settings_from(&path);
        assert_eq!(reloaded.loudness, want, "reloaded loudness should match saved");
        assert_eq!(reloaded.selection.headphone, selection.headphone, "reloaded headphone should match");
        assert_eq!(reloaded.selection.target, selection.target, "reloaded target should match");
        assert_eq!(reloaded.last_hash.as_deref(), Some("abc123"), "reloaded hash should match");
        assert!(!reloaded.confirm_final_volume, "reloaded confirm flag should match saved (false)");
        // The resume blob round-trips verbatim (UI-owned shape).
        assert_eq!(reloaded.resume.as_ref().and_then(|r| r["activeSlot"].as_str()), Some("B"), "resume blob should round-trip");
        // The preset library round-trips verbatim too.
        assert_eq!(
            reloaded.library.as_ref().and_then(|l| l["presets"][0]["name"].as_str()),
            Some("Warm & Relaxed"),
            "library blob should round-trip"
        );
        // A missing field (old settings.json) defaults the confirm flag ON.
        std::fs::write(&path, r#"{"loudness":{"base_pregain_db":-9.0,"mode":"Comparison"}}"#).unwrap();
        assert!(load_settings_from(&path).confirm_final_volume, "missing confirm flag defaults to true");

        let _ = std::fs::remove_file(&path);
    }
}
