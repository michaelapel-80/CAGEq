//! EqAPO `config.txt` writer — device-scoped, marker-delimited, atomic, with a
//! content-hash fingerprint for restart integrity (see filter.md §3.0 / §7.4).
//!
//! This is CAGE's first component. It retires the load-bearing risk (write a
//! block → EqAPO applies it, click-free) and is the one piece §7.4 requires the
//! Rust core to own outright, independent of the Python sidecar (redundant
//! safe-state writes). It depends on nothing else in the stack.
//!
//! Design note for the learning goal: everything except `write_managed_block` /
//! `read_block_state` is a *pure* function (string in, string out, no I/O). That
//! is what makes the fiddly parsing testable in the `#[cfg(test)]` block below
//! without a temp directory — worth internalising as a Rust habit.
//!
//! Deliberately OUT of scope for this first slice — each flagged inline `TODO`:
//!   * more than one CAGE-managed Device: block per file
//!   * preserving a foreign block's exact CRLF style (we emit LF)
//!   * the exact EqAPO filter-token spelling (verify vs. the config reference)
//!   * non-UTF-8 (legacy ANSI) config.txt: EqAPO tolerates it via a per-line
//!     CP_ACP fallback, but we support UTF-8 only and fail loudly with
//!     WriteError::NotUtf8 rather than add byte-level transcoding (see §7.4)
//!
//! Add to Cargo.toml:  sha2 = "0.10"  thiserror = "2"

use std::fmt::Write as _; // brings write!/writeln! for String targets into scope
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// Line ending CAGE emits for its OWN block. EqAPO parses LF fine; a real build
// would likely match the surrounding file's style (see foreign-content TODO).
// The parsing below is newline-agnostic, so this is a safe one-line change.
const NL: &str = "\n";
const BEGIN_PREFIX: &str = "#CAGE:BEGIN";
const END_MARKER: &str = "#CAGE:END";

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
    /// EqAPO's token for this filter type.
    /// TODO(verify): confirm LSC/HSC (shelf-with-Q) vs. LS/HS against the
    /// official EqAPO configuration reference before trusting these strings —
    /// same verify-don't-guess rule the rest of filter.md follows.
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

// `thiserror::Error` derives everything we previously hand-wrote — the entire
// `impl Display`, the `impl std::error::Error`, and the `impl From<io::Error>` all
// collapse into the attributes below. What each attribute generates:
//   * #[derive(thiserror::Error)] — the Display + std::error::Error impls.
//   * #[error("…")] on a variant   — that variant's Display message; `{0}` is its
//                                    first field, so the Io message interpolates
//                                    the wrapped io::Error's own Display.
//   * #[from] on the Io field       — generates `From<io::Error> for WriteError`
//                                    (so `?` still auto-converts) AND registers
//                                    that field as `Error::source()`, keeping the
//                                    cause chain intact. `#[from]` implies `#[source]`.
// `Debug` still has to be derived by hand — Error requires it as a supertrait,
// and thiserror doesn't provide it.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// config.txt or its directory couldn't be read/written.
    #[error("config.txt I/O error: {0}")]
    Io(#[from] io::Error),

    /// The config path had no parent directory — can't place the temp file on
    /// the same volume, which the atomic rename requires (§7.4).
    #[error("config.txt path has no parent directory for the temp file")]
    NoParentDir,

    /// config.txt exists but isn't valid UTF-8 — almost always a legacy file
    /// saved in the system ANSI code page. EqAPO itself tolerates this via a
    /// per-line CP_ACP fallback (verified in FilterEngine.cpp), but CAGE
    /// deliberately supports UTF-8 only and fails loudly here rather than
    /// transcode (a considered scope decision, see filter.md §7.4). The fix for
    /// the user is to re-save config.txt as UTF-8.
    #[error("config.txt is not valid UTF-8 (CAGE requires UTF-8; re-save it as UTF-8)")]
    NotUtf8,
}

/// What the config currently says about CAGE's block for a device — consumed at
/// startup (§3.0) to decide whether settings.json's resume state is trustworthy.
#[derive(Debug)]
pub enum BlockState {
    /// No Device: section, or one with no CAGE block: CAGE has never written
    /// this device. Normal first-run state, not an error.
    Absent,
    /// A CAGE block exists. `stored_hash` is what its BEGIN line claims;
    /// `actual_hash` is recomputed from the body right now. They match iff the
    /// block is byte-unchanged since CAGE last wrote it.
    Present {
        stored_hash: Option<String>,
        actual_hash: String,
    },
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Atomically set CAGE's managed block for `device`, preserving every other
/// Device: block and all foreign content byte-for-byte. Returns the fingerprint
/// now living in the BEGIN line — persist it next to the device's resume state
/// in settings.json (§3.0) for the next-startup integrity check.
///
/// Atomicity (§7.4): the full new text is written to a temp file *in the same
/// directory*, then `fs::rename`d over the target. On Windows that is
/// MoveFileExW with replace semantics — atomic on one volume, which is exactly
/// why the temp file must be a sibling of config.txt, not in %TEMP%.
pub fn write_managed_block(
    config_path: &Path,
    device: &str,
    preamp_db: f64,
    filters: &[Filter],
) -> Result<String, WriteError> {
    config_path.parent().ok_or(WriteError::NoParentDir)?;

    // "File doesn't exist yet" is a normal first run, not an error. A non-UTF-8
    // file (InvalidData is read_to_string's documented signal for exactly that)
    // is turned into the distinct NotUtf8 error rather than a generic Io error.
    let current = match fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(WriteError::NotUtf8),
        Err(e) => return Err(e.into()),
    };

    let (block, hash) = render_cage_block(preamp_db, filters);
    let new_text = splice_cage_block(&current, device, &block);

    let tmp = temp_sibling(config_path);
    fs::write(&tmp, new_text.as_bytes())?;
    if let Err(e) = fs::rename(&tmp, config_path) {
        let _ = fs::remove_file(&tmp); // best-effort cleanup; ignore failure
        return Err(e.into());
    }
    Ok(hash)
}

/// Read the current block's stored vs. recomputed hash for the startup check.
pub fn read_block_state(config_path: &Path, device: &str) -> Result<BlockState, WriteError> {
    let current = match fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BlockState::Absent),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(WriteError::NotUtf8),
        Err(e) => return Err(e.into()),
    };

    let Some((body_start, body_end)) = find_device_section(&current, device) else {
        return Ok(BlockState::Absent);
    };
    let body = &current[body_start..body_end];
    let Some((mb_start, mb_end)) = find_marker_range(body) else {
        return Ok(BlockState::Absent);
    };

    let block = &body[mb_start..mb_end];
    Ok(BlockState::Present {
        stored_hash: parse_begin_hash(block),
        actual_hash: content_hash(block_body(block)),
    })
}

// ---------------------------------------------------------------------------
// Pure helpers — rendering
// ---------------------------------------------------------------------------

/// The EqAPO lines CAGE manages for one device: a Preamp line plus one Filter
/// line per band. This exact text is what the content hash is taken over and
/// what lands between the markers, so `block_body` must reproduce it precisely.
fn render_managed_body(preamp_db: f64, filters: &[Filter]) -> String {
    let mut body = String::new();
    // Writing to a String is infallible, so discarding the Result is fine.
    let _ = write!(body, "Preamp: {:.1} dB{NL}", preamp_db);
    for f in filters {
        let _ = write!(
            body,
            "Filter: ON {} Fc {:.0} Hz Gain {:.1} dB Q {:.2}{NL}",
            f.kind.eqapo_token(),
            f.freq_hz,
            f.gain_db,
            f.q,
        );
    }
    body
}

/// Short content fingerprint over the managed body (§3.0). SHA-256 (not the std
/// `DefaultHasher`) is deliberate: this value is written to disk and re-read by
/// a possibly *newer* build after an app update, so the algorithm must be stable
/// across builds and platforms — `DefaultHasher`'s is not guaranteed to be.
/// Only 4 bytes (8 hex chars) are kept: this is a change-detector, not a
/// security hash, so collision resistance isn't a requirement.
fn content_hash(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    let mut s = String::with_capacity(8);
    for b in &digest[..4] {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Full CAGE block including markers, ready to splice into a device section.
/// Returns the block and its hash so the caller needn't re-parse it back out.
fn render_cage_block(preamp_db: f64, filters: &[Filter]) -> (String, String) {
    let body = render_managed_body(preamp_db, filters);
    let hash = content_hash(&body);
    // body already ends in NL, so END sits on its own line directly after it.
    let block = format!("{BEGIN_PREFIX} hash={hash}{NL}{body}{END_MARKER}{NL}");
    (block, hash)
}

// ---------------------------------------------------------------------------
// Pure helpers — locating & splicing
// ---------------------------------------------------------------------------

/// Insert/replace CAGE's block for `device` in `current`, leaving everything
/// else byte-for-byte intact. Three cases: replace an existing block, insert
/// into an existing device with no block yet, or append a fresh device section.
fn splice_cage_block(current: &str, device: &str, new_block: &str) -> String {
    match find_device_section(current, device) {
        Some((dev_start, dev_end)) => {
            let section = &current[dev_start..dev_end];
            match find_marker_range(section) {
                // Existing CAGE block -> replace it in place.
                Some((mb_start, mb_end)) => {
                    let (abs_start, abs_end) = (dev_start + mb_start, dev_start + mb_end);
                    let mut out = String::with_capacity(current.len() + new_block.len());
                    out.push_str(&current[..abs_start]);
                    out.push_str(new_block);
                    out.push_str(&current[abs_end..]);
                    out
                }
                // Device present, no block yet -> insert at the top of its body,
                // i.e. right after the `Device:` line (= dev_start).
                None => {
                    let mut out = String::with_capacity(current.len() + new_block.len());
                    out.push_str(&current[..dev_start]);
                    out.push_str(new_block);
                    out.push_str(&current[dev_start..]);
                    out
                }
            }
        }
        // Device absent -> append a fresh section at EOF.
        None => {
            let mut out = String::with_capacity(current.len() + device.len() + new_block.len() + 16);
            out.push_str(current);
            if !current.is_empty() && !current.ends_with('\n') {
                out.push_str(NL);
            }
            let _ = write!(out, "Device: {device}{NL}");
            out.push_str(new_block);
            out
        }
    }
}

/// Byte range of the *body* of `device`'s section: the region immediately after
/// the matching `Device: <name>` line, up to the next `Device:` line or EOF.
/// `None` if the device isn't present. TODO: takes the first match on a
/// duplicate device line; also does not handle EqAPO's device-name wildcards.
fn find_device_section(text: &str, device: &str) -> Option<(usize, usize)> {
    let mut body_start: Option<usize> = None;
    let mut offset = 0usize;

    // split_inclusive keeps the trailing '\n', so byte offsets stay exact.
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let is_target = trimmed
            .strip_prefix("Device:")
            .map(|rest| rest.trim() == device)
            .unwrap_or(false);
        let is_any_device = trimmed.trim_start().starts_with("Device:");

        match body_start {
            None if is_target => body_start = Some(offset + line.len()),
            Some(start) if is_any_device => return Some((start, offset)),
            _ => {}
        }
        offset += line.len();
    }
    body_start.map(|start| (start, text.len()))
}

/// Within a device body, the inclusive byte range of an existing CAGE block:
/// from the start of the BEGIN line to just past the END line's newline.
fn find_marker_range(body: &str) -> Option<(usize, usize)> {
    let begin = body
        .match_indices(BEGIN_PREFIX)
        .map(|(i, _)| i)
        .find(|&i| is_line_start(body, i))?;

    let end = body[begin..]
        .match_indices(END_MARKER)
        .map(|(i, _)| begin + i)
        .find(|&i| is_line_start(body, i))?;

    // Extend past the END line's own newline (or to EOF if it's the last line).
    let end_line_end = body[end..].find('\n').map_or(body.len(), |nl| end + nl + 1);
    Some((begin, end_line_end))
}

/// True if byte `idx` begins a line — guards against matching a marker string
/// that happens to appear mid-line rather than as an actual marker.
fn is_line_start(text: &str, idx: usize) -> bool {
    idx == 0 || text.as_bytes()[idx - 1] == b'\n'
}

/// Pull `hash=XXXXXXXX` off the BEGIN line, if present.
fn parse_begin_hash(block: &str) -> Option<String> {
    block
        .lines()
        .next()?
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("hash="))
        .map(str::to_string)
}

/// The body a hash is taken over: everything between the BEGIN line and the END
/// marker. Must reproduce `render_managed_body`'s output exactly (round-tripped
/// in the tests) or the startup integrity check would false-positive forever.
fn block_body(block: &str) -> &str {
    let after_begin = match block.find('\n') {
        Some(i) => &block[i + 1..],
        None => return "",
    };
    match after_begin.rfind(END_MARKER) {
        Some(i) => &after_begin[..i],
        None => after_begin,
    }
}

/// A temp path next to the target so the rename stays on one volume. The PID
/// suffix is enough here — §2's single-instance lock already precludes a second
/// concurrent CAGE writer. TODO: a crash between write and rename can leave a
/// stale `.cage-tmp-*`; a real impl would sweep these on startup.
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

    fn sample() -> Vec<Filter> {
        vec![
            Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 3.0, q: 0.7 },
            Filter { kind: FilterType::Peaking, freq_hz: 2500.0, gain_db: -2.4, q: 1.4 },
        ]
    }

    #[test]
    fn hash_round_trips_through_a_rendered_block() {
        // The invariant the whole restart check leans on: the hash recomputed
        // from a block's extracted body equals the hash stored when writing it.
        let (block, stored) = render_cage_block(-9.0, &sample());
        assert_eq!(content_hash(block_body(&block)), stored);
    }

    #[test]
    fn creates_a_section_in_an_empty_file() {
        let (block, _) = render_cage_block(-9.0, &sample());
        let out = splice_cage_block("", "USB DAC", &block);
        assert!(out.contains("Device: USB DAC"));
        assert!(out.contains(BEGIN_PREFIX) && out.contains(END_MARKER));
    }

    #[test]
    fn leaves_a_foreign_device_block_untouched() {
        let foreign = "Device: Other Speakers\nPreamp: -3.0 dB\nFilter: ON PK Fc 1000 Hz Gain 2 dB Q 1\n";
        let (block, _) = render_cage_block(-9.0, &sample());
        let out = splice_cage_block(foreign, "USB DAC", &block);
        // The foreign section must survive verbatim as a substring.
        assert!(out.contains(foreign), "foreign content was altered");
    }

    #[test]
    fn replaces_in_place_rather_than_duplicating() {
        let (b1, _) = render_cage_block(-9.0, &sample());
        let once = splice_cage_block("", "USB DAC", &b1);
        // A second write with different values must yield exactly one block.
        let (b2, _) = render_cage_block(-6.0, &sample());
        let twice = splice_cage_block(&once, "USB DAC", &b2);
        assert_eq!(twice.matches(BEGIN_PREFIX).count(), 1);
        assert!(twice.contains("Preamp: -6.0 dB"));
        assert!(!twice.contains("Preamp: -9.0 dB"));
    }

    #[test]
    fn error_displays_a_message_and_chains_its_cause() {
        use std::error::Error; // brings the `source()` method into scope for the test

        // `.to_string()` is provided for free by the Display impl (via the ToString
        // blanket impl), so this exercises Display without naming it directly.
        let e = WriteError::NotUtf8;
        assert!(e.to_string().contains("UTF-8"));
        assert!(e.source().is_none()); // originates here, no underlying cause

        // The Io variant should expose its wrapped io::Error as the chain source.
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let wrapped = WriteError::from(io); // uses the From impl -> WriteError::Io
        assert!(wrapped.source().is_some());
    }
}
