//! **The Equalizer APO backend** — [`EqApoBackend`], an implementation of
//! `cageq_backend::EqBackend` over EqAPO's two-file `Include` architecture
//! (filter.md §3.0/§7.4).
//!
//! Since CAGEq gained a second backend (its own APO — filter.md §5.3c), this crate is no
//! longer "the config writer" but "one of two ways to apply filters". The neutral domain
//! types it speaks (`Filter`, `DeviceConfig`, `AudioDevice`, `StartupDecision`) live in
//! `cageq-backend` and are re-exported below for convenience; everything EqAPO-specific
//! — the `PK`/`LSC`/`HSC` tokens, the `cageq.txt` rendering, the `config.txt` splice, the
//! ≥15 ms write spacing this backend declares — stays here, where it belongs.
//!
//! This backend is **permanently supported but feature-frozen**: existing EqualizerAPO
//! users keep a working app indefinitely, but capabilities that need an in-process engine
//! (live coefficient push, arbitrary-length transitions with carried filter state) are
//! declared `false` in [`EqBackend::capabilities`] rather than emulated here.
//!
//! Verified against how the reference tool (AQUA) and EqAPO itself work:
//!   * CAGEq writes all its filters into its **own** file, `cageq.txt`, and puts a
//!     single `Include: cageq.txt` line into EqAPO's `config.txt`. This is the
//!     pattern EqAPO's config reference explicitly recommends, and AQUA uses it
//!     (`aqua.txt` + `Include: aqua.txt`, verified in its `flush.ts`).
//!   * EqAPO watches its **whole config directory** for changes
//!     (`FindFirstChangeNotificationW(configPath, bWatchSubtree=true, ...)` in
//!     FilterEngine.cpp) and reloads on any file change — so writing `cageq.txt`
//!     triggers a reload + the native 10 ms crossfade exactly like editing
//!     config.txt would.
//!
//! Why this shape is simpler than splicing filters into config.txt directly:
//!   * `cageq.txt` is entirely ours -> wholesale atomic overwrite, no read-modify-
//!     write, no foreign content to preserve, no UTF-8/ANSI concern for filters.
//!   * `config.txt` only ever needs one pure-ASCII `Include:` line, wrapped in a
//!     `#CAGEq:BEGIN`/`#CAGEq:END` comment block so we can add/verify/replace just
//!     that block and leave everything else byte-for-byte intact.
//!
//! Design habit (worth internalising): the pure functions — rendering, hashing,
//! and the config.txt splice/parse — take strings and return strings with no I/O,
//! so the tricky logic is unit-testable without a temp directory. The thin public
//! functions add the fs::read/write/rename around them.
//!
//! Deliberately OUT of scope, flagged inline where relevant:
//!   * preserving a foreign config.txt block's exact CRLF style (we emit LF; only
//!     ever relevant to the one Include block, since cageq.txt is fully ours)
//!   * non-UTF-8 (legacy ANSI) config.txt: we support UTF-8 only and fail loudly
//!     with WriteError::NotUtf8 rather than transcode (a scope decision, §7.4)
//!
//! Add to Cargo.toml:  sha2 = "0.10"  thiserror = "2"

use std::fmt::Write as _; // brings write!/writeln! for String targets into scope
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

// The domain types this backend renders, and the trait it implements. Re-exported at the
// bottom of this section so existing callers (and the integration tests) keep importing
// `Filter`/`DeviceConfig`/… from here unchanged.
use cageq_backend::{BackendError, Capabilities, EqBackend};
pub use cageq_backend::{AudioDevice, DeviceConfig, Filter, FilterType, StartupDecision};

// Line ending CAGEq emits. EqAPO parses LF fine (it strips a trailing \r itself).
const NL: &str = "\n";
// Comment markers around the Include line in config.txt. A leading '#' makes each
// a comment EqAPO ignores; only the `Include:` line between them is a real command.
const INCLUDE_BEGIN: &str = "#CAGEq:BEGIN";
const INCLUDE_END: &str = "#CAGEq:END";
// First line of cageq.txt: a comment (no ':', starts with '#', so EqAPO ignores it)
// that carries the integrity hash of everything below it.
const CAGEQ_HEADER_PREFIX: &str = "# CAGEq managed file - generated, do not edit. hash=";
/// The filename CAGEq writes its filters into, alongside config.txt.
pub const CAGEQ_FILENAME: &str = "cageq.txt";

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// EqAPO's token for a filter type — this backend's wire format, which is exactly why it
/// lives here and not on the neutral [`FilterType`]. VERIFIED against AutoEq's own
/// EqualizerAPO exporter (`frequency_response.py::write_eqapo_parametric_eq`,
/// `types = {Peaking: 'PK', LowShelf: 'LSC', HighShelf: 'HSC'}`): `LSC`/`HSC` are the
/// center-frequency shelves taking an Fc/Gain/Q triple (the RBJ-biquad-with-Q form AutoEq
/// emits), not `LS`/`HS` or the slope-based `LSC x dB`.
fn eqapo_token(kind: FilterType) -> &'static str {
    match kind {
        FilterType::Peaking => "PK",
        FilterType::LowShelf => "LSC",
        FilterType::HighShelf => "HSC",
        FilterType::Bandpass => "BP",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("config.txt/cageq.txt I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("config path has no parent directory for the temp file")]
    NoParentDir,
    /// config.txt exists but isn't valid UTF-8 — almost always a legacy ANSI
    /// file. EqAPO tolerates it via a per-line CP_ACP fallback (FilterEngine.cpp),
    /// but CAGEq supports UTF-8 only and fails loudly here rather than transcode
    /// (§7.4). The fix for the user is to re-save config.txt as UTF-8.
    #[error("config.txt is not valid UTF-8 (CAGEq requires UTF-8; re-save it as UTF-8)")]
    NotUtf8,
}

/// What cageq.txt currently says — consumed at startup (§3.0) to decide whether
/// settings.json's remembered resume state is still trustworthy.
#[derive(Debug)]
pub enum BlockState {
    /// No cageq.txt, or one without a hash header: CAGEq has never written it (or it
    /// was wiped). Normal first-run state, not an error.
    Absent,
    /// A CAGEq-written cageq.txt exists. `stored_hash` is what its header claims;
    /// `actual_hash` is recomputed from the body now. (`decide_startup` trusts the
    /// settings.json hash over `stored_hash`, which a hand-edit could forge.)
    Present {
        stored_hash: Option<String>,
        actual_hash: String,
    },
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// EqAPO's two APO CLSIDs (braces stripped, upper-case) as they appear as REG_SZ GUID
/// values under an endpoint's `FxProperties` — pre-mix `{EACD2258-…}` / post-mix
/// `{EC1CC9CE-…}`, verified against a live install. Their presence is how
/// [`EqBackend::drives_endpoint`] knows EqAPO will actually process this endpoint.
use cageq_backend::EQAPO_APO_CLSIDS;

/// EqAPO does not reset its crossfade progress counter when a reload lands while a
/// transition is still running (FilterEngine.cpp:258-261) — the in-flight counter gets
/// applied to the new curve, audible as a jump. So consecutive writes must be spaced at
/// least this far apart (filter.md §5.3). Declared as a capability rather than enforced
/// here: it's the *orchestrator* that paces writes, and the other backend has no such wall.
const EQAPO_MIN_WRITE_SPACING: Duration = Duration::from_millis(15);

/// Is Equalizer APO's APO attached to this endpoint, i.e. will it actually process audio
/// there (§3.0)? A `Device:`-scoped config for an endpoint this returns `false` for is
/// inert — the UI warns and points at EqAPO's DeviceSelector instead of applying something
/// that silently does nothing.
///
/// Free function as well as an [`EqBackend::drives_endpoint`] impl because the answer
/// depends only on the registry, not on any config directory: the device picker needs it
/// even when CAGEq failed to start and has no live backend to ask.
pub fn eqapo_drives_endpoint(device_id: &str) -> bool {
    cageq_backend::endpoint_has_apo(device_id, &EQAPO_APO_CLSIDS)
}

/// Applies filters by writing Equalizer APO's config files. Holds the config directory
/// (where `config.txt` lives); everything else is derived from it.
#[derive(Debug, Clone)]
pub struct EqApoBackend {
    config_dir: PathBuf,
}

impl EqApoBackend {
    /// Target `config_dir` — EqAPO's config directory, normally from
    /// [`detect_eqapo_config_dir`]. Not validated here: the desktop app deliberately
    /// falls back to a dev temp dir on a machine without EqAPO so the rest of the app
    /// still runs, and says so in the UI.
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        EqApoBackend { config_dir: config_dir.into() }
    }

    /// EqAPO's config directory.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// The managed file this backend owns and rewrites.
    pub fn cageq_path(&self) -> PathBuf {
        self.config_dir.join(CAGEQ_FILENAME)
    }

    /// EqAPO's own config file, which CAGEq only ever adds one `Include:` line to.
    fn config_txt(&self) -> PathBuf {
        self.config_dir.join("config.txt")
    }
}

impl EqBackend for EqApoBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            min_write_spacing: EQAPO_MIN_WRITE_SPACING,
            // EqAPO's only transition is a fixed ~10 ms crossfade from a *cold* cascade,
            // so a longer, smoother tonal move (§5.3a) has to be built by the core out of
            // many spaced writes. Nothing this backend can do about that from outside.
            owns_transitions: false,
            // config.txt is shared with EqAPO itself and any other tool writing to it.
            manages_foreign_config: true,
        }
    }

    fn apply(&self, configs: &[DeviceConfig]) -> Result<String, BackendError> {
        apply(&self.config_dir, configs).map_err(BackendError::backend)
    }

    fn write_safe_state(&self) -> Result<(), BackendError> {
        write_safe_state(&self.cageq_path()).map_err(BackendError::backend)
    }

    fn startup_decision(&self, expected_hash: Option<&str>) -> Result<StartupDecision, BackendError> {
        let state = read_cageq_state(&self.cageq_path()).map_err(BackendError::backend)?;
        Ok(decide_startup(&state, expected_hash))
    }

    fn drives_endpoint(&self, device_id: &str) -> bool {
        eqapo_drives_endpoint(device_id)
    }

    fn location(&self) -> String {
        self.cageq_path().display().to_string()
    }

    fn applied_text(&self) -> Option<String> {
        // Deliberately re-read from disk rather than returning what was rendered: this is
        // the UI's end-to-end proof that the write actually landed, so it has to observe
        // the file, not our intention. Absent/unreadable reads as an empty preview.
        Some(fs::read_to_string(self.cageq_path()).unwrap_or_default())
    }

    fn foreign_directives(&self) -> Result<Vec<String>, BackendError> {
        foreign_config_directives(&self.config_txt()).map_err(BackendError::backend)
    }

    fn disable_foreign_config(&self) -> Result<bool, BackendError> {
        disable_foreign_config(&self.config_txt()).map_err(BackendError::backend)
    }

    fn restore_foreign_config(&self) -> Result<bool, BackendError> {
        restore_foreign_config(&self.config_txt()).map_err(BackendError::backend)
    }
}

// ---------------------------------------------------------------------------
// Public API — the free functions [`EqApoBackend`] is a thin shell over. Kept public
// because they are pure-ish, individually testable, and the integration tests drive
// them directly against a temp directory.
// ---------------------------------------------------------------------------

/// Apply `configs` to Equalizer APO: ensure config.txt includes cageq.txt, then
/// (atomically) write cageq.txt with the given per-device blocks. `config_dir` is
/// EqAPO's config directory (where config.txt lives). Returns cageq.txt's content
/// hash — persist it in settings.json for the next-startup integrity check (§3.0).
///
/// The `ensure_include` step is a cheap no-op after the first run (it only rewrites
/// config.txt when the Include block is missing/wrong), so the steady-state cost of
/// a change is a single cageq.txt write -> one EqAPO reload -> one native crossfade.
pub fn apply(config_dir: &Path, configs: &[DeviceConfig]) -> Result<String, WriteError> {
    ensure_include(&config_dir.join("config.txt"), CAGEQ_FILENAME)?;
    write_cageq_txt(&config_dir.join(CAGEQ_FILENAME), configs)
}

/// Atomically overwrite cageq.txt with the rendered device blocks. Returns the
/// content hash written into the header. cageq.txt is fully CAGEq-owned, so this is
/// a plain wholesale write — no splicing, no foreign content.
pub fn write_cageq_txt(cageq_path: &Path, configs: &[DeviceConfig]) -> Result<String, WriteError> {
    let (text, hash) = render_cageq_txt(configs);
    atomic_write(cageq_path, text.as_bytes())?;
    Ok(hash)
}

/// Ensure config.txt contains the `#CAGEq:BEGIN`/`Include: <cageq_filename>`/
/// `#CAGEq:END` block, preserving all other content byte-for-byte. Idempotent:
/// returns `Ok(false)` and writes nothing when the block is already exactly right,
/// `Ok(true)` when it added or fixed it. Appends at EOF when absent — placing it
/// last means cageq.txt's own leading `Device:` line controls scope and nothing
/// after it is affected (the AQUA-proven placement).
pub fn ensure_include(config_txt_path: &Path, cageq_filename: &str) -> Result<bool, WriteError> {
    let current = read_utf8_or_empty(config_txt_path)?;
    match splice_include(&current, cageq_filename) {
        Some(new_text) => {
            atomic_write(config_txt_path, new_text.as_bytes())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

// Per-line marker CAGEq prepends to disable a foreign config.txt directive: the leading '#'
// makes EqAPO treat the whole line as a comment, and the distinctive tag lets us strip it back
// off cleanly on restore. Space after so the original directive stays readable in the file.
const DISABLED_PREFIX: &str = "#CAGEqOff# ";

/// Active (non-comment, non-blank) directives in config.txt that live *outside* CAGEq's own
/// `#CAGEq:BEGIN`/`END` include block — the lines that stack on top of every CAGEq correction
/// (typically Equalizer APO's shipped default `Preamp:`/example filters on a fresh install).
/// Returned trimmed, for a UI preview. Lines CAGEq previously disabled already start with '#',
/// so they're excluded. Read-only.
pub fn foreign_config_directives(config_txt_path: &Path) -> Result<Vec<String>, WriteError> {
    Ok(find_foreign_directives(&read_utf8_or_empty(config_txt_path)?))
}

/// Comment out every foreign active directive (see [`foreign_config_directives`]) by prefixing it
/// with [`DISABLED_PREFIX`], so EqAPO ignores it and CAGEq's correction applies alone. Nothing is
/// deleted — reversible via [`restore_foreign_config`]. `Ok(true)` when it changed the file.
pub fn disable_foreign_config(config_txt_path: &Path) -> Result<bool, WriteError> {
    match disable_foreign(&read_utf8_or_empty(config_txt_path)?) {
        Some(new) => {
            atomic_write(config_txt_path, new.as_bytes())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Undo [`disable_foreign_config`]: strip the disable-marker off any lines carrying it, restoring
/// the original directives. `Ok(true)` when it changed the file.
pub fn restore_foreign_config(config_txt_path: &Path) -> Result<bool, WriteError> {
    match enable_foreign(&read_utf8_or_empty(config_txt_path)?) {
        Some(new) => {
            atomic_write(config_txt_path, new.as_bytes())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Byte offsets of the CAGEq include block, so per-line scans can skip it.
fn line_in_block(start: usize, block: Option<(usize, usize)>) -> bool {
    matches!(block, Some((b, e)) if start >= b && start < e)
}

/// The trimmed foreign active directives, outside the CAGEq block (pure).
fn find_foreign_directives(text: &str) -> Vec<String> {
    let block = find_marker_range(text);
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let t = line.trim();
        if !line_in_block(start, block) && !t.is_empty() && !t.starts_with('#') {
            out.push(t.to_string());
        }
    }
    out
}

/// Prefix each foreign active directive with the disable marker (pure). `None` if nothing to do.
fn disable_foreign(text: &str) -> Option<String> {
    let block = find_marker_range(text);
    let mut changed = false;
    let mut out = String::with_capacity(text.len() + DISABLED_PREFIX.len());
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let t = line.trim();
        if !line_in_block(start, block) && !t.is_empty() && !t.starts_with('#') {
            out.push_str(DISABLED_PREFIX);
            changed = true;
        }
        out.push_str(line);
    }
    changed.then_some(out)
}

/// Strip the disable marker off any line carrying it (pure). `None` if none present.
fn enable_foreign(text: &str) -> Option<String> {
    if !text.contains(DISABLED_PREFIX) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(line.strip_prefix(DISABLED_PREFIX).unwrap_or(line));
    }
    Some(out)
}

/// Atomically write CAGEq's hardcoded safe-state (§7.1/7.2) to cageq.txt, using the
/// same header+body format as a normal write so [`read_cageq_state`] +
/// [`decide_startup`] recognise it as [`StartupDecision::SafeStateStillActive`].
pub fn write_safe_state(cageq_path: &Path) -> Result<(), WriteError> {
    let (text, _) = wrap_with_hash_header(&safe_state_body());
    atomic_write(cageq_path, text.as_bytes())
}

/// Read cageq.txt's integrity state for the startup check (§3.0).
pub fn read_cageq_state(cageq_path: &Path) -> Result<BlockState, WriteError> {
    match fs::read_to_string(cageq_path) {
        Ok(s) => Ok(cageq_state_from_text(&s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BlockState::Absent),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(WriteError::NotUtf8),
        Err(e) => Err(e.into()),
    }
}

/// Decide, from cageq.txt's on-disk state and the hash CAGEq recorded in
/// settings.json, how much of the remembered resume state to trust at startup
/// (§3.0). Pure — no I/O — so the whole decision table is unit-testable.
///
/// `state` comes from [`read_cageq_state`]; `expected_hash` is the device's
/// `last_written_hash` from settings.json, or `None` if there's no record yet.
/// The trust signal is `actual_hash == expected_hash`: settings can't be forged by
/// hand-editing cageq.txt's own header, so it's the stronger of the two hashes.
pub fn decide_startup(state: &BlockState, expected_hash: Option<&str>) -> StartupDecision {
    let actual_hash = match state {
        BlockState::Absent | BlockState::Present { stored_hash: None, .. } => {
            return StartupDecision::FirstRun;
        }
        BlockState::Present { actual_hash, .. } => actual_hash,
    };

    if expected_hash == Some(actual_hash.as_str()) {
        return StartupDecision::ResumeTrusted;
    }

    // A real config's hash never equals the safe-state's (the safe-state writer
    // doesn't update settings' resume hash), so this can't shadow ResumeTrusted.
    if *actual_hash == content_hash(&safe_state_body()) {
        StartupDecision::SafeStateStillActive
    } else {
        StartupDecision::ExternallyModified
    }
}

/// CAGEq's hardcoded safe-state cageq.txt body (§7.2): effective silence on all
/// devices. Written by the watchdog on a fail-safe (§7.1) and recognised by the
/// startup check. One canonical definition so the writer and the detector can't
/// drift apart.
///
/// Silence is a large *negative preamp*, NOT a `Mute` command: EqAPO has no
/// `Mute` command — verified against source (it is not among the registered
/// filter factories in FilterEngine.cpp, nor in the config reference). An earlier
/// `Mute: On` would have been silently ignored, leaving only the -20 dB
/// attenuation (audible). -120 dB (linear ~1e-6) is inaudible on any pipeline;
/// the exact value is a tunable safety parameter.
pub fn safe_state_body() -> String {
    format!("Device: all{NL}Preamp: -120.0 dB{NL}")
}

// ---------------------------------------------------------------------------
// EqAPO config-directory detection (§3.0)
// ---------------------------------------------------------------------------

/// Locate Equalizer APO's config directory — where `config.txt` lives and where
/// cageq.txt must be written for EqAPO to load it (§3.0).
///
/// EqAPO records its paths under `HKLM\SOFTWARE\EqualizerAPO` at install time. We
/// read `ConfigPath` directly (the installer writes the resolved config dir there,
/// e.g. `C:\Program Files\EqualizerAPO\config`); if only `InstallPath` is present we
/// derive `<InstallPath>\config`. Returns `None` when EqAPO isn't installed, the key
/// is missing, or the resolved directory doesn't exist — the caller decides the
/// fallback (the desktop app drops to a dev temp dir and says so in the UI).
///
/// Read-only: this never creates or writes anything. On non-Windows it is a compile-
/// time `None` so the crate still builds and tests elsewhere.
pub fn detect_eqapo_config_dir() -> Option<PathBuf> {
    let dir = eqapo_config_dir_from_registry()?;
    dir.is_dir().then_some(dir)
}

#[cfg(windows)]
fn eqapo_config_dir_from_registry() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\EqualizerAPO")
        .ok()?;
    // Prefer the explicit ConfigPath the installer resolved; fall back to
    // <InstallPath>\config if only the install root is recorded.
    if let Ok(config_path) = key.get_value::<String, _>("ConfigPath") {
        let p = PathBuf::from(config_path.trim());
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    let install: String = key.get_value("InstallPath").ok()?;
    let install = install.trim();
    (!install.is_empty()).then(|| PathBuf::from(install).join("config"))
}

#[cfg(not(windows))]
fn eqapo_config_dir_from_registry() -> Option<PathBuf> {
    None // no Windows registry off-platform; detection is inherently Windows-only
}

// ---------------------------------------------------------------------------
// Pure helpers — rendering cageq.txt
// ---------------------------------------------------------------------------

/// One `Device:` block: the device line, a Preamp line, then one Filter line per
/// band. Line format verified byte-for-byte against AutoEq's exporter
/// (frequency_response.py, line 213): 1-based filter number (EqAPO ignores it but
/// AutoEq/REW emit it), then `ON <type> Fc <fc:.0> Hz Gain <gain:.1> dB Q <q:.2>`.
fn render_device_block(cfg: &DeviceConfig) -> String {
    let mut s = String::new();
    let _ = write!(s, "Device: {}{NL}", cfg.device);
    let _ = write!(s, "Preamp: {:.1} dB{NL}", cfg.preamp_db);
    // Emit filters sorted by centre frequency (low -> high). Order never changes the
    // combined response (a biquad cascade is commutative), but AutoEq returns its
    // peaking filters in optimiser-convergence order, which reads as an arbitrary
    // jumble in the file. Sorting makes the preview stable and readable — and later
    // merges custom filters (§3.4) into the same low->high list.
    let mut filters: Vec<&Filter> = cfg.filters.iter().collect();
    filters.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    for (i, f) in filters.iter().enumerate() {
        // A bandpass carries no gain (unity-peak), and EqAPO's BP line takes only Fc + Q.
        if f.kind == FilterType::Bandpass {
            let _ = write!(s, "Filter {}: ON BP Fc {:.0} Hz Q {:.2}{NL}", i + 1, f.freq_hz, f.q);
        } else {
            let _ = write!(
                s,
                "Filter {}: ON {} Fc {:.0} Hz Gain {:.1} dB Q {:.2}{NL}",
                i + 1,
                eqapo_token(f.kind),
                f.freq_hz,
                f.gain_db,
                f.q,
            );
        }
    }
    s
}

/// The full cageq.txt text (hash header + all device blocks) and its content hash.
fn render_cageq_txt(configs: &[DeviceConfig]) -> (String, String) {
    let mut body = String::new();
    for cfg in configs {
        body.push_str(&render_device_block(cfg));
    }
    wrap_with_hash_header(&body)
}

/// Prefix `body` with the `# … hash=<h>` header line. Returns (full text, hash).
/// The hash is over `body` only (not the header — that would be circular).
fn wrap_with_hash_header(body: &str) -> (String, String) {
    let hash = content_hash(body);
    (format!("{CAGEQ_HEADER_PREFIX}{hash}{NL}{body}"), hash)
}

/// Parse cageq.txt text into a [`BlockState`]: pull the hash out of the header line
/// and recompute it over the body. Must mirror `wrap_with_hash_header` exactly, or
/// the startup check would false-positive forever (guarded by a round-trip test).
fn cageq_state_from_text(text: &str) -> BlockState {
    let Some((header, body)) = text.split_once('\n') else {
        return BlockState::Absent;
    };
    match header.split_once("hash=") {
        Some((_, hash)) => BlockState::Present {
            stored_hash: Some(hash.trim().to_string()),
            actual_hash: content_hash(body),
        },
        None => BlockState::Absent,
    }
}

/// Short content fingerprint (§3.0). SHA-256 (not the std `DefaultHasher`) is
/// deliberate: this value is written to disk and re-read by a possibly newer build
/// after an app update, so the algorithm must be stable across builds/platforms —
/// `DefaultHasher`'s is not. Only 4 bytes (8 hex chars) are kept: a change
/// detector, not a security hash.
fn content_hash(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    let mut s = String::with_capacity(8);
    for b in &digest[..4] {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ---------------------------------------------------------------------------
// Pure helpers — the Include block in config.txt
// ---------------------------------------------------------------------------

/// The canonical Include block, markers included and trailing newline.
fn render_include_block(cageq_filename: &str) -> String {
    format!("{INCLUDE_BEGIN}{NL}Include: {cageq_filename}{NL}{INCLUDE_END}{NL}")
}

/// Compute the new config.txt content that ensures the Include block is present
/// and correct, or `None` if it's already exactly right (so no write is needed).
/// Everything outside the block is preserved byte-for-byte.
fn splice_include(current: &str, cageq_filename: &str) -> Option<String> {
    let block = render_include_block(cageq_filename);
    match find_marker_range(current) {
        Some((start, end)) => {
            if &current[start..end] == block.as_str() {
                None // already exactly right
            } else {
                let mut out = String::with_capacity(current.len() + block.len());
                out.push_str(&current[..start]);
                out.push_str(&block);
                out.push_str(&current[end..]);
                Some(out)
            }
        }
        None => {
            let mut out = String::with_capacity(current.len() + block.len() + 1);
            out.push_str(current);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(NL);
            }
            out.push_str(&block);
            Some(out)
        }
    }
}

/// Inclusive byte range of an existing CAGEq marker block: from the start of the
/// BEGIN line to just past the END line's newline. Guards against a marker string
/// appearing mid-line rather than as an actual marker.
fn find_marker_range(text: &str) -> Option<(usize, usize)> {
    let begin = text
        .match_indices(INCLUDE_BEGIN)
        .map(|(i, _)| i)
        .find(|&i| is_line_start(text, i))?;

    let end = text[begin..]
        .match_indices(INCLUDE_END)
        .map(|(i, _)| begin + i)
        .find(|&i| is_line_start(text, i))?;

    let end_line_end = text[end..].find('\n').map_or(text.len(), |nl| end + nl + 1);
    Some((begin, end_line_end))
}

fn is_line_start(text: &str, idx: usize) -> bool {
    idx == 0 || text.as_bytes()[idx - 1] == b'\n'
}

// ---------------------------------------------------------------------------
// Pure helpers — I/O plumbing
// ---------------------------------------------------------------------------

/// Read a text file as UTF-8, mapping "not found" to empty (a normal first run)
/// and "not valid UTF-8" (read_to_string's documented `InvalidData`) to the
/// distinct NotUtf8 error rather than a generic I/O error.
fn read_utf8_or_empty(path: &Path) -> Result<String, WriteError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(WriteError::NotUtf8),
        Err(e) => Err(e.into()),
    }
}

/// Write `bytes` to `target` atomically (§7.4): to a sibling temp file, then
/// `fs::rename` over the target. On Windows that is MoveFileExW with replace
/// semantics — atomic on one volume, which is why the temp must be a sibling. The
/// rename into EqAPO's config dir also fires its directory watcher, triggering the
/// reload + native crossfade.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    target.parent().ok_or(WriteError::NoParentDir)?;
    // Both steps can transiently fail on Windows while another process holds the file open:
    // EqAPO's directory watcher opens cageq.txt to read it right after our previous write
    // (and a `MoveFileExW` replace must *delete* the currently-open target — ERROR_ACCESS_
    // DENIED / os error 5 — until that reader closes), and an AV scanner can grab the freshly
    // written temp file for the same reason. It clears within milliseconds (the user's next
    // write always went through). Retry the *whole* write+rename on those transient lock
    // errors, with a fresh temp name each attempt (so a scanner still holding the previous
    // temp doesn't block the retry). A genuine failure (read-only dir, bad path) is not
    // transient, so it surfaces immediately without burning the budget.
    let mut attempt = 1u32;
    loop {
        match try_atomic_write(target, bytes) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < WRITE_ATTEMPTS && is_transient_lock(&e) => {
                std::thread::sleep(WRITE_BACKOFF * attempt);
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// One write+rename to a fresh sibling temp. Cleans up its temp on a failed rename.
fn try_atomic_write(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(target);
    fs::write(&tmp, bytes)?;
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp); // best-effort cleanup
        return Err(e);
    }
    Ok(())
}

/// Is this the kind of Windows "another process has the file open" error that clears on
/// its own? Covers `PermissionDenied` (ERROR_ACCESS_DENIED, 5) plus the raw sharing/lock
/// violations (32/33) that don't map to a named `ErrorKind`. Non-Windows: only the kind.
fn is_transient_lock(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// Retry budget for the transient-reader race above: up to 8 tries with a growing backoff
/// (20/40/…/140 ms ≈ 0.56 s worst case) before giving up. Generous because EqAPO can hold
/// the file across its whole reload, but still bounded so a real failure isn't masked long.
const WRITE_ATTEMPTS: u32 = 8;
const WRITE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// Monotonic suffix so each attempt/write uses a *distinct* temp name — a scanner still
/// holding a previous temp can't collide with the next one.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temp path next to the target so the rename stays on one volume. PID (§2's
/// single-instance lock precludes a second concurrent writer) + a monotonic counter for a
/// unique name per write. TODO: a crash between write and rename can leave a stale
/// `.cageq-tmp-*`; a real impl would sweep these on startup.
fn temp_sibling(target: &Path) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = target.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".cageq-tmp-{}-{}", std::process::id(), seq));
    target.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Tests — pure functions only, no disk I/O
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_lock_errors_are_retried_but_real_failures_are_not() {
        // The Windows "another process has it open" family retries…
        assert!(is_transient_lock(&io::Error::from_raw_os_error(5))); // ERROR_ACCESS_DENIED
        assert!(is_transient_lock(&io::Error::from_raw_os_error(32))); // ERROR_SHARING_VIOLATION
        assert!(is_transient_lock(&io::Error::from_raw_os_error(33))); // ERROR_LOCK_VIOLATION
        assert!(is_transient_lock(&io::Error::new(io::ErrorKind::PermissionDenied, "x")));
        // …while genuine, non-transient failures surface immediately (no wasted budget).
        assert!(!is_transient_lock(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!is_transient_lock(&io::Error::from(io::ErrorKind::InvalidData)));
    }

    #[test]
    fn foreign_directives_detected_disabled_and_restored_outside_the_cageq_block() {
        // A fresh-install config.txt: a default preamp + example include, plus a comment, then
        // CAGEq's own include block. Only the two active lines *outside* the block are foreign.
        let cfg = "# Equalizer APO default\nPreamp: -6 dB\nInclude: example.txt\n#CAGEq:BEGIN\nInclude: cageq.txt\n#CAGEq:END\n";
        assert_eq!(find_foreign_directives(cfg), vec!["Preamp: -6 dB", "Include: example.txt"]);

        // Disabling comments exactly those two lines (marker-prefixed), leaving comments, the
        // blank/CAGEq block, and cageq.txt's own Include untouched.
        let disabled = disable_foreign(cfg).expect("something to disable");
        assert!(disabled.contains("#CAGEqOff# Preamp: -6 dB"));
        assert!(disabled.contains("#CAGEqOff# Include: example.txt"));
        assert!(disabled.contains("#CAGEq:BEGIN\nInclude: cageq.txt\n#CAGEq:END")); // block intact
        assert!(find_foreign_directives(&disabled).is_empty()); // now all commented → none active
        assert!(disable_foreign(&disabled).is_none()); // idempotent

        // Restoring strips the marker back to byte-identical original.
        assert_eq!(enable_foreign(&disabled).as_deref(), Some(cfg));
        assert!(enable_foreign(cfg).is_none()); // nothing to restore
    }

    fn sample() -> Vec<DeviceConfig> {
        vec![DeviceConfig {
            device: "USB DAC".to_string(),
            preamp_db: -9.0,
            filters: vec![
                Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 3.0, q: 0.7 },
                Filter { kind: FilterType::Peaking, freq_hz: 2500.0, gain_db: -2.4, q: 1.4 },
            ],
        }]
    }

    #[test]
    fn renders_autoeq_verified_lines() {
        let block = render_device_block(&sample()[0]);
        assert!(block.contains("Device: USB DAC"));
        assert!(block.contains("Preamp: -9.0 dB"));
        assert!(block.contains("Filter 1: ON LSC Fc 105 Hz Gain 3.0 dB Q 0.70"));
        assert!(block.contains("Filter 2: ON PK Fc 2500 Hz Gain -2.4 dB Q 1.40"));
    }

    #[test]
    fn filters_are_emitted_sorted_by_frequency() {
        // AutoEq returns peaking filters in optimiser order; the writer sorts them
        // low->high so the file preview is stable (the combined response is identical
        // either way — a biquad cascade is commutative).
        let cfg = DeviceConfig {
            device: "DAC".into(),
            preamp_db: -9.0,
            filters: vec![
                Filter { kind: FilterType::HighShelf, freq_hz: 10000.0, gain_db: -1.0, q: 0.7 },
                Filter { kind: FilterType::Peaking, freq_hz: 191.0, gain_db: -5.0, q: 0.4 },
                Filter { kind: FilterType::Peaking, freq_hz: 22.0, gain_db: 1.6, q: 5.9 },
                Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 3.0, q: 0.7 },
            ],
        };
        let block = render_device_block(&cfg);
        let fcs: Vec<u32> = block
            .lines()
            .filter_map(|l| l.split("Fc ").nth(1))
            .filter_map(|rest| rest.split(" Hz").next())
            .filter_map(|n| n.parse().ok())
            .collect();
        assert_eq!(fcs, vec![22, 105, 191, 10000], "filters should be ascending by Fc");
    }

    #[test]
    fn cageq_text_hash_round_trips() {
        // The invariant the whole restart check leans on: the hash recomputed from
        // a written cageq.txt equals the hash returned when writing it.
        let (text, stored) = render_cageq_txt(&sample());
        match cageq_state_from_text(&text) {
            BlockState::Present { stored_hash, actual_hash } => {
                assert_eq!(stored_hash.as_deref(), Some(stored.as_str()));
                assert_eq!(actual_hash, stored);
            }
            BlockState::Absent => panic!("freshly rendered cageq.txt must parse as Present"),
        }
    }

    #[test]
    fn include_block_appended_when_absent() {
        let out = splice_include("", CAGEQ_FILENAME).expect("empty file needs the block added");
        assert!(out.contains(INCLUDE_BEGIN));
        assert!(out.contains("Include: cageq.txt"));
        assert!(out.contains(INCLUDE_END));
    }

    #[test]
    fn include_block_is_noop_when_already_correct() {
        let existing = render_include_block(CAGEQ_FILENAME);
        assert!(splice_include(&existing, CAGEQ_FILENAME).is_none());
    }

    #[test]
    fn include_preserves_foreign_config_content() {
        let foreign = "Device: Other\nFilter: ON PK Fc 1000 Hz Gain 2 dB Q 1\n";
        let out = splice_include(foreign, CAGEQ_FILENAME).expect("block must be appended");
        assert!(out.contains(foreign), "foreign content was altered");
        assert!(out.contains("Include: cageq.txt"));
    }

    #[test]
    fn error_displays_a_message_and_chains_its_cause() {
        use std::error::Error;
        let e = WriteError::NotUtf8;
        assert!(e.to_string().contains("UTF-8"));
        assert!(e.source().is_none());
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert!(WriteError::from(io).source().is_some());
    }

    // ---- startup decision table (§3.0) ----

    fn present(actual: &str) -> BlockState {
        BlockState::Present { stored_hash: Some(actual.to_string()), actual_hash: actual.to_string() }
    }

    #[test]
    fn startup_no_block_is_first_run() {
        assert_eq!(decide_startup(&BlockState::Absent, Some("abc")), StartupDecision::FirstRun);
    }

    #[test]
    fn startup_block_without_hash_is_first_run() {
        let s = BlockState::Present { stored_hash: None, actual_hash: "deadbeef".to_string() };
        assert_eq!(decide_startup(&s, Some("deadbeef")), StartupDecision::FirstRun);
    }

    #[test]
    fn startup_matching_hash_is_trusted() {
        assert_eq!(decide_startup(&present("cafe1234"), Some("cafe1234")), StartupDecision::ResumeTrusted);
    }

    #[test]
    fn startup_recognises_the_safe_state_block() {
        let ss = content_hash(&safe_state_body());
        assert_eq!(decide_startup(&present(&ss), Some("11111111")), StartupDecision::SafeStateStillActive);
    }

    #[test]
    fn startup_unknown_change_is_externally_modified() {
        let s = present("99999999");
        assert_eq!(decide_startup(&s, Some("11111111")), StartupDecision::ExternallyModified);
        assert_eq!(decide_startup(&s, None), StartupDecision::ExternallyModified);
    }

    #[test]
    fn detect_eqapo_config_dir_is_read_only_and_honours_its_contract() {
        // Environment-dependent (EqAPO may or may not be installed), so we can't assert
        // a specific path. What we *can* pin down is the function's contract: it never
        // panics, and whenever it returns Some, that path is an existing directory (the
        // caller relies on this to skip its temp-dir fallback). Read-only — no writes.
        if let Some(dir) = detect_eqapo_config_dir() {
            assert!(dir.is_dir(), "returned {dir:?}, which is not an existing directory");
        }
    }

    #[test]
    fn drives_endpoint_is_read_only_and_total() {
        // Environment-dependent (EqAPO may or may not be attached to anything), so we can
        // only pin the contract: it never panics, whatever endpoint id it is handed —
        // including one that cannot exist, since the picker's list can go stale between
        // enumeration and probe. Read-only — no writes.
        let backend = EqApoBackend::new(std::env::temp_dir());
        for d in cageq_backend::list_render_devices() {
            let _ = backend.drives_endpoint(&d.id);
        }
        assert!(!backend.drives_endpoint("{not-a-real-endpoint}"));
    }

    #[test]
    fn capabilities_report_eqapos_real_constraints() {
        // These three are the whole reason the backend seam exists — if any flips, the
        // orchestrator would silently start pacing writes or transitions wrongly.
        let caps = EqApoBackend::new(std::env::temp_dir()).capabilities();
        assert_eq!(caps.min_write_spacing, EQAPO_MIN_WRITE_SPACING, "§5.3 reload spacing");
        assert!(!caps.owns_transitions, "EqAPO's crossfade is cold + fixed-length; the core morphs");
        assert!(caps.manages_foreign_config, "config.txt is shared with other tools");
    }
}
