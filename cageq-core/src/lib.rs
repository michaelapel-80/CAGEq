//! Orchestrator — the "Rust Core" that ties the three building blocks into one
//! flow (filter.md §2, the Rust Core layer):
//!
//!   * owns the DSP sidecar through the fail-safe [`Supervisor`] (watchdog),
//!   * turns a user request into filters by asking the sidecar to
//!     `calculate_filters`, then writes them to EqAPO via the config-writer,
//!   * after a watchdog recovery, re-applies the last-good config so EqAPO leaves
//!     the safe state (the "auto-leave" §7.2 leaves to this layer),
//!   * runs the §3.0 startup-integrity check.
//!
//! The sidecar's `calculate_filters` reply carries the fitted filters plus two
//! curve-derived quantities (§4.1 loudness target, §4.2 curve peak); the core
//! composes the final `Preamp:` from them and the user's §4.0 base pre-gain, then
//! builds the config-writer's [`DeviceConfig`]. The DSP reports physics; the core
//! owns the preamp/clipping policy.
//!
//! ## Threads
//! One background **reconciler** thread watches the supervisor's recovery counter;
//! when it advances (the watchdog brought a fresh sidecar up), the reconciler
//! re-applies the last request. Everything else runs on the caller's thread. The
//! `Supervisor` is shared as `Arc<Supervisor>` (its `call`/`health`/`recoveries`
//! all take `&self`), so caller and reconciler can both drive it; the driver
//! serialises the actual requests, and an `apply_lock` serialises calc+write so the
//! two never interleave a write to cageq.txt.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cageq_config_writer::{self as cw, WriteError};
use cageq_watchdog::{Supervisor, SupervisorError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// Re-export the domain types through cageq-core so the app/UI layer depends only on
// this facade, not each building-block crate. Several are also used internally below
// (the `pub use` both re-exports and brings them into scope here).
pub use cageq_config_writer::{
    AudioDevice, DeviceConfig, Filter, FilterType, StartupDecision, detect_eqapo_config_dir,
    list_render_devices,
};
pub use cageq_sidecar::{Sidecar, SidecarError};
pub use cageq_watchdog::{Health, WatchdogConfig};

// ---------------------------------------------------------------------------
// Public request/response types
// ---------------------------------------------------------------------------

/// What the UI asks the core to apply. `device` selects the target; `inputs`
/// carries the DSP-specific payload (measurement, target curve, custom filters, …)
/// opaque to the core — it flattens into the JSON sent to the sidecar. Against the
/// stub, `inputs` can be empty.
#[derive(Debug, Clone, Serialize)]
pub struct CalcRequest {
    pub device: String,
    #[serde(flatten)]
    pub inputs: Map<String, Value>,
}

impl CalcRequest {
    /// Convenience for the common "just pick a device" case.
    pub fn for_device(device: impl Into<String>) -> Self {
        CalcRequest { device: device.into(), inputs: Map::new() }
    }
}

/// The result of a successful apply.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    /// cageq.txt content hash — persist in settings.json for the next startup check.
    pub hash: String,
    /// Device the config was written for (echoed by the DSP).
    pub device: String,
    /// The composed final `Preamp:` value written (filter.md §4.0/§4.2).
    pub preamp_db: f64,
    /// True when the §4.2 emergency clipping ceiling bound the level instead of the
    /// §4.1 loudness match — surfaced to the UI as the §5.1 per-slot clipping warning.
    pub clipping_warning: bool,
}

/// filter.md §4.0 default base pre-gain (user headroom), in dB. A conservative,
/// always-applied reserve so the §4.1 loudness match stays the binding term for
/// typical curves and the §4.2 ceiling only trips on genuinely extreme ones.
pub const DEFAULT_BASE_PREGAIN_DB: f64 = -9.0;

/// filter.md §4.0 loudness mode — the "Vergleichsmodus ↔ Finale Lautstärke" toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoudnessMode {
    /// A/B-fair: base pre-gain + §4.1 loudness match, capped by the §4.2 ceiling.
    /// Every curve ends up equally loud so comparisons judge timbre, not level.
    Comparison,
    /// Maximum clipping-free level (peak at 0 dBFS): no comfort buffer, no loudness
    /// match — AQUA's default. Loudest safe playback for actual listening.
    FinalVolume,
}

/// filter.md §4.0 user loudness settings: the base pre-gain and which mode is active.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoudnessSettings {
    /// §4.0 base pre-gain (user headroom), in dB. Applies in [`LoudnessMode::Comparison`].
    pub base_pregain_db: f64,
    pub mode: LoudnessMode,
}

impl Default for LoudnessSettings {
    fn default() -> Self {
        LoudnessSettings { base_pregain_db: DEFAULT_BASE_PREGAIN_DB, mode: LoudnessMode::Comparison }
    }
}

/// The sidecar's `calculate_filters` reply: the fitted filters plus the two
/// curve-derived quantities the core composes the preamp from. The DSP reports these
/// physical quantities; the *policy* (base pre-gain, clipping ceiling) lives here.
#[derive(Debug, Clone, Deserialize)]
struct CalcResult {
    device: String,
    filters: Vec<Filter>,
    /// §4.1 K-weighted loudness compensation for this curve. Defaults to 0 (a
    /// level-neutral offset) so the dependency-free stub still composes.
    #[serde(default)]
    g_target_db: f64,
    /// The composed EQ curve's positive peak, for the §4.2 clipping ceiling.
    #[serde(default)]
    g_max_peak_db: f64,
}

/// The composed preamp for one curve (filter.md §4.0 + §4.2).
struct Preamp {
    /// Final `Preamp:` value written to cageq.txt.
    db: f64,
    /// True when the §4.2 ceiling bound the level instead of the §4.1 loudness match.
    clipping_warning: bool,
}

/// Compose the final preamp from the curve quantities and the user's loudness
/// settings (filter.md §4.0 + §4.2).
///
/// [`LoudnessMode::FinalVolume`]: maximum clipping-free level — `Preamp = -G_max_peak`
/// (peak at 0 dBFS), no buffer, no loudness match; the ceiling can't be "overridden"
/// so there's nothing to warn about.
///
/// [`LoudnessMode::Comparison`]:
/// ```text
///   G_max_allowed = -G_max_peak - base_pregain_db   (§4.2, buffer-aware)
///   relative      = min(G_target, G_max_allowed)     (§4.2)
///   Preamp_final  = base_pregain_db + relative        (§4.0)
/// ```
/// When the ceiling binds (`G_max_allowed < G_target`) `base_pregain_db` cancels and
/// `Preamp_final == -G_max_peak` — the comfort buffer is spent to stay just below
/// 0 dBFS rather than needlessly quiet. The never-clip guarantee holds in both modes.
fn compose_preamp(g_target_db: f64, g_max_peak_db: f64, s: &LoudnessSettings) -> Preamp {
    match s.mode {
        LoudnessMode::FinalVolume => Preamp { db: -g_max_peak_db, clipping_warning: false },
        LoudnessMode::Comparison => {
            let base = s.base_pregain_db;
            let g_max_allowed = -g_max_peak_db - base;
            let clipping_warning = g_max_allowed < g_target_db;
            let relative = g_target_db.min(g_max_allowed);
            Preamp { db: base + relative, clipping_warning }
        }
    }
}

/// Errors from the core.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("could not start the sidecar: {0}")]
    Spawn(#[from] SidecarError),
    #[error("sidecar/watchdog error: {0}")]
    Supervisor(#[from] SupervisorError),
    #[error("config write error: {0}")]
    Write(#[from] WriteError),
    #[error("could not (de)serialize a DSP message: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// Core
// ---------------------------------------------------------------------------

/// State shared with the reconciler thread.
struct Inner {
    config_dir: PathBuf,
    /// Serialises a calc+write so a user apply and a recovery re-apply never
    /// interleave their writes to cageq.txt.
    apply_lock: Mutex<()>,
    last_applied: Mutex<Option<CalcRequest>>,
    applied_count: AtomicU32,
    shutdown: AtomicBool,
    startup: StartupDecision,
    /// §4.0 loudness settings (base pre-gain + mode) applied to every composed preamp.
    loudness: Mutex<LoudnessSettings>,
}

/// The orchestrator handle.
pub struct Core {
    supervisor: Arc<Supervisor>,
    inner: Arc<Inner>,
    reconciler: Option<JoinHandle<()>>,
}

impl Core {
    /// Start the core over EqAPO's `config_dir`. `spawn_fn` produces DSP sidecars
    /// (used by the watchdog for the initial spawn and every restart). `expected_hash`
    /// is the cageq.txt hash settings.json remembered, or `None` on a clean install —
    /// it drives the startup-integrity verdict ([`Core::startup_decision`]).
    pub fn start<F>(
        config_dir: impl Into<PathBuf>,
        spawn_fn: F,
        watchdog: WatchdogConfig,
        expected_hash: Option<&str>,
    ) -> Result<Self, CoreError>
    where
        F: Fn() -> Result<Sidecar, SidecarError> + Send + 'static,
    {
        let config_dir = config_dir.into();
        let cageq_path = config_dir.join(cw::CAGEQ_FILENAME);

        // §3.0: is what we remember still what's on disk?
        let state = cw::read_cageq_state(&cageq_path)?;
        let startup = cw::decide_startup(&state, expected_hash);

        let poll = watchdog.tick;
        let supervisor = Arc::new(Supervisor::start(spawn_fn, watchdog, cageq_path)?);
        let inner = Arc::new(Inner {
            config_dir,
            apply_lock: Mutex::new(()),
            last_applied: Mutex::new(None),
            applied_count: AtomicU32::new(0),
            shutdown: AtomicBool::new(false),
            startup,
            loudness: Mutex::new(LoudnessSettings::default()),
        });

        let reconciler = {
            let (sup, inner) = (Arc::clone(&supervisor), Arc::clone(&inner));
            thread::spawn(move || reconcile_loop(sup, inner, poll))
        };

        Ok(Core { supervisor, inner, reconciler: Some(reconciler) })
    }

    /// Calculate filters for `request` via the sidecar and write them to EqAPO.
    /// Records the request as last-good for post-recovery re-apply.
    pub fn apply(&self, request: CalcRequest) -> Result<Applied, CoreError> {
        do_apply(&self.supervisor, &self.inner, request)
    }

    /// Send a raw request to the sidecar (the core is the process's front door).
    /// Used for methods beyond `calculate_filters` and by tests to provoke faults.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, CoreError> {
        Ok(self.supervisor.call(method, params)?)
    }

    /// The startup-integrity verdict computed at [`Core::start`].
    pub fn startup_decision(&self) -> StartupDecision {
        self.inner.startup
    }

    /// Current supervisor health.
    pub fn health(&self) -> Health {
        self.supervisor.health()
    }

    /// How many times the sidecar has been recovered by the watchdog.
    pub fn recoveries(&self) -> u32 {
        self.supervisor.recoveries()
    }

    /// How many times a config has been written (initial applies + re-applies).
    pub fn applied_count(&self) -> u32 {
        self.inner.applied_count.load(Ordering::SeqCst)
    }

    /// The §4.0 loudness settings (base pre-gain + mode) currently in effect.
    pub fn loudness(&self) -> LoudnessSettings {
        *self.inner.loudness.lock().unwrap()
    }

    /// Set the §4.0 loudness settings. Takes effect on the next apply (see
    /// [`Core::reapply`] to push the change onto the currently-applied config) and on
    /// any post-recovery re-apply; does not rewrite the current config on its own.
    pub fn set_loudness(&self, settings: LoudnessSettings) {
        *self.inner.loudness.lock().unwrap() = settings;
    }

    /// Re-run the last applied request with the current settings — e.g. after a
    /// loudness-settings change, to update the written preamp live. `None` if nothing
    /// has been applied yet; otherwise the fresh [`Applied`] (or a write/DSP error).
    pub fn reapply(&self) -> Option<Result<Applied, CoreError>> {
        let last = self.inner.last_applied.lock().unwrap().clone()?;
        Some(do_apply(&self.supervisor, &self.inner, last))
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Stop the reconciler first, then let the Supervisor Arc drop with the struct
        // (its Drop tears down the watchdog threads and the sidecar).
        self.inner.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.reconciler.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// The calc+write step and the reconciler
// ---------------------------------------------------------------------------

/// The one place a config is produced and written: ask the sidecar to compute
/// filters, deserialize straight into `DeviceConfig`, write via the config-writer,
/// remember the request. Held under `apply_lock` so callers and the reconciler
/// serialise.
fn do_apply(supervisor: &Supervisor, inner: &Inner, request: CalcRequest) -> Result<Applied, CoreError> {
    let _guard = inner.apply_lock.lock().unwrap();

    let params = serde_json::to_value(&request)?;
    let reply = supervisor.call("calculate_filters", params)?;
    let result: CalcResult = serde_json::from_value(reply)?;

    // Compose the final preamp here (policy), from the curve quantities the DSP
    // reported (physics): §4.0 base pre-gain + §4.1 loudness match, capped by §4.2.
    let loudness = *inner.loudness.lock().unwrap();
    let preamp = compose_preamp(result.g_target_db, result.g_max_peak_db, &loudness);
    let device_config =
        DeviceConfig { device: result.device, preamp_db: preamp.db, filters: result.filters };

    let hash = cw::apply(&inner.config_dir, std::slice::from_ref(&device_config))?;

    *inner.last_applied.lock().unwrap() = Some(request);
    inner.applied_count.fetch_add(1, Ordering::SeqCst);
    Ok(Applied {
        hash,
        device: device_config.device,
        preamp_db: preamp.db,
        clipping_warning: preamp.clipping_warning,
    })
}

/// Watch the supervisor's recovery counter; each time it advances and the sidecar
/// is `Running` again, re-apply the last-good config so EqAPO leaves the safe state.
fn reconcile_loop(supervisor: Arc<Supervisor>, inner: Arc<Inner>, poll: Duration) {
    let mut handled = supervisor.recoveries();
    loop {
        thread::sleep(poll);
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let recoveries = supervisor.recoveries();
        if recoveries <= handled || !matches!(supervisor.health(), Health::Running) {
            continue;
        }
        // A fresh sidecar is up. Re-apply the last request; if it faults again the
        // watchdog handles it and we retry on the next recovery.
        let last = inner.last_applied.lock().unwrap().clone();
        match last {
            // Re-apply failure leaves `handled` unchanged so we retry next recovery.
            Some(req) => {
                if do_apply(&supervisor, &inner, req).is_ok() {
                    handled = recoveries;
                }
            }
            None => handled = recoveries, // nothing applied yet, nothing to restore
        }
    }
}

/// The cageq.txt path inside an EqAPO config directory.
pub fn cageq_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(cw::CAGEQ_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    fn comparison(base: f64) -> LoudnessSettings {
        LoudnessSettings { base_pregain_db: base, mode: LoudnessMode::Comparison }
    }

    #[test]
    fn typical_curve_the_loudness_match_binds() {
        // Moderate peak (+6), moderate loudness target (-4), default headroom.
        // G_max_allowed = -6 - (-9) = +3 > G_target, so G_target binds; no warning.
        let p = compose_preamp(-4.0, 6.0, &comparison(DEFAULT_BASE_PREGAIN_DB));
        approx(p.db, -13.0); // base_pregain (-9) + relative (-4)
        assert!(!p.clipping_warning);
    }

    #[test]
    fn extreme_peak_the_ceiling_binds_and_base_pregain_cancels() {
        // Stacked custom filters: peak +25. The §4.2 ceiling takes over and the
        // comfort buffer is spent — Preamp_final collapses to exactly -G_max_peak.
        let p = compose_preamp(-6.0, 25.0, &comparison(DEFAULT_BASE_PREGAIN_DB));
        approx(p.db, -25.0); // == -G_max_peak, independent of base_pregain
        assert!(p.clipping_warning);
    }

    #[test]
    fn ceiling_never_exceeds_zero_dbfs_regardless_of_base_pregain() {
        // For any base pre-gain, a positive-peak curve stays at or below -G_max_peak.
        for base in [-3.0, -9.0, -18.0] {
            let p = compose_preamp(10.0, 12.0, &comparison(base)); // absurd +10 vs +12 peak
            assert!(p.db <= -12.0 + 1e-9, "base {base}: preamp {} > -peak", p.db);
            assert!(p.clipping_warning);
        }
    }

    #[test]
    fn stub_defaults_yield_just_the_base_pregain() {
        // Missing g_target/g_max_peak default to 0 (the stub): a flat, level-neutral
        // curve leaves only the base pre-gain, and never trips the ceiling.
        let p = compose_preamp(0.0, 0.0, &comparison(DEFAULT_BASE_PREGAIN_DB));
        approx(p.db, DEFAULT_BASE_PREGAIN_DB);
        assert!(!p.clipping_warning);
    }

    #[test]
    fn final_volume_is_max_clipping_free_ignoring_buffer_and_match() {
        // FinalVolume: peak sits exactly at 0 dBFS, base pre-gain and loudness target
        // are both ignored, and there is no ceiling override to warn about.
        let s = LoudnessSettings { base_pregain_db: -9.0, mode: LoudnessMode::FinalVolume };
        let p = compose_preamp(-4.0, 6.0, &s);
        approx(p.db, -6.0); // -G_max_peak, regardless of base_pregain / G_target
        assert!(!p.clipping_warning);
    }
}
