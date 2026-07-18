//! Equalizer APO config writer — two-file `Include` architecture (filter.md §3.0/§7.4).
//!
//! Verified against how the reference tool (AQUA) and EqAPO itself work:
//!   * CAGE writes all its filters into its **own** file, `cage.txt`, and puts a
//!     single `Include: cage.txt` line into EqAPO's `config.txt`. This is the
//!     pattern EqAPO's config reference explicitly recommends, and AQUA uses it
//!     (`aqua.txt` + `Include: aqua.txt`, verified in its `flush.ts`).
//!   * EqAPO watches its **whole config directory** for changes
//!     (`FindFirstChangeNotificationW(configPath, bWatchSubtree=true, ...)` in
//!     FilterEngine.cpp) and reloads on any file change — so writing `cage.txt`
//!     triggers a reload + the native 10 ms crossfade exactly like editing
//!     config.txt would.
//!
//! Why this shape is simpler than splicing filters into config.txt directly:
//!   * `cage.txt` is entirely ours -> wholesale atomic overwrite, no read-modify-
//!     write, no foreign content to preserve, no UTF-8/ANSI concern for filters.
//!   * `config.txt` only ever needs one pure-ASCII `Include:` line, wrapped in a
//!     `#CAGE:BEGIN`/`#CAGE:END` comment block so we can add/verify/replace just
//!     that block and leave everything else byte-for-byte intact.
//!
//! Design habit (worth internalising): the pure functions — rendering, hashing,
//! and the config.txt splice/parse — take strings and return strings with no I/O,
//! so the tricky logic is unit-testable without a temp directory. The thin public
//! functions add the fs::read/write/rename around them.
//!
//! Deliberately OUT of scope, flagged inline where relevant:
//!   * preserving a foreign config.txt block's exact CRLF style (we emit LF; only
//!     ever relevant to the one Include block, since cage.txt is fully ours)
//!   * non-UTF-8 (legacy ANSI) config.txt: we support UTF-8 only and fail loudly
//!     with WriteError::NotUtf8 rather than transcode (a scope decision, §7.4)
//!
//! Add to Cargo.toml:  sha2 = "0.10"  thiserror = "2"

use std::fmt::Write as _; // brings write!/writeln! for String targets into scope
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// Line ending CAGE emits. EqAPO parses LF fine (it strips a trailing \r itself).
const NL: &str = "\n";
// Comment markers around the Include line in config.txt. A leading '#' makes each
// a comment EqAPO ignores; only the `Include:` line between them is a real command.
const INCLUDE_BEGIN: &str = "#CAGE:BEGIN";
const INCLUDE_END: &str = "#CAGE:END";
// First line of cage.txt: a comment (no ':', starts with '#', so EqAPO ignores it)
// that carries the integrity hash of everything below it.
const CAGE_HEADER_PREFIX: &str = "# CAGE managed file - generated, do not edit. hash=";
/// The filename CAGE writes its filters into, alongside config.txt.
pub const CAGE_FILENAME: &str = "cage.txt";

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    Peaking,   // PK
    LowShelf,  // LSC
    HighShelf, // HSC
}

impl FilterType {
    /// EqAPO's token for this filter type. VERIFIED against AutoEq's own
    /// EqualizerAPO exporter (`frequency_response.py::write_eqapo_parametric_eq`,
    /// `types = {Peaking: 'PK', LowShelf: 'LSC', HighShelf: 'HSC'}`): `LSC`/`HSC`
    /// are the center-frequency shelves taking an Fc/Gain/Q triple (the RBJ-biquad-
    /// with-Q form AutoEq emits), not `LS`/`HS` or the slope-based `LSC x dB`.
    fn eqapo_token(self) -> &'static str {
        match self {
            FilterType::Peaking => "PK",
            FilterType::LowShelf => "LSC",
            FilterType::HighShelf => "HSC",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub kind: FilterType,
    pub freq_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

/// One device's managed configuration — becomes one `Device:` block in cage.txt.
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    pub device: String,
    pub preamp_db: f64,
    pub filters: Vec<Filter>,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("config.txt/cage.txt I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("config path has no parent directory for the temp file")]
    NoParentDir,
    /// config.txt exists but isn't valid UTF-8 — almost always a legacy ANSI
    /// file. EqAPO tolerates it via a per-line CP_ACP fallback (FilterEngine.cpp),
    /// but CAGE supports UTF-8 only and fails loudly here rather than transcode
    /// (§7.4). The fix for the user is to re-save config.txt as UTF-8.
    #[error("config.txt is not valid UTF-8 (CAGE requires UTF-8; re-save it as UTF-8)")]
    NotUtf8,
}

/// What cage.txt currently says — consumed at startup (§3.0) to decide whether
/// settings.json's remembered resume state is still trustworthy.
#[derive(Debug)]
pub enum BlockState {
    /// No cage.txt, or one without a hash header: CAGE has never written it (or it
    /// was wiped). Normal first-run state, not an error.
    Absent,
    /// A CAGE-written cage.txt exists. `stored_hash` is what its header claims;
    /// `actual_hash` is recomputed from the body now. (`decide_startup` trusts the
    /// settings.json hash over `stored_hash`, which a hand-edit could forge.)
    Present {
        stored_hash: Option<String>,
        actual_hash: String,
    },
}

/// The verdict of the startup integrity check (§3.0) — how far to trust the
/// resume state remembered in settings.json.
#[derive(Debug, PartialEq, Eq)]
pub enum StartupDecision {
    /// No cage.txt yet (or one without a hash header). Start clean.
    FirstRun,
    /// cage.txt is byte-identical to what CAGE last wrote (hash matches
    /// settings.json). The remembered resume state is trustworthy.
    ResumeTrusted,
    /// cage.txt is CAGE's hardcoded safe-state config (§7.2): the safety shutdown
    /// was still active at the last exit/crash. Offer to restore the last state.
    SafeStateStillActive,
    /// cage.txt differs from what CAGE last wrote and isn't the safe-state config —
    /// changed by the user or another tool. Surface a neutral notice; don't keep
    /// asserting a specific preset/slot is active (fail-early).
    ExternallyModified,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Apply `configs` to Equalizer APO: ensure config.txt includes cage.txt, then
/// (atomically) write cage.txt with the given per-device blocks. `config_dir` is
/// EqAPO's config directory (where config.txt lives). Returns cage.txt's content
/// hash — persist it in settings.json for the next-startup integrity check (§3.0).
///
/// The `ensure_include` step is a cheap no-op after the first run (it only rewrites
/// config.txt when the Include block is missing/wrong), so the steady-state cost of
/// a change is a single cage.txt write -> one EqAPO reload -> one native crossfade.
pub fn apply(config_dir: &Path, configs: &[DeviceConfig]) -> Result<String, WriteError> {
    ensure_include(&config_dir.join("config.txt"), CAGE_FILENAME)?;
    write_cage_txt(&config_dir.join(CAGE_FILENAME), configs)
}

/// Atomically overwrite cage.txt with the rendered device blocks. Returns the
/// content hash written into the header. cage.txt is fully CAGE-owned, so this is
/// a plain wholesale write — no splicing, no foreign content.
pub fn write_cage_txt(cage_path: &Path, configs: &[DeviceConfig]) -> Result<String, WriteError> {
    let (text, hash) = render_cage_txt(configs);
    atomic_write(cage_path, text.as_bytes())?;
    Ok(hash)
}

/// Ensure config.txt contains the `#CAGE:BEGIN`/`Include: <cage_filename>`/
/// `#CAGE:END` block, preserving all other content byte-for-byte. Idempotent:
/// returns `Ok(false)` and writes nothing when the block is already exactly right,
/// `Ok(true)` when it added or fixed it. Appends at EOF when absent — placing it
/// last means cage.txt's own leading `Device:` line controls scope and nothing
/// after it is affected (the AQUA-proven placement).
pub fn ensure_include(config_txt_path: &Path, cage_filename: &str) -> Result<bool, WriteError> {
    let current = read_utf8_or_empty(config_txt_path)?;
    match splice_include(&current, cage_filename) {
        Some(new_text) => {
            atomic_write(config_txt_path, new_text.as_bytes())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Atomically write CAGE's hardcoded safe-state (§7.1/7.2) to cage.txt, using the
/// same header+body format as a normal write so [`read_cage_state`] +
/// [`decide_startup`] recognise it as [`StartupDecision::SafeStateStillActive`].
pub fn write_safe_state(cage_path: &Path) -> Result<(), WriteError> {
    let (text, _) = wrap_with_hash_header(&safe_state_body());
    atomic_write(cage_path, text.as_bytes())
}

/// Read cage.txt's integrity state for the startup check (§3.0).
pub fn read_cage_state(cage_path: &Path) -> Result<BlockState, WriteError> {
    match fs::read_to_string(cage_path) {
        Ok(s) => Ok(cage_state_from_text(&s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BlockState::Absent),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(WriteError::NotUtf8),
        Err(e) => Err(e.into()),
    }
}

/// Decide, from cage.txt's on-disk state and the hash CAGE recorded in
/// settings.json, how much of the remembered resume state to trust at startup
/// (§3.0). Pure — no I/O — so the whole decision table is unit-testable.
///
/// `state` comes from [`read_cage_state`]; `expected_hash` is the device's
/// `last_written_hash` from settings.json, or `None` if there's no record yet.
/// The trust signal is `actual_hash == expected_hash`: settings can't be forged by
/// hand-editing cage.txt's own header, so it's the stronger of the two hashes.
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

/// CAGE's hardcoded safe-state cage.txt body (§7.2): effective silence on all
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
// Pure helpers — rendering cage.txt
// ---------------------------------------------------------------------------

/// One `Device:` block: the device line, a Preamp line, then one Filter line per
/// band. Line format verified byte-for-byte against AutoEq's exporter
/// (frequency_response.py, line 213): 1-based filter number (EqAPO ignores it but
/// AutoEq/REW emit it), then `ON <type> Fc <fc:.0> Hz Gain <gain:.1> dB Q <q:.2>`.
fn render_device_block(cfg: &DeviceConfig) -> String {
    let mut s = String::new();
    let _ = write!(s, "Device: {}{NL}", cfg.device);
    let _ = write!(s, "Preamp: {:.1} dB{NL}", cfg.preamp_db);
    for (i, f) in cfg.filters.iter().enumerate() {
        let _ = write!(
            s,
            "Filter {}: ON {} Fc {:.0} Hz Gain {:.1} dB Q {:.2}{NL}",
            i + 1,
            f.kind.eqapo_token(),
            f.freq_hz,
            f.gain_db,
            f.q,
        );
    }
    s
}

/// The full cage.txt text (hash header + all device blocks) and its content hash.
fn render_cage_txt(configs: &[DeviceConfig]) -> (String, String) {
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
    (format!("{CAGE_HEADER_PREFIX}{hash}{NL}{body}"), hash)
}

/// Parse cage.txt text into a [`BlockState`]: pull the hash out of the header line
/// and recompute it over the body. Must mirror `wrap_with_hash_header` exactly, or
/// the startup check would false-positive forever (guarded by a round-trip test).
fn cage_state_from_text(text: &str) -> BlockState {
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
fn render_include_block(cage_filename: &str) -> String {
    format!("{INCLUDE_BEGIN}{NL}Include: {cage_filename}{NL}{INCLUDE_END}{NL}")
}

/// Compute the new config.txt content that ensures the Include block is present
/// and correct, or `None` if it's already exactly right (so no write is needed).
/// Everything outside the block is preserved byte-for-byte.
fn splice_include(current: &str, cage_filename: &str) -> Option<String> {
    let block = render_include_block(cage_filename);
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

/// Inclusive byte range of an existing CAGE marker block: from the start of the
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
    let tmp = temp_sibling(target);
    fs::write(&tmp, bytes)?;
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp); // best-effort cleanup
        return Err(e.into());
    }
    Ok(())
}

/// A temp path next to the target so the rename stays on one volume. The PID
/// suffix suffices — §2's single-instance lock precludes a second concurrent
/// writer. TODO: a crash between write and rename can leave a stale `.cage-tmp-*`;
/// a real impl would sweep these on startup.
fn temp_sibling(target: &Path) -> PathBuf {
    let mut name = target.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".cage-tmp-{}", std::process::id()));
    target.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Tests — pure functions only, no disk I/O
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn cage_text_hash_round_trips() {
        // The invariant the whole restart check leans on: the hash recomputed from
        // a written cage.txt equals the hash returned when writing it.
        let (text, stored) = render_cage_txt(&sample());
        match cage_state_from_text(&text) {
            BlockState::Present { stored_hash, actual_hash } => {
                assert_eq!(stored_hash.as_deref(), Some(stored.as_str()));
                assert_eq!(actual_hash, stored);
            }
            BlockState::Absent => panic!("freshly rendered cage.txt must parse as Present"),
        }
    }

    #[test]
    fn include_block_appended_when_absent() {
        let out = splice_include("", CAGE_FILENAME).expect("empty file needs the block added");
        assert!(out.contains(INCLUDE_BEGIN));
        assert!(out.contains("Include: cage.txt"));
        assert!(out.contains(INCLUDE_END));
    }

    #[test]
    fn include_block_is_noop_when_already_correct() {
        let existing = render_include_block(CAGE_FILENAME);
        assert!(splice_include(&existing, CAGE_FILENAME).is_none());
    }

    #[test]
    fn include_preserves_foreign_config_content() {
        let foreign = "Device: Other\nFilter: ON PK Fc 1000 Hz Gain 2 dB Q 1\n";
        let out = splice_include(foreign, CAGE_FILENAME).expect("block must be appended");
        assert!(out.contains(foreign), "foreign content was altered");
        assert!(out.contains("Include: cage.txt"));
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
}
