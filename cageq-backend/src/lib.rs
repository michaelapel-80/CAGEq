//! The EQ **backend abstraction** — what CAGEq needs from whatever actually applies a
//! filter set to the audio pipeline, and the backend-neutral domain types both sides
//! speak in.
//!
//! ## Why this crate exists
//! CAGEq shipped against Equalizer APO, reached through one file writer
//! (`cageq-config-writer`). It is moving to its **own APO** (filter.md §5.3c) to drop
//! that runtime dependency, but must keep working on EqAPO throughout — for the
//! transition period and permanently, for users who already run it. So the orchestrator
//! (`cageq-core`) talks to [`EqBackend`] instead of to a file writer, and the two
//! implementations differ far more than "where do the bytes go":
//!
//! | | Equalizer APO | CAGEq's own APO |
//! |---|---|---|
//! | transport | rewrite `cageq.txt`, EqAPO's directory watcher reloads | push coefficients into shared memory |
//! | write cadence | ≥15 ms apart, or a reload lands mid-crossfade (§5.3) | no such wall — no reload to collide with |
//! | transitions | fixed ~10 ms native crossfade, from a **cold** cascade | owned in-engine, state carried by construction |
//! | safe state | write a −120 dB `cageq.txt` | control flag, plus the APO self-bypassing on a stale heartbeat |
//! | foreign config | `config.txt` can carry other tools' directives | no shared config file to collide over |
//!
//! Those differences are modelled as [`Capabilities`] rather than hidden behind a
//! lowest-common-denominator interface: the point of the new backend is precisely the
//! things EqAPO *cannot* do, so pretending the two are interchangeable would forfeit it.
//! The EqAPO backend stays permanently but **feature-frozen** — capability-gated
//! features light up only on the backend that can actually honour them, and the UI is
//! expected to degrade gracefully rather than assume parity.
//!
//! ## What lives here vs. in a backend
//! Here: the domain types (a filter, a device config, an endpoint) and endpoint
//! enumeration — all of which are properties of *the audio system*, not of any backend.
//! In a backend: every rendering/transport/lifecycle detail, including how a filter is
//! serialised (EqAPO's `PK`/`LSC`/`HSC` tokens are one backend's wire format, not part
//! of the domain).

use std::time::Duration;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Domain types — backend-neutral
// ---------------------------------------------------------------------------

/// The filter shapes CAGEq fits and applies. Deliberately the *whole* set the app uses:
/// a custom backend only has to implement these five, which is what makes replacing
/// EqualizerAPO tractable at all (it implements a great deal more that CAGEq never asks
/// for — graphic EQ, convolution, expressions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterType {
    Peaking,
    LowShelf,
    HighShelf,
    /// No gain (unity-peak); used only by the §5.2 "isolate" audition.
    Bandpass,
    /// Pivots the spectrum around `freq_hz`: cut below, boost above (or vice versa for a
    /// negative `gain_db`), `gain_db` the *total* span between the two asymptotes. Not a
    /// biquad of its own — see [`expand_tilts`], which every backend/curve consumer must
    /// run before matching on `FilterType`, since neither EqualizerAPO nor the RBJ
    /// cookbook has a native single-stage tilt.
    Tilt,
}

/// One parametric band. All four fields are the standard RBJ-biquad parameters, so any
/// backend can build the same cascade from them without further negotiation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Filter {
    pub kind: FilterType,
    pub freq_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

/// Expand every [`FilterType::Tilt`] into the pair of complementary shelves that
/// actually realise it — a low shelf cut and a high shelf boost of equal-and-opposite
/// magnitude, pivoting at the same `freq_hz` — and pass every other filter through
/// unchanged. Every match on [`FilterType`] downstream of the domain layer (a backend's
/// wire format, the RBJ coefficient math, the chart's curve composition) must call this
/// first: none of those have — or need — a native single-stage tilt, since this
/// substitution is exact and reuses shelf math they already implement.
///
/// Callers can call this unconditionally; a filter list with no `Tilt` in it round-trips
/// through unchanged (aside from being cloned into a new `Vec`).
pub fn expand_tilts(filters: &[Filter]) -> Vec<Filter> {
    let mut out = Vec::with_capacity(filters.len());
    for f in filters {
        if f.kind == FilterType::Tilt {
            out.push(Filter { kind: FilterType::LowShelf, freq_hz: f.freq_hz, gain_db: -f.gain_db / 2.0, q: f.q });
            out.push(Filter { kind: FilterType::HighShelf, freq_hz: f.freq_hz, gain_db: f.gain_db / 2.0, q: f.q });
        } else {
            out.push(*f);
        }
    }
    out
}

/// One device's managed configuration. Deserializable so the sidecar's
/// `calculate_filters` reply maps straight onto it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Which endpoint this applies to, as the backend addresses it. Today that is the
    /// endpoint GUID for both backends (EqAPO matches its `Device:` line against it).
    pub device: String,
    pub preamp_db: f64,
    pub filters: Vec<Filter>,
}

/// A Windows audio playback (render) endpoint the user can scope the EQ to (§3.0).
///
/// Deliberately carries no "is a backend attached here" flag: that is a question *about
/// a backend*, answered by [`EqBackend::drives_endpoint`], and the answer differs per
/// backend on the same endpoint. Keeping it off this struct is what lets one device list
/// serve both backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    /// The endpoint GUID (registry subkey name), a stable unique id — and also what
    /// goes on a [`DeviceConfig::device`] line: exact (one device) and ASCII, unlike the
    /// display name, which is fuzzily matched and mojibake'd non-ASCII names into
    /// cageq.txt (the registry read is UTF-16, not the file's encoding).
    pub id: String,
    /// Human-readable name for the UI, e.g. "Lautsprecher (SPL Phonitor One)".
    pub name: String,
}

/// The verdict of the startup integrity check (§3.0) — how far to trust the resume state
/// remembered in settings.json. Backend-neutral: every backend has some notion of "what
/// is applied right now", even though only EqAPO's lives in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupDecision {
    /// The backend has no CAGEq state yet. Start clean.
    FirstRun,
    /// What is applied is byte-identical to what CAGEq last wrote (hash matches
    /// settings.json). The remembered resume state is trustworthy.
    ResumeTrusted,
    /// CAGEq's safe state (§7.2) is still applied: the safety shutdown was active at the
    /// last exit/crash. Offer to restore the last state.
    SafeStateStillActive,
    /// What is applied differs from what CAGEq last wrote and isn't the safe state —
    /// changed by the user or another tool. Surface a neutral notice; don't keep
    /// asserting a specific preset/slot is active (fail-early).
    ExternallyModified,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Whatever went wrong inside the backend, with its own error preserved whole —
    /// `#[error(transparent)]` forwards both `Display` and `source()`, so a backend's
    /// specific diagnostics (EqAPO's "config.txt is not valid UTF-8", say) reach the user
    /// unchanged rather than being flattened into a generic message here.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),
    /// This backend has no such capability — see [`Capabilities`]. Callers should gate on
    /// the capability flag rather than calling and catching this; it exists so a
    /// mis-gated call fails loudly instead of silently doing nothing.
    #[error("this backend does not support {0}")]
    Unsupported(&'static str),
}

impl BackendError {
    /// Wrap a backend's own error type, keeping its message and cause chain.
    pub fn backend<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        BackendError::Backend(Box::new(e))
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// What a backend can and cannot do, so the orchestrator adapts instead of assuming
/// EqAPO's constraints are universal. Every field here exists because the two backends
/// genuinely differ on it — none is speculative future-proofing.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Minimum spacing between consecutive [`EqBackend::apply`] calls.
    ///
    /// EqAPO needs ≥15 ms: it does **not** reset its crossfade progress counter when a
    /// reload lands mid-transition (FilterEngine.cpp:258-261), so the in-flight counter
    /// gets applied to the new curve — audible as a jump. An in-process APO has no
    /// reload to collide with and reports [`Duration::ZERO`].
    pub min_write_spacing: Duration,
    /// The backend performs tonal transitions itself, so the core must **not** emulate
    /// one by writing intermediate frames (§5.3a).
    ///
    /// False for EqAPO: its only transition is a fixed ~10 ms crossfade from a *cold*
    /// cascade, so a longer, smoother move has to be built out of many spaced writes.
    /// True for an in-process APO, which ramps coefficients in place over whatever
    /// duration it likes with the filter state carried — strictly better than the core
    /// could manage from outside, and the whole reason for building it.
    pub owns_transitions: bool,
    /// The backend shares a configuration file with other tools, so foreign directives
    /// can stack on top of CAGEq's correction and may need disabling (§7.4). EqAPO-only:
    /// an in-process APO has no shared config surface to collide over.
    pub manages_foreign_config: bool,
}

// ---------------------------------------------------------------------------
// The backend trait
// ---------------------------------------------------------------------------

/// Everything `cageq-core` needs from the thing that actually applies filters.
///
/// `Send + Sync` because the core shares one backend across the caller's thread and the
/// reconciler thread (and, for the safe state, the watchdog's threads); implementations
/// carry their own interior synchronisation where they need it.
pub trait EqBackend: Send + Sync {
    /// What this backend can do — see [`Capabilities`].
    fn capabilities(&self) -> Capabilities;

    /// Make `configs` the applied state. Returns a stable content hash of what was
    /// applied — persisted in settings.json and handed back to [`Self::startup_decision`]
    /// next launch (§3.0).
    fn apply(&self, configs: &[DeviceConfig]) -> Result<String, BackendError>;

    /// Apply CAGEq's safe state (§7.1/7.2): effective silence, recognised by
    /// [`Self::startup_decision`] as [`StartupDecision::SafeStateStillActive`]. Called by
    /// the watchdog on a fail-safe, so it must be independent of the Python sidecar and
    /// as close to infallible as the backend can manage.
    fn write_safe_state(&self) -> Result<(), BackendError>;

    /// The §3.0 startup-integrity verdict, given the hash settings.json remembered for
    /// this device (`None` on a clean install).
    fn startup_decision(&self, expected_hash: Option<&str>) -> Result<StartupDecision, BackendError>;

    /// Is this backend actually able to process audio on `device_id`? A `DeviceConfig`
    /// scoped to an endpoint this returns `false` for is inert — the UI warns rather than
    /// applying something that silently does nothing.
    fn drives_endpoint(&self, device_id: &str) -> bool;

    /// Where the applied state lives, for the UI to show (a file path for EqAPO). Purely
    /// informational — nothing parses it.
    fn location(&self) -> String;

    /// The applied state read back in whatever human-readable form the backend has, for
    /// the UI's config preview — for EqAPO, the `cageq.txt` it just wrote, which doubles
    /// as end-to-end proof that the write landed. `None` when the backend has no such
    /// textual form (an in-process APO holds coefficients, not a document), which the UI
    /// must treat as "no preview available" rather than "empty config".
    fn applied_text(&self) -> Option<String> {
        None
    }

    // -- Capability-gated (see `Capabilities::manages_foreign_config`) ----------------
    // Default to `Unsupported` so a backend without a shared config file doesn't have to
    // carry three stub methods.

    /// Active directives outside CAGEq's own block that stack on top of its correction.
    fn foreign_directives(&self) -> Result<Vec<String>, BackendError> {
        Err(BackendError::Unsupported("foreign-config inspection"))
    }

    /// Comment out every foreign active directive, reversibly. `Ok(true)` if it changed.
    fn disable_foreign_config(&self) -> Result<bool, BackendError> {
        Err(BackendError::Unsupported("foreign-config disabling"))
    }

    /// Undo [`Self::disable_foreign_config`]. `Ok(true)` if it changed.
    fn restore_foreign_config(&self) -> Result<bool, BackendError> {
        Err(BackendError::Unsupported("foreign-config restoring"))
    }
}

// ---------------------------------------------------------------------------
// Windows playback-device enumeration (§3.0)
// ---------------------------------------------------------------------------

/// Enumerate active Windows playback (render) endpoints (§3.0), for the device picker.
/// Read-only registry access; returns an empty list on non-Windows, when the key is
/// unreadable, or when nothing is active. Order follows the registry.
///
/// Backend-neutral by construction — it reports what the *system* has. Ask the backend
/// separately ([`EqBackend::drives_endpoint`]) whether it can actually drive one.
pub fn list_render_devices() -> Vec<AudioDevice> {
    render_devices_from_registry()
}

#[cfg(windows)]
fn render_devices_from_registry() -> Vec<AudioDevice> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    // Endpoint property keys (PROPERTYKEY "{fmtid},pid" as stored under Properties):
    //   DeviceDesc            -> the "connection name" (e.g. "Lautsprecher"/"Speakers")
    //   DeviceInterface name  -> the "device name"     (e.g. "SPL Phonitor One")
    const DEVICE_DESC: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
    const INTERFACE_NAME: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
    const DEVICE_STATE_ACTIVE: u32 = 0x1;

    let Ok(render) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(RENDER_KEY) else {
        return Vec::new();
    };

    let mut devices = Vec::new();
    for guid in render.enum_keys().flatten() {
        let Ok(endpoint) = render.open_subkey(&guid) else { continue };
        // Only active (plugged-in, enabled, present) endpoints.
        if endpoint.get_value::<u32, _>("DeviceState").ok() != Some(DEVICE_STATE_ACTIVE) {
            continue;
        }
        let Ok(props) = endpoint.open_subkey("Properties") else { continue };
        let desc = props.get_value::<String, _>(DEVICE_DESC).ok();
        let iface = props.get_value::<String, _>(INTERFACE_NAME).ok();
        let name = match (desc, iface) {
            (Some(d), Some(i)) => format!("{d} ({i})"),
            (Some(d), None) => d,
            (None, Some(i)) => i,
            (None, None) => guid.clone(),
        };
        devices.push(AudioDevice { id: guid, name });
    }
    devices
}

#[cfg(not(windows))]
fn render_devices_from_registry() -> Vec<AudioDevice> {
    Vec::new()
}

/// The MMDevices render branch. Shared with backends, which look inside a specific
/// endpoint's `FxProperties` to answer [`EqBackend::drives_endpoint`] — one definition so
/// the enumerator and the per-endpoint probes can't drift onto different keys.
pub const RENDER_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";

/// Does `device_id`'s endpoint carry any of `clsids` in its effect chain (`FxProperties`)?
/// The shared mechanic behind every backend's [`EqBackend::drives_endpoint`]: an APO

/// Equalizer APO's own APO CLSIDs — pre-mix and post-mix, verified against a live install.
///
/// Lives here rather than in either backend because **both** need it: the EqAPO backend to
/// know whether it will process an endpoint, and CAGEq's own APO backend to warn when EqAPO
/// is attached to the *same* endpoint, where the two corrections would silently stack and
/// double-filter. One definition, so those two answers cannot disagree.
pub const EQAPO_APO_CLSIDS: [&str; 2] =
    ["EACD2258-FCAC-4FF4-B36D-419E924A6D79", "EC1CC9CE-FAED-4822-828A-82A81A6F018F"];
/// advertises itself by putting its CLSID into that chain, so "is this backend live on
/// this endpoint" is the same registry question for all of them — only the CLSIDs differ.
///
/// Scans **every** value rather than hard-coding slot indices, so all install variants
/// (pre-mix/post-mix, normal/troubleshooting/install-as-LFX) register as attached.
/// `clsids` are compared brace-stripped and upper-cased; pass them that way.
#[cfg(windows)]
pub fn endpoint_has_apo(device_id: &str, clsids: &[&str]) -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::types::FromRegValue;

    let Ok(fx) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"{RENDER_KEY}\{device_id}\FxProperties"))
    else {
        return false;
    };
    fx.enum_values().flatten().any(|(_, val)| {
        let Ok(s) = String::from_reg_value(&val) else { return false };
        let s = s.to_ascii_uppercase();
        clsids.iter().any(|clsid| s.contains(clsid))
    })
}

#[cfg(not(windows))]
pub fn endpoint_has_apo(_device_id: &str, _clsids: &[&str]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_render_devices_honours_its_contract() {
        // Environment-dependent. Contract: never panics, and every entry carries a
        // non-empty id (which is also what a DeviceConfig addresses the endpoint by).
        for d in list_render_devices() {
            assert!(!d.id.is_empty(), "device id should be non-empty");
        }
    }

    #[test]
    fn endpoint_probe_is_read_only_and_total() {
        // Contract: never panics, whatever it is handed — including ids that cannot
        // exist, since the picker's list can go stale between enumeration and probe.
        assert!(!endpoint_has_apo("", &[]));
        assert!(!endpoint_has_apo("{not-a-real-endpoint}", &["DEADBEEF"]));
    }

    #[test]
    fn unsupported_names_the_missing_capability() {
        let e = BackendError::Unsupported("foreign-config disabling");
        assert!(e.to_string().contains("foreign-config disabling"));
    }

    #[test]
    fn expand_tilts_passes_non_tilt_filters_through_unchanged() {
        let f = Filter { kind: FilterType::Peaking, freq_hz: 1000.0, gain_db: 3.0, q: 1.4 };
        let out = expand_tilts(&[f]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, FilterType::Peaking);
        assert_eq!(out[0].freq_hz, 1000.0);
        assert_eq!(out[0].gain_db, 3.0);
        assert_eq!(out[0].q, 1.4);
    }

    #[test]
    fn expand_tilts_splits_into_complementary_shelves() {
        let tilt = Filter { kind: FilterType::Tilt, freq_hz: 500.0, gain_db: 6.0, q: 0.9 };
        let out = expand_tilts(&[tilt]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, FilterType::LowShelf);
        assert_eq!(out[0].freq_hz, 500.0);
        assert_eq!(out[0].gain_db, -3.0);
        assert_eq!(out[0].q, 0.9);
        assert_eq!(out[1].kind, FilterType::HighShelf);
        assert_eq!(out[1].freq_hz, 500.0);
        assert_eq!(out[1].gain_db, 3.0);
        assert_eq!(out[1].q, 0.9);
    }

    #[test]
    fn expand_tilts_preserves_order_and_mixes_kinds() {
        let filters = [
            Filter { kind: FilterType::Peaking, freq_hz: 100.0, gain_db: 1.0, q: 1.0 },
            Filter { kind: FilterType::Tilt, freq_hz: 2000.0, gain_db: -4.0, q: 0.7 },
            Filter { kind: FilterType::Bandpass, freq_hz: 300.0, gain_db: 0.0, q: 2.0 },
        ];
        let out = expand_tilts(&filters);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].kind, FilterType::Peaking);
        assert_eq!(out[1].kind, FilterType::LowShelf);
        assert_eq!(out[1].gain_db, 2.0);
        assert_eq!(out[2].kind, FilterType::HighShelf);
        assert_eq!(out[2].gain_db, -2.0);
        assert_eq!(out[3].kind, FilterType::Bandpass);
    }
}
