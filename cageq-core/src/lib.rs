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
//! The sidecar's `calculate_filters` reply deserializes *directly* into the
//! config-writer's [`DeviceConfig`] — the shared type is the contract between the
//! Python DSP and the Rust writer.
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
use cageq_sidecar::{Sidecar, SidecarError};
use cageq_watchdog::{Supervisor, SupervisorError, WatchdogConfig};
use serde::Serialize;
use serde_json::{Map, Value};

// Re-export the domain types through cageq-core so the app/UI layer depends only on
// this facade, not each building-block crate. These are also used internally below.
pub use cageq_config_writer::{DeviceConfig, Filter, FilterType, StartupDecision};
pub use cageq_watchdog::Health;

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
#[derive(Debug, Clone)]
pub struct Applied {
    /// cageq.txt content hash — persist in settings.json for the next startup check.
    pub hash: String,
    /// Device the config was written for (echoed by the DSP).
    pub device: String,
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
    let device_config: DeviceConfig = serde_json::from_value(reply)?;

    let hash = cw::apply(&inner.config_dir, std::slice::from_ref(&device_config))?;

    *inner.last_applied.lock().unwrap() = Some(request);
    inner.applied_count.fetch_add(1, Ordering::SeqCst);
    Ok(Applied { hash, device: device_config.device })
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
